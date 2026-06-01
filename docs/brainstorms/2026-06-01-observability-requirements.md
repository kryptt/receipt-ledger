---
date: 2026-06-01
topic: observability
---

# receipt-ledger Observability (logs · log-derived metrics · traces)

## Problem Frame

`receipt-ledger` is a one-shot hourly CronJob that books real money into Firefly
III. Today its only window into a run is **plain-text** `tracing` logs to stdout.
When something goes wrong it is found late and by hand. Two failure modes matter:

- **Silent no-progress.** Since the transient-defer work, an LLM/FX outage now
  *defers* a message (keeps it in INBOX, re-fetched next run) rather than routing
  it to Review. A persistent/borderline outage can make a message **defer forever**
  — never booked, never reviewed, the run exits 0, and **nothing alerts**.
- **Review pile-up.** By-design flags still accumulate in the Review mailbox
  (statement reconcile flags, cross-currency wires, hard extraction failures) and
  no one notices until it's large.

We want to *see* run health and *be alerted* on both, using the cluster's existing
Grafana stack. Logs already reach **Loki** (via the **Alloy** collector); the app
emits no structured fields, metrics, or traces yet.

**Approach decision (shapes everything):** the app is one-shot, so rather than push
app metrics on exit, **Phase 1 derives metrics from the structured logs it already
ships to Loki** (LogQL recording rules) plus **kube-state-metrics** for run
liveness. No app metrics SDK, no OTLP, no flush-on-exit risk in Phase 1. OTLP and
its dependency weight are confined to **Phase 2 traces** (Tempo), which is
value-gated.

## Requirements

**Phase 1 — Structured logging + log-derived metrics (no new heavy deps)**
- R1. Emit **structured (JSON)** logs to stdout (Loki ingestion already exists via
  Alloy; this makes every field below queryable and aggregatable in LogQL).
- R2. Emit per-message and per-run **disposition** fields — `source`
  (paypal / banco / swift / statement) × `disposition`
  (booked / duplicate / review / skipped / deferred) — so LogQL recording rules can
  count them. These fields ARE the metric substrate; the counters themselves are
  recording rules, not app code.
- R3. Emit the **co-primary signal** fields:
  - *Review-rate*: per-run routed-to-review count (+ per-message review `reason`
    and `source`).
  - *No-progress*: per-run `deferred` / `hold_state`, and the **count of messages
    still in INBOX after the run** — the signal that catches a defer-forever
    backlog the review-rate misses.
