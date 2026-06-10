//! Register-pinning analysis (optimizer agenda #1, Phase 1).
//!
//! Turns a recorded do...loop op buffer (the `(action, payload)` records the
//! kernel captures in Phase 0) into a *pin plan*: which `hotvariable`s can have
//! their value kept in a register across the loop, in which register, and
//! whether each is read-only / write-only / read-write.
//!
//! This module is pure logic — no memory dereference, no kernel state — so it is
//! unit-tested over synthetic buffers. The kernel-facing shim `rt_pin_analyze`
//! (in `lib.rs`) feeds it the live buffer and a [`Vocab`] resolved at boot, then
//! logs the plan. Phase 1 only logs; Phase 2 consumes the plan to emit code.
//!
//! See docs/design/register_pinning_v1.md.

/// One recorded compile action: `action` is the compile-time helper (a `ct`
/// pointer, or `literal`/`execute`), `payload` is the xt (or literal value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub action: u64,
    pub payload: u64,
}

/// The handful of kernel addresses/xts the analysis keys on. Resolved once at
/// session boot. `Default` (all zero) makes every comparison fail, i.e. an
/// unconfigured vocab yields an empty plan — a safe no-op.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vocab {
    /// `ct` of a `hotvariable` reference (the inline address-push helper).
    pub inline_var_comp: u64,
    /// `ct` that emits a `call rel32` to a non-inlined word — a real call,
    /// which clobbers the caller-saved pin pool and disqualifies the loop.
    pub compile_comma: u64,
    /// Variant for return-stack-juggling words; also a real call.
    pub compile_comma_no_tco: u64,
    /// xt of `@` — a hot-variable read.
    pub fetch_xt: u64,
    /// xt of `!` — a hot-variable write.
    pub store_xt: u64,
    /// xt of `+!` — a hot-variable read-modify-write.
    pub plus_store_xt: u64,
    /// xt of `f@` — a hot-float read (push to the FP stack).
    pub f_fetch_xt: u64,
    /// xt of `f!` — a hot-float write (pop from the FP stack).
    pub f_store_xt: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinClass {
    /// Read in the loop, never written: load on entry, no write-back.
    ReadOnly,
    /// Written, never read: no load, write-back on exit.
    WriteOnly,
    /// Both (includes any `+!`): load on entry, write-back on exit.
    ReadWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinKind {
    /// Integer cell, accessed via @ / ! / +!, pinned in a GP register.
    Int,
    /// Float cell, accessed via f@ / f!, pinned in an xmm register.
    Float,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinEntry {
    /// xt of the hotvariable's create stub (its body address is `[xt+6]`).
    pub xt: u64,
    /// register number to pin the value in (GP r9.. for Int, xmm6.. for Float).
    pub reg: u8,
    pub class: PinClass,
    pub kind: PinKind,
}

/// GP registers safe to pin an integer across a call-free loop body: r9/r10/r11.
/// None is touched at runtime by any inline body, the `do`/`loop` machinery, or
/// `idiv` (rdx). They do NOT survive a call, so integer pins need a call-free loop.
pub const PIN_POOL_INT: [u8; 3] = [9, 10, 11];

/// XMM registers safe to pin a float: xmm6-xmm13. No Forth primitive touches
/// xmm6-15 (the FP ops use only xmm0/xmm1), they are Win64 callee-saved (so even
/// callouts preserve them), and forth_main spills/reloads them for the host — so
/// the float pool survives FP-op calls and ordinary calls alike. (The one
/// exception is a native LET/CODE word that uses high xmm; rare and user-owned.)
pub const PIN_POOL_FLOAT: [u8; 8] = [6, 7, 8, 9, 10, 11, 12, 13];

/// Float register pinning is DISABLED — it is not useful. This is the flag that
/// enabled it; with it false, hotfvariable references still inline their body
/// address (a hotvariable's win) but are never value-pinned into an xmm reg.
///
/// Why: measured on the FP Mandelbrot, pinning removed only the 8 f@/f! memory
/// ops per loop (call_count 42 -> 34) for a ~2% gain. The dominating cost is
/// that f+/f*/f-/f< are themselves CALLs that round-trip the whole FP stack
/// through memory (see kernel/float.masm — every op reloads user_FSP and movsd's
/// its operands out of memory). Pinning a few user variables can't touch that.
///
/// The real lever is the FP stack design itself: there is no cached FP
/// top-of-stack the way RAX caches the data TOS, so every float op pays a
/// memory round-trip. Caching FTOS/FNOS in fixed xmm registers would speed ALL
/// float code, not just hot vars, and subsumes this feature. The emit_f* movsd
/// machinery below is kept dormant (not deleted) because it is reusable for that
/// work. See docs/design/fp-stack-register-cache.md.
const ENABLE_FLOAT_PINNING: bool = false;

/// Decide which hotvariables in a recorded loop body to pin.
///
/// Returns an empty plan (no pinning) when the body contains a real call —
/// caller-saved pins would not survive it.
pub fn analyze(records: &[Record], vocab: &Vocab) -> Vec<PinEntry> {
    // A call to a non-inlined word clobbers the caller-saved GP pool, so integer
    // pins need a call-free loop. The float pool (xmm6-15) survives calls (no
    // primitive touches it), so float pins are allowed regardless — see below.
    let has_call = records.iter().any(|r| {
        r.action == vocab.compile_comma || r.action == vocab.compile_comma_no_tco
    });

    struct Var {
        xt: u64,
        reads: bool,
        writes: bool,
        escaped: bool,
        kind: PinKind,
        mixed: bool, // accessed as both int and float -> not pinnable
    }
    let mut vars: Vec<Var> = Vec::new();

    for i in 0..records.len() {
        let r = records[i];
        // Only hotvariable references matter; everything else is context.
        if r.action != vocab.inline_var_comp {
            continue;
        }
        let xt = r.payload;
        // Classify this occurrence by the immediately following access word. A
        // hotvariable not followed by a recognised access has its bare address
        // used for something else (escape) — it cannot be safely value-pinned.
        let (read, write, escape, kind) = match records.get(i + 1).map(|n| n.payload) {
            Some(p) if p == vocab.fetch_xt => (true, false, false, PinKind::Int),
            Some(p) if p == vocab.store_xt => (false, true, false, PinKind::Int),
            Some(p) if p == vocab.plus_store_xt => (true, true, false, PinKind::Int),
            Some(p) if p == vocab.f_fetch_xt => (true, false, false, PinKind::Float),
            Some(p) if p == vocab.f_store_xt => (false, true, false, PinKind::Float),
            _ => (false, false, true, PinKind::Int),
        };
        let idx = match vars.iter().position(|v| v.xt == xt) {
            Some(k) => k,
            None => {
                vars.push(Var { xt, reads: false, writes: false, escaped: false, kind, mixed: false });
                vars.len() - 1
            }
        };
        let v = &mut vars[idx];
        if !escape && v.kind != kind && (v.reads || v.writes) {
            v.mixed = true; // int and float access to the same var
        }
        if !escape {
            v.kind = kind;
        }
        v.reads |= read;
        v.writes |= write;
        v.escaped |= escape;
    }

    // Allocate each pool in first-use order; variables beyond their pool (or
    // disqualified) fall back to unpinned.
    let mut plan = Vec::new();
    let mut int_pool = PIN_POOL_INT.iter();
    let mut float_pool = PIN_POOL_FLOAT.iter();
    for v in &vars {
        if v.escaped || v.mixed || (!v.reads && !v.writes) {
            continue;
        }
        let reg = match v.kind {
            PinKind::Int => {
                if has_call {
                    continue; // GP pool dies across a call
                }
                match int_pool.next() {
                    Some(&r) => r,
                    None => continue,
                }
            }
            PinKind::Float => {
                if !ENABLE_FLOAT_PINNING {
                    // Disabled — not useful (see ENABLE_FLOAT_PINNING). The float
                    // is still classified above so it is never mis-pinned as an
                    // int; here it simply falls back to normal memory f@/f!.
                    continue;
                }
                match float_pool.next() {
                    Some(&r) => r,
                    None => continue,
                }
            }
        };
        let class = match (v.reads, v.writes) {
            (true, true) => PinClass::ReadWrite,
            (true, false) => PinClass::ReadOnly,
            (false, true) => PinClass::WriteOnly,
            (false, false) => unreachable!("escaped/empty vars were skipped"),
        };
        plan.push(PinEntry { xt: v.xt, reg, class, kind: v.kind });
    }
    plan
}

// ── Code emission (Phase 2) ────────────────────────────────────────────────
// Pure byte emitters for the pinned forms. `reg` is an x86 register number in
// {9,10,11}; all encodings below use rcx as the address scratch (the inline
// bodies' own scratch is rcx too, but these run at the loop prologue/epilogue
// where no body op is mid-flight). Unit-tested against exact bytes.

/// `mov reg, [body]` — load a variable's value into its pin reg. RIP-relative
/// (`mov reg,[rip+disp32]`, 7 B) when the body is within ±2 GB — the normal case
/// given near placement, and the form the body_hash normalizer can zero so the
/// gate stays deterministic. Otherwise an absolute `mov rcx,imm64; mov reg,[rcx]`
/// fallback (13 B). `here` is the address the first emitted byte will occupy.
pub fn emit_load(out: &mut Vec<u8>, here: u64, body: u64, reg: u8) {
    let disp = body as i64 - (here as i64 + 7);
    if i32::try_from(disp).is_ok() {
        out.push(0x4C);
        out.push(0x8B);
        out.push(((reg & 7) << 3) | 0x05); // mov reg, [rip+disp32]
        out.extend_from_slice(&(disp as i32).to_le_bytes());
    } else {
        out.push(0x48);
        out.push(0xB9);
        out.extend_from_slice(&body.to_le_bytes()); // mov rcx, imm64
        out.push(0x4C);
        out.push(0x8B);
        out.push(((reg & 7) << 3) | 0x01); // mov reg, [rcx]
    }
}

/// `mov [body], reg` — write a pinned value back, RIP-relative when in range
/// (else the absolute `mov rcx,imm64; mov [rcx],reg` fallback). See `emit_load`.
pub fn emit_writeback(out: &mut Vec<u8>, here: u64, body: u64, reg: u8) {
    let disp = body as i64 - (here as i64 + 7);
    if i32::try_from(disp).is_ok() {
        out.push(0x4C);
        out.push(0x89);
        out.push(((reg & 7) << 3) | 0x05); // mov [rip+disp32], reg
        out.extend_from_slice(&(disp as i32).to_le_bytes());
    } else {
        out.push(0x48);
        out.push(0xB9);
        out.extend_from_slice(&body.to_le_bytes());
        out.push(0x4C);
        out.push(0x89);
        out.push(((reg & 7) << 3) | 0x01); // mov [rcx], reg
    }
}

/// Emit a pinned access. `kind`: 0 = `@` (push reg), 1 = `!` (store + drop),
/// 2 = `+!` (add + drop). Mirrors the data-stack discipline of the inline
/// `@`/`!`/`+!` bodies, but the address is the pinned register, not memory.
pub fn emit_access(out: &mut Vec<u8>, kind: u8, reg: u8) {
    match kind {
        0 => {
            // hv @  -> push reg's value:
            //   mov [rbp-8], rax ; sub rbp, 8 ; mov rax, reg
            out.extend_from_slice(&[0x48, 0x89, 0x45, 0xF8]);
            out.extend_from_slice(&[0x48, 0x83, 0xED, 0x08]);
            // mov rax, reg : 4C 89 (11 reg 000)
            out.extend_from_slice(&[0x4C, 0x89, 0xC0 | ((reg & 7) << 3)]);
        }
        1 => {
            // hv !  -> store TOS into reg, drop:
            //   mov reg, rax ; mov rax, [rbp] ; add rbp, 8
            out.extend_from_slice(&[0x49, 0x89, 0xC0 | (reg & 7)]);
            out.extend_from_slice(&[0x48, 0x8B, 0x45, 0x00]);
            out.extend_from_slice(&[0x48, 0x83, 0xC5, 0x08]);
        }
        2 => {
            // hv +! -> add TOS into reg, drop:
            //   add reg, rax ; mov rax, [rbp] ; add rbp, 8
            out.extend_from_slice(&[0x49, 0x01, 0xC0 | (reg & 7)]);
            out.extend_from_slice(&[0x48, 0x8B, 0x45, 0x00]);
            out.extend_from_slice(&[0x48, 0x83, 0xC5, 0x08]);
        }
        _ => unreachable!("emit_access kind must be 0/1/2"),
    }
}

// ── Float emission (hotfloat) ─────────────────────────────────────────────
// xmm pool is 6..=13; xmm>=8 needs a REX.R prefix (after the mandatory F2).

/// `movsd` reg<->mem opcode+modrm: `F2 [REX.R] 0F (10|11) (reg<<3 | modrm_low)`.
fn movsd_reg_mem(out: &mut Vec<u8>, store: bool, xmm: u8, modrm_low: u8) {
    out.push(0xF2);
    if xmm >= 8 {
        out.push(0x44); // REX.R
    }
    out.push(0x0F);
    out.push(if store { 0x11 } else { 0x10 });
    out.push(((xmm & 7) << 3) | modrm_low);
}

/// Encoded length of a `movsd xmm, [rip+disp32]` / `[rip+disp32], xmm`.
fn movsd_rip_len(xmm: u8) -> i64 {
    // F2 [44] 0F op modrm disp32
    1 + (xmm >= 8) as i64 + 2 + 1 + 4
}

/// `movsd xmm, [body]` — load a float into its pin reg; RIP-relative in range,
/// else `mov rcx,imm64; movsd xmm,[rcx]`.
pub fn emit_fload(out: &mut Vec<u8>, here: u64, body: u64, xmm: u8) {
    let disp = body as i64 - (here as i64 + movsd_rip_len(xmm));
    if i32::try_from(disp).is_ok() {
        movsd_reg_mem(out, false, xmm, 0x05); // [rip+disp32]
        out.extend_from_slice(&(disp as i32).to_le_bytes());
    } else {
        out.push(0x48);
        out.push(0xB9);
        out.extend_from_slice(&body.to_le_bytes()); // mov rcx, imm64
        movsd_reg_mem(out, false, xmm, 0x01); // movsd xmm, [rcx]
    }
}

/// `movsd [body], xmm` — write a pinned float back; RIP-relative in range.
pub fn emit_fwriteback(out: &mut Vec<u8>, here: u64, body: u64, xmm: u8) {
    let disp = body as i64 - (here as i64 + movsd_rip_len(xmm));
    if i32::try_from(disp).is_ok() {
        movsd_reg_mem(out, true, xmm, 0x05);
        out.extend_from_slice(&(disp as i32).to_le_bytes());
    } else {
        out.push(0x48);
        out.push(0xB9);
        out.extend_from_slice(&body.to_le_bytes());
        movsd_reg_mem(out, true, xmm, 0x01);
    }
}

/// Emit a pinned float access. `kind` 3 = `f@` (push xmm onto the FP stack),
/// 4 = `f!` (pop the FP-stack top into xmm). `fsp_off` = user_FSP from UP (rbx).
/// The FP stack grows down; rcx is scratch.
pub fn emit_faccess(out: &mut Vec<u8>, kind: u8, xmm: u8, fsp_off: u32) {
    // mov rcx, [rbx + fsp_off]
    out.extend_from_slice(&[0x48, 0x8B, 0x8B]);
    out.extend_from_slice(&fsp_off.to_le_bytes());
    match kind {
        3 => {
            out.extend_from_slice(&[0x48, 0x83, 0xE9, 0x08]); // sub rcx, 8
            movsd_reg_mem(out, true, xmm, 0x01); // movsd [rcx], xmm
        }
        4 => {
            movsd_reg_mem(out, false, xmm, 0x01); // movsd xmm, [rcx]
            out.extend_from_slice(&[0x48, 0x83, 0xC1, 0x08]); // add rcx, 8
        }
        _ => unreachable!("emit_faccess kind must be 3/4"),
    }
    // mov [rbx + fsp_off], rcx
    out.extend_from_slice(&[0x48, 0x89, 0x8B]);
    out.extend_from_slice(&fsp_off.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sentinel addresses for the synthetic vocab — any distinct nonzero values.
    const VAR: u64 = 0x1000; // inline_var_comp
    const CC: u64 = 0x2000; // compile_comma
    const CC_NT: u64 = 0x2008; // compile_comma_no_tco
    const FETCH: u64 = 0x3000; // @ xt
    const STORE: u64 = 0x3008; // ! xt
    const PLUSST: u64 = 0x3010; // +! xt
    const FFETCH: u64 = 0x3018; // f@ xt
    const FSTORE: u64 = 0x3020; // f! xt
    const OTHER: u64 = 0x9000; // some other action (e.g. inline_leaf_comp)

    fn vocab() -> Vocab {
        Vocab {
            inline_var_comp: VAR,
            compile_comma: CC,
            compile_comma_no_tco: CC_NT,
            fetch_xt: FETCH,
            store_xt: STORE,
            plus_store_xt: PLUSST,
            f_fetch_xt: FFETCH,
            f_store_xt: FSTORE,
        }
    }

    // A hotvariable reference record (action = inline_var_comp, payload = xt).
    fn href(xt: u64) -> Record {
        Record { action: VAR, payload: xt }
    }
    // A non-hotvar record carrying `payload` as its xt (action irrelevant here).
    fn op(payload: u64) -> Record {
        Record { action: OTHER, payload }
    }

    #[test]
    fn empty_buffer_is_empty_plan() {
        assert!(analyze(&[], &vocab()).is_empty());
    }

    #[test]
    fn read_only_variable() {
        let recs = [href(0xAA), op(FETCH)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![PinEntry { xt: 0xAA, reg: 9, class: PinClass::ReadOnly, kind: PinKind::Int }]);
    }

    #[test]
    fn write_only_variable() {
        let recs = [href(0xAA), op(STORE)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![PinEntry { xt: 0xAA, reg: 9, class: PinClass::WriteOnly, kind: PinKind::Int }]);
    }

    #[test]
    fn plus_store_is_read_write() {
        let recs = [href(0xAA), op(PLUSST)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![PinEntry { xt: 0xAA, reg: 9, class: PinClass::ReadWrite, kind: PinKind::Int }]);
    }

    #[test]
    fn read_then_write_is_read_write() {
        // hot-sum shape: `hv @ ... hv !`
        let recs = [href(0xAA), op(FETCH), op(0x55), href(0xAA), op(STORE)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![PinEntry { xt: 0xAA, reg: 9, class: PinClass::ReadWrite, kind: PinKind::Int }]);
    }

    #[test]
    fn bare_address_escape_is_not_pinned() {
        // `hv +` — hv not followed by @ / ! / +!  -> escape -> excluded.
        let recs = [href(0xAA), op(0x55)];
        assert!(analyze(&recs, &vocab()).is_empty());
    }

    #[test]
    fn escape_is_sticky_even_with_a_good_use() {
        // One good read, one escaping use of the same var -> excluded entirely.
        let recs = [href(0xAA), op(FETCH), href(0xAA), op(0x55)];
        assert!(analyze(&recs, &vocab()).is_empty());
    }

    #[test]
    fn a_call_disqualifies_the_whole_loop() {
        let recs = [href(0xAA), op(FETCH), Record { action: CC, payload: 0x77 }];
        assert!(analyze(&recs, &vocab()).is_empty());
        let recs2 = [href(0xAA), op(FETCH), Record { action: CC_NT, payload: 0x77 }];
        assert!(analyze(&recs2, &vocab()).is_empty());
    }

    #[test]
    fn multiple_vars_allocate_in_first_use_order_then_exhaust() {
        let recs = [
            href(0xA), op(FETCH),  // RO -> r9
            href(0xB), op(STORE),  // WO -> r10
            href(0xC), op(FETCH),  // RO -> r11
            href(0xD), op(FETCH),  // pool exhausted -> not pinned
        ];
        let plan = analyze(&recs, &vocab());
        assert_eq!(
            plan,
            vec![
                PinEntry { xt: 0xA, reg: 9, class: PinClass::ReadOnly, kind: PinKind::Int },
                PinEntry { xt: 0xB, reg: 10, class: PinClass::WriteOnly, kind: PinKind::Int },
                PinEntry { xt: 0xC, reg: 11, class: PinClass::ReadOnly, kind: PinKind::Int },
            ]
        );
    }

    #[test]
    fn same_var_repeated_is_one_entry() {
        let recs = [href(0xAA), op(FETCH), href(0xAA), op(FETCH)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![PinEntry { xt: 0xAA, reg: 9, class: PinClass::ReadOnly, kind: PinKind::Int }]);
    }

    #[test]
    fn hotvar_as_last_record_escapes() {
        // Trailing hotvar with no following access -> escape.
        let recs = [op(FETCH), href(0xAA)];
        assert!(analyze(&recs, &vocab()).is_empty());
    }

    // ── emission ────────────────────────────────────────────────────────
    #[test]
    fn load_encodings_rip_relative() {
        // body within range of `here` -> mov reg, [rip+disp32]
        let mut v = Vec::new();
        emit_load(&mut v, 0x1000, 0x1100, 9); // disp = 0x1100 - (0x1000+7) = 0xF9
        assert_eq!(&v[..3], &[0x4C, 0x8B, 0x0D]);
        assert_eq!(&v[3..], &0xF9_i32.to_le_bytes());
        let mut v10 = Vec::new();
        emit_load(&mut v10, 0x1000, 0x1100, 10);
        assert_eq!(&v10[..3], &[0x4C, 0x8B, 0x15]);
        let mut v11 = Vec::new();
        emit_load(&mut v11, 0x1000, 0x1100, 11);
        assert_eq!(&v11[..3], &[0x4C, 0x8B, 0x1D]);
    }

    #[test]
    fn load_far_falls_back_to_imm64() {
        let mut v = Vec::new();
        emit_load(&mut v, 0, 0x1_0000_0000, 9); // 4 GB away -> out of i32 range
        assert_eq!(&v[..2], &[0x48, 0xB9]); // mov rcx, imm64
        assert_eq!(&v[10..], &[0x4C, 0x8B, 0x09]); // mov r9, [rcx]
    }

    #[test]
    fn writeback_encodings_rip_relative() {
        let mut v = Vec::new();
        emit_writeback(&mut v, 0x2000, 0x2050, 9); // disp = 0x2050 - (0x2000+7) = 0x49
        assert_eq!(&v[..3], &[0x4C, 0x89, 0x0D]);
        assert_eq!(&v[3..], &0x49_i32.to_le_bytes());
        let mut v11 = Vec::new();
        emit_writeback(&mut v11, 0x2000, 0x2050, 11);
        assert_eq!(&v11[..3], &[0x4C, 0x89, 0x1D]);
        let mut vf = Vec::new();
        emit_writeback(&mut vf, 0, 0x1_0000_0000, 9); // far -> imm64 fallback
        assert_eq!(&vf[..2], &[0x48, 0xB9]);
        assert_eq!(&vf[10..], &[0x4C, 0x89, 0x09]);
    }

    #[test]
    fn access_fetch_push_reg() {
        let mut v = Vec::new();
        emit_access(&mut v, 0, 9); // hv @
        assert_eq!(v, vec![
            0x48, 0x89, 0x45, 0xF8, // mov [rbp-8], rax
            0x48, 0x83, 0xED, 0x08, // sub rbp, 8
            0x4C, 0x89, 0xC8,       // mov rax, r9
        ]);
        let mut v10 = Vec::new();
        emit_access(&mut v10, 0, 10);
        assert_eq!(&v10[8..], &[0x4C, 0x89, 0xD0]); // mov rax, r10
    }

    #[test]
    fn access_store_and_drop() {
        let mut v = Vec::new();
        emit_access(&mut v, 1, 9); // hv !
        assert_eq!(v, vec![
            0x49, 0x89, 0xC1,       // mov r9, rax
            0x48, 0x8B, 0x45, 0x00, // mov rax, [rbp]
            0x48, 0x83, 0xC5, 0x08, // add rbp, 8
        ]);
        let mut v11 = Vec::new();
        emit_access(&mut v11, 1, 11);
        assert_eq!(&v11[..3], &[0x49, 0x89, 0xC3]); // mov r11, rax
    }

    #[test]
    fn access_plus_store_and_drop() {
        let mut v = Vec::new();
        emit_access(&mut v, 2, 10); // hv +!
        assert_eq!(&v[..3], &[0x49, 0x01, 0xC2]); // add r10, rax
        assert_eq!(&v[3..], &[0x48, 0x8B, 0x45, 0x00, 0x48, 0x83, 0xC5, 0x08]);
    }

    // ── float analysis (pinning DISABLED — see ENABLE_FLOAT_PINNING) ──────
    #[test]
    fn float_var_not_pinned_when_disabled() {
        // A float read/write that would have pinned into xmm6 now stays unpinned.
        let recs = [href(0xAA), op(FFETCH), href(0xAA), op(FSTORE)];
        assert!(analyze(&recs, &vocab()).is_empty());
    }

    #[test]
    fn int_still_pins_while_float_does_not() {
        // The int var pins; the float access is classified (so it is not mistaken
        // for an int) but yields no pin.
        let recs = [href(0xA), op(FETCH), href(0xB), op(FFETCH)];
        let plan = analyze(&recs, &vocab());
        assert_eq!(plan, vec![
            PinEntry { xt: 0xA, reg: 9, class: PinClass::ReadOnly, kind: PinKind::Int },
        ]);
    }

    #[test]
    fn mixed_int_and_float_access_not_pinned() {
        let recs = [href(0xAA), op(FETCH), href(0xAA), op(FFETCH)];
        assert!(analyze(&recs, &vocab()).is_empty());
    }

    // ── float emission ──────────────────────────────────────────────────
    #[test]
    fn fload_rip_relative_and_rex() {
        let mut v = Vec::new();
        emit_fload(&mut v, 0x1000, 0x1100, 6); // disp = 0x1100 - (0x1000+8) = 0xF8
        assert_eq!(&v[..4], &[0xF2, 0x0F, 0x10, 0x35]); // movsd xmm6, [rip+disp32]
        assert_eq!(&v[4..], &0xF8_i32.to_le_bytes());
        let mut v8 = Vec::new();
        emit_fload(&mut v8, 0x1000, 0x1100, 8); // 9-byte instr -> disp = 0x1100 - 0x1009
        assert_eq!(&v8[..5], &[0xF2, 0x44, 0x0F, 0x10, 0x05]); // REX.R, xmm8
        assert_eq!(&v8[5..], &0xF7_i32.to_le_bytes());
    }

    #[test]
    fn fwriteback_and_faccess_encodings() {
        let mut v = Vec::new();
        emit_fwriteback(&mut v, 0x2000, 0x2050, 7); // disp = 0x2050 - (0x2000+8) = 0x48
        assert_eq!(&v[..4], &[0xF2, 0x0F, 0x11, 0x3D]); // movsd [rip+disp32], xmm7
        assert_eq!(&v[4..], &0x48_i32.to_le_bytes());

        let mut fa = Vec::new();
        emit_faccess(&mut fa, 3, 6, 0x1218); // f@: push xmm6 to FP stack
        assert_eq!(&fa[..3], &[0x48, 0x8B, 0x8B]); // mov rcx, [rbx+disp32]
        assert_eq!(&fa[3..7], &0x1218_u32.to_le_bytes());
        assert_eq!(&fa[7..11], &[0x48, 0x83, 0xE9, 0x08]); // sub rcx, 8
        assert_eq!(&fa[11..15], &[0xF2, 0x0F, 0x11, 0x31]); // movsd [rcx], xmm6
        assert_eq!(&fa[15..18], &[0x48, 0x89, 0x8B]); // mov [rbx+disp32], rcx

        let mut fp = Vec::new();
        emit_faccess(&mut fp, 4, 6, 0x1218); // f!: pop FP stack into xmm6
        assert_eq!(&fp[7..11], &[0xF2, 0x0F, 0x10, 0x31]); // movsd xmm6, [rcx]
        assert_eq!(&fp[11..15], &[0x48, 0x83, 0xC1, 0x08]); // add rcx, 8
    }
}
