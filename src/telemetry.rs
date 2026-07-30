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
//! The money path never waits on telemetry. [`Telemetry::shutdown`] shuts the
//! provider down on a **bounded** budget ([`FLUSH_TIMEOUT`]) on a blocking thread;
//! if the collector is unreachable or slow the shutdown is abandoned and the run
//! exits with its normal code. Telemetry can never delay the run past the timeout
//! nor flip the exit code.
//!
//! The bound has a subtlety: when the [`tokio::time::timeout`] fires it *detaches*
//! the `spawn_blocking` task — but the multi-threaded runtime's `Drop` then waits
//! for outstanding blocking tasks to finish (blocking threads can't be safely
//! interrupted), so a half-open collector would still delay process exit well past
//! [`FLUSH_TIMEOUT`]. The bound is therefore guaranteed in `main`, which calls
//! [`std::process::exit`] right after [`Telemetry::shutdown`] returns — terminating
//! the process without running the runtime destructor.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Attach the `fmt` layer (JSON or human-readable) to a subscriber registry and
/// install it process-wide. A macro because `registry.with(json_layer)` and
/// `registry.with(text_layer)` produce different types — a function cannot return
/// both without boxing, and boxing would impose runtime cost on every log event
/// for no value. The macro is private to this module and used exactly twice.
macro_rules! init_with_fmt {
    ($registry:expr, $json:expr) => {
        if $json {
            $registry
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        } else {
            $registry
                .with(tracing_subscriber::fmt::layer().with_target(false))
                .init();
        }
    };
}

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
    /// Shut the provider down on a **bounded** budget, draining the final batch.
    ///
    /// The export runs on the SDK's own dedicated thread via a BLOCKING reqwest
    /// client (no `rt-tokio`; see [`build_provider`]). `provider.shutdown()` itself
    /// drains the `BatchSpanProcessor` (a final export) then stops it and `join`s
    /// that thread — a synchronous, network-bound, potentially blocking call — so
    /// it runs on a `spawn_blocking` task wrapped in a [`FLUSH_TIMEOUT`]
    /// [`tokio::time::timeout`]. A reachable collector drains promptly; an
    /// unreachable/half-open one trips the timeout and the blocking task is
    /// **detached**.
    ///
    /// A detached blocking task would otherwise stall the multi-threaded runtime's
    /// destructor (it waits for outstanding blocking threads), so the hard bound is
    /// completed by `main` calling [`std::process::exit`] immediately after this
    /// returns — the process terminates without running the runtime destructor.
    /// This never propagates an error and never flips the exit code. Must be called
    /// from within the tokio runtime (i.e. from `main`).
    pub async fn shutdown(self) {
        let provider = self.provider;
        let flush = tokio::task::spawn_blocking(move || {
            // `shutdown()` already drains+exports the buffered batch before stopping
            // the processor, so a separate `force_flush()` is redundant. The SDK's
            // own thread-join inside `shutdown()` calls `handle.join().unwrap()`,
            // which can panic if that thread already unwound; isolate it so a flush
            // panic can never abort the process or poison anything. Best-effort: a
            // failed export is logged by the SDK's internal handler and ignored.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = provider.shutdown();
            }));
        });
        if tokio::time::timeout(FLUSH_TIMEOUT, flush).await.is_err() {
            tracing::debug!("telemetry flush exceeded budget; abandoning (run unaffected)");
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
    let endpoint = crate::config::optional(OTEL_ENDPOINT_ENV);

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
            // Sanitize before logging: an operator could embed credentials in the
            // URL (`http://user:pass@host:4318`); the raw string must never reach
            // Loki. `sanitize_endpoint` strips any userinfo.
            tracing::warn!(error = %e, endpoint = %sanitize_endpoint(&endpoint), "OTLP exporter init failed; continuing without traces");
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
    init_with_fmt!(registry, json);
    Some(Telemetry { provider })
}

/// Install the fmt-only subscriber (the off-path / fallback). Kept identical in
/// shape to the historical `init_tracing` so the default run is unchanged.
fn install_fmt_only(json: bool, env_filter: tracing_subscriber::EnvFilter) {
    let registry = tracing_subscriber::registry().with(env_filter);
    init_with_fmt!(registry, json);
}

