//! Golden capture — the byte oracle for the Rasm migration (Sprint 0).
//!
//! Captures the exact machine-code bytes emitted for every
//! assembler-emitted kernel symbol (`forth_main` + every primitive + every
//! helper), keyed by name. This is **irreplaceable**:
//! and the existing
//! `bench/baseline` corpus only covers ~10 compiled `.f` files, not the
//! primitives. Sprint 2 diffs the native `RasmEncoder` against this golden,
//! per symbol, to prove byte-identity.
//!
//! Two byte strings per symbol:
//!   * `norm` — ASLR-normalized: displacement/branch fields whose target lies
//!     **outside** the kernel region (host `rt_*` externs, at ASLR-varying
//!     distance) are zeroed. Internal, layout-dependent displacements are
//!     **kept**, so a wrong intra-kernel offset is still caught.
//!   * `raw` — present only when the symbol is extern-free (nothing was zeroed),
//!     i.e. its raw bytes are ASLR-stable. These are the self-inspection-safe
//!     forms the kernel byte-copies and pattern-matches at runtime
//!     (`inline_leaf_comp` + the T3 peephole); their byte-identity is a
//!     behavioral requirement, not cosmetic.
//!
//! Gated behind `opt-metrics` (reuses the iced-x86 decoder already pulled in
//! there). Run the `golden-capture` binary to (re)write `bench/golden/kernel.json`.

