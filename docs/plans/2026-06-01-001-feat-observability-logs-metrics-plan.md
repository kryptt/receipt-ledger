---
title: "feat: Observability — structured logs + log-derived metrics (Phase 1), traces (Phase 2, gated)"
type: feat
status: shipped
date: 2026-06-01
deepened: 2026-06-01
shipped: 2026-06-01  # Phase 1 (Units 1–6) → 0.15.0; Phase 2 (Unit 7 traces) + Unit-6 follow-ons (field constants/contract test, dashboard) → 0.16.0. Both deployed via hr-fleet.
origin: docs/brainstorms/2026-06-01-observability-requirements.md
---

# Observability — structured logs + log-derived metrics (Phase 1), traces (Phase 2, gated)

## Overview

Make `receipt-ledger`'s hourly run observable using the cluster's existing Grafana
stack, **without adding heavy dependencies in Phase 1**. The app already emits a
structured end-of-run `Summary` and per-message disposition logs; Phase 1 switches
log output to **JSON** and **enriches the emitted fields** so cluster-side **LogQL
recording rules** (over Loki) + **kube-state-metrics** can derive every metric and
alert. The two co-primary alerts — *no-progress* (defer-forever) and *review-rate*
— plus an outage signal and a statement *correctness* signal (closing-balance
delta) all come from log fields, no app metrics SDK. Phase 2 (OTLP traces to Tempo)
is planned but **value-gated** — it is the only pillar that pulls a new dependency
stack, so it ships only if the per-run span tree earns its weight.

## Problem Frame

The one-shot CronJob books real money but its only window is plain-text stdout
logs. Two failure modes go unseen: a **silent defer-forever** run (a persistent
LLM/FX outage keeps a message in INBOX, re-fetched hourly, never booked, exits 0,
no alert) and a **Review pile-up** of by-design flags. See origin:
`docs/brainstorms/2026-06-01-observability-requirements.md`.

## Requirements Trace

- R1. Structured (JSON) logs to stdout (origin R1).
- R2. Per-message + per-run disposition fields (`source` × `disposition`) as the
  LogQL metric substrate (origin R2).
- R3. Co-primary signal fields: review-rate **and** no-progress (post-run INBOX
  count / `hold_state` / deferred) (origin R3).
- R4. Provider-outage/defer fields: transient LLM defer, transient FX defer,
  model-selection failure, DOP-rate retry count (origin R4).
- R5. Statement reconcile-health fields + **closing-balance delta** per section
  (origin R5).
- R6. PII discipline; per-row detail gated behind a default-off flag independent of
  `RUST_LOG` (origin R6).
- R7–R9. Phase 2 traces (root+child spans, OTLP→Tempo, `trace_id` in logs)
  (origin R7–R9) — **value-gated/deferred**.
- R10–R14. Non-blocking; no-PII/secrets across logs, traces, metric dimensions
  (origin R10–R14).

## Scope Boundaries

- Phase-1 metrics are **LogQL recording rules + kube-state-metrics authored
  cluster-side** — the app emits *log fields*, not counters. No metrics SDK, no
  OTLP, no Pushgateway in Phase 1.
- Alert rules + Grafana dashboards are cluster-side; **example** rules/dashboards in
  the repo are an optional nice-to-have (Unit 6).
- Phase-2 traces (OTLP/Tempo) are value-gated — only build if traces earn their
  dependency weight against the FROM-scratch single-binary minimalism.
- No change to booking, reconcile, or routing logic.

## Context & Research

### Relevant Code and Patterns

- `src/main.rs::init_tracing` — `tracing_subscriber::fmt().with_env_filter(...).init()`;
  currently plain text. The end-of-run `Summary` is already logged here as a
  structured `info!` event (processed/booked/duplicates/review/skipped/statements/
  corrected). This is the natural per-run metric source.
- `src/lib.rs` — `Summary` struct; the per-message loop with `route()`, `hold_state`,
  the statement `Ok`-arm that logs `?report`; `process_message` dispositions
  (`Booked`/`Duplicate`/`Skipped`/`Review`), `is_transient_outage`. The transient
  defer is already centralized here (no `Defer` disposition — transient errors are
  classified at one chokepoint).
