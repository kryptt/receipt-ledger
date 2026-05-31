//! PDF decryption + positioned-text extraction for Banco Popular statements.
//!
//! The statement is RC4/Standard-encrypted (the simplest PDF encryption). We
//! decrypt it **in memory** with the [`pdf`] crate — no subprocess, no temp
//! file, so the cleartext never touches disk and the password never reaches an
//! argv. (`lopdf` was tried first and rejected: its xref parser fails to load
//! these real statements.)
//!
//! Text position is recovered the standard way: walk each page's content
//! operations tracking the CTM (`q`/`Q`/`cm`) and the text matrix
//! (`BT`/`Tm`/`Td`/`T*`), then map each drawn run's origin through
//! `Trm = Tm × CTM` to device space. Runs are grouped into rows by `y` and
//! ordered by `x`, which faithfully reconstructs the statement's columns (the
//! whole approach validated against a real sample in Phase 0).
//!
//! The geometry that turns runs into rows ([`group_runs`]) is pure and unit
//! tested; the PDF-specific extraction ([`extract_rows`]) is a thin wrapper.

use anyhow::{Context, Result, anyhow};
use pdf::content::{Op, Point};
use pdf::file::FileOptions;
use pdf::object::{Resolve, Resources, XObject};
use pdf::primitive::Primitive;

use super::{Cell, Run, TextRow};

/// Runs whose `y` differ by less than this (PDF points) belong to one row.
const ROW_Y_TOLERANCE: f32 = 2.5;

/// Max Form-XObject nesting we recurse into (loop/runaway guard).
const MAX_FORM_DEPTH: u8 = 8;

/// A 2-D affine transform `[a b c d e f]` (PDF's row-vector convention:
/// `[x y 1] · M`). Used for both the CTM and the text matrix.
#[derive(Clone, Copy)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    fn identity() -> Self {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    fn translate(x: f32, y: f32) -> Self {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    fn from_pdf(m: &pdf::content::Matrix) -> Self {
        Affine {
            a: m.a,
            b: m.b,
            c: m.c,
            d: m.d,
            e: m.e,
            f: m.f,
        }
    }

    /// `self × other` under the row-vector convention: a point transformed by
    /// `self` then `other`. Used to compose `Td×Tlm`, `Tm×CTM`, `cm×CTM`.
    fn then(&self, o: &Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.b * o.c,
            b: self.a * o.b + self.b * o.d,
            c: self.c * o.a + self.d * o.c,
            d: self.c * o.b + self.d * o.d,
            e: self.e * o.a + self.f * o.c + o.e,
            f: self.e * o.b + self.f * o.d + o.f,
        }
    }

    /// Map a point through the transform.
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

/// Decrypt `pdf_bytes` with `password` and return every page's text as
/// x-ordered [`TextRow`]s, in page then top-to-bottom order.
///
/// `password` is trimmed (the statement password is digits; a trailing newline
/// from a secret file must not break decryption).
pub fn extract_rows(pdf_bytes: Vec<u8>, password: &str) -> Result<Vec<TextRow>> {
    let pw = password.trim();
    let file = FileOptions::cached()
        .password(pw.as_bytes())
        .load(pdf_bytes)
        .context("opening/decrypting statement PDF")?;

    let resolver = file.resolver();
    let mut rows = Vec::new();
    for page_num in 0..file.num_pages() {
        let page = file
            .get_page(page_num)
            .with_context(|| format!("reading page {page_num}"))?;
        let content = match page.contents.as_ref() {
            Some(c) => c,
            None => continue,
        };
        let ops = content
            .operations(&resolver)
            .with_context(|| format!("decoding page {page_num} content"))?;
        let resources = page.resources().ok().map(|r| &**r);
        let mut runs = Vec::new();
        collect_runs(&ops, Affine::identity(), &resolver, resources, &mut runs, 0);
        rows.extend(group_runs(runs));
    }
    if rows.is_empty() {
        return Err(anyhow!("statement PDF yielded no text rows"));
    }
    Ok(rows)
}

