# receipt-ledger

A one-shot CLI that ingests transaction-notification emails from a dedicated
mailbox and books the transactions into [Firefly III](https://www.firefly-iii.org/).
It runs once and exits — designed to be invoked hourly as a Kubernetes CronJob.

v1 implements the **PayPal** source only.

## Pipeline

```
JMAP read (Stalwart, incremental via Email/changes state cursor)
  → unwrap Gmail forward + detect original sender
  → per-sender adapter builds an extraction prompt
  → ollama-router (liveness-selected small model) returns JSON
  → deterministic validation gates (status approved, amount > 0, known
    currency, merchant present)
  → dedup (PayPal Transaction ID; composite sha256 fallback)
  → POST Firefly /api/v1/transactions (error_if_duplicate_hash)
  → move the message to the Processed or Review mailbox
```

The money-touching steps (validation, dedup, submission) are deterministic and
unit-tested. The LLM only extracts fields; nothing it returns is booked without
passing the gates.

### Exit codes

- `0` — success, including "nothing new to do".
- non-zero — a real failure (config / auth / connection), so the CronJob's
  `CronJobFailing` alert fires. Per-message parse/validation failures route the
  offending message to `Review` and do **not** fail the job.

## Configuration (environment variables)

| Variable | Default | Notes |
|---|---|---|
| `RECEIPT_JMAP_URL` | `http://stalwart.system.svc.cluster.local:8080` | JMAP base; session discovered at `/.well-known/jmap`. |
| `RECEIPT_JMAP_USER` | `ledger@example.test` | Basic-auth user. |
| `RECEIPT_JMAP_PASSWORD` | — (required) | Basic-auth password. |
| `RECEIPT_STATE_PATH` | `/state/jmap.state` | Persisted `Email/changes` state cursor. |
| `RECEIPT_OLLAMA_URL` | `http://ollama-router.ai:11434/v1` | OpenAI-compatible base. |
| `RECEIPT_MODEL_ALLOWLIST` | `gemma4:e2b` | Comma-separated, priority order. |
| `RECEIPT_FIREFLY_URL` | `http://firefly:8080` | Firefly III base. |
| `FIREFLY_III_ACCESS_TOKEN` | — (required) | Firefly personal access token. |
| `RECEIPT_PAYPAL_ACCOUNT` | — (required) | PayPal asset account: name or numeric id. |
| `RECEIPT_PROCESSED_MAILBOX` | `Processed` | Destination for booked mail. |
| `RECEIPT_REVIEW_MAILBOX` | `Review` | Destination for un-bookable mail. |
| `RUST_LOG` | `info` | `tracing` env filter. |

## Build & test

```bash
./test.sh    # docker buildx build --target test (runs cargo test in musl)
./build.sh   # builds + pushes registry.hr-home.xyz/kryptt/receipt-ledger:<Cargo.toml version>
```

`build.sh` refuses to push a tag that already exists — bump `version` in
`Cargo.toml` first.

## License

MIT — see [LICENSE](LICENSE).