/// The OTLP/HTTP signal path for traces, per the OTLP spec's endpoint rules.
///
/// The SDK appends this itself **only** when it reads [`OTEL_ENDPOINT_ENV`] from
/// the environment on its own. An endpoint handed to `with_endpoint` is treated
/// as programmatic configuration and used **verbatim** — see `resolve_http_endpoint`
/// in `opentelemetry-otlp`, whose first branch returns `provided_endpoint` unmodified.
/// [`init`] reads the variable itself (it is the on/off gate) and therefore owes
/// the append; without it every export POSTs to the collector's root path, which
/// Tempo answers with 404.
const TRACES_PATH: &str = "/v1/traces";

/// Append [`TRACES_PATH`] to a base OTLP endpoint, idempotently.
///
/// `http://tempo.monitor:4318` (and its trailing-slash form) → `.../v1/traces`;
/// an endpoint that already names the signal path is returned unchanged, so an
/// operator may set either form of [`OTEL_ENDPOINT_ENV`] and get one working URL.
fn traces_endpoint(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(TRACES_PATH) {
        base.to_string()
    } else {
        format!("{base}{TRACES_PATH}")
    }
}

/// Build the batch tracer provider with an OTLP/HTTP-JSON span exporter.
///
/// `endpoint` is the **base** collector URL (the [`OTEL_ENDPOINT_ENV`] value);
/// [`traces_endpoint`] resolves it to the signal URL actually POSTed to.
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
        .with_endpoint(traces_endpoint(endpoint))
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

/// Strip any `userinfo` (`user:pass@`) from an OTLP endpoint URL so embedded
/// credentials never reach the logs. Dependency-free: splits on the first `://`
/// and drops everything up to and including the last `@` in the authority (the
/// segment before the next `/`). A URL with no scheme or no userinfo is returned
/// unchanged. This is a log-sanitizer, not a URL parser — it does not validate.
fn sanitize_endpoint(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return endpoint.to_string();
    };
    // The authority ends at the first '/' (path), if any; userinfo lives only in
    // the authority, so confine the '@' search to it.
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let host = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    match path {
        Some(p) => format!("{scheme}://{host}/{p}"),
        None => format!("{scheme}://{host}"),
    }
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

