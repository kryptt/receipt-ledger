# Observability — log fields, LogQL rules, and alerts

receipt-ledger is a one-shot hourly CronJob. It does **not** push metrics; it emits
**structured JSON logs** (set `RECEIPT_LOG_FORMAT=json`) that Alloy already ships to
Loki. Metrics and alerts are derived **cluster-side** from these log fields via LogQL
recording rules, plus **kube-state-metrics** for run liveness.

> These rules/dashboards are **examples to apply to your cluster**. The app's job is
> to emit the fields below; wiring the recording/alert rules into Mimir/Prometheus +
> Alertmanager and a Grafana dashboard is the cluster-side step that makes alerts
> actually fire. **Until that step is done, the fields are queryable but nothing
> pages.**

## Stable field contract

The LogQL rules key on these field names. They are compile-time literals at the
emission sites (`src/main.rs`, `src/lib.rs`, `src/statement/pipeline.rs`); renaming a
field is a code change visible in review — keep this list and the rules in sync.

**Run-complete event** — message `run complete` (one per successful run, `src/main.rs`):
`processed`, `booked`, `duplicates`, `review`, `skipped`, `statements`, `corrected`,
`deferred`.
- `deferred > 0` ⇒ messages were held in INBOX this run (no-progress / provider outage).

**Per-message outcome** — message `message outcome` (`src/lib.rs`):
`source` (`paypal`/`paypal_payment`/`banco_popular`/`unknown`), `disposition`
(`booked`/`duplicate`/`skipped`/`review`), `review_reason_category` (bounded:
`no_adapter`/`not_a_forward`/`over_ceiling`/`double_book`/`currency_mismatch`/
`no_account`/`extraction`/`other`; only on review).

**Statement reconcile** — message `statement reconciliation complete`
(`src/statement/pipeline.rs`): `reconciled`, `booked_new`, `payments_booked`,
`amount_mismatch`, `corrected`, `unmatched_booked`, `balance_mismatch`, `deferred`,
`review`, `balance_checked` (bool), `balance_delta` (number, or `absent`).
- correctness: `balance_checked == true AND balance_delta != 0` (or `balance_mismatch > 0`).

**Model-selection failure** — message `model selection failed`, field
`stage="model_selection"` (`src/lib.rs`). Emitted before the run aborts (non-zero exit).

## Signal division of labor

| Failure | Caught by |
|---|---|
| Hard failure (model-selection abort, JMAP-connect fail, panic → non-zero exit) | **kube-state-metrics** (`kube_job_status_failed`) + the `model selection failed` log event |
| When did a run last *succeed* | **kube-state-metrics** `kube_cronjob_status_last_successful_time` |
| Exit-0 **no-progress** (defer-forever: run succeeds but books nothing, INBOX not draining) | **log**: `deferred > 0` sustained — NOT a Job failure, so KSM is blind to it |
| Review pile-up (by-design flags) | **log**: `message outcome` `disposition="review"` rate |
| Wrong-but-booked statement | **log**: `balance_checked=true AND balance_delta!=0` |

The **no-progress** co-primary alert combines both: `deferred > 0` sustained across N
runs **AND/OR** KSM `last_successful_time` age — neither alone is sufficient (a hard
abort emits no run-complete summary; an exit-0 stuck run is not a Job failure).

> **Loki retention must be ≥ the no-progress window.** A "deferred across N runs"
> alert needs the run-complete lines retained that long. If retention is short, key
> no-progress on KSM `last_successful_time` age (which persists) instead.
>
> **Verified (2026-06-01, this cluster):** `loki-stack` (ns `monitor`, filesystem
> tsdb, `compactor.retention_enabled: true`) has a **14d default retention**; the
> `retention_stream` overrides don't match `{namespace="home", app="receipt-ledger"}`,
> so our lines get the 14d default. The example windows here are ≤ 12h (`deferred`
> 3h, `balance_delta` 12h) — **14d ≫ 12h**, so log-counted no-progress is safe. The
> co-primary alert still ANDs in KSM `last_successful_time`, so it holds even if
> retention is later shortened.

