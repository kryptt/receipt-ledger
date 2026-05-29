# Plan: Banco Popular monthly statement (*estado de cuenta*) ingestion

- **Status:** ready (reviewed, 2 passes — 2026-05-29)
- **Date:** 2026-05-29
- **Owner:** rhansen
- **Repo:** `receipt-ledger` (deployed via `hr-fleet/fleet/home/receipt-ledger.yaml`)
- **Related:** `hr-fleet/docs/brainstorms/2026-05-27-receipt-ledger.md` (original service brainstorm)

## 1. Problem & goal

`receipt-ledger` currently ingests **per-transaction notification emails** (PayPal
"receipt", Banco Popular "Notificación de Consumo"), one charge per email, from the
text body, into Firefly III.

Banco Popular also emails a **monthly statement** (*estado de cuenta*) as a
**password-protected PDF**, forwarded monthly to `ledger@hr-home.xyz`. The statement is
the **authoritative, complete** list of what posted for the cycle — but it **overlaps**
the individual consumo notifications already booked.

**Goal:** make Firefly's Banco Popular accounts for a billing cycle equal the statement —
book charges the notifications missed, reconcile/confirm the ones already present, and
surface discrepancies for human review.

**Bounded source-of-truth premise** (review #18): the statement is authoritative for the
**posted amount and the existence** of a charge in the cycle. It is *not* treated as
authoritative for dispute/reversal status. Amount corrections preserve the original estimate
(see §3.7) and are guarded against both crafted input and concurrent human edits. The
**success criterion is balance parity**, asserted by the closing-balance check (§3.6, Phase-1
definition-of-done), *not* merely "every row matched" (review #13: row-level bucketing can
pass while the ledger is over-booked).

Chosen shape (2026-05-29): **"book new + reconcile"**, static sealed PDF password,
**deterministic table parsing** (no LLM — review #14), **pure-Rust PDF decryption** (review #1).

## 2. Ground truth — the real statement (decrypted sample, cut 22/05/2026)

Confirmed by decrypting a real sample:

- **Encryption: RC4 / Standard security handler, PDF 1.4** (`pdfinfo`: `algorithm:RC4`). This
  is old, widely-supported encryption → **pure-Rust decryption is viable** (`lopdf`/`pdf`
  crates handle RC4). This is the basis for dropping `pdftotext`/poppler (review #1, #7, #17).
- **One PDF = two statements stacked**: a **VISA PRESTIGE DOP** section (1 page) and a
  **VISA PRESTIGE USD** section (8 pages). Same card last-4 **7524**, same cut/payment-due
  date. Routes to the two existing Firefly accounts: **USD → 106**, **DOP → 107**.
- **Per-section header**: `LÍNEA DE CRÉDITO | CRÉDITO DISPONIBLE | FECHA DE CORTE |
  FECHA LÍMITE DE PAGO | BALANCE ANTERIOR`; footer carries `BALANCE A PAGAR` /
  `BALANCE TOTAL`. Currency implied by section title; last-4 from `****-****-****-NNNN`.
- **Cardholder sub-groups**: `TRANSACCIONES TARJETAHABIENTE PRINCIPAL` / `... ADICIONAL`.
- **Row grammar** — two physical lines per transaction:

  ```
   ENTRADA  TRANSAC  NO_DE_REFERENCIA           DESCRIPTION [ LOCATION]            [-]AMOUNT
   23/04    21/04    74542016112077674827117    JR EAST SIBUYAKU                       50.93
                                                4112   069737      <- MCC  AUTH code
  ```

  - **Two DD/MM dates, NO year**: `ENTRADA` = posting date, `TRANSAC` = authorization date.
  - **`NO. DE REFERENCIA`**: a per-row reference (10 digits for payments, ~23 for purchases).
    Notifications do not carry it. (Uniqueness/non-emptiness must be validated — review coherence #2.)
  - Continuation line: `MCC(4) AUTH(6)`.
  - **`(-)` prefix = credit/payment** (`Direction::In`); positive = charge (`Direction::Out`).
- **No original-currency or exchange-rate line anywhere.** The USD card **bills foreign
  charges already converted to USD** by the VISA network.
- Sample volume: **82 rows** (2 DOP + 80 USD), including **two identical rows**
  (`7-Eleven B315 Kastrup 7.28`) — see §3.6 cluster rule (review #5).

### 2.1 The reconciliation crux

For a **foreign** charge, the consumo notification carried the **original** currency and
amount (e.g. JPY 5130). But note **what is actually stored in Firefly**: `firefly.rs::build_group`
converts that foreign charge to the USD account currency at a Frankfurter (ECB) rate and books
the **converted USD figure as the journal's primary `amount`**, keeping the original only in
Firefly's `foreign_amount` / `foreign_currency_code`. The statement carries the **bank's actual
billed USD amount** (the VISA-network rate). Therefore `list_transactions(106)` returns
USD-denominated journals, and reconciliation is comparing the statement's **billed USD** against
the notification's **estimated USD** — two USD numbers for the same charge that are *close but
unequal*. Therefore:

- Statement and notification **will never match on amount exactly** for foreign charges (two
  independent USD estimates of the same charge).
- The matchable signals are **`TRANSAC` (auth) date ± W + card last-4 (per account) + fuzzy
  merchant**, with **amount as a within-tolerance corroborator, never an exact key** (review #3).
- **Both the merchant-similarity and the `TRANSAC`-date==notification-date premises are
  unverified and load-bearing** → they are gated behind the Phase-0 calibration (§7, review #4, #11).

## 3. Architecture decisions

### 3.1 Module placement
Not a new `Source`, not an `Adapter`. Statement rows are `Source::BancoPopular`, routed to
accounts 106/107 like consumos. New parallel module tree `src/statement/`
(`pdf.rs`, `parse.rs`, `reconcile.rs`).

### 3.2 Detection & pipeline integration (review #9)
Detection is **attachment-driven**: a message carrying an `application/pdf` attachment whose
filename/subject matches the statement pattern (subject `Cuenta: ****-****-****-NNNN | Fecha: … |`,
file `NNNN.pdf`). Currency/account derive from **PDF content** (section titles), not the sender.
A pure `classify_message()` returns `Ingest::Notification | Ingest::Statement`.

The statement path is a **separate branch in `run()`**, not `process_message` (which is
hard-wired one-message→one-`Disposition`). The branch:
1. parses → reconciles → produces a `StatementReport { reconciled, amount_mismatch, booked_new,
   unmatched_booked, payments, rows_skipped, balance_delta }`;
2. routes the **single message** to **Processed** iff the report is fully clean, else **Review**;
3. aggregates report counts into the run `Summary`.

### 3.3 JMAP attachment fetch (review #22)
The existing `client.download(blob_id)` fetches the **whole RFC822 message blob** — it is *not*
a per-attachment accessor. Fetching the PDF part requires `Property::Attachments` (or
`BodyStructure`), reading the PDF part's **own `blobId`**, then `client.download(that_blob_id)`.
This is a new code path in `jmap.rs`; `FetchedMessage` gains attachment metadata
`{ blob_id, content_type, name, size }`.

### 3.4 Decrypt + extract — pure Rust (review #1, #7, #17 — **Phase-0 spike DONE 2026-05-29**)
**No subprocess, no poppler, keeps the `FROM scratch` static-musl image.** Validated against the
real sample:
- **Crate decision: `pdf` (pdf-rs) 0.9, NOT `lopdf`.** `lopdf` 0.38 fails to load this file — its
  xref parser populates only 1 object (Root unresolvable), despite the file being spec-clean
  (classic xref, 53 objects, `/Filter /Standard /V 1 /R 2` = 40-bit RC4). `pdf` opens it cleanly
  (9 pages) and decrypts with `FileOptions::cached().password(pw).open()`.
- **`RECEIPT_BP_STATEMENT_PASSWORD`** drives decryption in-memory; **never on any argv** (no
  subprocess). Trailing-whitespace must be trimmed (the password is digits).
- **Positioned-text extraction confirmed**: iterate `page.contents.operations()`, track the CTM
  (Save/Restore/`Transform`) and text matrix (`BeginText`/`SetTextMatrix`/`MoveTextPosition`/
  `TextNewline`/`Leading`), compute each run's device (x,y) via `Trm = Tm × CTM`, group runs into
  rows by y (±~2.5pt) and order by x. This reconstructs both the DOP and USD tables faithfully —
  the documented 2-line row grammar (txn row + MCC/auth continuation), the `-` credit sign, and
  merchant+location all land in clean columns. Spike code: `/tmp/bp-spike2` (throwaway).
- **Known detail for implementation**: text bytes must be decoded via the font's encoding
  (Windows-1252 / WinAnsi), not `from_utf8_lossy` — the latter drops accented chars (`próximo` →
  `pr ximo`). Transaction-critical fields (dates, amounts, ASCII merchants) are unaffected, but
  Spanish merchant names with accents need proper decoding. Map the single-byte font encoding.
- Treat the PDF as **untrusted input** (review #17): bounded/iteration-capped parsing, pod runs
  unprivileged, tmpfs the only writable path, decrypted bytes held in memory and never written to
  disk (eliminates the tmpfs-cleartext-lingering residual).
- **Documented fallback** (only if pure-Rust layout proves inadequate in the spike): a distroless
  runtime base + statically-bundled `pdftotext`, password via stdin/file (never argv). This
  abandons the scratch/musl single-binary design and is a last resort.

### 3.5 Deterministic table parser (review #20, #21)
Segment by `VISA PRESTIGE (DOP|USD)` headers → `{currency, account, last4, cut_date, balances}`;
within each, scan the 2-line row grammar; classify direction by the `(-)` sign; capture
ref# + MCC + auth. Reuses `adapters::parse` primitives (`strip_thousands_commas`, day-first).

**Year inference rule (concrete — review #20):** dates are DD/MM, anchored on `FECHA DE CORTE`.
For each date, assume the most recent occurrence on/before the cut date; if `month > cut_month`,
assume the **prior** year (the Dec→Jan wrap). Applied independently to `ENTRADA` and `TRANSAC`
(they can straddle Jan 1). Statement dates are **DR-local** (UTC-4) DD/MM — stated assumption.

**No LLM fallback** (review #14): rows the deterministic scanner cannot confidently parse route to
**Review** (already the disposition for parse gaps). An LLM fallback is revisited only if a future
real statement produces rows the scanner genuinely cannot handle — scoped then, against that row.

Every row still passes the existing `validate` gate.

### 3.6 Reconciliation (review #2, #3, #5, #10, #13)
Add a **read** capability to `FireflyClient`:
`list_transactions(account, start, end)` → `GET /api/v1/accounts/{id}/transactions?start=&end=`.
Firefly wraps the list in `data[]` + `meta.pagination` (+ `links`); **loop `?page=` until
`meta.pagination.current_page == total_pages`**. Per journal read: `date`, `amount`,
`currency_code`, `foreign_amount`, `foreign_currency_code`, `description`, `external_id`, `tags`,
journal id. Pull `receipt-ledger`-tagged journals for accounts 106 **and** 107 over the cycle
window (± margin for the auth/posting/timezone lag).

**Match key** (review #2, #3; refined by Phase-0): candidate = same account (106/107 from section),
`|TRANSAC − journal date| ≤ W`; **score = fuzzy merchant similarity** (named crate, e.g. `strsim`
Jaro-Winkler / token-set) **+ amount-within-tolerance corroboration** (a band — exact only for
same-currency/DOP rows; foreign rows are two USD estimates). **last-4 is NOT a usable key**
(Phase-0 finding): the statement prints only the *primary* card's last-4 in the section header
(`****-****-****-7524`), never per-row, while consumos carry the *specific* cardholder's last-4
(e.g. the additional cardholder's `3389`). So last-4 cannot be matched between a statement row and a
consumo-booked journal — the key is account + date-window + merchant + same-currency-amount only.
`W` and the tolerance band are set from calibration data when a statement + its overlapping consumos
coexist (see §7 Phase-0 outcome).

**Greedy assignment + explicit ambiguity (review #5, #21, coherence #5):** assign best-score
candidates greedily, but **force a whole cluster to Review** when ≥2 candidates fall within a
score epsilon, OR when >1 statement row / >1 journal share the same
`(date-window, merchant, last4, amount-band)` cluster (the identical-rows case). Two identical
consumos collapse to **one** `composite_hash` journal, so a 2-row/1-journal cluster is expected and
reconciled **by count**, not per-row.

**Four outcomes:**
- **Matched, amount equal** → confirmed (read-only).
- **Matched, amount differs** (foreign estimate vs billed) → **Phase 1: report + Review only**
  (strictly read-only — review coherence #1/#6). **Phase 2: optional auto-correct** (§3.7).
- **Statement row, no match** → **book it**, `external_id = bpstmt:<ref#>` (validated non-empty;
  see double-book guard below). Idempotent on statement re-send.
- **Firefly journal, no statement row** → audit flag → **forces the message to Review** (review
  coherence #3). A released hold / late decline / wrong-cycle charge.

**Cross-path double-booking guard (review #2 — P0, pass-2 #7):** the consumo path keys by
`composite_hash` and the statement path by `bpstmt:<ref#>`; Firefly cannot dedup across the two
namespaces. So **before booking an unmatched statement row, re-probe the in-window journals using
the same fuzzy-key criteria as the matcher** (same date-window W, same merchant-threshold, same
amount-band, same account). If **any** unmatched journal scores above the match threshold, route to
Review instead of booking. Invariant: *no statement row is booked as new while a plausible
composite-hash journal exists unmatched in the window.* Operational precondition: ingest a statement
only after the cycle's notifications are known-booked (the reverse race — a late notification after
the statement booked — is documented and guarded in Phase 2 by a symmetric probe on the consumo
path).

**`NO. DE REFERENCIA` uniqueness (pass-2 #10):** the `bpstmt:<ref#>` external_id scheme requires
ref# to be non-empty **and unique within the statement** (across both sections). Phase-0 spike must
verify uniqueness in real samples. If two rows share a ref#, the parser flags them as ambiguous and
routes both to Review (Firefly's dedup would silently drop one otherwise).

**Closing-balance check = Phase-1 definition-of-done (review #13, pass-2 #5, #8):** a **per-cycle
delta check**, not an absolute-balance comparison (the latter would require all prior cycles to be
reconciled). After reconciliation: `sum(statement debits) − sum(statement credits)` for the cycle
window must equal `sum(Firefly journals in the same window, same account)` ±rounding. Payment rows
are now booked as transfers (credit to 106/107 — see §3.7), so they appear in the Firefly journal
sum naturally; no separate unbooked-payment adjustment is needed. A non-zero delta → **Review**.
Additionally, `BALANCE TOTAL − BALANCE ANTERIOR` can be cross-checked against the parsed-row sum
as an **internal consistency guard** before reconciliation begins (cheap; both values already
parsed).

### 3.7 Payments / credits (review #16 — decided)
Negative rows (`Pago Via App`, etc.) are `Direction::In`, which the existing `validate()` gate
hard-routes to Review (and `submit` requires a `Validated`). **Decision (2026-05-29):** payments
**are booked as transfers** from the paying bank accounts into the card liability accounts. This
requires:
- A **new `ValidatedTransfer` newtype** (pass-2 #2) that preserves the type-level booking guarantee.
  The statement path calls `validate_transfer()` → mints `ValidatedTransfer` → `submit_transfer`
  requires it. The existing `Validated` / `validate()` / `submit()` path is unchanged for
  notifications. Every write path (withdrawal, transfer, correction) requires a newtype token — no
  raw `Extracted` can reach Firefly.
- Firefly transaction type `"transfer"` (not `"withdrawal"`): `source_id` = paying bank account,
  `destination_id` = card liability account (106/107).
- **`RECEIPT_MAX_AMOUNT` applies to transfers too** (pass-2 #3): the existing USD-equivalent
  ceiling gates payment/transfer bookings, not just withdrawals. A crafted large-negative PDF row
  cannot trigger an uncapped transfer from the savings account.
- **Two new paying-account config env vars** (dual-currency card payments come from two bank
  accounts), following the **optional-with-Review pattern** (pass-2 #6, same as
  `banco_popular_usd_account`):
  - `RECEIPT_BP_PAYING_USD_ACCOUNT` — Banco Popular **USD savings** account (Firefly numeric id).
  - `RECEIPT_BP_PAYING_DOP_ACCOUNT` — Banco Popular **DOP checking** account (Firefly numeric id).
  Payment rows on the USD section → transfer from USD savings → account 106; payment rows on the
  DOP section → transfer from DOP checking → account 107. **If absent, payment rows route to Review
  (not a startup crash)** — this avoids breaking existing deployments that haven't added them.
- Payment rows are included in the **closing-balance math** and the reconciliation report.

**Amount auto-correction (Phase 2 — pass-2 #1):** moved from Phase 1 to Phase 2. Rationale: it is
the only non-idempotent write path that mutates existing journals; Phase 1 achieves balance parity
by detecting mismatches and routing to Review; the closing-balance check already surfaces the delta.
Deferring to Phase 2 removes the highest-risk write path from the MVP and simplifies idempotency
reasoning. When enabled (`RECEIPT_BP_AUTOCORRECT_AMOUNTS`, default off):
- **TOCTOU guard (pass-2 #4):** before PUTting, read the journal's current amount and compare to
  the expected pre-correction value (the ECB estimate). If it matches **neither** the estimate
  **nor** the billed amount (i.e. a human or another process modified it), skip the correction →
  Review. This prevents silently overwriting manual corrections.
- **Bounded delta**: reject corrections where `|billed − estimate| / estimate` exceeds a
  configurable `RECEIPT_BP_MAX_CORRECTION_PCT` (e.g. 20%) → route to Review instead.
- **Preserve the original**: store the old estimate in a Firefly note or tag
  (`bp-estimate:<old_amount>`) so the correction is auditable and reversible.
- **Idempotency**: a re-run that re-corrects to the same billed amount is a no-op (PUT the same
  value). The note/tag is written once (detected by checking existing notes), not appended.

### 3.8 Idempotency & re-run safety (review #12)
No per-row state is persisted; the JMAP cursor saves only at end-of-run. A crash mid-statement
re-processes the message next run, which is safe because: charge booking uses the stable
`bpstmt:<ref#>` external_id (Firefly dedups re-creates); transfer booking uses a similar stable id;
reconciliation read + report + audit flags are **recomputed each run** (idempotent, not persisted).
Phase-1 writes are all-or-nothing safe: POST with dedup key is idempotent. The Phase-2 amount
auto-correction PUT is the only path that mutates existing journals — its idempotency + TOCTOU
properties are documented in §3.7.

## 4. Disposition, config, fleet

- **`Summary`** gains `reconciled / amount_mismatch / booked_new / unmatched_booked / payments /
  rows_skipped / balance_delta`. Message → Processed iff fully clean; Review on any unmatched-booked
  journal, amount mismatch, ambiguous cluster, parse gap, payment row, or non-zero balance delta.
- **Logging (review security #8):** the per-run report logs **aggregate counts at `info!`**;
  per-row merchant/amount/last-4 detail only at `debug!` (off in prod) or masked. Financial line
  items must not land in Loki at info level. (Note Loki retention for `receipt-ledger`.)
- **Config** (new env, `config.rs` boundary pattern):
  - `RECEIPT_BP_STATEMENT_PASSWORD` — required-for-feature, **SealedSecret**.
  - `RECEIPT_BP_STATEMENT_SENDER` / subject discriminator.
  - `RECEIPT_BP_RECONCILE_DATE_WINDOW_DAYS` — set from Phase-0, not guessed.
  - `RECEIPT_BP_MERCHANT_MATCH_THRESHOLD` — set from Phase-0 calibration.
  - `RECEIPT_BP_PAYING_USD_ACCOUNT` — optional, Banco Popular USD savings (Firefly numeric id).
    Absent → payment rows on USD section route to Review.
  - `RECEIPT_BP_PAYING_DOP_ACCOUNT` — optional, Banco Popular DOP checking (Firefly numeric id).
    Absent → payment rows on DOP section route to Review.
  - `RECEIPT_BP_AUTOCORRECT_AMOUNTS` — Phase 2, default off. When on, enables amount auto-correction.
  - `RECEIPT_BP_MAX_CORRECTION_PCT` — Phase 2, max allowed `|billed−estimate|/estimate` for
    auto-correction (default 20%; exceeding → Review).
- **Firefly token scope (pass-2 #9):** the existing `FIREFLY_III_ACCESS_TOKEN` is read+write and
  shared across all paths. Firefly does not natively support scoped tokens. **Accepted risk:**
  compromise of the pod grants full ledger write. Mitigation: the pod runs on stable-labeled nodes
  only; the SealedSecret is scoped to `receipt-ledger-secrets` (not namespace-wide); the write
  surface is bounded (only POST with dedup key in Phase 1; PUT with TOCTOU guard in Phase 2).
  If Firefly gains scoped tokens, prefer a read-only credential for Phase-1.
- **Fleet** (`fleet/home/receipt-ledger.yaml` + `sealed-receipt-ledger.yaml`): add the sealed
  password key and a tmpfs scratch volume. **No `poppler-utils`** (pure-Rust decrypt). Release path
  is **ghcr via tag-triggered CI** (`.github/workflows/release.yml`), *not* the internal
  `registry.hr-home.xyz`/`build.sh` flow: bump `Cargo.toml` version → `./test.sh` → push a semver
  git tag (CI builds + pushes `ghcr.io/kryptt/receipt-ledger:<tag>`) → bump `image:` in the fleet
  manifest → run `scripts/validate-manifests.sh` → push.
- **Job budget (review #6/P3):** Phase-1 is deterministic-only (no per-row LLM), comfortably within
  `activeDeadlineSeconds: 600` for ~82 rows + paginated reads. (The LLM-per-row budget risk is moot
  now that the fallback is cut.)

## 5. Testing
- **Unit:** `classify_message`; section segmentation; the 2-line row grammar; sign→direction; the
  concrete year-inference rule (incl. Dec→Jan wrap, both date columns); ref#-non-empty validation;
  the matcher (date window, merchant similarity, amount-tolerance corroboration, identical-rows
  cluster→Review, double-book guard, greedy ambiguity→Review).
- **Integration** (`tests/banco_statement_pipeline.rs`): decrypted-layout fixture → parse →
  reconcile against a **mocked** Firefly transaction list (incl. `meta.pagination`) → assert the
  buckets + balance-delta.
- **Decrypt:** pure-Rust, testable on host (no native dep) — a sanitized encrypted fixture.
- **Eval:** sanitized (digit-masked) statement fixtures under `eval/dataset/`.

## 6. Resolved decisions
**Architecture (review pass 1):** decrypt approach (pure-Rust RC4), LLM fallback (cut),
closing-balance audit (Phase-1 DoD), source-of-truth scope (posted amount/existence only),
Phase-2 split into independent items.

**User calls (2026-05-29):**
1. **Payments** — book as transfers from USD savings / DOP checking → card liability (§3.7).
2. **Amount correction** — auto-correct, bounded + audit-noted (§3.7).

**Review pass 2 revisions:** amount auto-correction moved to Phase 2 (non-idempotent write, MVP
achieves parity without it); `ValidatedTransfer` newtype preserves type-level booking safety;
paying-account config uses optional-with-Review pattern; `RECEIPT_MAX_AMOUNT` applies to transfers;
TOCTOU guard on PUT; closing-balance is a per-cycle delta check; double-book guard uses the same
fuzzy criteria as the matcher; Phase-0 gains exit criteria and ref# uniqueness check; Firefly token
scope is an accepted/documented risk.

## 7. Phasing

### Phase 0 — Calibration & decrypt spike (GATE, review #4, #11, #23, #1)
Cheap, before building the matcher. Using the real sample + key:
1. **Decrypt spike** — ✅ **DONE 2026-05-29**: `pdf` crate decrypts the RC4 sample and positioned
   text reassembles both DOP+USD tables faithfully (§3.4). `lopdf` rejected (xref parse fails). The
   distroless+pdftotext fallback is **not needed**.
2. **TRANSAC-date check** — ⚠️ **inconclusive (data) 2026-05-29**: consumo `Fecha` is a clean
   `DD/MM/YYYY` transaction date (matchable in principle), but the mailbox lacks cleanly-parseable
   in-cycle consumos to confirm `Fecha == TRANSAC` exactly. Deferred to a cycle with overlap; until
   then use a generous `W` (≈5d) and route non-exact to Review.
3. **Merchant calibration** — ⚠️ **insufficient data 2026-05-29**: too few in-cycle consumo↔row
   pairs to set a threshold (see Phase-0 outcome). **Default conservative**: non-exact merchant
   matches → Review (no auto-book), per the plan's existing rule. Calibrate on a future cycle.
4. **Baseline sizing** — ✅ **DONE 2026-05-29, decisive**: the mailbox holds ~16 consumo emails for
   **one card only** (3389, additional cardholder) vs **82 charges** on the statement. Notifications
   cover **<20%** of the cycle. **The statement is highly additive — the "do-nothing baseline" /
   "low miss rate" concern (adversarial #23) is refuted for this real data.** Row-level
   reconciliation (not just a balance audit) is justified.
5. **Ref# uniqueness** — ✅ **DONE 2026-05-29**: 82 txn rows, 82 distinct ref#s, 0 duplicates,
   0 empty (both identical `7-Eleven` rows carry distinct ref#s). `bpstmt:<ref#>` is safe.

**Exit criteria (pass-2 #11):**
- Decrypt spike fails → fall back to distroless+pdftotext (§3.4 documented fallback).
- TRANSAC dates differ by >3 days from notification dates → widen `W` but flag the documented risk
  that a wider window increases ambiguity; if >7 days, date-anchoring is unreliable and the
  architecture must shift to ref#-based or amount-based matching.
- Merchant similarity below 0.6 on >30% of pairs → date+amount-only matching with **mandatory
  Review** for all non-exact matches (no auto-book); merchant signal is demoted to tiebreaker.
- Miss rate <5% → consider making the closing-balance audit the MVP and deferring row-level
  reconciliation.

**Phase-0 outcome (2026-05-29):** decrypt ✅ (pure-Rust `pdf`), ref# uniqueness ✅, baseline ✅
(notifications cover <20% → row-level reconciliation justified, build it). Calibration of `W` +
merchant threshold is **blocked on data** (mailbox consumos are sparse, single-card, and mostly
out-of-cycle), **not on design** — so Phase 1 ships with the conservative default (non-exact →
Review, no auto-book) and `W≈5d`, and the threshold is tuned on the first cycle where a statement
and its overlapping consumos are both present. Two structural refinements landed: **last-4 is not a
cross-source match key** (§3.6), and **notification forwarding is partial/unreliable** (only one
cardholder's card present), reinforcing statement-as-source-of-truth.

### Phase 1 — MVP
detect → pure-Rust decrypt → deterministic parse (both sections) → reconcile (read-only for
amount-differs) → book unmatched charges (double-book guard + `bpstmt:` id) → **book payments as
transfers** (USD savings / DOP checking → card liability) → **closing-balance check** → report +
Review on any flag. Amount mismatches detected and reported but **not auto-corrected** (deferred
to Phase 2).

**Phase-1 progress (2026-05-29):**
- ✅ **Decrypt + extraction** (`src/statement/pdf.rs`): `pdf` crate, in-memory `.load()`, CTM +
  text-matrix tracking, Windows-1252 decoding, pure `group_runs` (unit-tested). 
- ✅ **Deterministic parser** (`src/statement/parse.rs`): section segmentation (page-repeat
  dedup), 2-line row grammar, sign→direction, year inference (Dec→Jan), MCC/auth continuation —
  12 unit tests. **Validated end-to-end on the real PDF: 2 sections (DOP+USD), 82 txns (80 charges
  + 2 payments), correct cut date + last-4.** Dev tool: `examples/statement_dump.rs`.
- ⚠️ **`balance_total` not yet captured**: the footer summary box (`CUOTAS VENCIDAS … BALANCE
  TOTAL`) is rendered inside a **Form XObject** that the extractor doesn't recurse into — `Op::
  XObject` handling (resolve the form's own content stream, process with the current CTM) is the
  fix, to be done with the reconcile module (which consumes `balance_total` for the closing-balance
  check). The transaction table is in the page content stream and extracts fully.
- ✅ **Reconcile matcher** (`src/statement/reconcile.rs`, pure): fuzzy match of statement charges
  against `ExistingJournal`s — Jaro-Winkler merchant similarity + date window + amount-tolerance,
  greedy 1:1, and the four outcomes (Confirmed / AmountMismatch / BookNew / Review). Includes the
  **cross-path double-book guard** (gray-zone near-match → Review, never book), ambiguity/identical-
  cluster → Review, prior-`bpstmt:`-booking confirm (re-run idempotency), and unmatched-journal
  audit. 8 unit tests. Thresholds in `ReconcileParams` (default `merchant≥0.85`, `W=5d`) —
  documented as calibration-pending (Phase-0 couldn't tune them).
- ✅ **Firefly read** (`firefly.rs::list_transactions`): paginated `GET …/accounts/{id}/transactions`
  → `ExistingJournal`s (receipt-ledger-tagged only), pure `parse_transactions_page` + tests.
- ✅ **Transfer write path** (payments): `ValidatedTransfer` gate (`validate.rs`) +
  `FireflyClient::submit_transfer` (currency-routed paying→card, no FX) + the two
  `RECEIPT_BP_PAYING_{USD,DOP}_ACCOUNT` config vars (optional-with-Review). Charge booking already
  works via `to_extracted`→`validate`→`submit`.
- ⏭️ **Remaining (final wiring)**: JMAP attachment fetch (§3.3, per-part blob + `FetchedMessage`
  attachment metadata), `classify_message` + the `run()` statement branch (§3.2) that drives
  detect→decrypt→parse→reconcile→book charges (`BookNew`)+payments(transfers)→report→route, apply
  `RECEIPT_MAX_AMOUNT` to the statement path, and the closing-balance check (needs the `Op::XObject`
  footer fix for `balance_total`).

### Phase 2 — independent, separately-shippable items (review #15), each tagged by goal
- **(a) Amount auto-correction** — *parity goal*: auto-correct foreign-charge amounts to the
  statement's billed figure (PUT with TOCTOU guard + bounded delta + audit note — see §3.7).
  `RECEIPT_BP_AUTOCORRECT_AMOUNTS` (default off).
- **(b) Emailed reconciliation report via JMAP submission** — *convenience* (note: re-exports PII
  over email; security pass needed when planned).
- **(c) MCC → Firefly category** — *categorization, not parity*; consider a separate plan. MCC is
  already captured in the parsed row regardless.
- **(d) Symmetric consumo-side double-book probe** — closes the late-notification race (review
  #5/#2 reverse).
