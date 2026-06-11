//! WF66 token-IR optimizing compiler — Rust-side core.
//!
//! This is the first slice of the WF66 compiler rewrite (roadmap Phase 0). The
//! charter fixes the architecture: the outer interpreter stays the front end and
//! *drives an IR builder* instead of emitting final bytes; the IR, optimizer, and
//! (later) register allocator live Rust-side because they are type-heavy and
//! test-heavy — "easier to build and verify there" (charter *Optimizer
//! Implementation: Rust-Side*). This module is that core, built and unit-tested in
//! isolation before any kernel `interp.masm` capture hook is wired in.
//!
//! Phase 0 scope (roadmap):
//!   - a per-definition **token IR** (`Token` spans),
//!   - the **const-fold** pass (`Lit a, Lit b, Inline(op)` -> `Lit (a op b)`),
//!   - naive **settle-everywhere** lowering to MC-flavour Intel asm text, ready
//!     for `wfasm::rasm::assemble` (the same encoder the LET path uses).
//!
//! It does NOT yet handle control flow, inlining, or register allocation — those
//! are Phases 1-4. `Word`/`Opaque` tokens are represented but lowering them is a
//! later sprint (they become the settle-to-canonical fallback); for now `lower`
//! rejects them explicitly rather than emitting wrong code.
//!
//! Data-stack ABI (carried from WF65, charter *Carries Over*): `RAX` = TOS,
//! `RBP` = DSP (points at NOS, grows down by `cell` = 8). A colon body is native
//! code entered with that convention live and ending in `ret`.

const CELL: i64 = 8;

/// A primitive operation that lowers to its own native instruction — the
/// charter's `Inline(fop)`. Phase 0 covers the foldable integer arithmetic /
/// bitwise ops with simple, register-cached lowerings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fop {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
}

impl Fop {
    /// Compile-time evaluation for const-fold. Forth stack effect `( a b -- r )`
    /// with `a` = NOS (deeper), `b` = TOS. Wrapping 2's-complement to match the
    /// 64-bit cell, so the folded literal is byte-for-state identical to running
    /// the op at runtime.
    fn eval(self, a: i64, b: i64) -> i64 {
        match self {
            Fop::Add => a.wrapping_add(b),
            Fop::Sub => a.wrapping_sub(b),
            Fop::Mul => a.wrapping_mul(b),
            Fop::And => a & b,
            Fop::Or => a | b,
            Fop::Xor => a ^ b,
        }
    }

    /// Bare-op lowering: the operands are TOS in `rax` and NOS at `[rbp]`; the
    /// result is left in `rax` and NOS is dropped (`add rbp, cell`). This is the
    /// settle-everywhere form — correct, not yet optimal (no cross-op register
    /// residency until Phase 4).
    fn emit_bare(self, out: &mut String) {
        match self {
            // Commutative: fold NOS straight into TOS.
            Fop::Add => out.push_str("    add rax, [rbp]\n"),
            Fop::Mul => out.push_str("    imul rax, [rbp]\n"),
            Fop::And => out.push_str("    and rax, [rbp]\n"),
            Fop::Or => out.push_str("    or rax, [rbp]\n"),
            Fop::Xor => out.push_str("    xor rax, [rbp]\n"),
            // `-` is ( a b -- a-b ) = NOS - TOS, non-commutative.
            Fop::Sub => {
                out.push_str("    sub [rbp], rax\n");
                out.push_str("    mov rax, [rbp]\n");
            }
        }
        out.push_str(&format!("    add rbp, {CELL}\n"));
    }
}

/// A pure stack-shuffle primitive (Phase 1.1). Lowered settle-everywhere for now
/// (parity with WF65's inlined shuffles); a later refinement turns these into
/// zero-cost SSA renames. `rot`/`tuck` are intentionally omitted — they taint
/// and fall back until the rename scheduler lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackOp {
    Dup,
    Drop,
    Swap,
    Over,
    Nip,
}

