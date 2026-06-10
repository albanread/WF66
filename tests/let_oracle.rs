//! LET differential oracle regression test: every LET source in the corpus
//! must encode byte-identically under RasmEncoder and LLVM-MC/MCJIT.
//!
//! The sibling of `golden_oracle.rs` (which does this for the kernel). Gated on
//! `feature = "llvm"` — needs both encoders. Run the verbose iterate loop with
//! `cargo run --bin let-diff` while driving divergences to zero.

#[cfg(feature = "llvm")]
#[test]
fn let_codegen_byte_identical_to_llvm() {
    let diffs = wf64::let_oracle::diff_corpus().expect("LET oracle run");
    let mut failures = Vec::new();
    for d in &diffs {
        if let Some(m) = &d.mismatch {
            failures.push(format!("  {} ({} bytes):\n    {}", d.name, d.rasm_len, m));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} LET sources diverge between RasmEncoder and LLVM-MC:\n{}",
        failures.len(),
        diffs.len(),
        failures.join("\n"),
    );
}