## Example LogQL recording rules

Adapt the Loki stream selector (`{namespace="home", app="receipt-ledger"}`) to your setup.

```yaml
# Loki ruler recording rules (example)
groups:
  - name: receipt-ledger
    rules:
      # Per-source × disposition rate (review-rate, volumes).
      - record: receiptledger:outcomes:rate5m
        expr: |
          sum by (source, disposition) (
            count_over_time(
              {namespace="home", app="receipt-ledger"}
                | json | line_format "{{.disposition}}"
                | disposition != "" [5m]
            )
          )
      # No-progress: deferred messages on the last run-complete line.
      - record: receiptledger:deferred:last
        expr: |
          last_over_time(
            {namespace="home", app="receipt-ledger"}
              | json | __error__="" | deferred != ""
              | unwrap deferred [3h]
          )
      # Correctness: statement closing-balance delta (when checked).
      - record: receiptledger:balance_delta:last
        expr: |
          last_over_time(
            {namespace="home", app="receipt-ledger"}
              | json | balance_checked="true"
              | unwrap balance_delta [12h]
          )
```

## Example alert rules

```yaml
groups:
  - name: receipt-ledger
    rules:
      # CO-PRIMARY 1 — no-progress: messages stuck deferred AND no recent success.
      - alert: ReceiptLedgerNoProgress
        expr: |
          receiptledger:deferred:last > 0
          and on()
          (time() - kube_cronjob_status_last_successful_time{cronjob="receipt-ledger"} > 3*3600)
        for: 3h
        labels: { severity: warning }
        annotations:
          summary: "receipt-ledger is deferring without progress (provider outage / defer-forever)"

      # CO-PRIMARY 2 — review-rate over a window.
      - alert: ReceiptLedgerReviewPileup
        expr: sum(receiptledger:outcomes:rate5m{disposition="review"}) > 5
        for: 30m
        labels: { severity: warning }
        annotations:
          summary: "receipt-ledger is routing many messages to Review"

      # CORRECTNESS — statement booked but does not reconcile.
      - alert: ReceiptLedgerBalanceMismatch
        expr: abs(receiptledger:balance_delta:last) > 0.01
        for: 0m
        labels: { severity: warning }
        annotations:
          summary: "A Banco Popular statement booked but its closing balance does not reconcile"

      # LIVENESS — run failed (hard error / model-selection abort).
      - alert: ReceiptLedgerRunFailing
        expr: kube_job_status_failed{job_name=~"receipt-ledger.*"} > 0
        for: 0m
        labels: { severity: warning }
        annotations:
          summary: "receipt-ledger run failed (non-zero exit)"
```

## Traces

Phase 2 (value-gated, **off by default**). When an OTLP endpoint is configured the
binary exports a per-run span tree over **OTLP/HTTP (JSON)** so a run's Loki log lines
link to its Tempo trace. With the endpoint unset, behavior is byte-for-byte identical
to logs-only: no exporter, no provider, no per-event cost.

**Enable it** by setting the standard env var on the CronJob:

```
OTEL_EXPORTER_OTLP_ENDPOINT=http://<tempo-distributor>.<ns>:4318
```

- Traces export **only** when `OTEL_EXPORTER_OTLP_ENDPOINT` is set to a non-blank
  value. The **fleet manifest must set this** to turn traces on; the operator wires
  the real endpoint. Tempo's OTLP/HTTP ingest is typically
  `http://<tempo-distributor>.<ns>:4318` (the binary appends `/v1/traces`).
- Transport is **HTTP, not gRPC** — no `tonic` is pulled. The only new dependencies
  are pure-Rust (`opentelemetry*`, `prost`, `const-hex`); the static-musl build is
  unaffected.

**Span tree.** One **root span per run** (`stage="run"`), with child stage spans:
`fetch`, `model_selection`, per-message `process` (with a child `extract` span for the
LLM call), and for statements `statement` → `decrypt` / `parse` / `reconcile_book`.