impl StackOp {
    /// Settle-everywhere lowering on the WF65 ABI (TOS in `rax`, NOS at `[rbp]`,
    /// stack grows down by `cell`). `rcx` is scratch (caller-saved in a leaf body).
    fn emit(self, out: &mut String) {
        match self {
            // ( a -- a a )
            StackOp::Dup => {
                out.push_str(&format!("    mov [rbp - {CELL}], rax\n"));
                out.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            // ( a -- )
            StackOp::Drop => {
                out.push_str("    mov rax, [rbp]\n");
                out.push_str(&format!("    add rbp, {CELL}\n"));
            }
            // ( a b -- b a )
            StackOp::Swap => {
                out.push_str("    mov rcx, [rbp]\n");
                out.push_str("    mov [rbp], rax\n");
                out.push_str("    mov rax, rcx\n");
            }
            // ( a b -- a b a )
            StackOp::Over => {
                out.push_str(&format!("    mov [rbp - {CELL}], rax\n"));
                out.push_str("    mov rax, [rbp]\n");
                out.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            // ( a b -- b )
            StackOp::Nip => {
                out.push_str(&format!("    add rbp, {CELL}\n"));
            }
        }
    }
}

/// One IR token of a straight-line span (charter §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Integer literal push.
    Lit(i64),
    /// An inlinable primitive on the top two stack cells (lowers to its own
    /// instruction).
    Inline(Fop),
    /// `op` of the current TOS with an immediate `k` — the result of folding a
    /// `Lit(k)` into a following `Inline(op)` (Phase 1.2). Lowers to a single
    /// register-immediate instruction (strength-reduced for `Mul`), with no push
    /// and no data-stack memory traffic.
    ImmOp { op: Fop, k: i64 },
    /// A stack-shuffle primitive (Phase 1.1).
    Stack(StackOp),
    /// A non-inlined call to another word by absolute xt. A settle-to-canonical
    /// boundary; lowering is a later sprint.
    Word { xt: u64 },
    /// `CODE:` body, raw byte poke, or any unknown effect — forces opaque /
    /// settled compilation. Lowering is a later sprint.
    Opaque,
}

/// The per-definition IR builder — the "IR builder object on the compiling
/// state" the charter and roadmap (0.1) call for. The future capture hook in
/// `interp.masm` appends to one of these instead of emitting bytes; `;` runs the
/// finalizer ([`compile_definition`]).
#[derive(Debug, Default, Clone)]
pub struct IrBuilder {
    tokens: Vec<Token>,
}

impl IrBuilder {
    pub fn new() -> Self {
        Self { tokens: Vec::new() }
    }

    pub fn lit(&mut self, v: i64) {
        self.tokens.push(Token::Lit(v));
    }

    pub fn inline(&mut self, f: Fop) {
        self.tokens.push(Token::Inline(f));
    }

    pub fn stack(&mut self, op: StackOp) {
        self.tokens.push(Token::Stack(op));
    }

    pub fn word(&mut self, xt: u64) {
        self.tokens.push(Token::Word { xt });
    }

    pub fn opaque(&mut self) {
        self.tokens.push(Token::Opaque);
    }

    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    pub fn into_tokens(self) -> Vec<Token> {
        self.tokens
    }
}

/// Error from lowering a token span to native asm text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// A token Phase 0 cannot yet lower (a non-inlined `Word` or an `Opaque`
    /// region). In the wired compiler this routes to the settle-to-canonical
    /// fallback; here it is surfaced so callers never emit wrong code.
    Unsupported(Token),
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::Unsupported(t) => {
                write!(f, "WF66 Phase 0 cannot lower token {t:?} (needs the settle fallback)")
            }
        }
    }
}

impl std::error::Error for LowerError {}

/// Constant folding: `[.. Lit a, Lit b, Inline(op) ..]` -> `[.. Lit (a op b) ..]`,
/// to a fixpoint over the straight-line span (`5 7 +` -> `Lit 12`, and chains
/// like `2 3 * 4 +` -> `Lit 10`). Because the whole token span is visible at
/// once, this already exceeds WF65's one-slot literal watermark (charter
/// *Replaced* / *Peephole subsumption*).
///
/// Folding treats the running `out` vector as the abstract value stack: an
/// `Inline` whose two most recent producers are both `Lit` collapses; otherwise
/// the operand is runtime-dependent and the op is kept.
pub fn const_fold(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        match t {
            Token::Inline(op) => {
                let n = out.len();
                if n >= 2 {
                    if let (Token::Lit(b), Token::Lit(a)) = (out[n - 1], out[n - 2]) {
                        out.truncate(n - 2);
                        out.push(Token::Lit(op.eval(a, b)));
                        continue;
                    }
                }
                out.push(t);
            }
            other => out.push(other),
        }
    }
    out
}

