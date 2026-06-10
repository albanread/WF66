//! LET differential oracle — the MCJIT byte oracle for the Rasm migration of
//! the `LET` DSL (the sibling of [`crate::golden`], which oracles the kernel).
//!
//! Each LET source is lowered once to MC-flavour Intel asm text
//! ([`let_lang::compile`]), then that *same text* is encoded two ways:
//!
//!   * **LLVM-MC / MCJIT** ([`wfasm::Jit`]) — load it, look up the function,
//!     read the loaded bytes. This is the golden reference.
//!   * **RasmEncoder** ([`wfasm::rasm::assemble`]) — the native encoder we are
//!     proving byte-identical.
//!
//! Unlike the kernel, LET output has **no relocations to normalise**: libm
//! calls are baked as `movabs rax, <abs_addr>; call rax` with an address that
//! is identical in both encoders (same process, same `libm_address_table`).
//! So the comparison is a straight `memcmp` — any difference is a real encoder
//! divergence (or an alignment-padding difference, which is also Rasm's job to
//! match). This drives RasmEncoder's LET instruction set (`sqrtsd`, `andpd`,
//! `cmpCCsd`, `roundsd`, multi-value `.quad`, `xmmword ptr`, `movabs`) to
//! byte-identity exactly the way the kernel golden did.
//!
//! Gated behind `feature = "llvm"` — it needs both encoders side by side, so it
//! only runs in the dual-stack (transitional) build.

use anyhow::{bail, Context, Result};
use wfasm::Jit;

/// Representative LET sources covering every codegen path: arithmetic, the
/// constant pool (`pi`), multi-in/out, unary minus + sign-mask, division,
/// every SSE intrinsic (sqrt/abs/min/max/floor/ceil/round/trunc), libm calls
/// (sin/cos/pow/hypot/atan2/exp/log), comparisons, and select/clamp blends.
/// `(fn_name, source)`. Names must be unique (one JIT module each).
pub const CORPUS: &[(&str, &str)] = &[
    ("orc_id", "LET (x) -> (y) = x END"),
    ("orc_quad", "LET (x) -> (y) = x * x + 1 END"),
    ("orc_area", "LET (r) -> (a) = pi * r * r END"),
    ("orc_addsub", "LET (a, b) -> (diff, sum) = a - b, a + b END"),
    ("orc_neg", "LET (x) -> (y) = -x END"),
    ("orc_div", "LET (a, b) -> (q) = a / b END"),
    ("orc_sqrt", "LET (x) -> (y) = sqrt(x) END"),
    ("orc_abs", "LET (x) -> (y) = abs(x) END"),
    ("orc_min", "LET (a, b) -> (m) = min(a, b) END"),
    ("orc_max", "LET (a, b) -> (m) = max(a, b) END"),
    ("orc_floor", "LET (x) -> (y) = floor(x) END"),
    ("orc_ceil", "LET (x) -> (y) = ceil(x) END"),
    ("orc_round", "LET (x) -> (y) = round(x) END"),
    ("orc_trunc", "LET (x) -> (y) = trunc(x) END"),
    ("orc_sin", "LET (x) -> (y) = sin(x) END"),
    ("orc_cos", "LET (x) -> (y) = cos(x) END"),
    ("orc_pow", "LET (b, e) -> (r) = pow(b, e) END"),
    ("orc_starstar", "LET (x) -> (y) = x ** 3 END"),
    ("orc_hypot", "LET (a, b) -> (r) = hypot(a, b) END"),
    ("orc_atan2", "LET (y, x) -> (a) = atan2(y, x) END"),
    ("orc_explog", "LET (x) -> (y) = log(exp(x)) END"),
    (
        "orc_nested",
        "LET (x) -> (y) = sqrt(sin(x)*sin(x) + cos(x)*cos(x)) END",
    ),
    ("orc_lt", "LET (x) -> (y) = x < 5 END"),
    ("orc_eq", "LET (a, b) -> (y) = a == b END"),
    ("orc_ne", "LET (a, b) -> (y) = a != b END"),
    ("orc_gt", "LET (a, b) -> (y) = a > b END"),
    ("orc_ge", "LET (a, b) -> (y) = a >= b END"),
    ("orc_le", "LET (a, b) -> (y) = a <= b END"),
    ("orc_sign", "LET (x) -> (y) = (x < 0) * -1 + (x >= 0) * 1 END"),
    ("orc_sel", "LET () -> (y) = select(1, 99, 42) END"),
    (
        "orc_abs_sel",
        "LET (x) -> (y) = select(x < 0, -x, x) END",
    ),
    (
        "orc_mbrot",
        "LET (z_re, z_im, x, y) -> (z_next_re, z_next_im, mag) = \
            re, im, rmag \
            WHERE re   = z_re * z_re - z_im * z_im + x \
            WHERE im   = 2 * z_re * z_im + y \
            WHERE rmag = re * re + im * im \
         END",
    ),
    (
        "orc_clamp",
        "LET (x, lo, hi) -> (y) = select(x < lo, lo, select(x > hi, hi, x)) END",
    ),
    (
        "orc_smooth",
        "LET (t) -> (y) = u * u * (3 - 2 * u) \
             WHERE u = select(t < 0, 0, select(t > 1, 1, t)) END",
    ),
    (
        "orc_dist",
        "LET (x1, y1, x2, y2) -> (d) = hypot(x2 - x1, y2 - y1) END",
    ),
];

