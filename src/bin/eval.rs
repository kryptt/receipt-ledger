//! `eval` — the objective extraction-accuracy judge.
//!
//! Runs the REAL extraction path (`unwrap_message` → adapter `prompt` → live
//! `/chat/completions` with the same params the pipeline uses → `extract_json`
//! → `postprocess` → `validate` + routing projection) for every (model,
//! example) pair in the labeled dataset, scores each field against the
//! ground-truth label, and prints a per-model × per-field accuracy matrix.
//!
//! This binary hits live models, so it is NOT part of `./test.sh`. The pure
//! pieces it leans on — the scorer and the matrix aggregation — ARE unit tested
//! there (`receipt_ledger::eval`).
//!
//! Usage:
//! ```text
//!   RECEIPT_OLLAMA_URL=http://localhost:11434/v1 cargo run --bin eval
//!   cargo run --bin eval -- --models gemma4:e2b,qwen3.6-low --json
//! ```
//! Models: `--models a,b,c` or `RECEIPT_EVAL_MODELS=a,b,c`
//!   (default `gemma4:e2b,gemma4:e4b,qwen3.6-low,qwen3.6-medium`).
//! Ollama URL: `RECEIPT_OLLAMA_URL` (default the in-cluster router).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use receipt_ledger::adapters::{self, Outcome};
use receipt_ledger::eval::{Expected, Matrix, Produced, score};
use receipt_ledger::llm::LlmClient;
use receipt_ledger::unwrap;
use receipt_ledger::validate::{Verdict, validate};

const DEFAULT_MODELS: &str = "gemma4:e2b,gemma4:e4b,qwen3.6-low,qwen3.6-medium";
const DEFAULT_OLLAMA_URL: &str = "http://ollama-router.ai:11434/v1";
/// Per-request timeout for an extraction call. Generous: a cold model on slow
/// hardware can take a while for the first request of a run.
const EVAL_TIMEOUT: Duration = Duration::from_secs(600);