fn fits_i32(k: i64) -> bool {
    i32::try_from(k).is_ok()
}

/// Fold a `Lit(k)` into a following `Inline(op)` (Phase 1.2). `5 *` becomes
/// `ImmOp{Mul,5}` — one instruction operating on TOS in place — instead of a
/// literal push plus a memory-operand op. Runs *after* [`const_fold`], so any
/// `Lit Lit op` has already collapsed; what remains is a literal applied to a
/// runtime value. Only fires when `k` fits a sign-extended imm32 (the common
/// case); otherwise the pair stays as settle-everywhere `Lit`+`Inline`.
///
/// This subsumes WF65's watermark literal-fold and `imul-immed` peepholes as a
/// whole-span IR rewrite (charter *Replaced* / *Peephole subsumption*).
pub fn fold_imm_ops(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        match t {
            Token::Inline(op) => {
                if let Some(&Token::Lit(k)) = out.last() {
                    if fits_i32(k) {
                        out.pop();
                        out.push(Token::ImmOp { op, k });
                        continue;
                    }
                }
                out.push(t);
            }
            other => out.push(other),
        }
    }
    out
}

/// Lower `op TOS, k` (TOS in `rax`), strength-reducing `Mul` by a constant to
/// `shl`/`lea`/`neg`/`xor`/nop where possible (WF32's `imul-immed`, as codegen).
fn emit_imm_op(op: Fop, k: i64, out: &mut String) {
    match op {
        Fop::Add => out.push_str(&format!("    add rax, {k}\n")),
        Fop::Sub => out.push_str(&format!("    sub rax, {k}\n")),
        Fop::And => out.push_str(&format!("    and rax, {k}\n")),
        Fop::Or => out.push_str(&format!("    or rax, {k}\n")),
        Fop::Xor => out.push_str(&format!("    xor rax, {k}\n")),
        Fop::Mul => match k {
            0 => out.push_str("    xor eax, eax\n"), // a*0 = 0
            1 => {}                                  // a*1 = a (nop)
            -1 => out.push_str("    neg rax\n"),
            _ if k > 0 && (k & (k - 1)) == 0 => {
                out.push_str(&format!("    shl rax, {}\n", k.trailing_zeros()));
            }
            3 => out.push_str("    lea rax, [rax + rax*2]\n"),
            5 => out.push_str("    lea rax, [rax + rax*4]\n"),
            9 => out.push_str("    lea rax, [rax + rax*8]\n"),
            _ => out.push_str(&format!("    imul rax, rax, {k}\n")),
        },
    }
}

