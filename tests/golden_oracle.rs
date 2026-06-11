//! Golden oracle stability test (Rasm migration, Sprint 0).
//!
//! Re-captures the kernel byte golden from the live build and
//! asserts it matches the committed `bench/golden/kernel.json`. Because it
//! is deterministic and the capture is ASLR-normalized, this is byte-stable for
//! a given kernel source — so a failure means EITHER the kernel changed (then
//! re-run `cargo run --bin golden-capture --features opt-metrics` and review the
//! diff) OR the capture logic regressed. It proves the oracle is faithful before
//! we ever trust it to gate the native `RasmEncoder`.
#![cfg(feature = "opt-metrics")]

use std::collections::BTreeMap;
use std::path::Path;

use wf64::golden::{self, SymbolGolden};
use wf64::Wf64Session;

fn golden_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench").join("golden").join("kernel.json")
}

/// The live kernel build must match the committed golden byte-for-byte.
/// This proves the native loader places exactly what the RasmEncoder emits.
#[test]
fn golden_matches_live_build() {
    let committed: BTreeMap<String, SymbolGolden> = serde_json::from_str(
        &std::fs::read_to_string(golden_path()).expect("read bench/golden/kernel.json"),
    )
    .expect("parse golden json");

    let mut session = Wf64Session::new().expect("boot WF65 session");
    let live = golden::capture(&mut session).expect("capture live golden");

    let mut diffs: Vec<String> = Vec::new();
    for (name, lg) in &live {
        match committed.get(name) {
            None => diffs.push(format!("+ {name}: present live, absent in golden")),
            Some(cg) if cg != lg => diffs.push(format!(
                "~ {name}: differs (golden len {}, live len {})",
                cg.len, lg.len
            )),
            _ => {}
        }
    }
    for name in committed.keys() {
        if !live.contains_key(name) {
            diffs.push(format!("- {name}: in golden, absent live"));
        }
    }

    assert!(
        diffs.is_empty(),
        "golden drift on {} symbol(s) — re-capture with `cargo run --bin golden-capture \
         --features opt-metrics` if the kernel changed intentionally:\n{}",
        diffs.len(),
        diffs.iter().take(40).cloned().collect::<Vec<_>>().join("\n"),
    );
}

/// The native RasmEncoder assembles the whole kernel byte-identically to
/// the committed golden. Locks in the Sprint-1 milestone —
/// a regression here means the from-scratch encoder drifted.
#[test]
fn rasm_encoder_byte_identical() {
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("kernel").join("main.masm");
    let divergences = golden::rasm_divergent_symbols(&kernel).expect("assemble + diff kernel");
    assert!(
        divergences.is_empty(),
        "RasmEncoder diverged from the golden on {} symbol(s):\n{}",
        divergences.len(),
        divergences.iter().take(30).cloned().collect::<Vec<_>>().join("\n"),
    );
}

/// The self-inspected leaf forms the kernel byte-copies and pattern-matches at
/// runtime MUST carry a raw (extern-free) golden — their byte-identity is a
/// behavioral requirement, not cosmetic (inline_leaf_comp + the T3 peephole).
#[test]
fn self_inspected_leaves_have_raw_golden() {
    let committed: BTreeMap<String, SymbolGolden> = serde_json::from_str(
        &std::fs::read_to_string(golden_path()).expect("read golden"),
    )
    .expect("parse golden");

    // dup spills TOS (48 89 45 F8 / 48 83 ED 08); plus is the inline-leaf
    // add rax,[rbp] / add rbp,8 body — both are read back by the dup-fusion
    // peephole at kernel/compile.masm:216-231.
    for (name, want_prefix) in [("dup_", "488945f84883ed08"), ("plus", "480345004883c508")] {
        let g = committed.get(name).unwrap_or_else(|| panic!("{name} missing from golden"));
        let raw = g.raw.as_ref().unwrap_or_else(|| panic!("{name} has no extern-free raw golden"));
        assert!(raw.starts_with(want_prefix), "{name} raw {raw} lacks self-inspected prefix {want_prefix}");
        assert!(raw.ends_with("c3"), "{name} raw {raw} should end in ret (c3)");
    }
}
