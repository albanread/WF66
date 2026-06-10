//! rasm-diff — assemble the real kernel with the native RasmEncoder and diff
//! every symbol's bytes against the committed LLVM golden.
//!
//!   cargo run --bin rasm-diff --features opt-metrics
//!
//! Drives the byte-identity gate: first it must ASSEMBLE the whole kernel (any
//! unsupported instruction surfaces here with line context); then it compares,
//! per symbol, RasmEncoder's normalized bytes against `bench/golden/kernel.json`
//! (both sides zero the host-extern fields). Prints a divergence report.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use wf64::golden::SymbolGolden;

const SENTINEL: &str = "__rasm_kernel_end__";

fn main() -> ExitCode {
    match run() {
        Ok(0) => {
            println!("\nrasm-diff: BYTE-IDENTICAL — all symbols match the golden");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("\nrasm-diff: {n} symbol(s) diverge from the golden");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("rasm-diff failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<usize> {
    // 1. Assemble the kernel text exactly as with_kernel does (+ sentinel).
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("kernel").join("main.masm");
    let mut asm = wfasm::Assembler::new();
    asm.register_macro("stk", wfasm::asm::macros::stk);
    let mut text = asm.assemble_file(&kernel)?;
    text.push_str("\n.globl __rasm_kernel_end__\n__rasm_kernel_end__:\n  ret\n");

    // 2. RasmEncoder: text -> EncodedModule.
    let m = wfasm::rasm::assemble(&text)?;
    println!(
        "assembled: {} bytes, {} symbols, {} relocs, {} externs",
        m.code.len(),
        m.symbols.len(),
        m.relocs.len(),
        m.externs.len()
    );

    // 3. Normalize: zero every host-extern field (matches the golden's
    //    out-of-region zeroing — Rasm's relocs ARE exactly the externs).
    let mut norm = m.code.clone();
    for r in &m.relocs {
        for i in 0..r.size as usize {
            if r.at + i < norm.len() {
                norm[r.at + i] = 0;
            }
        }
    }

    // 4. Load the golden.
    let gpath = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench").join("golden").join("kernel.json");
    let golden: BTreeMap<String, SymbolGolden> =
        serde_json::from_str(&std::fs::read_to_string(&gpath)?)?;

    // 5. Per-symbol slice. The golden was captured with only its own symbol set
    //    (forth_main + PRIMITIVES + KERNEL_HELPERS) as cut points — untracked
    //    .globl procs (e.g. pin_begin_maybe) get lumped into the preceding
    //    tracked symbol's body. Mirror that: cut rasm's bodies at the SAME
    //    golden symbols, so the comparison is apples-to-apples (a divergence in
    //    an untracked proc surfaces under its preceding tracked symbol).
    let sentinel = *m
        .symbols
        .get(SENTINEL)
        .ok_or_else(|| anyhow::anyhow!("rasm output missing {SENTINEL}"))?;
    // (golden name -> rasm offset) cut points, sorted, plus the sentinel.
    let mut cuts: Vec<usize> = golden
        .keys()
        .filter_map(|n| m.symbols.get(n).copied())
        .chain(std::iter::once(sentinel))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut diverged = 0usize;
    let mut shown = 0usize;
    for (name, g) in &golden {
        let Some(&start) = m.symbols.get(name) else {
            println!("  MISSING in rasm: {name}");
            diverged += 1;
            continue;
        };
        let next = cuts.iter().copied().find(|&o| o > start).unwrap_or(sentinel);
        let end = if next == sentinel || next < start + 8 { next } else { next - 8 };
        let body = &norm[start..end.min(norm.len())];
        let want = hex_decode(&g.norm);
        if body != want.as_slice() {
            diverged += 1;
            if shown < 25 {
                shown += 1;
                let at = first_diff(body, &want);
                println!(
                    "  DIFF {name}: len rasm {} vs golden {} ; first diff @ {at}\n        rasm:   {}\n        golden: {}",
                    body.len(),
                    want.len(),
                    hex_window(body, at),
                    hex_window(&want, at),
                );
            }
        }
    }
    Ok(diverged)
}

fn first_diff(a: &[u8], b: &[u8]) -> usize {
    for i in 0..a.len().min(b.len()) {
        if a[i] != b[i] {
            return i;
        }
    }
    a.len().min(b.len())
}

fn hex_window(b: &[u8], at: usize) -> String {
    let lo = at.saturating_sub(2);
    let hi = (at + 8).min(b.len());
    let mut s = String::new();
    for (i, byte) in b[lo..hi].iter().enumerate() {
        if lo + i == at {
            s.push('[');
        }
        s.push_str(&format!("{byte:02x}"));
        if lo + i == at {
            s.push(']');
        }
        s.push(' ');
    }
    s
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect()
}