/// Lower a token span to MC-flavour Intel asm text for `wfasm::rasm::assemble`,
/// matching the LET path's preamble. Settle-everywhere codegen: each token emits
/// its native sequence and the body ends in `ret`. The data stack is the WF65
/// ABI throughout (RAX=TOS, RBP=DSP). `ImmOp` operates on TOS in a register with
/// no memory traffic; bare `Inline`/`Lit` still touch the data stack in memory.
pub fn lower(tokens: &[Token], fn_name: &str) -> Result<String, LowerError> {
    let mut s = String::new();
    s.push_str("    .intel_syntax noprefix\n");
    s.push_str("    .text\n");
    s.push_str(&format!("    .globl {fn_name}\n"));
    s.push_str(&format!("{fn_name}:\n"));

    for &t in tokens {
        match t {
            Token::Lit(v) => {
                // Push: spill old TOS to the cell below NOS, load the new TOS,
                // commit the push by lowering DSP.
                s.push_str(&format!("    mov [rbp - {CELL}], rax\n"));
                s.push_str(&format!("    movabs rax, {v}\n"));
                s.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            Token::Inline(op) => op.emit_bare(&mut s),
            Token::ImmOp { op, k } => emit_imm_op(op, k, &mut s),
            Token::Stack(op) => op.emit(&mut s),
            Token::Word { .. } | Token::Opaque => return Err(LowerError::Unsupported(t)),
        }
    }

    s.push_str("    ret\n");
    Ok(s)
}

/// The per-`;` finalizer pipeline in one call: capture (the `tokens`) -> optimize
/// (`const_fold`) -> lower. This is the entry point the wired `;` will call once
/// the kernel capture hook lands.
pub fn compile_definition(tokens: &[Token], fn_name: &str) -> Result<String, LowerError> {
    let folded = const_fold(tokens);
    let scheduled = fold_imm_ops(&folded);
    lower(&scheduled, fn_name)
}

/// True when every token is in the Phase 0 deferrable subset (`Lit`/`Inline`) —
/// i.e. WF66 can lower the whole body. Any `Word`/`Opaque` (an unknown word, an
/// immediate word's emission, a `CODE:` region) makes the span non-deferrable;
/// the wired `;` then leaves the eager body in place (the settle fallback).
pub fn is_deferrable(tokens: &[Token]) -> bool {
    tokens.iter().all(|t| {
        matches!(
            t,
            Token::Lit(_) | Token::Inline(_) | Token::ImmOp { .. } | Token::Stack(_)
        )
    })
}

/// Error from the per-definition finalizer.
#[derive(Debug)]
pub enum CompileError {
    /// The span contains a token outside the Phase 0 subset — the caller must
    /// keep the eagerly-compiled body (settle-to-canonical fallback).
    NotDeferrable,
    /// Lowering rejected a token (should not happen after `is_deferrable`).
    Lower(LowerError),
    /// The native assembler rejected the lowered text.
    Assemble(String),
    /// The assembled module did not expose the body entry symbol.
    MissingSymbol,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::NotDeferrable => write!(f, "span is not Phase-0 deferrable"),
            CompileError::Lower(e) => write!(f, "lower: {e}"),
            CompileError::Assemble(e) => write!(f, "assemble: {e}"),
            CompileError::MissingSymbol => write!(f, "assembled module missing body symbol"),
        }
    }
}