- `src/statement/pipeline.rs` — `StatementReport` (reconciled / booked_new /
  amount_mismatch / corrected / unmatched_booked / balance_mismatch / deferred);
  `check_balance` already computes the closing-balance delta but only logs it, does
  not store it on the report.
- `src/fx.rs` (`RateError::Transient`) and `src/llm.rs` (`LlmError::Transient`) —
  the typed transient classifications that drive defers; `model_selection` failure
  surfaces from `run()` before the loop.
- `src/config.rs` — `Config::from_env` boundary pattern: `env_or`, `env_bool`,
  typed parse-at-boundary. Add the log-format + PII-gate flags here.
- `Cargo.toml` / `Dockerfile` — FROM-scratch static-musl, `opt-level=z`, hand-pruned
  features. Phase-2 dependency weight must respect this.

### Institutional Learnings

- No `docs/solutions/`. Carried project knowledge: PII-at-`debug` rule (statement
  plan §4); telemetry must never delay/fail a booking (the go-lives were killed at
  `activeDeadlineSeconds`); the binary's minimalism is a hard design value.

### External References

- Skipped for Phase 1 (structured `tracing` JSON + field enrichment is well-trodden
  and locally patterned). Phase 2 will need `tracing-opentelemetry` + OTLP
  flush-on-exit guidance — researched at the time Phase 2 is committed, not now.

## Key Technical Decisions

- **Log-first metrics**: emit structured fields; derive metrics cluster-side
  (LogQL + kube-state-metrics). Rationale: data already flows to Loki; no heavy
  deps, no flush-on-exit risk, fits the one-shot lifecycle. (see origin)
- **Per-message structured "outcome" event** carrying `{source, disposition}` (no
  PII) is the LogQL substrate for the source×disposition metric; the existing
  aggregate `Summary` event is the run-level rollup. Two granularities, one cheap
  emission each.
- **No-progress is a first-class field**: emit `messages_remaining` (in INBOX after
  the run) + `hold_state` + `deferred` on the run summary so a defer-forever backlog
  is alertable — the gap the transient-defer work opened.
- **Closing-balance delta becomes a `StatementReport` field**, not just a log line,
  so it serializes into the structured `?report` event for a correctness alert.
- **JSON via a format toggle** (`RECEIPT_LOG_FORMAT`, default `json` in the
  container; `text` for local dev) rather than hard-switching, so local runs stay
  readable.
- **PII gate is a dedicated default-off flag** (`RECEIPT_LOG_PII`), independent of
  `RUST_LOG`, so a misconfigured `debug` cannot ship financial PII to Loki.
- **Phase 2 traces deferred + value-gated**: the only pillar adding an OTLP/transport
  dependency; build only if justified.

## Open Questions

### Resolved During Planning

- *Where do metrics come from?* → LogQL recording rules over the structured logs +
  kube-state-metrics; no app metrics code (origin decision).
- *How to get per-source/disposition counts?* → a per-message structured outcome
  event with `{source, disposition}` fields; LogQL aggregates.
- *Closing-balance delta plumbing?* → add a field to `StatementReport`, set it in
  `check_balance` (the value is already computed there); model the absent case
  (Option / `balance_checked`) so absent ≠ reconciled.
- *Where does the model-selection-failure signal live?* → NOT the run summary
  (`select_model` fails before the summary exists → non-zero exit). A dedicated
  `error!` event + kube-state-metrics on the non-zero exit (Unit 3).
- *Per-message `source`?* → `process_message` returns it (signature change);
  canonical label set = `adapter.name()` + `unknown` (Unit 2).

### Deferred to Implementation

- **Verify Loki retention ≥ the no-progress alert window** (e.g. N runs / K hours);
  if retention is short or rate-limited, key no-progress on KSM
  `last_successful_time` age instead of counting summary lines.
- No-progress shape: `messages_remaining` from the explicit `deferred_messages`
  counter (Unit 3); an oldest-INBOX-item-age gauge (extra JMAP query) only if the
  simpler field proves insufficient.
- Phase-2 only: `tracing-opentelemetry` wiring + `force_flush()`/`shutdown()`
  ordering before the `#[tokio::main]` runtime drops; OTLP-HTTP vs gRPC to limit
  dependency weight; bounded flush timeout.
- Exact JSON field names/casing the LogQL rules key on — finalize against the
  fmt-json output during implementation.

## Implementation Units

### Phase 1 — structured logs + log-derived metric fields (no new heavy deps)