// -- telemetry unit tests --
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

    /// req 5 (flush is bounded + non-blocking): against a collector that accepts
    /// (or never answers) the TCP connection but never responds, `Telemetry::
    /// shutdown` must still return within ~`FLUSH_TIMEOUT` and never hang — so a
    /// down/slow Tempo can never delay the run or flip its exit code.
    ///
    /// The endpoint is an unroutable TEST-NET / private address (`10.255.255.1`):
    /// the connect HANGS until the export's own timeout, exercising the half-open /
    /// slow-collector path (NOT an instantly-refused port like `:1`, which would
    /// return immediately and never prove the bound is enforced by our timeout).
    /// The bound here is the `Telemetry::shutdown` `tokio::time::timeout` over the
    /// `spawn_blocking` task; the process-level hard bound (`std::process::exit`
    /// after this returns, skipping the runtime destructor that would otherwise
    /// await the detached blocking task) lives in `main` and can't be unit-tested
    /// without forking. We assert the in-task bound is TIGHT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_is_bounded_when_collector_unreachable() {
        // A non-routable address: the TCP connect hangs rather than being refused,
        // so the export sits until its own timeout — the slow-collector case.
        let provider = build_provider("http://10.255.255.1:4318/v1/traces")
            .expect("provider builds even though the endpoint is unreachable");
        // Produce a span so there is a batch to (fail to) flush.
        {
            let tracer = provider.tracer("test");
            tracer.in_span("doomed-export", |_| {});
        }
        let telemetry = Telemetry { provider };

        let start = std::time::Instant::now();
        // shutdown runs the flush on a spawn_blocking task under a tokio timeout;
        // assert it returns within a TIGHT window of the budget even though the
        // endpoint never responds.
        telemetry.shutdown().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < FLUSH_TIMEOUT + Duration::from_millis(500),
            "shutdown must abandon a hanging collector within ~the timeout, took {elapsed:?}"
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

    #[derive(Default)]
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
        // Capture: persist new span's fields into its extensions.
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let grab = grab_fields(attrs);
            let span_ref = ctx.span(id).unwrap();
            // Persist the (possibly later-recorded) fields in the span extension
            // so on_record updates land too.
            span_ref.extensions_mut().insert(SpanFields(grab.0));
        }

        // Capture: merge dynamically-recorded values into the existing span field set.
        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).unwrap();
            let grab = grab_fields(values);
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

        // Capture: snapshot the enclosing span's name, trace_id, and field names
        // for each event that fires within an instrumented scope.
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

    /// Collect all fields from a span's attributes or recorded values into a
    /// `FieldGrab`. Shared between `on_new_span` (attributes) and `on_record`
    /// (dynamic values) since both do the same grab-and-collect.
    fn grab_fields(recordable: &impl RecordFields) -> FieldGrab {
        let mut grab = FieldGrab::default();
        recordable.record_to(&mut grab);
        grab
    }

    /// Trait abstracting the `record(&mut Visit)` call shared by
    /// `tracing::span::Attributes` and `tracing::span::Record`.
    trait RecordFields {
        fn record_to(&self, visitor: &mut FieldGrab);
    }
    impl RecordFields for tracing::span::Attributes<'_> {
        fn record_to(&self, grab: &mut FieldGrab) {
            self.record(grab);
        }
    }
    impl RecordFields for tracing::span::Record<'_> {
        fn record_to(&self, visitor: &mut FieldGrab) {
            self.record(visitor);
        }
    }

    /// Build a provider + OTel layer over a given in-memory exporter. Returns
    /// both so the caller can assert on exported spans while the layer feeds them
    /// genuine (non-zero) trace_ids with no network. The provider is leaked so it
    /// outlives the test subscriber (dropped at test exit).
    fn otel_layer_from_exporter<S: Subscriber + for<'a> LookupSpan<'a>>(
        exporter: opentelemetry_sdk::trace::InMemorySpanExporter,
    ) -> (SdkTracerProvider, impl Layer<S>) {
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let tracer = provider.tracer("test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        (provider, layer)
    }

    /// Build a real `tracing-opentelemetry` layer over an in-memory exporter so
    /// spans get genuine (non-zero) trace_ids without any network.
    fn otel_layer_in_memory<S: Subscriber + for<'a> LookupSpan<'a>>() -> impl Layer<S> {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let (provider, layer) = otel_layer_from_exporter(exporter);
        // Leak the provider so it outlives the test subscriber (dropped at exit).
        Box::leak(Box::new(provider));
        layer
    }

    #[test]
    fn run_produces_one_root_span_with_child_stages() {
        // req 4 happy path: a run produces ONE root span whose children are the
        // expected stages, all sharing one trace_id. We mirror the instrumentation
        // shape (run → {fetch, process → extract, statement}) and assert against
        // the spans the in-memory exporter received.
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let (provider, otel) = otel_layer_from_exporter(exporter.clone());
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

    /// Install a capture layer + in-memory OTel, run `body`, return the capture.
    fn run_with_otel_capture(body: impl FnOnce()) -> Capture {
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry()
            .with(otel_layer_in_memory())
            .with(cap.clone());
        tracing::subscriber::with_default(subscriber, body);
        cap
    }

    /// Like [`run_with_otel_capture`] but pre-creates and enters a root `"run"`
    /// span with `trace_id = Empty`, then calls [`record_trace_id`] — the
    /// boilerplate shared by every test that needs a real trace_id. The closure
    /// receives the capture *after* the body returns (same as `run_with_otel_capture`).
    fn capture_under_root(body: impl FnOnce()) -> Capture {
        // Shared root-span boilerplate: create "run" span, enter, record trace_id.
        run_with_otel_capture(|| {
            let run_span = tracing::info_span!("run", trace_id = Empty);
            let _guard = run_span.enter();
            record_trace_id();
            body();
        })
    }

    #[test]
    fn event_within_span_carries_the_span_trace_id() {
        // req 4 (Loki↔Tempo): a log line inside a stage carries the SAME trace_id
        // recorded on the run's span — proven against an in-memory OTel exporter,
        // no live collector.
        let cap = capture_under_root(|| {
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
        // ALLOWLIST — exactly `{stage}`. An allowlist (not a denylist) means ANY
        // added field of ANY name now fails this test, not just the handful of
        // known-PII names we'd have to remember to enumerate.
        let cap = capture_under_root(|| {
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
        let captured: std::collections::HashSet<&str> = fields.iter().map(String::as_str).collect();
        let allowed: std::collections::HashSet<&str> = ["stage"].into_iter().collect();
        assert_eq!(
            captured, allowed,
            "extract span attributes must be exactly {{stage}}; had {fields:?}"
        );
    }

    #[test]
    fn traces_endpoint_appends_the_signal_path_idempotently() {
        // The bare base endpoint, as OTEL_EXPORTER_OTLP_ENDPOINT carries it.
        assert_eq!(
            traces_endpoint("http://tempo.monitor:4318"),
            "http://tempo.monitor:4318/v1/traces"
        );
        // Trailing slash must not produce a doubled separator.
        assert_eq!(
            traces_endpoint("http://tempo.monitor:4318/"),
            "http://tempo.monitor:4318/v1/traces"
        );
        // Already a signal URL → unchanged (no `/v1/traces/v1/traces`).
        assert_eq!(
            traces_endpoint("http://tempo.monitor:4318/v1/traces"),
            "http://tempo.monitor:4318/v1/traces"
        );
        // A collector behind a path prefix keeps the prefix.
        assert_eq!(
            traces_endpoint("http://gw:4318/otlp"),
            "http://gw:4318/otlp/v1/traces"
        );
    }

    /// The regression test that actually matters: assert the **wire request**,
    /// not the string helper.
    ///
    /// `opentelemetry-otlp`'s `resolve_http_endpoint` uses a programmatic
    /// `with_endpoint` value VERBATIM — it appends `/v1/traces` only for an
    /// endpoint it reads from the environment itself. Passing the raw
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` value straight through therefore POSTed to
    /// the collector's root path, which Tempo answers 404 (verified against the
    /// live cluster Tempo: `POST /` → 404, `POST /v1/traces` → 200) — and with
    /// `internal-logs` off that 404 was silent, so every run's trace vanished.
    ///
    /// A one-shot loopback listener stands in for the collector and captures the
    /// request line. Pins the path AND the POST method against any future
    /// endpoint-resolution change in the SDK.
    #[test]
    fn export_posts_to_the_v1_traces_path() {
        use std::io::{Read as _, Write as _};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback test collector");
        let port = listener
            .local_addr()
            .expect("the bound listener has a local address")
            .port();

        // Accept exactly one export and hand back the request line.
        let collector = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("the exporter connects");
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).expect("the exporter sends a request");
            // Answer 200 so the export completes rather than tripping its timeout.
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = sock.flush();
            String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        });

        // The BASE endpoint, exactly as the CronJob env supplies it.
        let provider = build_provider(&format!("http://127.0.0.1:{port}"))
            .expect("provider builds against the loopback collector");
        {
            let tracer = provider.tracer("test");
            tracer.in_span("exported", |_| {});
        }
        provider.force_flush().expect("the batch exports");

        let request_line = collector.join().expect("the collector thread finishes");
        assert_eq!(
            request_line, "POST /v1/traces HTTP/1.1",
            "the exporter must POST the OTLP traces signal path, not the collector root"
        );

        let _ = provider.shutdown();
    }

    #[test]
    fn sanitize_endpoint_strips_userinfo_and_preserves_the_rest() {
        // Credentials in the authority are removed; everything else is untouched.
        assert_eq!(
            sanitize_endpoint("http://user:pass@tempo:4318/v1/traces"),
            "http://tempo:4318/v1/traces"
        );
        // No userinfo → unchanged (incl. the path).
        assert_eq!(
            sanitize_endpoint("http://tempo:4318/v1/traces"),
            "http://tempo:4318/v1/traces"
        );
        // No path.
        assert_eq!(
            sanitize_endpoint("https://user:pass@host:4318"),
            "https://host:4318"
        );
        // A '@' in the PATH must not be mistaken for userinfo.
        assert_eq!(
            sanitize_endpoint("http://tempo:4318/v1/tr@ces"),
            "http://tempo:4318/v1/tr@ces"
        );
        // No scheme → returned as-is (we don't guess).
        assert_eq!(sanitize_endpoint("tempo:4318"), "tempo:4318");
    }
}