/// Walk a content stream's operations, tracking the CTM (seeded with `base_ctm`)
/// and text matrix, collecting a positioned [`Run`] for every text-drawing
/// operator. Recurses into Form XObjects (the statement's footer summary — incl.
/// `BALANCE TOTAL` — lives in one), applying the form's matrix on top of the CTM
/// at the `Do` site, bounded by [`MAX_FORM_DEPTH`].
fn collect_runs<R: Resolve>(
    ops: &[Op],
    base_ctm: Affine,
    resolve: &R,
    resources: Option<&Resources>,
    runs: &mut Vec<Run>,
    depth: u8,
) {
    let mut ctm = base_ctm;
    let mut ctm_stack: Vec<Affine> = Vec::new();
    let mut tlm = Affine::identity(); // text line matrix
    let mut tm = Affine::identity(); // text matrix
    let mut leading = 0f32;

    for op in ops {
        match op {
            Op::Save => ctm_stack.push(ctm),
            Op::Restore => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
            }
            Op::Transform { matrix } => ctm = Affine::from_pdf(matrix).then(&ctm),
            Op::BeginText => {
                tlm = Affine::identity();
                tm = Affine::identity();
            }
            Op::Leading { leading: l } => leading = *l,
            Op::SetTextMatrix { matrix } => {
                tlm = Affine::from_pdf(matrix);
                tm = tlm;
            }
            Op::MoveTextPosition {
                translation: Point { x, y },
            } => {
                tlm = Affine::translate(*x, *y).then(&tlm);
                tm = tlm;
            }
            Op::TextNewline => {
                tlm = Affine::translate(0.0, -leading).then(&tlm);
                tm = tlm;
            }
            Op::TextDraw { text } => {
                push_run(runs, &ctm, &tm, &decode_win1252(text.as_bytes()));
            }
            Op::TextDrawAdjusted { array } => {
                let mut s = String::new();
                for el in array {
                    // `TextDrawAdjusted::Spacing` (kerning) elements are dropped
                    // — they nudge intra-run glyph spacing, not the run origin.
                    if let pdf::content::TextDrawAdjusted::Text(t) = el {
                        s.push_str(&decode_win1252(t.as_bytes()));
                    }
                }
                push_run(runs, &ctm, &tm, &s);
            }
            // Recurse into Form XObjects (the footer summary box lives in one),
            // bounded by MAX_FORM_DEPTH. Beyond the limit the guard fails and the
            // op falls through to the ignored-ops wildcard below (a no-op) — the
            // same effect as the previous inner depth check.
            Op::XObject { name } if depth < MAX_FORM_DEPTH => {
                recurse_xobject(name, ctm, resolve, resources, runs, depth);
            }
            // Deliberately ignored: graphics/path/color operators (no text) and
            // text-state operators that do not move the glyph origin we track —
            // `TextFont`/`CharSpacing`/`WordSpacing`/`TextScaling`/`TextRise`/
            // `TextRenderMode`. `Op` is an external, open-ended enum, so a
            // wildcard is the right tool here.
            _ => {}
        }
    }
}

/// Resolve a named Form XObject and recurse into its content with the current
/// CTM (× the form's own matrix). Image/PostScript XObjects and resolution
/// failures are silently ignored — we only want text.
fn recurse_xobject<R: Resolve>(
    name: &pdf::primitive::Name,
    ctm: Affine,
    resolve: &R,
    resources: Option<&Resources>,
    runs: &mut Vec<Run>,
    depth: u8,
) {
    let Some(res) = resources else { return };
    let Some(xref) = res.xobjects.get(name) else {
        return;
    };
    let Ok(xobj) = resolve.get(*xref) else { return };
    let XObject::Form(form) = &*xobj else { return };
    let Ok(form_ops) = form.operations(resolve) else {
        return;
    };
    let dict = form.dict();
    let form_ctm = form_matrix(&dict.matrix).then(&ctm);
    // The form may carry its own resource dictionary; fall back to the parent's.
    let form_res = dict.resources.as_deref().or(resources);
    collect_runs(&form_ops, form_ctm, resolve, form_res, runs, depth + 1);
}

/// Parse a Form XObject `/Matrix` (a 6-number array) into an [`Affine`];
/// identity when absent or malformed.
fn form_matrix(matrix: &Option<Primitive>) -> Affine {
    let Some(Primitive::Array(a)) = matrix else {
        return Affine::identity();
    };
    if a.len() != 6 {
        return Affine::identity();
    }
    let mut v = [0f32; 6];
    for (slot, p) in v.iter_mut().zip(a) {
        match p.as_number() {
            Ok(n) => *slot = n,
            Err(_) => return Affine::identity(),
        }
    }
    Affine {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    }
}

