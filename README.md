# receipt-ledger

A one-shot CLI that ingests transaction-notification emails from a dedicated
mailbox and books the transactions into [Firefly III](https://www.firefly-iii.org/).
It runs once and exits — designed to be invoked hourly as a Kubernetes CronJob.

Supports the **PayPal** and **Banco Popular Dominicano** sources.

## Pipeline

```
JMAP read (Stalwart, incremental via Email/changes state cursor)
  → unwrap Gmail forward + detect original sender
  → per-sender adapter: deterministic pre-filter (skip non-transaction mail),
    then build an extraction prompt
  → ollama-router (liveness-selected small model) returns JSON
  → deterministic validation gates (closed status classification — only an
    Approved status books; outgoing direction only; amount > 0; known currency;
    merchant present)
  → dedup (PayPal Transaction ID; composite sha256 over
    source|date|amount|currency|merchant|last4|status as fallback)
  → route to the Firefly account (PayPal balance/credit, Banco USD/DOP),
    convert foreign charges to the account currency via FX (Frankfurter)
  → USD-equivalent ceiling: convert the charge to USD via FX and route to
    Review if it exceeds the optional RECEIPT_MAX_AMOUNT (a USD threshold)
  → POST Firefly /api/v1/transactions (error_if_duplicate_hash)
  → move the message to Processed (booked / duplicate / clean skip) or Review
```

The money-touching steps (validation, dedup, FX conversion, submission) are
deterministic and unit-tested. The LLM only extracts fields; nothing it returns
is booked without passing the gates — enforced at the type level: only the
validation pass can mint the `Validated` token that `firefly::submit` requires.

### Exit codes

- `0` — success, including "nothing new to do".
- non-zero — a real failure (config / auth / connection), so the CronJob's
  `CronJobFailing` alert fires. Per-message parse/validation failures route the
  offending message to `Review` and do **not** fail the job. Mail that is not a
  transaction at all (shipping updates, plan reminders, surveys) is a clean skip
  to `Processed`, not a Review.

## Configuration (environment variables)

| Variable | Default | Notes |
|---|---|---|
| `RECEIPT_JMAP_URL` | `http://stalwart.system.svc.cluster.local:8080` | JMAP base; session discovered at `/.well-known/jmap`. |
| `RECEIPT_JMAP_USER` | `ledger@example.test` | Basic-auth user. |
| `RECEIPT_JMAP_PASSWORD` | — (required) | Basic-auth password. |
| `RECEIPT_STATE_PATH` | `/state/jmap.state` | Persisted `Email/changes` state cursor. |
| `RECEIPT_OLLAMA_URL` | `http://ollama-router.ai:11434/v1` | OpenAI-compatible base. |
| `RECEIPT_MODEL_ALLOWLIST` | `gemma4:e2b` | Comma-separated, priority order. |
| `RECEIPT_LLM_TIMEOUT_SECS` | `600` | Per-request timeout for the extraction call. |
| `RECEIPT_FIREFLY_URL` | `http://firefly:8080` | Firefly III base. |
| `FIREFLY_III_ACCESS_TOKEN` | — (required) | Firefly personal access token. |
| `RECEIPT_FX_URL` | `https://api.frankfurter.dev/v1` | FX-rate provider (Frankfurter-compatible). |
| `RECEIPT_PAYPAL_BALANCE_ACCOUNT` | — (required) | PayPal balance account — **numeric Firefly id**. |
| `RECEIPT_PAYPAL_CREDIT_ACCOUNT` | — (optional) | PayPal Credit account id; absent → credit-funded mail → Review. |
| `RECEIPT_BANCO_POPULAR_USD_ACCOUNT` | — (optional) | Banco Popular USD account id; absent → non-DOP mail → Review. |
| `RECEIPT_BANCO_POPULAR_DOP_ACCOUNT` | — (optional) | Banco Popular DOP account id; absent → DOP mail → Review. |
| `RECEIPT_MAX_AMOUNT` | — (optional) | Plausibility ceiling, **in US dollars**. A charge whose USD-equivalent (`fx_rate(currency→USD) × amount`) exceeds it routes to Review. Unset → no upper bound. So `100000` means ">$100,000 USD → Review"; a ₩100,000 (≈ $72) charge does **not** trip it. |
| `RECEIPT_PROCESSED_MAILBOX` | `Processed` | Destination for booked / duplicate / skipped mail. |
| `RECEIPT_REVIEW_MAILBOX` | `Review` | Destination for un-bookable mail. |
| `RUST_LOG` | `info` | `tracing` env filter. |

All account variables take a **numeric Firefly account id** (e.g. `103`); a
non-numeric value is a hard startup error. The deployment routes ids
`103`/`105`/`106`/`107` (PayPal balance / PayPal credit / Banco USD / Banco DOP).

## Build & test

```bash
./test.sh    # docker buildx build --target test (runs cargo test in musl)
./build.sh   # builds + pushes registry.hr-home.xyz/kryptt/receipt-ledger:<Cargo.toml version>
```

`build.sh` refuses to push a tag that already exists — bump `version` in
`Cargo.toml` first.

## Extraction-accuracy eval

An objective judge for comparing models and prompt changes. It runs the **real**
extraction path (`unwrap_message` → adapter `prompt` → live `/chat/completions`
with the same params the pipeline uses → `extract_json` → `postprocess` →
`validate` + routing) over a labeled dataset and scores each field
(kind / amount / currency / direction / date / merchant / status / routed
account) against the ground truth, printing a per-model × per-field accuracy
matrix.

- **Dataset**: `eval/dataset/` — paired `*.txt` (forwarded email body) + `*.json`
  (ground-truth label). All values are invented/scrubbed.
- **Pure scorer**: `src/eval/` (`score`, the matrix aggregation) is unit tested
  under `./test.sh`. Only the live model calls (in `src/bin/eval.rs`) hit the
  network, so the crate builds and tests offline.

```bash
# Models in-cluster: port-forward ollama-router first.
kubectl port-forward -n ai svc/ollama-router 11434:11434 &
RECEIPT_OLLAMA_URL=http://localhost:11434/v1 cargo run --bin eval

# Specific models + JSON dump:
RECEIPT_OLLAMA_URL=http://localhost:11434/v1 \
  cargo run --bin eval -- --models gemma4:e2b,qwen3.6-low --json
```

Models default to `gemma4:e2b,gemma4:e4b,qwen3.6-low,qwen3.6-medium`
(override with `--models a,b,c` or `RECEIPT_EVAL_MODELS`). The `eval` binary is
**not** part of `./test.sh`.

## License

MIT — see [LICENSE](LICENSE).