- [x] **Unit 1: JSON log output behind a format toggle**

**Goal:** Emit machine-parseable JSON logs so Loki fields are queryable; keep
local dev readable.

**Requirements:** R1.

**Dependencies:** None.

**Files:**
- Modify: `Cargo.toml` (add the `json` feature to `tracing-subscriber` — it is **not**
  enabled today; `.json()` will not compile without it. `serde_json` is already a
  direct dep, so the size impact is small.)
- Modify: `src/main.rs` (`init_tracing`)
- Modify: `src/config.rs` (add `RECEIPT_LOG_FORMAT`, default `json`)
- Test: `src/config.rs` (tests module)

**Approach:**
- `init_tracing` reads `RECEIPT_LOG_FORMAT` directly from env (it runs before
  `Config::from_env`; do **not** read from `Config` — keep it ordering-independent).
- `.json()` and `.compact()` return **different** `SubscriberBuilder` format types,
  so branch on the format and call `.init()` **inside each arm** (no shared `let`
  binding, no `Box<dyn>`). Keep `EnvFilter` unchanged.

**Patterns to follow:** existing `env_or`/`env_bool` in `config.rs`.

**Test scenarios:**
- Happy path: `RECEIPT_LOG_FORMAT=json` → format resolves to JSON; unset → JSON
  (prod default); `text` → compact text.
- Edge case: unknown value falls back to the default with a warning, never panics.

**Verification:** a run with the default prints one JSON object per log line; the
end-of-run summary line is valid JSON with the expected keys.

- [x] **Unit 2: Per-message structured outcome event + per-source run rollup**

**Goal:** Emit `{source, disposition}` per message (the LogQL substrate) and ensure
the run `Summary` carries enough for source-broken-down recording rules.

**Requirements:** R2.

**Dependencies:** Unit 1.

