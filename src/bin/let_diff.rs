//! `let-diff` — verbose LET differential oracle (RasmEncoder vs LLVM-MC/MCJIT).
//!
//! Iterate loop for driving the LET codegen to byte-identity: prints every
//! corpus source, its module length, and the first byte divergence (if any).
//! The sibling of `rasm-diff` (kernel). Requires the `llvm` feature (default).
//!
//!   cargo run --bin let-diff

fn main() -> anyhow::Result<()> {
    let diffs = wf64::let_oracle::diff_corpus()?;
    let mut ok = 0usize;
    let mut bad = 0usize;
    for d in &diffs {
        match &d.mismatch {
            None => {
                ok += 1;
                println!("  ok    {:<14} {} bytes", d.name, d.rasm_len);
            }
            Some(m) => {
                bad += 1;
                println!("  DIFF  {:<14} {} bytes\n        {}", d.name, d.rasm_len, m);
            }
        }
    }
    println!("\n{ok} identical, {bad} divergent of {} LET sources", diffs.len());
    if bad != 0 {
        std::process::exit(1);
    }
    Ok(())
}