- R4. Emit **provider-outage / defer** fields: transient LLM defers, transient FX
  defers, model-selection failure (couldn't pick/load an extraction model),
  DOP-rate retry count. (FX-cache hit/miss is *efficiency*, optional, not an outage
  signal.)
- R5. Emit **statement reconcile-health** fields per statement run: reconciled /
  booked_new / amount_mismatch / corrected (*a Phase-2 auto-correction applied*) /
  unmatched_booked / balance_mismatch / deferred, **plus the closing-balance delta**
  per section (the per-cycle definition-of-done) — so a *correctness* alert can
  fire when a run books but the balance does not reconcile.
- R6. **PII discipline**: aggregate counts at `info`; per-row financial detail
  (merchant / amount / last-4 / ref#) only at `debug`. The prod boundary must
  **not rest solely on `RUST_LOG`** — gate per-row detail behind a dedicated flag
  that defaults off (a misconfigured `debug` otherwise ships PII to Loki for the
  whole retention window). Carries the statement plan §4 rule.

**Phase 2 — Traces (Tempo, OTLP — the only heavy-dependency pillar; value-gated)**
- R7. One **root span per run**; child spans for the JMAP fetch, each message's
  stages (unwrap → extract/LLM → validate → FX → submit), and the statement path
  (decrypt → parse → reconcile → book).
- R8. Export spans via **OTLP to Tempo** (directly or via Alloy), bounded-timeout
  flush before exit (see R10). This is the only place OTLP / a new transport stack
  enters — Phase 2 must justify that dependency weight against the project's
  FROM-scratch single-binary minimalism, or stay deferred.
- R9. Inject `trace_id` / `span_id` into log lines for Loki ↔ Tempo cross-link.
  Phase-1 logs carry no trace field (`trace_id` only exists once spans do); requires
  the JSON log layer and the OpenTelemetry layer to share one subscriber registry.

**Cross-cutting**
- R10. **Non-blocking telemetry**: nothing here may prevent or delay a booking. The
  Phase-2 trace flush runs **after booking is durably complete**, is **bounded by a
  short timeout**, and a slow/hung exporter is abandoned (logged at `warn`) — never
  consuming the `activeDeadlineSeconds` budget or flipping the exit code. (Phase-1
  logging is plain stdout, so it carries no such risk.)
- R11. **No PII / low cardinality** in anything used as a metric dimension: the log
  fields LogQL turns into labels are limited to `source`, `disposition`,
  `section_currency`; never merchant / amount / account / ref#.
- R12. **No PII in traces**: span names/attributes carry only low-cardinality
  identifiers (source, disposition, stage); never merchant / amount / account /
  last-4 / IBAN / ref# / email / raw bodies.
- R13. **No raw model I/O in telemetry**: the extract/LLM stage must never record
  raw email bodies, the prompt, or the completion in any log/span/event at any level.
- R14. **No secrets in telemetry**: Firefly token, JMAP / BP-statement / DOP
  credentials must never appear in any log/span/error; redact URLs (no token in
  query string or `Authorization`). Applies at all levels, including `debug`.

## Success Criteria
- **Co-primary alerts fire from logs + kube-state-metrics, no app metrics code**:
  (a) *no-progress* — messages held/deferred or INBOX non-empty across N runs; and
  (b) *review-rate* — routed-to-review over a window.
- An LLM/FX **outage is distinguishable from a clean run** (defer/outage fields move)
  — visible *before* a backlog grows.
- A **correctness** alert can fire when a statement booked but its closing-balance
  delta ≠ 0.
- A run's logs are **queryable in Loki by structured field**, and (Phase 2)
  **linkable to its Tempo trace** by `trace_id`.
- Enabling observability **never changes or delays booking** — exporter/flush
  problems are best-effort and abandoned.

## Scope Boundaries
- **Phase-1 metrics are LogQL recording rules + kube-state-metrics, authored
  cluster-side** — the app's job is to emit the right *log fields*, not counters.
  No app metrics SDK, no OTLP, no Pushgateway in Phase 1.
- **Alert rules and Grafana dashboards** are cluster-side config (example rules /
  dashboards are a nice-to-have, not a requirement).
- **Per-stage latency histograms** deferred — Phase-2 traces give the breakdown.
- **No per-merchant / per-amount / per-account** signals as metric dimensions.
- **Phase-2 traces (OTLP/Tempo) are value-gated** — only build if the per-run span
  tree earns its dependency weight; structured logs may already answer the
  debugging questions.
- No change to booking, reconcile, or routing logic.

## Key Decisions
- **Log-first metrics**: derive Phase-1 metrics from the structured Loki logs
  (LogQL recording rules) + kube-state-metrics; no app metrics SDK. Rationale: the
  data is already going to Loki, this adds no heavy deps / no flush-on-exit risk,
  and it fits the one-shot lifecycle better than pushing on exit.
- **Co-primary signals**: *no-progress* (defer-forever / INBOX-not-draining) AND
  *review-rate*. No-progress is the gap the transient-defer work opened; review-rate
  still catches by-design flags.
- **Correctness signal**: surface the statement **closing-balance delta** so a
  wrong-but-booked run (incl. a Phase-2 auto-correction) can alert — dispositions
  alone don't prove money booked right.
- **OTLP / heavy deps confined to Phase-2 traces**, which is value-gated against the
  project's minimalism.
- **Non-blocking**; **PII/secret discipline across all sinks** (logs, traces, and any
  field used as a metric label).

## Dependencies / Assumptions
- Cluster has Alloy, Loki, Tempo (OTLP 4317/4318), Prometheus (rancher-monitoring)
  with kube-state-metrics. Assumed: Alloy already ships pod stdout → Loki (the
  Phase-1 foundation), and kube-state-metrics exposes CronJob/Job success +
  last-successful-time. **Verify in planning** (lighter gate than before — Phase 1
  no longer depends on an OTLP→Prometheus pipeline).

## Outstanding Questions

### Deferred to Planning
- [Affects R2–R5][Needs research] Confirm the **log-derived path is sufficient**:
  that the structured fields + LogQL recording rules + kube-state-metrics actually
  express the co-primary + outage + reconcile signals (cardinality, rule authoring,
  Loki retention vs alert window). If a signal genuinely can't be derived from logs,
  that — and only that — justifies a minimal app metric.
- [Affects R3][Technical] No-progress signal shape: per-run `deferred`/`hold_state`
  field + post-run INBOX count (cheap, in-app) vs an oldest-INBOX-item-age gauge
  (needs a JMAP query). Pick what the alert keys on; the per-run field is the
  simpler default.
- [Affects R7–R8][Technical] Phase-2 only: the **flush-on-exit shutdown ordering**
  (hold provider handles in `main()`, `force_flush()` + `shutdown()` before the
  `#[tokio::main]` runtime drops; runtime-compatible exporter; bounded timeout) and
  the **dependency/binary-size cost** of the OTLP stack vs the value of traces.
- [Affects R5][Technical] Closing-balance delta is already computed in
  `check_balance`; confirm emitting it as a structured field (not just a log line)
  is straightforward.

## Next Steps
→ `/ce:plan` for structured implementation planning (Phase 1 first; Phase 2 traces
value-gated).