/// The finalizer core: fold + lower + assemble a closed, deferrable definition to
/// **position-independent machine-code bytes** (the body, including the trailing
/// `ret`). The wired `;` rewinds HERE to the body start and copies these bytes
/// over the eagerly-compiled body. Returns `NotDeferrable` (cheaply) when the
/// span is outside the Phase 0 subset, so the caller keeps the eager body.
///
/// The bytes are position-independent — Phase 0 lowering uses only `movabs`
/// immediates and RAX/RBP-relative memory, no RIP-relative or absolute *code*
/// references — so they run correctly wherever they are copied.
pub fn compile_body_bytes(tokens: &[Token]) -> Result<Vec<u8>, CompileError> {
    if !is_deferrable(tokens) {
        return Err(CompileError::NotDeferrable);
    }
    let asm = compile_definition(tokens, "wf66_body").map_err(CompileError::Lower)?;
    let module = wfasm::rasm::assemble(&asm).map_err(|e| CompileError::Assemble(format!("{e:#}")))?;
    let fn_off = *module
        .symbols
        .get("wf66_body")
        .ok_or(CompileError::MissingSymbol)?;
    Ok(module.code[fn_off..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- const-fold (pure IR -> IR) -------------------------------------

    #[test]
    fn fold_two_literals() {
        // 5 7 +  ->  Lit 12   (the roadmap Phase 0 headline)
        let ir = [Token::Lit(5), Token::Lit(7), Token::Inline(Fop::Add)];
        assert_eq!(const_fold(&ir), vec![Token::Lit(12)]);
    }

    #[test]
    fn fold_chain_to_single_literal() {
        // 2 3 * 4 +  ->  Lit 10   (whole-span visibility beats the one-slot watermark)
        let ir = [
            Token::Lit(2),
            Token::Lit(3),
            Token::Inline(Fop::Mul),
            Token::Lit(4),
            Token::Inline(Fop::Add),
        ];
        assert_eq!(const_fold(&ir), vec![Token::Lit(10)]);
    }

    #[test]
    fn fold_respects_subtraction_order() {
        // 10 3 -  ->  Lit 7    ( a b -- a-b ), not 3-10
        let ir = [Token::Lit(10), Token::Lit(3), Token::Inline(Fop::Sub)];
        assert_eq!(const_fold(&ir), vec![Token::Lit(7)]);
    }

    #[test]
    fn no_fold_when_operand_is_runtime() {
        // : bar 5 * 2 + ;  is ( n -- n*5+2 ); nothing folds (n is runtime input).
        let ir = vec![
            Token::Lit(5),
            Token::Inline(Fop::Mul),
            Token::Lit(2),
            Token::Inline(Fop::Add),
        ];
        assert_eq!(const_fold(&ir), ir);
    }

    #[test]
    fn fold_wraps_like_a_64bit_cell() {
        let ir = [Token::Lit(i64::MAX), Token::Lit(1), Token::Inline(Fop::Add)];
        assert_eq!(const_fold(&ir), vec![Token::Lit(i64::MIN)]);
    }

    // ---- lowering (IR -> asm text, then prove it encodes) ---------------

    #[test]
    fn lower_folded_constant_is_a_single_push() {
        let asm = compile_definition(
            &[Token::Lit(5), Token::Lit(7), Token::Inline(Fop::Add)],
            "wf66_t_const",
        )
        .unwrap();
        assert_eq!(
            asm,
            "    .intel_syntax noprefix\n    .text\n    .globl wf66_t_const\n\
             wf66_t_const:\n\
             \x20   mov [rbp - 8], rax\n    movabs rax, 12\n    sub rbp, 8\n\
             \x20   ret\n"
        );
    }

    #[test]
    fn lower_rejects_unsupported_tokens() {
        assert_eq!(
            lower(&[Token::Word { xt: 0x1000 }], "wf66_t_word"),
            Err(LowerError::Unsupported(Token::Word { xt: 0x1000 }))
        );
        assert_eq!(
            lower(&[Token::Opaque], "wf66_t_op"),
            Err(LowerError::Unsupported(Token::Opaque))
        );
    }

    /// The strongest cheap check: the lowered text must actually encode through
    /// the same native assembler the live system uses (`wfasm::rasm::assemble`),
    /// and expose the entry symbol. Proves the asm is real machine code, not just
    /// a plausible string.
    #[test]
    fn lowered_body_assembles_through_rasm() {
        // : bar 5 * 2 + ;  ( n -- n*5+2 ) — exercises a literal push, a
        // commutative bare op, and a non-commutative-free arithmetic chain.
        let asm = compile_definition(
            &[
                Token::Lit(5),
                Token::Inline(Fop::Mul),
                Token::Lit(2),
                Token::Inline(Fop::Add),
            ],
            "wf66_t_bar",
        )
        .unwrap();
        let module = wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("rasm rejected WF66 lowering: {e:#}\nasm was:\n{asm}"));
        assert!(!module.code.is_empty(), "no code bytes emitted");
        assert!(
            module.symbols.contains_key("wf66_t_bar"),
            "entry symbol missing from module: {:?}",
            module.symbols.keys().collect::<Vec<_>>()
        );
        assert!(module.externs.is_empty(), "unexpected externs: {:?}", module.externs);
    }

    // ---- finalizer core (IR -> position-independent body bytes) ---------

    #[test]
    fn body_bytes_for_folded_constant() {
        // : twelve 5 7 + ;  -> Lit 12 -> push 12; ret
        let bytes =
            compile_body_bytes(&[Token::Lit(5), Token::Lit(7), Token::Inline(Fop::Add)]).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.contains(&0xC3), "body must contain a ret (0xC3): {bytes:02x?}");
    }

    #[test]
    fn body_bytes_for_runtime_expression() {
        // : bar 5 * 2 + ;  ( n -- n*5+2 ) — lowers, does not fold.
        let bytes = compile_body_bytes(&[
            Token::Lit(5),
            Token::Inline(Fop::Mul),
            Token::Lit(2),
            Token::Inline(Fop::Add),
        ])
        .unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.contains(&0xC3));
    }

    // ---- fold_imm_ops + strength reduction (Phase 1.2) ------------------

    #[test]
    fn imm_ops_fold_literal_into_op() {
        // 5 * 2 +  ->  ImmOp{Mul,5}, ImmOp{Add,2}  (no pushes left)
        let ir = const_fold(&[
            Token::Lit(5),
            Token::Inline(Fop::Mul),
            Token::Lit(2),
            Token::Inline(Fop::Add),
        ]);
        assert_eq!(
            fold_imm_ops(&ir),
            vec![
                Token::ImmOp { op: Fop::Mul, k: 5 },
                Token::ImmOp { op: Fop::Add, k: 2 },
            ]
        );
    }

    #[test]
    fn imm_ops_leave_runtime_only_op_alone() {
        // bare + (both operands runtime) stays an Inline.
        assert_eq!(fold_imm_ops(&[Token::Inline(Fop::Add)]), vec![Token::Inline(Fop::Add)]);
    }

    #[test]
    fn imm_ops_skip_oversized_immediate() {
        // k beyond imm32 keeps the settle-everywhere Lit+Inline form.
        let big = i64::from(i32::MAX) + 1;
        let ir = [Token::Lit(big), Token::Inline(Fop::Add)];
        assert_eq!(fold_imm_ops(&ir), ir);
    }

    #[test]
    fn strength_reduced_multiplies_assemble() {
        // pow2 -> shl, 3/5/9 -> lea, 0 -> xor, -1 -> neg, other -> imul imm.
        for k in [0i64, 1, -1, 2, 4, 8, 3, 5, 9, 7, 100] {
            let asm = lower(&[Token::ImmOp { op: Fop::Mul, k }], "wf66_t_mul").unwrap();
            wfasm::rasm::assemble(&asm)
                .unwrap_or_else(|e| panic!("mul k={k} rejected: {e:#}\nasm:\n{asm}"));
        }
    }

    #[test]
    fn imm_arith_ops_assemble() {
        for op in [Fop::Add, Fop::Sub, Fop::And, Fop::Or, Fop::Xor] {
            let asm = lower(&[Token::ImmOp { op, k: 7 }], "wf66_t_imm").unwrap();
            wfasm::rasm::assemble(&asm)
                .unwrap_or_else(|e| panic!("{op:?} imm rejected: {e:#}\nasm:\n{asm}"));
        }
    }

    #[test]
    fn pipeline_strength_reduces_through_compile_body_bytes() {
        // : bar 5 * 2 + ;  now lowers via ImmOp (lea + add), still a valid body.
        let bytes = compile_body_bytes(&[
            Token::Lit(5),
            Token::Inline(Fop::Mul),
            Token::Lit(2),
            Token::Inline(Fop::Add),
        ])
        .unwrap();
        assert!(bytes.contains(&0xC3));
    }

    #[test]
    fn body_bytes_refuses_non_deferrable() {
        // A call to another word is the settle fallback's job, not WF66 Phase 0.
        assert!(matches!(
            compile_body_bytes(&[Token::Lit(1), Token::Word { xt: 0x4000 }]),
            Err(CompileError::NotDeferrable)
        ));
    }

    #[test]
    fn deferrable_classification() {
        assert!(is_deferrable(&[Token::Lit(1), Token::Inline(Fop::Add)]));
        assert!(!is_deferrable(&[Token::Lit(1), Token::Word { xt: 0 }]));
        assert!(!is_deferrable(&[Token::Opaque]));
    }

    // ---- stack shuffles (Phase 1.1) ------------------------------------

    #[test]
    fn stack_ops_assemble() {
        for op in [
            StackOp::Dup,
            StackOp::Drop,
            StackOp::Swap,
            StackOp::Over,
            StackOp::Nip,
        ] {
            let asm = lower(&[Token::Stack(op)], "wf66_t_stk").unwrap();
            wfasm::rasm::assemble(&asm)
                .unwrap_or_else(|e| panic!("{op:?} rejected: {e:#}\nasm:\n{asm}"));
        }
    }

    #[test]
    fn shuffle_body_is_deferrable_and_compiles() {
        // : sq dup * ;
        assert!(is_deferrable(&[Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)]));
        let bytes =
            compile_body_bytes(&[Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)]).unwrap();
        assert!(bytes.contains(&0xC3));
    }

    #[test]
    fn lowered_subtraction_assembles() {
        // : d - ;  bare subtraction, the non-commutative lowering path.
        let asm = lower(&[Token::Inline(Fop::Sub)], "wf66_t_sub").unwrap();
        let module = wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("rasm rejected sub lowering: {e:#}\nasm was:\n{asm}"));
        assert!(!module.code.is_empty());
    }
}