**Files:**
- Modify: `src/lib.rs` (`process_message` **signature change** — return the source
  alongside the disposition, e.g. `(Option<&'static str>, Disposition)`, because the
  adapter — which owns `name()` → `paypal`/`paypal_payment`/`banco_popular` — is
  selected *inside* `process_message` and currently dropped; `Disposition` carries no
  source. The statement path's source is statically `banco_popular`.)
- Modify: `src/lib.rs` (`run` per-message loop emission)
- Test: `src/lib.rs` (tests)

**Approach:**
- At each message's terminal point, emit one `info!` outcome event with structured
  `source` + `disposition` + (when review) a **bounded** `review_reason_category`
  (e.g. `no_adapter` / `extraction_failed` / `declined` / `reconcile_flag` /
  `cross_currency`). **Critical:** `disposition` is the bare enum *discriminant*
  (`booked`/`duplicate`/`skipped`/`review`/`deferred`); the `Skipped(String)` /
  `Review(String)` **reason payload is NEVER serialized** (those strings carry
  last-4/currency/account hints → PII). The category is a fixed vocabulary, not the
  free-form reason.
- Pin the canonical `source` label set to `adapter.name()` values
  (`paypal`/`paypal_payment`/`banco_popular`) + `unknown` for no-adapter /
  unrecognized-forward messages; document it (Unit 6 LogQL keys on these).
- Keep the existing aggregate `Summary` info event; LogQL can use either grain.

**Patterns to follow:** the existing `info!(id = %msg.id, ...)` disposition logs in
`run()`; `Summary` field-logging in `main.rs`; `adapter.name()`.

**Test scenarios:**
- Happy path: the source+discriminant mapping yields the right `(source,
  disposition)` for each variant and the statement outcomes.
- Edge case: a no-adapter / unrecognised-forward message → `source = unknown`,
  never empty.
- **PII (must):** a `Review("...merchant/last-4...")` outcome serializes
  `disposition = review` + a bounded `review_reason_category`, and the event
  contains **none** of the reason String's PII.

**Verification:** a fixture batch yields one outcome event per message with
low-cardinality `source`/`disposition`/`review_reason_category` and zero PII.

- [x] **Unit 3: No-progress + outage/defer run fields**

**Goal:** Make the co-primary *no-progress* signal and the outage indicators
alertable from the run summary.

**Requirements:** R3, R4.

**Dependencies:** Unit 2.

**Files:**
- Modify: `src/lib.rs` (`run` summary emission; `Summary` gains `messages_remaining`,
  `hold_state`/`deferred`, and outage counters)
- Modify: `src/fx.rs` / `src/llm.rs` call sites only if a transient classification
  needs a structured counter increment (the classification already exists)
- Test: `src/lib.rs` (tests)

**Approach:**
- Add an explicit `deferred_messages` counter incremented at the two
  `is_transient_outage` arms + the statement-deferred arm (today `route()` returns
  `()` uncounted and deferred messages don't call it — so `processed − moved` is
  **not** derivable; use a real counter). `messages_remaining` = `deferred_messages`
  (the messages left in INBOX because state didn't advance).
- Surface `hold_state` (bool: any defer this run) on the summary. For the outage
  signal, the **must-have** is a single `transient_defers` total (LLM+FX) sufficient
  for "outage distinguishable from clean run"; the per-provider split
  (`llm_transient_defers`/`fx_transient_defers`/`dop_rate_retries`) is **optional
  diagnostic** enrichment.
- **Drop `model_selection_failed` from the summary** — `select_model` runs with `?`
  *before* the loop and *before* `Summary` exists; on failure `run()` returns `Err`
  and **no summary is emitted** (main.rs logs the fatal error + non-zero exit). Emit
  it instead as a **dedicated structured `error!` event** (e.g. `stage =
  "model_selection"`) at the failure site, and let kube-state-metrics catch the
  non-zero exit. The LogQL rule keys on that event, not the summary.

**Signal division of labor (must state in the plan):**
- **kube-state-metrics** owns hard failures (non-zero exit: model-selection abort,
  JMAP-connect failure, panic) and `last_successful_time` / job age.
- **Log-derived** owns the *exit-0* cases: in-loop defer (`messages_remaining > 0`,
  `hold_state`).
- **Gap to close:** an *exit-0 no-progress* run (defer-forever) is **not** a Job
  failure, so KSM is blind to it — the log signal must cover it; and the empty-mail
  path returns `Summary::default()` (so a drained-but-stuck INBOX looks idle). The
  no-progress alert therefore combines **`messages_remaining > 0` sustained across N
  runs** (log) **with** KSM `last_successful_time` age — neither alone is sufficient.

**Patterns to follow:** the `Summary` struct + its `main.rs` field-log; the
`is_transient_outage` chokepoint and the statement-deferred arm in `run()`.

**Test scenarios:**
- Happy path: a clean run → `messages_remaining = 0`, `hold_state = false`,
  `transient_defers = 0`.
- Edge case (no-progress): all messages defer → `messages_remaining = deferred_messages`,
  `hold_state = true`, `transient_defers > 0`.
- Edge case: partial run → `messages_remaining` equals the deferred subset only.

**Verification:** the summary event distinguishes clean / partial-defer / total-defer
runs by these fields; the model-selection failure surfaces as its own event (not the
summary); the plan's signal-division-of-labor is reflected in Unit 6's example rules.

- [x] **Unit 4: Statement reconcile-health fields + closing-balance delta**

**Goal:** Surface statement health and a correctness signal as structured fields.

**Requirements:** R5.

**Dependencies:** Unit 1.

**Files:**
- Modify: `src/statement/pipeline.rs` (`StatementReport` gains `balance_delta`;
  `check_balance` sets it; `process_statement` ensures `?report` serializes the
  fields as structured, not just `Debug`)
- Test: `src/statement/pipeline.rs` (tests)

**Approach:**
- Add `balance_delta` to `StatementReport`; `check_balance` already computes `delta`
  — store it. **Model the absent case** (`check_balance` early-returns when
  anterior/total are missing, so a bare `0` conflates "reconciled" with "never
  checked"): use `Option<Decimal>` or a `balance_checked: bool` companion so the
  correctness alert doesn't read "absent" as "reconciled". `check_balance` runs
  **per section** in a loop (USD + DOP); decide the report aggregation (max-abs
  delta is the safest single scalar) — `StatementReport` is flat/`Copy` today and
  `Decimal` is `Copy`, so this stays compatible.
- **Emit explicit named fields, not `?report`.** Under `fmt().json()`, `?report`
  serializes as one opaque Debug *string*, which LogQL can't field-query. Pick the
  **one canonical** statement event (the `process_statement` "reconciliation
  complete" site is the natural home) and emit `reconciled = …, amount_mismatch = …,
  unmatched_booked = …, balance_mismatch = …, corrected = …, balance_delta = …` as
  structured fields. `balance_delta` (+ `balance_mismatch`) is the **must-have**
  correctness signal; the other counts are R5 health-visibility (near-free once
  structured).

**Patterns to follow:** existing `StatementReport` + `check_balance`; the `?report`
log in `run()`'s statement arm.

**Test scenarios:**
- Happy path: a reconciling statement → `balance_delta = 0`, `balance_mismatch = 0`.
- Edge case: a statement with an unmodeled fee → `balance_delta != 0`,
  `balance_mismatch = 1` (the correctness alert fires).
- Edge case: a corrected charge increments `corrected` and remains clean
  (`amount_mismatch` not incremented) — confirms the field feeds the right signal.

**Verification:** the statement summary event carries a numeric `balance_delta`; a
non-reconciling fixture surfaces it and the mismatch flag.

- [x] **Unit 5: PII/secret discipline guardrail**

**Goal:** Make the prod PII boundary fail-safe and audit the telemetry sinks for
secret/raw-body leakage.

**Requirements:** R6, R13, R14 (Phase-1+2: "no raw bodies" / "no secrets" apply to
**any** log/event at any level). R12 (no-PII-in-traces) is trace-only → Phase 2 / Unit 7.

**Dependencies:** Unit 1; coordinated with Units 2–4 (their new events are in scope).

**Files:**
- Modify: `src/config.rs` (add `RECEIPT_LOG_PII`, default off)
- Modify: the per-row `debug!` sites **and** the dry-run `info!` site
  (`statement/pipeline.rs` charge-plan logs `merchant` at `info!` when `dry_run` —
  the current operational mode — so the gate must wrap that arm too), adapters
- Create/Modify: a `redact()` helper (`src/...`) applied to error strings on the
  FX/DOP/Firefly paths
- Test: `src/config.rs` (tests); `redact()` unit test

**Approach:**
- Add a default-off `RECEIPT_LOG_PII` flag + `pii_logging_enabled()` gating per-row
  financial fields independent of `RUST_LOG`. **Also gate the dry-run `info!` path**
  that currently promotes `merchant` to info (otherwise PII reaches Loki during the
  active dry-run observation).
- **Concrete redaction (not review-only):** `fx.rs` builds error strings embedding
  the **raw provider response body** (e.g. `"DOP token endpoint returned {status}:
  {body}"`) and these `RateError`s are logged via `warn!(error = %e)` — a token /
  client-credentials error body can echo secrets. Ship a `redact()` helper (strip
  bearer tokens, `Authorization`, query-string secrets, and don't echo raw token-
  endpoint bodies) and apply it before those errors are logged. Confirm the LLM path
  never logs raw bodies/prompts/completions.
- **The new `info!` events (Units 2–4) are emitted unconditionally — the flag does
  NOT protect them.** Their PII-safety comes from *field selection* (bare
  discriminants, bounded categories, aggregate numbers), reviewed against a named
  no-PII allowlist. U5 owns that review.

**Patterns to follow:** existing `debug!`/dry-run `info!` per-row sites; `env_bool`;
`fx.rs` error-string construction.

**Test scenarios:**
- Happy path: flag off (default) → per-row financial detail suppressed even at
  `RUST_LOG=debug`, including the dry-run `info!` path.
- Edge case: flag on + debug → per-row detail emitted (dev/diagnostic).
- Error path: `redact()` strips a bearer token / token-endpoint body from a
  synthesized FX/DOP error string before it is logged.
- PII (must): the Unit-2 outcome event and Unit-4 statement event emit **no**
  merchant/amount/last-4/ref# at the default flag-off state.

**Verification:** with defaults, no merchant/amount/last-4/ref# appears at any
`RUST_LOG` (incl. dry-run); `redact()` provably strips secrets from error logs; the
new info events are PII-free by construction.

- [x] **Unit 6: Recording + alert rules, dashboard, and a field-name contract test** _(complete — 0.15.0 + 0.16.0)_
  - 0.15.0: the field contract + example LogQL recording rules + Prometheus alert
    rules (ReceiptLedgerNoProgress, ReviewPileup, BalanceMismatch, RunFailing) in
    `docs/observability/README.md`.
  - 0.16.0: the in-app field-name **constants module** (`src/obs_fields.rs`) + an
    automated **contract test** (`tests/obs_field_contract.rs`, exact-equality — a
    rename/stray field now fails CI) + the **Grafana dashboard JSON**
    (`docs/observability/dashboard.json`).
  - Still cluster-side (operator): applying the recording/alert rules to Mimir +
    Alertmanager and importing the dashboard — until then the fields are queryable
    but nothing pages.

**Goal:** Close the loop to a *working* alert (not just emitted fields), and prevent
silent field-name drift from breaking alerts. Promoted from optional → **required**:
without the rules, the motivating defect (a silent defer-forever run) stays exactly
as unalerted as today — Units 1–5 produce the raw material, this delivers the alert.

**Requirements:** R3/R4/R5 success criteria (the co-primary + correctness alerts).

**Dependencies:** Units 2–4 (field names finalized).

**Files:**
- Create: `docs/observability/` — LogQL recording rules (disposition, review-rate,
  no-progress, outage, balance-delta), Prometheus/Alertmanager rules (the two
  co-primary alerts: no-progress = `messages_remaining > 0` sustained **AND** KSM
  `last_successful_time` age; review-rate; plus the correctness alert on
  `balance_delta`), and a Grafana dashboard JSON.
- Create/Modify: stable field-name **constants** in the app (one module) that the
  emission sites use, so the names are a single source of truth.
- Test: a **contract test** asserting the structured events emit exactly those
  field-name constants (a rename then fails CI instead of silently breaking the
  alert — directly mitigating the field-drift risk).

**Approach:** rules derive from the field-name constants; mark the rules/dashboard as
**examples to apply cluster-side**. **Acceptance gate:** "alerts fire" is only true
once an operator applies these to Alertmanager/Grafana — name that as the explicit
cluster-side follow-on so Phase 1 isn't mistaken for "done = alerting" when it's
"done = alertable + rules provided".

**Test scenarios:**
- Contract: each emitted structured event contains the expected field-name
  constants (fails if a field is renamed/dropped).

**Verification:** the example rules reference only field names that the contract test
pins; applying them cluster-side produces a firing no-progress alert on a forced
defer.

### Phase 2 — traces (value-gated; deferred)

- [x] **Unit 7 (Phase 2, gated): OTLP traces to Tempo + `trace_id` in logs** _(shipped 0.16.0)_
  - Per-run span tree (`run` → `fetch`/`model_selection`/`process`→`extract`,
    `statement`→`decrypt`/`parse`/`reconcile_book`) over OTLP/HTTP (JSON), reusing
    the existing reqwest/rustls stack (no tonic/gRPC, no new C deps; binary +~335 KB).
  - **Runtime-gated OFF** by default — exports only when `OTEL_EXPORTER_OTLP_ENDPOINT`
    is set. `trace_id` recorded on the root span (renders under `spans[]` in the JSON
    log → Loki `spans_0_trace_id`; documented). No-PII span allowlist + contract test.
  - Flush-on-exit is bounded (3s) and **`main` hard-exits via `std::process::exit`**
    so a half-open collector can't delay termination (ce:review P1 fix).
  - **Not enabled in the fleet manifest** — the operator sets the OTLP endpoint when
    Tempo ingest is verified. The original value-gate ("don't build until a real
    incident") was overridden by explicit user request; it ships off-by-default so the
    gate effectively moves to runtime-enable.

**Goal:** Per-run span tree exported to Tempo, with Loki↔Tempo correlation.

**Requirements:** R7, R8, R9, R10 (flush), R12 (no PII in spans).

**Dependencies:** Phase 1 complete; an explicit decision that traces earn their
dependency weight; verification that Alloy/Tempo OTLP ingest works.

**Files:**
- Modify: `src/main.rs` (provider init + bounded `force_flush`/`shutdown` before the
  runtime drops), `src/lib.rs`/pipeline (span instrumentation), `Cargo.toml` (OTLP
  deps — prefer OTLP-HTTP to avoid a gRPC/`tonic` stack), `src/config.rs` (OTLP
  endpoint).
- Test: span-shaping unit tests where pure; flush-ordering verified via a real/local
  collector at implementation time.

**Approach:** root span per run; child spans per stage; bounded-timeout flush after
booking; share one subscriber registry so the JSON layer can read span context for
`trace_id`. **Concrete no-PII span rule:** span attributes are an allowlist of
`stage` / `duration` / `outcome` / counts only — the extract/LLM span must **not**
carry the prompt, completion, raw email body, merchant, amount, last-4, or ref#
(this is where raw bodies live: the stage builds `adapter.prompt(&unwrapped.body)`).
Add a span-shaping test asserting the LLM/extract span has none of these.

**Severable:** Phase 1 alone satisfies every success criterion **except** the
Loki↔Tempo trace-link. Phase 2 can be cut entirely with no loss to the
no-progress / review-rate / correctness goals.

**Execution note:** value-gated — do not start until Phase 1 is in use and there is a
**falsifiable trigger** (a real incident where Phase-1 logs were insufficient to
root-cause), not just "earns its weight". Default is *don't build it*.

**Test scenarios:**
- Happy path: a run produces one root span with the expected child stages.
- Edge case: exporter unreachable → run still books, flush abandoned within timeout,
  exit code unaffected.
- Integration: a log line within a stage carries the same `trace_id` as the span
  (Loki↔Tempo link).

**Verification:** a trace appears in Tempo for a run; its `trace_id` matches the
run's log lines; a down/slow collector never delays or fails the run.

## System-Wide Impact

- **Interaction graph:** logging touches `main.rs::init_tracing` and the emission
  sites in `run()`/`process_statement`; no change to booking/routing control flow.
- **Error propagation:** unchanged — telemetry is additive and non-blocking (R10);
  Phase-2 flush must not flip the exit code.
- **State lifecycle risks:** none in Phase 1 (no new I/O on the money path);
  `messages_remaining` is derived from existing loop state.
- **API surface parity:** new env vars (`RECEIPT_LOG_FORMAT`, `RECEIPT_LOG_PII`,
  later an OTLP endpoint) — document in README + fleet manifest.
- **Unchanged invariants:** booking, dedup, reconcile, and routing behavior are
  untouched; the run's exit-code contract is unchanged.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| LogQL/kube-state-metrics can't express a needed signal | Verify the derivation per signal in implementation; only then add a minimal app metric (deferred question). |
| **JSON field names drift → alert silently breaks** (the same silent-failure class we're fighting) | Stable field-name **constants** + a **contract test** that fails CI on a rename (Unit 6) — not just docs. |
| **No-progress alert is blind to an exit-0 stuck run / hard-fail abort** | Explicit signal division of labor (Unit 3): KSM owns non-zero-exit + last-success age; log owns in-loop defer; the alert combines both. |
| **Loki retention < the multi-run no-progress window** makes the alert impossible | Verify retention ≥ window in planning; if short, key no-progress on KSM `last_successful_time` age (persists) rather than counting log lines. |
| **Secret echoed in an FX/DOP error body logged at `warn!`** | Concrete `redact()` helper applied to error strings before logging (Unit 5), with a test. |
| A misconfigured `debug` (incl. dry-run `info!`) ships PII to Loki | `RECEIPT_LOG_PII` default-off gate independent of `RUST_LOG`, covering the dry-run path (Unit 5). |
| Phase-2 OTLP bloats the FROM-scratch binary | Value-gated + severable; prefer OTLP-HTTP; falsifiable trigger before building. |
| Phase-2 flush hangs and eats the deadline | Bounded timeout, runs after booking, abandoned-not-fatal (R10). |

## Documentation / Operational Notes

- README: document `RECEIPT_LOG_FORMAT`, `RECEIPT_LOG_PII` (and later the OTLP
  endpoint) in the config table.
- Fleet manifest: set `RECEIPT_LOG_FORMAT=json`; keep `RECEIPT_LOG_PII` unset (off).
- Cluster-side (out of app scope): author the LogQL recording rules + the
  co-primary/correctness alert rules + dashboard (Unit 6 provides examples).

## Sources & References

- **Origin document:** [docs/brainstorms/2026-06-01-observability-requirements.md](docs/brainstorms/2026-06-01-observability-requirements.md)
- Related code: `src/main.rs::init_tracing`, `src/lib.rs` (`Summary`, `run`),
  `src/statement/pipeline.rs` (`StatementReport`, `check_balance`), `src/fx.rs`/`src/llm.rs`
  (transient classifications), `src/config.rs`.