#[tokio::main]
async fn main() -> Result<()> {
    install_crypto_provider();

    let opts = Options::parse();
    eprintln!(
        "eval: {} models × dataset at {}\n  models: {}\n  ollama: {}\n",
        opts.models.len(),
        opts.dataset_dir.display(),
        opts.models.join(", "),
        opts.ollama_url,
    );

    let examples = load_dataset(&opts.dataset_dir)
        .with_context(|| format!("loading dataset from {}", opts.dataset_dir.display()))?;
    if examples.is_empty() {
        anyhow::bail!("no examples found under {}", opts.dataset_dir.display());
    }
    eprintln!("loaded {} examples\n", examples.len());

    let http = Client::builder()
        .user_agent(concat!("receipt-ledger-eval/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .context("building HTTP client")?;

    let mut matrix = Matrix::new();

    for model in &opts.models {
        eprintln!("=== model: {model} ===");
        let llm = LlmClient::new(&http, &opts.ollama_url, model.clone(), EVAL_TIMEOUT);
        for ex in &examples {
            let produced = match run_one(&llm, ex).await {
                Ok(p) => p,
                Err(e) => {
                    // A model/transport error on one example is not fatal: treat
                    // it as a fully-wrong extraction (a non-transaction
                    // projection scores Wrong on every applicable field) so the
                    // matrix reflects the failure rather than crashing the run.
                    eprintln!("  [{}] {model}: ERROR {e:#}", ex.name);
                    Produced::not_a_transaction()
                }
            };
            let expected = Produced::from_expected(&ex.expected)
                .with_context(|| format!("projecting label for {}", ex.name))?;
            let scores = score(&expected, &produced);
            matrix.record(model, &scores);
            eprintln!("  [{}] {}", ex.name, summarize(&scores));
        }
        eprintln!();
    }

    // The readable table on stdout (always).
    println!("{}", matrix.render_table());

    // Optional machine-readable JSON dump.
    if opts.json {
        let json = serde_json::to_string_pretty(&matrix).context("serializing matrix to JSON")?;
        println!("\n{json}");
    }

    Ok(())
}

/// Run the REAL extraction path for one example against one model, projecting
/// the end-to-end result into the comparable [`Produced`] shape.
///
/// Faithfully mirrors `receipt_ledger::process_message`:
///   unwrap_message → adapter select → `is_transaction` prefilter →
///   adapter.prompt → llm.extract_json → adapter.postprocess →
///   validate (per record) → routing projection.
/// A record that fails validation is NOT a transaction-extraction failure (the
/// model may have extracted it perfectly; the gate just refuses to book), so we
/// still project the extracted fields — the eval judges *extraction*, while
/// `status`/`direction` capture whatever made it un-bookable.
async fn run_one(llm: &LlmClient<'_>, ex: &Example) -> Result<Produced> {
    // 1. Unwrap the forward and recover the original sender.
    let unwrapped = match unwrap::unwrap_message(Some(&ex.expected.from), &ex.body) {
        Some(u) => u,
        None => return Ok(Produced::not_a_transaction()),
    };

    // 2. Adapter selection.
    let adapter = match adapters::select(&unwrapped.original_sender) {
        Some(a) => a,
        None => return Ok(Produced::not_a_transaction()),
    };

    // 3. Deterministic non-transaction prefilter (no LLM call), matching the
    //    pipeline: a clean non-transaction projects to NotATransaction.
    if !adapter.is_transaction(&unwrapped.body) {
        return Ok(Produced::not_a_transaction());
    }

    // 4. Real LLM extraction with the SAME request params as the pipeline.
    let prompt = adapter.prompt(&unwrapped.body);
    let json = llm.extract_json(&prompt).await.context("LLM extraction")?;

    // 5. Postprocess into typed records (or a model-declared non-transaction).
    //    Uses the body-aware path so the eval scores the SAME deterministic
    //    overrides the pipeline applies (PayPal P1 cross-currency USD total).
    let records = match adapter
        .postprocess_with_body(&json, &unwrapped.body)
        .context("adapter postprocess")?
    {
        Outcome::Transaction(records) => records,
        // A transfer (PayPal-payment receipt) has no merchant/direction the eval
        // scores, and is never produced via this LLM path anyway (it is
        // deterministically extracted in the pipeline); project it like a
        // non-transaction for this purchase-extraction harness.
        Outcome::Transfer(_) => return Ok(Produced::not_a_transaction()),
        Outcome::NotATransaction { .. } => return Ok(Produced::not_a_transaction()),
    };
    let Some(record) = records.into_iter().next() else {
        return Ok(Produced::not_a_transaction());
    };

    // 6. Run the sync validation gate for parity with the pipeline. Either
    //    verdict, we project the EXTRACTED fields: the eval scores extraction
    //    accuracy, and `status`/`direction` already encode why a record would
    //    not book (a declined/refund/incoming record is correctly extracted yet
    //    intentionally routed to Review).
    match validate(record.clone()) {
        Verdict::Booked(_) | Verdict::Review { .. } => {}
    }

    Ok(Produced::from_record(&record))
}

/// A one-line per-example summary: `kind=ok amount=ok currency=WRONG ...`.
fn summarize(scores: &receipt_ledger::eval::FieldScores) -> String {
    use receipt_ledger::eval::FieldScore;
    scores
        .iter()
        .into_iter()
        .map(|(name, fs)| {
            let mark = match fs {
                FieldScore::Correct => "ok",
                FieldScore::Wrong => "WRONG",
                FieldScore::NotApplicable => "-",
            };
            format!("{name}={mark}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One labeled dataset example: the email body + its ground-truth label.
struct Example {
    /// Stem name (e.g. `08_banco_eur_approved`) for log lines.
    name: String,
    /// The raw forwarded email text (`.txt`).
    body: String,
    /// The parsed ground-truth label (`.json`).
    expected: Expected,
}

/// Load every `*.txt` + matching `*.json` pair under `dir`, sorted by name.
fn load_dataset(dir: &Path) -> Result<Vec<Example>> {
    let mut stems: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading dataset dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("txt") {
                p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    stems.sort();

    stems
        .into_iter()
        .map(|name| {
            let txt = dir.join(format!("{name}.txt"));
            let js = dir.join(format!("{name}.json"));
            let body = std::fs::read_to_string(&txt)
                .with_context(|| format!("reading {}", txt.display()))?;
            let raw = std::fs::read_to_string(&js)
                .with_context(|| format!("reading {}", js.display()))?;
            let expected: Expected = serde_json::from_str(&raw)
                .with_context(|| format!("parsing label {}", js.display()))?;
            Ok(Example {
                name,
                body,
                expected,
            })
        })
        .collect()
}

/// Parsed CLI options.
struct Options {
    models: Vec<String>,
    ollama_url: String,
    json: bool,
    dataset_dir: PathBuf,
}

impl Options {
    fn parse() -> Self {
        let mut models: Option<String> = None;
        let mut json = false;
        let mut dataset_dir: Option<PathBuf> = None;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--models" => models = args.next(),
                "--json" => json = true,
                "--dataset" => dataset_dir = args.next().map(PathBuf::from),
                other => eprintln!("ignoring unknown arg {other:?}"),
            }
        }

        let models = models
            .or_else(|| std::env::var("RECEIPT_EVAL_MODELS").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODELS.to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let ollama_url = std::env::var("RECEIPT_OLLAMA_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());

        // Default the dataset dir relative to the crate root so `cargo run`
        // from the repo finds it without an arg.
        let dataset_dir = dataset_dir.unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("eval")
                .join("dataset")
        });

        Options {
            models,
            ollama_url,
            json,
            dataset_dir,
        }
    }
}

/// Install the ring crypto provider for rustls (same as the main binary): the
/// `rustls-no-provider` feature keeps the default off, so a provider must be
/// installed process-wide before any TLS.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