/// Record a run at the current text origin (mapped through `Tm × CTM`), unless
/// it is blank.
fn push_run(runs: &mut Vec<Run>, ctm: &Affine, tm: &Affine, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let (x, y) = tm.then(ctm).apply(0.0, 0.0);
    runs.push(Run {
        x,
        y,
        text: text.to_string(),
    });
}

/// Group positioned runs into x-ordered rows, top-to-bottom.
///
/// Pure (no PDF types) so the row/column reconstruction is unit testable. Runs
/// are sorted by descending `y` (PDF y grows upward) then ascending `x`; a new
/// row starts when a run's `y` falls more than [`ROW_Y_TOLERANCE`] below the
/// current row's *anchor* (its first run's `y`) — anchored, not adjacent, so a
/// slow within-row drift cannot chain distinct rows together.
#[must_use]
pub fn group_runs(mut runs: Vec<Run>) -> Vec<TextRow> {
    runs.sort_by(|a, b| {
        b.y.partial_cmp(&a.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut rows: Vec<TextRow> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut anchor_y: Option<f32> = None;

    for run in runs {
        match anchor_y {
            Some(y) if (run.y - y).abs() > ROW_Y_TOLERANCE => {
                rows.push(TextRow {
                    y,
                    cells: std::mem::take(&mut current),
                });
                anchor_y = Some(run.y);
            }
            None => anchor_y = Some(run.y),
            _ => {}
        }
        current.push(Cell {
            x: run.x,
            text: run.text,
        });
    }
    if let Some(y) = anchor_y {
        rows.push(TextRow { y, cells: current });
    }
    rows
}

/// Decode bytes using Windows-1252 (≈ PDF `WinAnsiEncoding`, the common font
/// encoding for these statements). Differs from Latin-1 only in `0x80..=0x9F`;
/// the rest maps straight to the matching Unicode code point. This recovers the
/// accented characters that a naive UTF-8 decode would drop (e.g. `próximo`).
/// Bytes with no Windows-1252 assignment become U+FFFD.
fn decode_win1252(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| win1252_char(b)).collect()
}

fn win1252_char(b: u8) -> char {
    match b {
        // The 0x80..=0x9F block where Windows-1252 diverges from Latin-1.
        0x80 => '\u{20AC}', // €
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}', // …
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}', // ’
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}', // –
        0x97 => '\u{2014}', // —
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        // The five unassigned Windows-1252 positions.
        0x81 | 0x8D | 0x8F | 0x90 | 0x9D => '\u{FFFD}',
        // Everything else is Latin-1 (== matching Unicode scalar).
        other => other as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(x: f32, y: f32, t: &str) -> Run {
        Run {
            x,
            y,
            text: t.to_string(),
        }
    }

    #[test]
    fn groups_runs_into_rows_top_to_bottom_left_to_right() {
        // Two rows; deliberately out of order on input.
        let runs = vec![
            run(252.0, 613.0, "Pago Via App"),
            run(29.0, 613.0, "28/04"),
            run(76.0, 613.0, "28/04"),
            run(25.0, 695.0, "VISA PRESTIGE DOP"),
        ];
        let rows = group_runs(runs);
        assert_eq!(rows.len(), 2);
        // Higher y first.
        assert_eq!(rows[0].joined(), "VISA PRESTIGE DOP");
        // Within the second row, cells are x-ordered.
        assert_eq!(rows[1].cells[0].text, "28/04");
        assert_eq!(rows[1].cells[1].text, "28/04");
        assert_eq!(rows[1].cells[2].text, "Pago Via App");
    }

    #[test]
    fn near_equal_y_stays_one_row() {
        // y within tolerance should not split.
        let runs = vec![run(10.0, 500.0, "a"), run(50.0, 501.0, "b")];
        let rows = group_runs(runs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells.len(), 2);
    }

    #[test]
    fn empty_runs_yield_no_rows() {
        assert!(group_runs(vec![]).is_empty());
    }

    #[test]
    fn win1252_recovers_accents_and_specials() {
        // 0xF3 = ó (Latin-1 region), 0xED = í, 0x80 = €, 0x96 = en-dash.
        assert_eq!(
            decode_win1252(&[b'p', b'r', 0xF3, b'x', b'i', b'm', b'o']),
            "próximo"
        );
        assert_eq!(decode_win1252(&[0x80]), "€");
        assert_eq!(decode_win1252(&[0x96]), "–");
        // Unassigned position → replacement char, not a panic.
        assert_eq!(decode_win1252(&[0x81]), "\u{FFFD}");
    }
}
