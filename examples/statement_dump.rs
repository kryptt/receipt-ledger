//! Dev tool: decrypt + parse a Banco Popular statement PDF and print a summary.
//! Usage: cargo run --example statement_dump -- <pdf-path> <password>
//! (Operates on local files only; prints aggregate counts, not full PII.)
use receipt_ledger::statement::{parse, pdf};

fn main() -> anyhow::Result<()> {
    let (path, pw) = match (std::env::args().nth(1), std::env::args().nth(2)) {
        (Some(p), Some(pw)) => (p, pw),
        _ => {
            eprintln!("usage: statement_dump <pdf-path> <password>");
            std::process::exit(2);
        }
    };
    let bytes = std::fs::read(&path)?;
    let rows = pdf::extract_rows(bytes, &pw)?;
    eprintln!("extracted {} text rows", rows.len());
    let st = parse::parse_statement(&rows)?;
    println!("sections: {}", st.sections.len());
    for s in &st.sections {
        println!(
            "  {:?} last4={} cut={} balance_total={:?}",
            s.currency,
            s.primary_last4.as_str(),
            s.cut_date,
            s.balance_total
        );
    }
    println!("transactions: {}", st.txns.len());
    let charges = st.txns.iter().filter(|t| matches!(t.direction, receipt_ledger::schema::Direction::Out)).count();
    let payments = st.txns.len() - charges;
    println!("  charges: {charges}, payments/credits: {payments}");
    Ok(())
}
