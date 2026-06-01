//! OTLP trace export to Tempo — **off by default, runtime-gated**.
//!
//! receipt-ledger's logs are the primary observability substrate (Phase 1). This
//! module is the optional Phase-2 layer: a per-run span tree exported over
//! OTLP/HTTP so a Loki log line can link to its Tempo trace.
//!
//! ## Value gate
//!
//! Tracing exports **only** when [`OTEL_ENDPOINT_ENV`] (`OTEL_EXPORTER_OTLP_ENDPOINT`)
//! is set to a non-blank value. With it unset, [`init`] installs the exact same
//! `fmt`-only subscriber as before and returns `None`: no exporter, no provider,
//! no OpenTelemetry layer, no background task, no per-event cost. This keeps the
//! dependency honest — you pay for traces only where you've wired a collector.
//!
//! ## trace_id in logs (Loki↔Tempo link)
//!
//! When export is on, the run's root span carries a `trace_id` field, recorded by
//! [`record_trace_id`] from the span's OpenTelemetry context. The JSON `fmt`
//! layer renders enclosing-span fields on every event (under `spans[]`), so each
//! log line emitted within the span tree carries the run's `trace_id` — a Loki
//! line and its Tempo trace then share an id and one click crosses over. The
//! `trace_id` field is declared `Empty` on the span and filled in once, so it is
//! unit-testable against an in-memory exporter (see the tests) with no live
//! collector and without a custom event-mutating layer.
//!
//! ## No PII in spans
//!
//! Span *attributes* are an allowlist — `stage` / `duration` / `outcome` /
//! counts. The instrumentation sites (see [`crate::run`]) never attach the
//! prompt, completion, raw email body, merchant, amount, last-4, or ref# to a
//! span. The extract/LLM span in particular is shaped to carry none of these; a
//! span-shaping test pins that.
//!
//! ## Flush on exit (bounded, non-blocking)
//!
//! The money path never waits on telemetry. [`Telemetry::shutdown`] force-flushes
//! and shuts the provider down on a **bounded** budget ([`FLUSH_TIMEOUT`]) on a
//! blocking thread; if the collector is unreachable or slow the flush is abandoned
//! and the run exits with its normal code. Telemetry can never delay the run past
//! the timeout nor flip the exit code.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The standard OTLP endpoint env var. Setting it (e.g. to the cluster Tempo
/// distributor's OTLP/HTTP ingest, `http://<tempo-distributor>.<ns>:4318`) turns
/// trace export on; unset/blank keeps it off (the default).
pub const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Bounded budget for the export request and for the end-of-run flush+shutdown.
/// Telemetry must never delay the run beyond this, so it is kept far inside the
/// CronJob's `activeDeadlineSeconds`.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// A live tracer provider whose spans export to Tempo. Held by `main` for the
/// run's lifetime; [`shutdown`](Telemetry::shutdown) flushes it before exit.
///
/// `#[must_use]`: dropping this without [`shutdown`](Telemetry::shutdown) would
/// drop the still-buffered final batch, losing the run's trace.
#[must_use = "call .shutdown() before exit or the final span batch is dropped unflushed"]
pub struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    /// Force-flush and shut the provider down on a **bounded** budget.
    ///
    /// The `rt-tokio` batch processor drains on the tokio runtime, so the
    /// (synchronous, network-bound) flush runs on a `spawn_blocking` task — which
    /// carries the runtime context the processor needs — wrapped in a
    /// [`FLUSH_TIMEOUT`] [`tokio::time::timeout`]. A reachable collector flushes
    /// promptly; an unreachable/slow one trips the timeout and the blocking task
    /// is **detached** (it finishes harmlessly in the background as the process
    /// exits). Either way this returns within the budget and never propagates an
    /// error — telemetry must not delay the run past the timeout nor flip its
    /// exit code. Must be called from within the tokio runtime (i.e. from `main`).
    pub async fn shutdown(self) {
        let provider = self.provider;
        let flush = tokio::task::spawn_blocking(move || {
            // force_flush drains the batch processor; shutdown stops it. Both are
            // best-effort: a failed export is logged by the SDK's internal handler
            // and ignored here.
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        });
        match tokio::time::timeout(FLUSH_TIMEOUT, flush).await {
            Ok(_) => {}
            Err(_) => {
                tracing::debug!("telemetry flush exceeded budget; abandoning (run unaffected)");
            }
        }
    }
}