use anyhow::{Context, Result};
use iced_x86::{Code, Decoder, DecoderOptions, FlowControl, Instruction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{Wf64Session, KERNEL_HELPERS, PRIMITIVES};

/// The kernel-end sentinel symbol `with_kernel` appends after every real
/// symbol, so the last symbol has a well-defined end.
pub const SENTINEL: &str = "__rasm_kernel_end__";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolGolden {
    /// Byte length of the symbol body `[start, next_symbol_start)`.
    pub len: usize,
    /// Hex of the ASLR-normalized bytes (extern targets zeroed).
    pub norm: String,
    /// Hex of the raw bytes — present iff extern-free (raw == norm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

/// Every assembler-emitted kernel symbol name (deduped, sorted).
pub fn kernel_symbol_names() -> Vec<String> {
    let mut names: Vec<String> = vec!["forth_main".to_string()];
    names.extend(PRIMITIVES.iter().map(|(_, sym, _)| sym.to_string()));
    names.extend(KERNEL_HELPERS.iter().map(|s| s.to_string()));
    names.sort();
    names.dedup();
    names
}

/// Capture the per-symbol golden from a live session.
pub fn capture(session: &mut Wf64Session) -> Result<BTreeMap<String, SymbolGolden>> {
    let names = kernel_symbol_names();

    // Resolve every symbol address + the end sentinel.
    let mut syms: Vec<(String, u64)> = Vec::with_capacity(names.len());
    for n in &names {
        let a = session.xt_of(n).with_context(|| format!("lookup symbol `{n}`"))?;
        syms.push((n.clone(), a));
    }
    let sentinel = session
        .xt_of(SENTINEL)
        .context("lookup kernel-end sentinel — is the with_kernel sentinel present?")?;

    // Sort by address (ties broken by name for determinism).
    syms.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    // Region bounds: [first symbol address, sentinel).
    let region_start = syms.first().map(|(_, a)| *a).unwrap_or(sentinel);
    let region_end = sentinel;

    // Distinct, ascending addresses (for next-start end computation).
    let mut addrs: Vec<u64> = syms.iter().map(|(_, a)| *a).collect();
    addrs.push(sentinel);
    addrs.sort_unstable();
    addrs.dedup();

    // Every `proc(...)` emits an 8-byte `.quad 0` xt-metadata cell immediately
    // before its label (XT_META_OFFSET; written at boot by
    // write_primitive_xt_backref). So a symbol's pure body ends 8 bytes before
    // the NEXT proc's xt. The appended sentinel is raw asm with no such cell, so
    // the final real symbol's body runs right up to it.
    const META: u64 = 8;

    let mut out: BTreeMap<String, SymbolGolden> = BTreeMap::new();
    for (name, start) in &syms {
        let start = *start;
        // Next distinct address strictly greater than start.
        let next = match addrs.binary_search(&start) {
            Ok(i) => addrs.get(i + 1).copied().unwrap_or(sentinel),
            Err(_) => sentinel, // shouldn't happen — start is in addrs
        };
        // Trim the next proc's metadata cell from this body (the sentinel has none).
        let end = if next == sentinel || next < start + META { next } else { next - META };
        if end <= start {
            // Symbol aliases a later symbol at the same address; the bytes are
            // captured under whichever name sorts first. Record a zero-length
            // alias marker so the set of keys is complete.
            out.insert(name.clone(), SymbolGolden { len: 0, norm: String::new(), raw: None });
            continue;
        }
        let len = (end - start) as usize;
        // SAFETY: [start, end) is a live, finalized, executable kernel range.
        let code: &[u8] = unsafe { std::slice::from_raw_parts(start as *const u8, len) };
        let (norm, extern_free) = normalize(code, start, region_start, region_end);
        out.insert(
            name.clone(),
            SymbolGolden {
                len,
                norm: hex(&norm),
                raw: if extern_free { Some(hex(code)) } else { None },
            },
        );
    }
    Ok(out)
}

/// Zero only displacement/branch bytes whose target is **outside**
/// `[region_start, region_end)`. Returns `(normalized, extern_free)`, where
/// `extern_free` is true iff nothing was zeroed (raw bytes are ASLR-stable).
fn normalize(code: &[u8], start: u64, region_start: u64, region_end: u64) -> (Vec<u8>, bool) {
    let mut norm = code.to_vec();
    let mut dec = Decoder::with_ip(64, code, start, DecoderOptions::NONE);
    let mut insn = Instruction::default();
    let mut touched = false;
    let inside = |t: u64| t >= region_start && t < region_end;

    while dec.can_decode() {
        let pos = dec.position();
        dec.decode_out(&mut insn);
        if insn.is_invalid() {
            continue; // decoder already advanced one byte
        }
        let ilen = insn.len();

        // RIP-relative memory operand — zero the disp32 iff it points outside
        // the kernel (kernel RIP-rel targets internal labels; nothing here
        // should point at an extern, but be precise).
        if insn.is_ip_rel_memory_operand() && !inside(insn.ip_rel_memory_address()) {
            let co = dec.get_constant_offsets(&insn);
            zero_field(&mut norm, pos + co.displacement_offset(), co.displacement_size());
            touched = true;
        }

        // Near branch (call/jmp/jcc rel) — zero the rel iff target is outside
        // the region (a host extern). Internal branches are layout-dependent
        // and ASLR-stable, so they are kept.
        let rel_size: Option<usize> = match insn.code() {
            Code::Call_rel32_64 | Code::Jmp_rel32_64 => Some(4),
            Code::Jmp_rel8_64 => Some(1),
            _ if insn.flow_control() == FlowControl::ConditionalBranch => {
                Some(if ilen >= 5 { 4 } else { 1 })
            }
            _ => None,
        };
        if let Some(rs) = rel_size {
            if !inside(insn.near_branch_target()) {
                zero_field(&mut norm, pos + ilen - rs, rs);
                touched = true;
            }
        }
    }
    (norm, !touched)
}

/// Assemble the kernel with the native `RasmEncoder` and return a divergence
/// report vs the committed golden — empty `Vec` means byte-identical to
/// Used by the `rasm-diff` binary (verbose) and the byte-identity
/// regression test. `kernel_path` is the kernel entry (`kernel/main.masm`).
pub fn rasm_divergent_symbols(kernel_path: &std::path::Path) -> Result<Vec<String>> {
    use std::collections::BTreeMap;

    // Assemble the kernel text exactly as with_kernel does (+ the sentinel).
    let mut asm = wfasm::Assembler::new();
    asm.register_macro("stk", wfasm::asm::macros::stk);
    let mut text = asm.assemble_file(kernel_path)?;
    text.push_str("\n.globl __rasm_kernel_end__\n__rasm_kernel_end__:\n  ret\n");

    let m = wfasm::rasm::assemble(&text)?;

    // Normalize: zero every host-extern field (matches the golden's
    // out-of-region zeroing — Rasm's relocs ARE exactly the externs).
    let mut norm = m.code.clone();
    for r in &m.relocs {
        for i in 0..r.size as usize {
            if r.at + i < norm.len() {
                norm[r.at + i] = 0;
            }
        }
    }

    let gpath = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join("golden")
        .join("kernel.json");
    let golden: BTreeMap<String, SymbolGolden> =
        serde_json::from_str(&std::fs::read_to_string(&gpath)?)?;

    let sentinel = *m
        .symbols
        .get(SENTINEL)
        .ok_or_else(|| anyhow::anyhow!("rasm output missing {SENTINEL}"))?;
    // Cut bodies at the golden's own symbol set (untracked .globl procs lump
    // into the preceding tracked symbol — mirror the golden capture).
    let mut cuts: Vec<usize> = golden
        .keys()
        .filter_map(|n| m.symbols.get(n).copied())
        .chain(std::iter::once(sentinel))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut diverged = Vec::new();
    for (name, g) in &golden {
        let Some(&start) = m.symbols.get(name) else {
            diverged.push(format!("{name}: MISSING in rasm"));
            continue;
        };
        let next = cuts.iter().copied().find(|&o| o > start).unwrap_or(sentinel);
        let end = if next == sentinel || next < start + 8 { next } else { next - 8 };
        let body = &norm[start..end.min(norm.len())];
        let want = hex_to_bytes(&g.norm);
        if body != want.as_slice() {
            let at = body.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(body.len().min(want.len()));
            diverged.push(format!(
                "{name}: differs (rasm {} vs golden {} bytes, first @ {at})",
                body.len(),
                want.len()
            ));
        }
    }
    Ok(diverged)
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect()
}

fn zero_field(norm: &mut [u8], off: usize, size: usize) {
    if size > 0 && off + size <= norm.len() {
        for b in &mut norm[off..off + size] {
            *b = 0;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