**Loki↔Tempo link.** Every JSON log line emitted within the run carries the run's
Tempo `trace_id`, recorded on the **root span** — `tracing`'s JSON fmt layer renders
enclosing-span fields under a top-level **`spans`** array (one object per enclosing
span, outermost first), **not** as a top-level key. A confirmed line looks like:

```json
{"timestamp":"…","level":"INFO","fields":{"message":"new messages to process","count":3},
 "target":"receipt_ledger","span":{"stage":"fetch","name":"fetch"},
 "spans":[{"stage":"run","trace_id":"5b8aa5a2…","name":"run"},{"stage":"fetch","name":"fetch"}]}
```

So `trace_id` lives at `spans[0].trace_id` (the root `run` span is element 0). A naive
`| json | trace_id != ""` finds nothing because there is no top-level `trace_id`. Loki's
`| json` flattens nested JSON with `_` separators and array elements by index, so the
root span's id is exposed as the label **`spans_0_trace_id`**:

```logql
# Find a run's lines and surface its Tempo trace id, then pivot to Tempo by that id.
{namespace="apps", app="receipt-ledger"}
  | json
  | spans_0_trace_id != ""
  | line_format "{{.spans_0_trace_id}} {{.message}}"
```

The flattened accessor name (`spans_0_trace_id`) is what Loki produced for the JSON shape
above; **verify it against your Loki version** (older/newer `| json` flatteners may differ,
and a `label_format`/`drop` in your pipeline can rename it) and adjust the accessor if
needed. The JSON shape itself — `trace_id` under `spans[0]` (the root `run` span), never
top-level — is fixed by the emitter and confirmed from the binary's output.

**No PII in spans.** Span attributes are an allowlist — `stage`, `outcome`, and counts
only. In particular the `extract`/LLM span carries **none** of: the prompt, the model
completion, the raw email body, merchant, amount, last-4, or ref#. A span-shaping test
(`extract_span_carries_no_pii_fields`) pins this.

**Flush is bounded and non-blocking.** After the run, the tracer provider is
force-flushed and shut down on a small bounded budget (3s). If the collector is
unreachable or slow the flush is abandoned within the timeout — the run still books and
exits with its normal code; telemetry can never delay the run or flip the exit code
(test: `shutdown_is_bounded_when_collector_unreachable`).

## Dashboard

A ready-to-import Grafana dashboard lives at [`dashboard.json`](./dashboard.json)
(schemaVersion 39, templated `datasource`). Its panels mirror the recording/alert
rules above:

- **Dispositions by source over time** — `receiptledger:outcomes:rate5m` stacked by
  `source` / `disposition`.
- **Review rate (5m)** — `sum(receiptledger:outcomes:rate5m{disposition="review"})`,
  red past the `ReceiptLedgerReviewPileup` threshold (> 5).
- **Deferred on last run (no-progress)** — `receiptledger:deferred:last`.
- **Time since last successful run** — age of
  `kube_cronjob_status_last_successful_time{cronjob="receipt-ledger"}`, red past the
  3h no-progress window (the two together are the `ReceiptLedgerNoProgress` co-primary).
- **Failed jobs** — `kube_job_status_failed{job_name=~"receipt-ledger.*"}`
  (`ReceiptLedgerRunFailing`).
- **Statement closing-balance delta** — `abs(receiptledger:balance_delta:last)`, red
  past `0.01` (`ReceiptLedgerBalanceMismatch`).

Point its `datasource` variable at the Prometheus/Mimir instance holding the recorded
metrics + kube-state-metrics. As with the rules, importing it is the cluster-side step;
the app's job is only to emit the fields.

## Follow-up

- The automated field-name **contract test** is now implemented
  (`tests/obs_field_contract.rs`). A `tracing` capture layer records the field-name set
  per event message; the test drives each emit-helper (`log_run_complete`,
  `log_message_outcome`, `log_statement_reconcile`, `log_model_selection_failed`) and
  asserts the captured set equals the canonical constants in `src/obs_fields.rs` (the
  single source of truth for every field name and event message above). Renaming or
  dropping a field at an emission site now fails CI instead of silently breaking a rule.