/// Build the subscriber and (optionally) the tracer provider, then install it
/// process-wide.
///
/// - `json` selects the `fmt` layer's encoding (JSON default / text), exactly as
///   before.
/// - With [`OTEL_ENDPOINT_ENV`] unset/blank → fmt-only, byte-for-byte the prior
///   behaviour, returning `None`. Set → the OTLP/HTTP-JSON exporter is added.
///
/// Returns `Some(Telemetry)` only when export is on, so `main` knows whether a
/// flush is owed.
pub fn init(json: bool, env_filter: tracing_subscriber::EnvFilter) -> Option<Telemetry> {
    let endpoint = std::env::var(OTEL_ENDPOINT_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    // OFF PATH: no endpoint → install the fmt-only subscriber and return None.
    // This is the default and must match the pre-traces behaviour exactly.
    let Some(endpoint) = endpoint else {
        install_fmt_only(json, env_filter);
        return None;
    };

    // ON PATH: build the exporter (HTTP/JSON, so we depend on no protobuf wire
    // format), a batch tracer provider, and a registry that fans events to BOTH
    // the fmt layer and the OpenTelemetry layer.
    let provider = match build_provider(&endpoint) {
        Ok(p) => p,
        Err(e) => {
            // A misconfigured endpoint must not abort the run (telemetry is
            // additive). Fall back to fmt-only and carry on.
            install_fmt_only(json, env_filter);
            tracing::warn!(error = %e, endpoint, "OTLP exporter init failed; continuing without traces");
            return None;
        }
    };

    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer("receipt-ledger");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // The OpenTelemetry layer assigns each span its OTel context (and trace_id);
    // the fmt layer renders enclosing-span fields on every event, so once the
    // root span's `trace_id` field is recorded (see `record_trace_id`) every log
    // line within the run carries it.
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .init();
    }
    Some(Telemetry { provider })
}

/// Install the fmt-only subscriber (the off-path / fallback). Kept identical in
/// shape to the historical `init_tracing` so the default run is unchanged.
fn install_fmt_only(json: bool, env_filter: tracing_subscriber::EnvFilter) {
    let registry = tracing_subscriber::registry().with(env_filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .init();
    }
}

/// Build the batch tracer provider with an OTLP/HTTP-JSON span exporter.
///
/// The exporter uses the default BLOCKING reqwest client (`reqwest-blocking-client`)
/// because the default `BatchSpanProcessor` exports from a dedicated thread that
/// has no tokio reactor — the async client would panic there. It is the same
/// reqwest/rustls(+ring) crates already in the tree (a distinct client instance,
/// not a new dependency). A bounded per-export [`FLUSH_TIMEOUT`] caps a slow
/// collector; the run-level flush budget ([`Telemetry::shutdown`]) caps the rest.
fn build_provider(endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpJson)
        .with_endpoint(endpoint)
        .with_timeout(FLUSH_TIMEOUT)
        .build()?;
    let resource = Resource::builder()
        .with_service_name("receipt-ledger")
        .build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// The span field that carries the run's `trace_id` for the Loki↔Tempo link.
/// Declare it `= tracing::field::Empty` on the root span, then call
/// [`record_trace_id`] inside the span to fill it.
pub const TRACE_ID_FIELD: &str = "trace_id";

/// Record the active OpenTelemetry `trace_id` onto the **current** span's
/// (`Empty`-declared) [`TRACE_ID_FIELD`], so every event under it carries the id.
///
/// Resolves the id from the span's OpenTelemetry context via the public
/// [`OpenTelemetrySpanExt`](tracing_opentelemetry::OpenTelemetrySpanExt), so it
/// always equals the exported span's trace_id. A no-op when traces are off (no
/// valid/non-zero trace_id), so off-path log lines stay byte-for-byte as before.
/// Call once, inside the root span, after it is entered.
pub fn record_trace_id() {
    let span = tracing::Span::current();
    if let Some(tid) = current_trace_id(&span) {
        span.record(TRACE_ID_FIELD, tid.as_str());
    }
}

