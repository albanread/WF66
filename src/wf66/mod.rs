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

/// One IR token of a straight-line span (charter §2). Phase 0 subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Integer literal push.
    Lit(i64),
    /// An inlinable primitive (lowers to its own instruction).
    Inline(Fop),
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

/// Lower a token span to MC-flavour Intel asm text for `wfasm::rasm::assemble`,
/// matching the LET path's preamble. Naive settle-everywhere codegen: each token
/// emits its bare native sequence and the body ends in `ret`. The data stack is
/// the WF65 ABI throughout (RAX=TOS, RBP=DSP).
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
    lower(&folded, fn_name)
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

    #[test]
    fn lowered_subtraction_assembles() {
        // : d - ;  bare subtraction, the non-commutative lowering path.
        let asm = lower(&[Token::Inline(Fop::Sub)], "wf66_t_sub").unwrap();
        let module = wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("rasm rejected sub lowering: {e:#}\nasm was:\n{asm}"));
        assert!(!module.code.is_empty());
    }
}