/// One source's diff result.
pub struct LetDiff {
    pub name: String,
    /// Byte length RasmEncoder produced for the function module.
    pub rasm_len: usize,
    /// `None` if byte-identical; else `(offset, llvm_byte, rasm_byte)` of the
    /// first mismatch (or a length/structural note).
    pub mismatch: Option<String>,
}

/// Compile + dual-encode + compare every [`CORPUS`] entry. Returns one
/// [`LetDiff`] per source; `.mismatch == None` means byte-identical.
pub fn diff_corpus() -> Result<Vec<LetDiff>> {
    // SEH dumper so an encoder bug that produces a faulting body gives a
    // readable dump rather than a silent abort.
    let _ = wfasm::seh::install();
    let libm = crate::runtime::libm_address_table();

    let mut out = Vec::with_capacity(CORPUS.len());
    for (name, source) in CORPUS {
        out.push(diff_one(name, source, &libm)?);
    }
    Ok(out)
}

fn diff_one(name: &str, source: &str, libm: &crate::let_lang::LibmTable) -> Result<LetDiff> {
    let compiled = crate::let_lang::compile(source, name, libm)
        .with_context(|| format!("LET compile `{name}`: {source}"))?;

    // RasmEncoder — the native bytes (no relocs expected for LET).
    let rasm = wfasm::rasm::assemble(&compiled.asm_text)
        .with_context(|| format!("rasm assemble `{name}`"))?;
    if !rasm.relocs.is_empty() {
        bail!(
            "`{name}`: RasmEncoder produced {} relocations — LET output is expected to be \
             self-contained (libm baked as movabs/call rax). Externs: {:?}",
            rasm.relocs.len(),
            rasm.externs
        );
    }
    let rasm_len = rasm.code.len();

    // LLVM-MC / MCJIT — load the same text and read the function's bytes.
    let mut jit = Jit::new(&format!("let_oracle_{name}"))
        .with_context(|| format!("Jit::new `{name}`"))?;
    jit.add_asm(&compiled.asm_text)
        .map_err(|e| anyhow::anyhow!("add_asm `{name}`: {e:?}\nasm:\n{}", compiled.asm_text))?;
    jit.declare_fn(name, 0)
        .map_err(|e| anyhow::anyhow!("declare_fn `{name}`: {e:?}"))?;
    let addr = jit
        .lookup_addr(name)
        .map_err(|e| anyhow::anyhow!("lookup_addr `{name}`: {e:?}"))?;

    // SAFETY: `addr` is the live, executable LET function; reading `rasm_len`
    // bytes stays within its (page-granular) MCJIT allocation. The function +
    // its in-`.text` constant pool are contiguous from `addr`, so this captures
    // exactly the region RasmEncoder also emitted from offset 0.
    let llvm: &[u8] = unsafe { std::slice::from_raw_parts(addr as *const u8, rasm_len) };

    let mismatch = first_mismatch(llvm, &rasm.code);
    // Keep the module mapped until after we've read its bytes.
    drop(jit);
    Ok(LetDiff { name: name.to_string(), rasm_len, mismatch })
}

/// First differing byte (with a short hex window of context), or `None`.
fn first_mismatch(llvm: &[u8], rasm: &[u8]) -> Option<String> {
    if let Some(i) = llvm.iter().zip(rasm).position(|(a, b)| a != b) {
        let lo = i.saturating_sub(4);
        let hi_l = (i + 8).min(llvm.len());
        let hi_r = (i + 8).min(rasm.len());
        return Some(format!(
            "first diff @ {i}: llvm={:02x} rasm={:02x}\n    llvm [{lo}..]: {}\n    rasm [{lo}..]: {}",
            llvm[i],
            rasm[i],
            hex_window(&llvm[lo..hi_l]),
            hex_window(&rasm[lo..hi_r]),
        ));
    }
    None
}

fn hex_window(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for x in b {
        let _ = write!(s, "{x:02x} ");
    }
    s.trim_end().to_string()
}