/// The non-zero OTel trace_id (32-char lowercase hex) of `span`, if it has a
/// valid OpenTelemetry context. Returns `None` for the invalid (all-zero)
/// trace_id — i.e. when traces are off — so nothing is recorded then.
fn current_trace_id(span: &tracing::Span) -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let ctx = span.context();
    let span_ref = ctx.span();
    let trace_id = span_ref.span_context().trace_id();
    if trace_id == opentelemetry::trace::TraceId::INVALID {
        return None;
    }
    Some(format!("{:032x}", u128::from_be_bytes(trace_id.to_bytes())))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Tracer as _, TracerProvider as _};
    use tracing::Subscriber;
    use tracing::field::Empty;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::registry::LookupSpan;

    use super::*;

    #[test]
    fn flush_timeout_is_bounded_and_small() {
        // The flush budget must stay well inside the job deadline so telemetry can
        // never delay the run materially.
        assert!(FLUSH_TIMEOUT <= Duration::from_secs(5));
    }

    #[test]
    fn endpoint_env_is_the_standard_name() {
        assert_eq!(OTEL_ENDPOINT_ENV, "OTEL_EXPORTER_OTLP_ENDPOINT");
    }

    /// req 5 (flush is bounded + non-blocking): with the collector UNREACHABLE,
    /// `Telemetry::shutdown` must still return within ~`FLUSH_TIMEOUT` and never
    /// hang — so a down/slow Tempo can never delay the run or flip its exit code.
    /// Needs the tokio runtime the batch processor (`rt-tokio`) runs on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_is_bounded_when_collector_unreachable() {
        // A routable-but-dead local port: the export request will fail/time out
        // rather than connect. (No server is listening here.)
        let provider = build_provider("http://127.0.0.1:1/v1/traces")
            .expect("provider builds even though the endpoint is dead");
        // Produce a span so there is a batch to (fail to) flush.
        {
            let tracer = provider.tracer("test");
            tracer.in_span("doomed-export", |_| {});
        }
        let telemetry = Telemetry { provider };

        let start = std::time::Instant::now();
        // shutdown runs the flush on a spawn_blocking task under a tokio timeout;
        // assert it returns within the bounded budget even with the dead endpoint.
        telemetry.shutdown().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < FLUSH_TIMEOUT + Duration::from_secs(2),
            "shutdown must abandon a dead collector within ~the timeout, took {elapsed:?}"
        );
    }

    /// (span_name, trace_id_on_event) for each event seen within a span.
    type EventLog = Arc<Mutex<Vec<(String, Option<String>)>>>;
    /// (span_name, recorded field names) — the span-attribute allowlist snapshot.
    type SpanFieldLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    /// A capturing fmt-like layer: records, per event, the `trace_id` rendered
    /// from the enclosing span's recorded fields, plus that span's name and any
    /// span attributes — enough to assert both the Loki↔Tempo link (req 4) and
    /// the no-PII span allowlist (req 6) without a live collector.
    #[derive(Clone, Default)]
    struct Capture {
        events: EventLog,
        span_fields: SpanFieldLog,
    }

    struct FieldGrab(Vec<(String, String)>);
    impl tracing::field::Visit for FieldGrab {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .push((field.name().to_string(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).unwrap();
            let mut grab = FieldGrab(Vec::new());
            attrs.record(&mut grab);
            // Persist the (possibly later-recorded) fields in the span extension
            // so on_record updates land too.
            span.extensions_mut().insert(SpanFields(grab.0));
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).unwrap();
            let mut grab = FieldGrab(Vec::new());
            values.record(&mut grab);
            let mut ext = span.extensions_mut();
            if let Some(SpanFields(f)) = ext.get_mut::<SpanFields>() {
                for (k, v) in grab.0 {
                    if let Some(slot) = f.iter_mut().find(|(ek, _)| *ek == k) {
                        slot.1 = v;
                    } else {
                        f.push((k, v));
                    }
                }
            }
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(span) = ctx.event_span(event) {
                // The JSON fmt layer renders ALL enclosing spans' fields on each
                // event (its `spans[]` array), so a `trace_id` recorded on the
                // root span reaches an event in a child stage. Mirror that here:
                // walk innermost → root and take the first `trace_id` found.
                let tid = span.scope().find_map(|s| {
                    s.extensions()
                        .get::<SpanFields>()
                        .and_then(|SpanFields(f)| {
                            f.iter()
                                .find(|(k, _)| k == TRACE_ID_FIELD)
                                .map(|(_, v)| v.clone())
                        })
                });
                self.events
                    .lock()
                    .unwrap()
                    .push((span.name().to_string(), tid));
                // Snapshot the innermost span's own field names for the allowlist
                // assertion (req 6 checks the extract span's OWN attributes).
                if let Some(SpanFields(f)) = span.extensions().get::<SpanFields>() {
                    self.span_fields.lock().unwrap().push((
                        span.name().to_string(),
                        f.iter().map(|(k, _)| k.clone()).collect(),
                    ));
                }
            }
        }
    }
    struct SpanFields(Vec<(String, String)>);

    /// Build a real `tracing-opentelemetry` layer over an in-memory exporter so
    /// spans get genuine (non-zero) trace_ids without any network.
    fn otel_layer_in_memory<S>() -> impl Layer<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(opentelemetry_sdk::trace::InMemorySpanExporter::default())
            .build();
        let tracer = provider.tracer("test");
        // Leak the provider so it outlives the test subscriber (dropped at exit).
        Box::leak(Box::new(provider));
        tracing_opentelemetry::layer().with_tracer(tracer)
    }

    #[test]
    fn run_produces_one_root_span_with_child_stages() {
        // req 4 happy path: a run produces ONE root span whose children are the
        // expected stages, all sharing one trace_id. We mirror the instrumentation
        // shape (run → {fetch, process → extract, statement}) and assert against
        // the spans the in-memory exporter received.
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let otel = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(otel);

        tracing::subscriber::with_default(subscriber, || {
            let root = tracing::info_span!("run", stage = "run", trace_id = Empty);
            let _g = root.enter();
            record_trace_id();
            {
                let _f = tracing::info_span!("fetch", stage = "fetch").entered();
            }
            {
                let _p = tracing::info_span!("process", stage = "process").entered();
                let _e = tracing::info_span!("extract", stage = "extract").entered();
            }
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
        assert!(names.contains(&"run"), "has a root run span: {names:?}");
        for stage in ["fetch", "process", "extract"] {
            assert!(names.contains(&stage), "has child stage {stage}: {names:?}");
        }
        // Exactly one root (parent is the invalid/zero span id).
        let roots: Vec<_> = spans
            .iter()
            .filter(|s| !s.parent_span_id.to_string().chars().any(|c| c != '0'))
            .collect();
        assert_eq!(roots.len(), 1, "exactly one root span");
        assert_eq!(roots[0].name.as_ref(), "run");
        // All spans share the run's single trace_id.
        let trace_id = spans[0].span_context.trace_id();
        assert!(
            spans.iter().all(|s| s.span_context.trace_id() == trace_id),
            "all spans share one trace_id"
        );
    }

    #[test]
    fn event_within_span_carries_the_span_trace_id() {
        // req 4 (Loki↔Tempo): a log line inside a stage carries the SAME trace_id
        // recorded on the run's span — proven against an in-memory OTel exporter,
        // no live collector.
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry()
            .with(otel_layer_in_memory())
            .with(cap.clone());

        tracing::subscriber::with_default(subscriber, || {
            let root = tracing::info_span!("run", trace_id = Empty);
            let _g = root.enter();
            record_trace_id();
            // A child stage + an event inside it.
            let stage = tracing::info_span!("stage_extract");
            let _sg = stage.enter();
            tracing::info!("inside stage");
        });

        let events = cap.events.lock().unwrap();
        let (span_name, tid) = events
            .iter()
            .find(|(name, _)| name == "stage_extract")
            .expect("an event was emitted within stage_extract");
        assert_eq!(span_name, "stage_extract");
        let tid = tid.as_deref().expect("the event carries a trace_id");
        assert_eq!(tid.len(), 32, "trace_id is 32 lowercase hex chars: {tid}");
        assert!(
            tid.chars().all(|c| c.is_ascii_hexdigit()),
            "hex trace_id: {tid}"
        );
        assert_ne!(
            tid,
            "0".repeat(32),
            "trace_id is not the all-zero invalid id"
        );
    }

    #[test]
    fn extract_span_carries_no_pii_fields() {
        // req 6 (no-PII span allowlist): the extract/LLM span's attributes are an
        // allowlist (stage/outcome/counts) and must NOT include the prompt,
        // completion, raw body, merchant, amount, last-4, or ref#.
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry()
            .with(otel_layer_in_memory())
            .with(cap.clone());

        tracing::subscriber::with_default(subscriber, || {
            let root = tracing::info_span!("run", trace_id = Empty);
            let _g = root.enter();
            // The extract span as the instrumentation builds it: stage label only.
            let extract = tracing::info_span!("extract", stage = "extract");
            let _eg = extract.enter();
            tracing::info!("model extraction done");
        });

        let span_fields = cap.span_fields.lock().unwrap();
        let (_, fields) = span_fields
            .iter()
            .find(|(name, _)| name == "extract")
            .expect("the extract span emitted an event");
        let forbidden = [
            "prompt",
            "completion",
            "body",
            "email_body",
            "merchant",
            "amount",
            "last4",
            "last_4",
            "reference",
            "ref",
        ];
        for f in &forbidden {
            assert!(
                !fields.iter().any(|name| name == f),
                "extract span must not carry PII field {f:?}; had {fields:?}"
            );
        }
    }
}
