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
    Tuck,    // ( a b -- b a b )
    Rot,     // ( a b c -- b c a )
    NegRot,  // ( a b c -- c a b )  (-rot)
    TwoDup,  // ( a b -- a b a b )
    TwoDrop, // ( a b -- )
    TwoSwap, // ( a b c d -- c d a b )
    TwoOver, // ( a b c d -- a b c d a b )
    TwoNip,  // ( a b c d -- c d )
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
            // ( a b -- b a b ) : insert a copy of TOS under NOS
            StackOp::Tuck => {
                out.push_str("    mov rcx, [rbp]\n"); // a
                out.push_str("    mov [rbp], rax\n"); // becomes NNOS = b
                out.push_str(&format!("    mov [rbp - {CELL}], rcx\n")); // new NOS = a
                out.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            // ( a b c -- b c a ) : a=[rbp+8], b=[rbp], c=rax
            StackOp::Rot => {
                out.push_str(&format!("    mov rcx, [rbp + {CELL}]\n")); // a
                out.push_str("    mov rdx, [rbp]\n"); // b
                out.push_str(&format!("    mov [rbp + {CELL}], rdx\n")); // NNOS = b
                out.push_str("    mov [rbp], rax\n"); // NOS = c
                out.push_str("    mov rax, rcx\n"); // TOS = a
            }
            // ( a b c -- c a b )
            StackOp::NegRot => {
                out.push_str("    mov rcx, [rbp]\n"); // b
                out.push_str(&format!("    mov rdx, [rbp + {CELL}]\n")); // a
                out.push_str(&format!("    mov [rbp + {CELL}], rax\n")); // NNOS = c
                out.push_str("    mov [rbp], rdx\n"); // NOS = a
                out.push_str("    mov rax, rcx\n"); // TOS = b
            }
            // ( a b -- a b a b )
            StackOp::TwoDup => {
                out.push_str("    mov rcx, [rbp]\n"); // a
                out.push_str(&format!("    mov [rbp - {CELL}], rax\n")); // b
                out.push_str(&format!("    mov [rbp - {}], rcx\n", 2 * CELL)); // a
                out.push_str(&format!("    sub rbp, {}\n", 2 * CELL));
            }
            // ( a b -- )
            StackOp::TwoDrop => {
                out.push_str(&format!("    mov rax, [rbp + {CELL}]\n")); // new TOS = NNOS
                out.push_str(&format!("    add rbp, {}\n", 2 * CELL));
            }
            // ( a b c d -- c d a b ) : swap rax<->[rbp+8] and [rbp]<->[rbp+16]
            StackOp::TwoSwap => {
                out.push_str(&format!("    mov rcx, [rbp + {CELL}]\n"));
                out.push_str(&format!("    mov [rbp + {CELL}], rax\n"));
                out.push_str("    mov rax, rcx\n");
                out.push_str("    mov rcx, [rbp]\n");
                out.push_str(&format!("    mov rdx, [rbp + {}]\n", 2 * CELL));
                out.push_str("    mov [rbp], rdx\n");
                out.push_str(&format!("    mov [rbp + {}], rcx\n", 2 * CELL));
            }
            // ( a b c d -- a b c d a b )
            StackOp::TwoOver => {
                out.push_str(&format!("    mov rcx, [rbp + {}]\n", 2 * CELL)); // a
                out.push_str(&format!("    mov rdx, [rbp + {CELL}]\n")); // b
                out.push_str(&format!("    mov [rbp - {CELL}], rax\n")); // d
                out.push_str(&format!("    mov [rbp - {}], rcx\n", 2 * CELL)); // a
                out.push_str("    mov rax, rdx\n"); // TOS = b
                out.push_str(&format!("    sub rbp, {}\n", 2 * CELL));
            }
            // ( a b c d -- c d ) : drop the 2nd pair, keep top pair
            StackOp::TwoNip => {
                out.push_str("    mov rcx, [rbp]\n"); // c
                out.push_str(&format!("    add rbp, {}\n", 2 * CELL));
                out.push_str("    mov [rbp], rcx\n"); // new NOS = c
            }
        }
    }
}

/// A memory access primitive (Phase 2.1). Lowered in program order
/// (settle-everywhere); since no pass reorders across these tokens, program
/// order is the implicit memory-ordering barrier until the CFG/regalloc phases
/// add motion (then an explicit barrier is needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemOp {
    Fetch,  // @  ( addr -- val )
    Store,  // !  ( val addr -- )
    CFetch, // c@ ( addr -- byte )
    CStore, // c! ( byte addr -- )
}

impl MemOp {
    /// Settle-everywhere lowering (TOS in `rax`, NOS at `[rbp]`, `rcx` scratch).
    fn emit(self, out: &mut String) {
        match self {
            MemOp::Fetch => out.push_str("    mov rax, [rax]\n"),
            MemOp::CFetch => out.push_str("    movzx eax, byte ptr [rax]\n"),
            // ! / c! : addr in rax, value in [rbp]; store, then drop both cells
            // (new TOS = the cell below the value).
            MemOp::Store | MemOp::CStore => {
                out.push_str("    mov rcx, [rbp]\n");
                if matches!(self, MemOp::Store) {
                    out.push_str("    mov [rax], rcx\n");
                } else {
                    out.push_str("    mov [rax], cl\n");
                }
                out.push_str(&format!("    mov rax, [rbp + {CELL}]\n"));
                out.push_str(&format!("    add rbp, {}\n", 2 * CELL));
            }
        }
    }
}

/// A structured control-flow marker (Phase 4a). Lowered settle-everywhere: the
/// data stack is canonical (TOS in `rax`, rest in memory) at every branch and
/// join, so branches need no phis — behavior-identical to WF65, the safe
/// substrate the register allocator (4b) builds on. Branch targets are emitted
/// as rasm labels (the assembler resolves them; the body stays
/// position-independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctl {
    If,
    Else,
    Then,
    Begin,
    Until,
    Again,
    While,
    Repeat,
    Exit, // early return ( -- )
}

/// A flag-producing comparison (Phase 4a). Forth flags are all-bits 0 / -1.
/// Unary forms compare TOS with 0; binary forms compare NOS (`a`) with TOS
/// (`b`), flag = `a REL b`. When immediately consumed by `IF`/`UNTIL`/`WHILE`
/// these fuse to a branch off the operands directly (no materialized boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    // unary ( n -- flag ) : n vs 0
    ZeroEq,
    ZeroNe,
    ZeroLt,
    ZeroGt,
    // binary ( a b -- flag ) : a vs b
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    ULt,
    UGt,
}

impl CmpOp {
    fn is_binary(self) -> bool {
        use CmpOp as C;
        matches!(self, C::Eq | C::Ne | C::Lt | C::Gt | C::Le | C::Ge | C::ULt | C::UGt)
    }

    /// setcc suffix for materializing the flag (after `test`/`cmp`).
    fn setcc(self) -> &'static str {
        use CmpOp as C;
        match self {
            C::ZeroEq | C::Eq => "e",
            C::ZeroNe | C::Ne => "ne",
            C::ZeroLt | C::Lt => "l",
            C::ZeroGt | C::Gt => "g",
            C::Le => "le",
            C::Ge => "ge",
            C::ULt => "b",
            C::UGt => "a",
        }
    }

    /// jcc suffix for the fused branch: jump when the flag would be FALSE (the
    /// inverse condition), so the control word skips/exits/loops correctly.
    fn inv_jcc(self) -> &'static str {
        use CmpOp as C;
        match self {
            C::ZeroEq | C::Eq => "ne",
            C::ZeroNe | C::Ne => "e",
            C::ZeroLt | C::Lt => "ge",
            C::ZeroGt | C::Gt => "le",
            C::Le => "g",
            C::Ge => "l",
            C::ULt => "ae",
            C::UGt => "be",
        }
    }

    /// The unary `0xx` equivalent of a binary comparison against a literal 0
    /// (`a 0 = ` -> `a 0=`), if one exists.
    fn zero_form(self) -> Option<CmpOp> {
        use CmpOp as C;
        match self {
            C::Eq => Some(C::ZeroEq),
            C::Ne => Some(C::ZeroNe),
            C::Lt => Some(C::ZeroLt),
            C::Gt => Some(C::ZeroGt),
            _ => None,
        }
    }

    /// Materialize the -1/0 flag in `rax` (the non-fused case).
    fn emit(self, out: &mut String) {
        if matches!(self, CmpOp::ZeroLt) {
            out.push_str("    sar rax, 63\n"); // n<0 -> all-ones, cheapest form
            return;
        }
        if self.is_binary() {
            out.push_str("    mov rcx, [rbp]\n"); // a
            out.push_str(&format!("    add rbp, {CELL}\n")); // drop NOS (2 in, 1 out)
            out.push_str("    cmp rcx, rax\n"); // a vs b
        } else {
            out.push_str("    test rax, rax\n"); // n vs 0
        }
        out.push_str(&format!("    set{} al\n", self.setcc()));
        out.push_str("    movzx eax, al\n");
        out.push_str("    neg rax\n");
    }
}

/// An open control-flow frame on the lowering control stack.
enum CtlFrame {
    If { id: u32, has_else: bool },
    Begin { id: u32 },
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
    /// `dup` immediately followed by a binary op — TOS combined with itself
    /// (the result of fusing `[Stack(Dup), Inline(op)]`). `dup +` -> `add rax,rax`,
    /// `dup *` -> `imul rax,rax` (square), `dup xor`/`dup -` -> 0, `dup and`/
    /// `dup or` -> nop. Matches WF65's dup-fuse peephole so shuffle+op stops
    /// regressing vs eager under settle-everywhere lowering.
    DupOp(Fop),
    /// A stack-shuffle primitive (Phase 1.1).
    Stack(StackOp),
    /// A memory access primitive (Phase 2.1).
    Mem(MemOp),
    /// A structured control-flow marker (Phase 4a).
    Ctl(Ctl),
    /// A flag-producing comparison (Phase 4a).
    Cmp(CmpOp),
    /// A comparison immediately consumed by a flag-testing control word
    /// (`IF`/`UNTIL`/`WHILE`) — compare→branch fusion. Branches off the
    /// comparison's operand directly, with no materialized boolean. The fused
    /// `Ctl` is always one of If/Until/While.
    CmpCtl(CmpOp, Ctl),
    /// A captured `pick` whose index isn't yet known to be constant. Only a
    /// constant `<lit> pick` is optimizable; this token is NOT deferrable, so a
    /// runtime pick falls back to the kernel's `pick`.
    PickWord,
    /// A constant `n pick` (n>=2): copy the n-th data-stack cell to TOS. (0/1
    /// pick reduce to dup/over.) Lowers to a single load — faster than runtime
    /// pick and than the deep shuffles written to avoid it.
    Pick(u32),
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

    pub fn mem(&mut self, op: MemOp) {
        self.tokens.push(Token::Mem(op));
    }

    pub fn ctl(&mut self, c: Ctl) {
        self.tokens.push(Token::Ctl(c));
    }

    pub fn cmp(&mut self, op: CmpOp) {
        self.tokens.push(Token::Cmp(op));
    }

    pub fn pick_word(&mut self) {
        self.tokens.push(Token::PickWord);
    }

    /// Splice a callee's token body into the current definition (Phase 3
    /// inlining). The spliced tokens then participate in the caller's fold /
    /// strength-reduce / DCE passes — folding across the former call boundary.
    pub fn splice(&mut self, toks: &[Token]) {
        self.tokens.extend_from_slice(toks);
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
    /// Mismatched control-flow markers (e.g. `ELSE`/`THEN` without `IF`, or an
    /// unclosed `IF`). The caller keeps the eager body.
    UnbalancedControl,
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::Unsupported(t) => {
                write!(f, "WF66 Phase 0 cannot lower token {t:?} (needs the settle fallback)")
            }
            LowerError::UnbalancedControl => write!(f, "WF66: unbalanced control flow"),
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

/// Dead-code elimination (Phase 1.3): a side-effect-free producer immediately
/// followed by `drop` cancels — the pushed value is never observed. Covers
/// `Lit drop`, `dup drop`, and `over drop`. Checking the running output's last
/// token after each cancellation reaches a fixpoint in one pass (e.g.
/// `5 dup drop drop` -> nothing). `Inline`/`ImmOp` are *not* pure producers
/// (they consume operands), so `op drop` is left alone.
pub fn dce(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        if matches!(t, Token::Stack(StackOp::Drop)) {
            if matches!(
                out.last(),
                Some(Token::Lit(_))
                    | Some(Token::Stack(StackOp::Dup))
                    | Some(Token::Stack(StackOp::Over))
            ) {
                out.pop();
                continue;
            }
        }
        out.push(t);
    }
    out
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

/// Load an immediate into `rax` with the smallest correct encoding (mirrors
/// WF65's literal emit): 0 -> `xor eax,eax` (2B); 1..=u32::MAX -> `mov eax,imm`
/// (5B, zero-extended); other i32 (small negatives) -> `mov rax,imm` (7B,
/// sign-extended); otherwise `movabs rax,imm64` (10B). Replaces the previous
/// always-`movabs`, which inflated every folded constant.
fn emit_load_imm(v: i64, out: &mut String) {
    if v == 0 {
        out.push_str("    xor eax, eax\n");
    } else if (1..=0xFFFF_FFFF).contains(&v) {
        out.push_str(&format!("    mov eax, {v}\n"));
    } else if i32::try_from(v).is_ok() {
        out.push_str(&format!("    mov rax, {v}\n"));
    } else {
        out.push_str(&format!("    movabs rax, {v}\n"));
    }
}

/// Fuse `dup` followed by a binary op into a single self-combining instruction
/// (`[Stack(Dup), Inline(op)]` -> `DupOp(op)`). Matches WF65's dup-fuse so a
/// shuffle+op no longer regresses vs eager under settle-everywhere lowering.
pub fn fold_dup_op(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        if let Token::Inline(op) = t {
            if matches!(out.last(), Some(Token::Stack(StackOp::Dup))) {
                out.pop();
                out.push(Token::DupOp(op));
                continue;
            }
        }
        out.push(t);
    }
    out
}

/// Lower `op TOS, TOS` (TOS in `rax`): the value combined with itself.
fn emit_dup_op(op: Fop, out: &mut String) {
    match op {
        Fop::Add => out.push_str("    add rax, rax\n"), // a+a = 2a
        Fop::Mul => out.push_str("    imul rax, rax\n"), // a*a = a^2
        Fop::And | Fop::Or => {}                          // a&a = a, a|a = a (nop)
        Fop::Sub | Fop::Xor => out.push_str("    xor eax, eax\n"), // a-a = 0, a^a = 0
    }
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

    // Control-flow lowering state: a stack of open frames and a monotonic id for
    // unique labels within this body.
    let mut ctl: Vec<CtlFrame> = Vec::new();
    let mut ctl_id: u32 = 0;

    for &t in tokens {
        match t {
            Token::Lit(v) => {
                // Push: spill old TOS to the cell below NOS, load the new TOS
                // (smallest correct encoding), commit the push by lowering DSP.
                s.push_str(&format!("    mov [rbp - {CELL}], rax\n"));
                emit_load_imm(v, &mut s);
                s.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            Token::Inline(op) => op.emit_bare(&mut s),
            Token::ImmOp { op, k } => emit_imm_op(op, k, &mut s),
            Token::DupOp(op) => emit_dup_op(op, &mut s),
            Token::Stack(op) => op.emit(&mut s),
            Token::Mem(op) => op.emit(&mut s),
            Token::Cmp(op) => op.emit(&mut s),
            Token::CmpCtl(c, cc) => emit_cmp_ctl(c, cc, &mut s, &mut ctl, &mut ctl_id)?,
            Token::Ctl(c) => emit_ctl(c, &mut s, &mut ctl, &mut ctl_id)?,
            // constant pick (n>=2): copy the n-th cell to TOS.
            Token::Pick(k) => {
                let off = (k as i64 - 1) * CELL;
                s.push_str(&format!("    mov [rbp - {CELL}], rax\n"));
                s.push_str(&format!("    mov rax, [rbp + {off}]\n"));
                s.push_str(&format!("    sub rbp, {CELL}\n"));
            }
            Token::PickWord | Token::Word { .. } | Token::Opaque => {
                return Err(LowerError::Unsupported(t))
            }
        }
    }

    if !ctl.is_empty() {
        return Err(LowerError::UnbalancedControl); // unclosed IF
    }
    s.push_str("    ret\n");
    Ok(s)
}

/// Consume the TOS flag (settle-everywhere): save it in `rcx`, reload TOS from
/// memory (a drop), and `test` the saved flag so a following `jz` can branch.
fn emit_consume_flag(out: &mut String) {
    out.push_str("    mov rcx, rax\n"); // save flag
    out.push_str("    mov rax, [rbp]\n"); // new TOS = NOS
    out.push_str(&format!("    add rbp, {CELL}\n")); // drop flag cell
    out.push_str("    test rcx, rcx\n");
}

/// Compare→branch fusion: a comparison (`Cmp`) immediately consumed by a
/// flag-testing control word (`IF`/`UNTIL`/`WHILE`) fuses to `CmpCtl`, so the
/// branch tests the comparison's operand directly with no materialized boolean.
pub fn fold_cmp_branch(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        if let Token::Ctl(ctl @ (Ctl::If | Ctl::Until | Ctl::While)) = t {
            if let Some(&Token::Cmp(c)) = out.last() {
                out.pop();
                out.push(Token::CmpCtl(c, ctl));
                continue;
            }
        }
        out.push(t);
    }
    out
}

/// Lower a fused compare→branch. Consume the comparison's operand, test it, and
/// branch with the *inverted* condition (the control word branches when the flag
/// would be FALSE): `0=` -> `jnz`, `0<` -> `jns`. No boolean is materialized.
fn emit_cmp_ctl(
    c: CmpOp,
    ctl: Ctl,
    out: &mut String,
    stack: &mut Vec<CtlFrame>,
    next_id: &mut u32,
) -> Result<(), LowerError> {
    if c.is_binary() {
        // ( a b -- ) : compare NOS vs TOS, consume both, branch off the flags
        out.push_str("    mov rcx, [rbp]\n"); // a
        out.push_str("    cmp rcx, rax\n"); // a vs b (flags)
        out.push_str(&format!("    mov rax, [rbp + {CELL}]\n")); // new TOS = NNOS
        out.push_str(&format!("    lea rbp, [rbp + {}]\n", 2 * CELL)); // drop 2, keep flags
    } else {
        emit_consume_flag(out); // consume the operand, `test` it
    }
    let jcc = c.inv_jcc();
    match ctl {
        Ctl::If => {
            let id = *next_id;
            *next_id += 1;
            out.push_str(&format!("    j{jcc} .wf66_c{id}_f\n"));
            stack.push(CtlFrame::If { id, has_else: false });
        }
        Ctl::Until => match stack.pop() {
            Some(CtlFrame::Begin { id }) => {
                out.push_str(&format!("    j{jcc} .wf66_c{id}_top\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::While => match stack.last() {
            Some(CtlFrame::Begin { id }) => {
                let id = *id;
                out.push_str(&format!("    j{jcc} .wf66_c{id}_exit\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        _ => return Err(LowerError::UnbalancedControl),
    }
    Ok(())
}

/// Lower a structured control marker against the control stack, emitting rasm
/// labels the assembler resolves. Mismatched markers (e.g. `THEN` closing a
/// `BEGIN`) return `UnbalancedControl` so the caller keeps the eager body.
fn emit_ctl(
    c: Ctl,
    out: &mut String,
    ctl: &mut Vec<CtlFrame>,
    next_id: &mut u32,
) -> Result<(), LowerError> {
    match c {
        Ctl::If => {
            let id = *next_id;
            *next_id += 1;
            emit_consume_flag(out);
            out.push_str(&format!("    jz .wf66_c{id}_f\n"));
            ctl.push(CtlFrame::If { id, has_else: false });
        }
        Ctl::Else => match ctl.last_mut() {
            Some(CtlFrame::If { id, has_else }) => {
                *has_else = true;
                let id = *id;
                out.push_str(&format!("    jmp .wf66_c{id}_e\n"));
                out.push_str(&format!(".wf66_c{id}_f:\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::Then => match ctl.pop() {
            Some(CtlFrame::If { id, has_else }) => {
                let lbl = if has_else { "e" } else { "f" };
                out.push_str(&format!(".wf66_c{id}_{lbl}:\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::Begin => {
            let id = *next_id;
            *next_id += 1;
            out.push_str(&format!(".wf66_c{id}_top:\n"));
            ctl.push(CtlFrame::Begin { id });
        }
        Ctl::Until => match ctl.pop() {
            Some(CtlFrame::Begin { id }) => {
                emit_consume_flag(out);
                out.push_str(&format!("    jz .wf66_c{id}_top\n")); // loop back while false
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::Again => match ctl.pop() {
            Some(CtlFrame::Begin { id }) => {
                out.push_str(&format!("    jmp .wf66_c{id}_top\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::While => match ctl.last() {
            Some(CtlFrame::Begin { id }) => {
                let id = *id;
                emit_consume_flag(out);
                out.push_str(&format!("    jz .wf66_c{id}_exit\n")); // exit while false
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        Ctl::Repeat => match ctl.pop() {
            Some(CtlFrame::Begin { id }) => {
                out.push_str(&format!("    jmp .wf66_c{id}_top\n"));
                out.push_str(&format!(".wf66_c{id}_exit:\n"));
            }
            _ => return Err(LowerError::UnbalancedControl),
        },
        // early return; the fall-through path keeps going (and gets the final ret)
        Ctl::Exit => out.push_str("    ret\n"),
    }
    Ok(())
}

/// Combine two adjacent same-op immediates into one (`+a +b -> +(a+b)`,
/// `*a *b -> *(a*b)`, `-a -b -> -(a+b)`, bitwise likewise), if the combined
/// immediate still fits a sign-extended imm32. Returns `None` to leave them split.
fn combine_imm(op: Fop, a: i64, b: i64) -> Option<i64> {
    let r = match op {
        Fop::Add | Fop::Sub => a.checked_add(b)?, // x-a-b = x-(a+b)
        Fop::Mul => a.checked_mul(b)?,
        Fop::And => a & b,
        Fop::Or => a | b,
        Fop::Xor => a ^ b,
    };
    fits_i32(r).then_some(r)
}

/// Try to reduce a single adjacent token pair to a fused "opti" replacement.
/// This is the rule table — each arm replaces a common sequence with one fused
/// IR node that lowers to optimal inline code (no call). Returns the replacement
/// (0, 1, or 2 tokens) or `None` if no rule applies.
fn reduce_pair(a: Token, b: Token) -> Option<Vec<Token>> {
    use StackOp::{Drop, Dup, Over, Swap};
    use Token::{Cmp, DupOp, ImmOp, Inline, Lit, Pick, PickWord, Stack};
    Some(match (a, b) {
        // DCE: a side-effect-free producer then drop -> nothing
        (Lit(_) | Stack(Dup) | Stack(Over), Stack(Drop)) => vec![],
        // two drops collapse to one 2drop (frequent: `drop drop`, miner-ranked)
        (Stack(Drop), Stack(Drop)) => vec![Stack(StackOp::TwoDrop)],
        // common idiom -> rarer single-op equivalent (Forth favors common ops)
        (Stack(Swap), Stack(Drop)) => vec![Stack(StackOp::Nip)], // swap drop = nip
        (Stack(Over), Stack(Over)) => vec![Stack(StackOp::TwoDup)], // over over = 2dup
        (Stack(Swap), Stack(Swap)) => vec![],                    // swap swap = identity
        // literal-zero comparison -> unary zero form (then fuses with if/until)
        (Lit(0), Cmp(c)) if c.zero_form().is_some() => vec![Cmp(c.zero_form().unwrap())],
        // constant pick -> direct cell copy (0/1 pick are just dup/over)
        (Lit(0), PickWord) => vec![Stack(StackOp::Dup)],
        (Lit(1), PickWord) => vec![Stack(StackOp::Over)],
        (Lit(k), PickWord) if k >= 2 && fits_i32((k - 1) * CELL) => vec![Pick(k as u32)],
        // literal folded into an op -> register-immediate op
        (Lit(k), Inline(op)) if fits_i32(k) => vec![ImmOp { op, k }],
        // dup + binary op -> self-combining op (a+a, a*a, ...)
        (Stack(Dup), Inline(op)) => vec![DupOp(op)],
        // comparison feeding a flag test -> branch off the operand directly
        (Cmp(c), Token::Ctl(ctl @ (Ctl::If | Ctl::Until | Ctl::While))) => {
            vec![Token::CmpCtl(c, ctl)]
        }
        // constant through a self-op (k dup* = k*k) -> constant
        (Lit(k), DupOp(op)) => vec![Lit(op.eval(k, k))],
        // constant through an immediate op -> constant
        (Lit(k), ImmOp { op, k: j }) => vec![Lit(op.eval(k, j))],
        // two same-op immediates collapse (7 + 3 + -> +10; 1+ 1+ 1+ 1+ -> +4)
        (ImmOp { op: o1, k: k1 }, ImmOp { op: o2, k: k2 }) if o1 == o2 => {
            vec![ImmOp { op: o1, k: combine_imm(o1, k1, k2)? }]
        }
        _ => return None,
    })
}

/// Repeatedly reduce the tail of `out` until no rule fires (window-3 const-fold
/// first, then the window-2 rule table). Shift-reduce: because reductions only
/// shorten the tail and the new tail is re-checked, one forward sweep reaches a
/// fixpoint, including cascades.
fn reduce_tail(out: &mut Vec<Token>) {
    loop {
        let n = out.len();
        if n >= 3 {
            if let (Token::Lit(a), Token::Lit(b), Token::Inline(op)) =
                (out[n - 3], out[n - 2], out[n - 1])
            {
                let v = op.eval(a, b);
                out.truncate(n - 3);
                out.push(Token::Lit(v));
                continue;
            }
        }
        if n >= 2 {
            if let Some(rep) = reduce_pair(out[n - 2], out[n - 1]) {
                out.truncate(n - 2);
                out.extend(rep);
                continue;
            }
        }
        break;
    }
}

/// The unified reduction engine: shift each token onto the output and reduce the
/// tail to a fixpoint. Subsumes const-fold / DCE / imm-fold / dup-fuse /
/// compare→branch as one rule table run to fixpoint, and catches the cascades a
/// fixed-order pipeline misses (`7 + 3 + -> +10`, `1+ 1+ 1+ 1+ -> +4`,
/// `2 dup * -> 4`). Whole-definition visibility is what makes this possible.
pub fn reduce(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for &t in tokens {
        out.push(t);
        reduce_tail(&mut out);
    }
    out
}

/// The per-`;` finalizer pipeline in one call: capture -> reduce -> lower.
pub fn compile_definition(tokens: &[Token], fn_name: &str) -> Result<String, LowerError> {
    let reduced = reduce(tokens);
    lower(&reduced, fn_name)
}

// ── Deferred assembly: instruction records (Phase 4b substrate) ─────────────
//
// "We have our own assembler, so we defer assembly": instead of handing the
// lowered text straight to the encoder, we lex it into a buffer of instruction
// records, (later) reduce that buffer with the same recognize->replace engine
// at the instruction level, then re-render. Step 1 only proves the round-trip
// is byte-for-byte identity — `render(parse_instrs(x)) == x` — so introducing
// the buffer changes nothing until a pass is added. Only the data-stack-pointer
// adjust is structured for now (what the first rbp-coalescing pass needs); every
// other line rides as `Raw` verbatim and is promoted as later passes require it.

/// A deferred-assembly instruction record.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Instr {
    /// `add rbp, n` (n>=0) or `sub rbp, -n` (n<0): the DSP adjust.
    AdjustDsp(i64),
    /// Any other emitted line, verbatim (without its trailing newline).
    Raw(String),
}

/// Lex lowered asm text into instruction records. Only the rbp adjust is
/// recognized; everything else (including labels and the preamble) is `Raw`.
fn parse_instrs(asm: &str) -> Vec<Instr> {
    asm.lines()
        .map(|line| {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("add rbp, ") {
                if let Ok(v) = rest.trim().parse::<i64>() {
                    return Instr::AdjustDsp(v);
                }
            }
            if let Some(rest) = t.strip_prefix("sub rbp, ") {
                if let Ok(v) = rest.trim().parse::<i64>() {
                    return Instr::AdjustDsp(-v);
                }
            }
            Instr::Raw(line.to_string())
        })
        .collect()
}

/// Render instruction records back to asm text. Inverse of [`parse_instrs`]:
/// `render(parse_instrs(x)) == x`.
fn render(instrs: &[Instr]) -> String {
    let mut s = String::new();
    for i in instrs {
        match i {
            Instr::AdjustDsp(n) if *n >= 0 => s.push_str(&format!("    add rbp, {n}\n")),
            Instr::AdjustDsp(n) => s.push_str(&format!("    sub rbp, {}\n", -n)),
            Instr::Raw(l) => {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    s
}

/// True when every token is in the Phase 0 deferrable subset (`Lit`/`Inline`) —
/// i.e. WF66 can lower the whole body. Any `Word`/`Opaque` (an unknown word, an
/// immediate word's emission, a `CODE:` region) makes the span non-deferrable;
/// the wired `;` then leaves the eager body in place (the settle fallback).
pub fn is_deferrable(tokens: &[Token]) -> bool {
    tokens.iter().all(|t| {
        matches!(
            t,
            Token::Lit(_)
                | Token::Inline(_)
                | Token::ImmOp { .. }
                | Token::DupOp(_)
                | Token::Stack(_)
                | Token::Mem(_)
                | Token::Ctl(_)
                | Token::Cmp(_)
                | Token::CmpCtl(_, _)
                | Token::Pick(_)
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
    // Deferred assembly: lex to instruction records, (reduce — later), re-render.
    // render(parse_instrs(asm)) == asm today, so this is behaviour-identical.
    let asm = render(&parse_instrs(&asm));
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
             \x20   mov [rbp - 8], rax\n    mov eax, 12\n    sub rbp, 8\n\
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

    // ---- deferred-assembly instruction buffer (Step 1: identity) --------

    #[test]
    fn instr_roundtrip_is_identity() {
        // render(parse_instrs(lower(x))) == lower(x) across a spread of shapes
        // that exercise every emitter (pushes, imm/dup ops, all stack ops, mem,
        // compares, control flow, pick). This is the "emitter unchanged" proof.
        let bodies: Vec<Vec<Token>> = vec![
            vec![Token::Lit(5), Token::Lit(7), Token::Inline(Fop::Add)],
            vec![Token::Lit(5), Token::Inline(Fop::Mul), Token::Lit(2), Token::Inline(Fop::Add)],
            vec![Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)],
            vec![Token::Stack(StackOp::Rot)],
            vec![Token::Stack(StackOp::TwoSwap)],
            vec![Token::Stack(StackOp::TwoOver)],
            vec![Token::Mem(MemOp::Fetch)],
            vec![Token::Mem(MemOp::Store)],
            vec![Token::Cmp(CmpOp::Lt)],
            vec![
                Token::CmpCtl(CmpOp::ZeroEq, Ctl::If),
                Token::Lit(1),
                Token::Ctl(Ctl::Else),
                Token::Lit(2),
                Token::Ctl(Ctl::Then),
            ],
            vec![
                Token::Ctl(Ctl::Begin),
                Token::ImmOp { op: Fop::Sub, k: 1 },
                Token::Stack(StackOp::Dup),
                Token::CmpCtl(CmpOp::ZeroEq, Ctl::Until),
            ],
            vec![Token::Pick(3)],
            vec![Token::Ctl(Ctl::Exit)],
        ];
        for b in bodies {
            let asm = lower(&b, "rt").unwrap();
            assert_eq!(
                render(&parse_instrs(&asm)),
                asm,
                "round-trip not identity for {b:?}"
            );
        }
    }

    // ---- unified reduce engine: cascades a fixed pipeline misses ---------

    #[test]
    fn reduce_combines_consecutive_immediates() {
        // 7 + 3 +  ->  +10
        assert_eq!(
            reduce(&[
                Token::Lit(7),
                Token::Inline(Fop::Add),
                Token::Lit(3),
                Token::Inline(Fop::Add),
            ]),
            vec![Token::ImmOp { op: Fop::Add, k: 10 }]
        );
        // 1+ 1+ 1+ 1+  ->  +4
        let mut ir = Vec::new();
        for _ in 0..4 {
            ir.push(Token::Lit(1));
            ir.push(Token::Inline(Fop::Add));
        }
        assert_eq!(reduce(&ir), vec![Token::ImmOp { op: Fop::Add, k: 4 }]);
    }

    #[test]
    fn reduce_idioms_to_single_ops() {
        // common idioms collapse to their rarer single-op equivalents
        assert_eq!(
            reduce(&[Token::Stack(StackOp::Swap), Token::Stack(StackOp::Drop)]),
            vec![Token::Stack(StackOp::Nip)]
        );
        assert_eq!(
            reduce(&[Token::Stack(StackOp::Over), Token::Stack(StackOp::Over)]),
            vec![Token::Stack(StackOp::TwoDup)]
        );
        assert_eq!(
            reduce(&[Token::Stack(StackOp::Swap), Token::Stack(StackOp::Swap)]),
            vec![]
        );
        // 0 = -> 0=
        assert_eq!(
            reduce(&[Token::Lit(0), Token::Cmp(CmpOp::Eq)]),
            vec![Token::Cmp(CmpOp::ZeroEq)]
        );
        // 0 < if  cascades: zero-form then compare->branch fusion
        assert_eq!(
            reduce(&[
                Token::Lit(0),
                Token::Cmp(CmpOp::Lt),
                Token::Ctl(Ctl::If),
                Token::Lit(1),
                Token::Ctl(Ctl::Then),
            ]),
            vec![
                Token::CmpCtl(CmpOp::ZeroLt, Ctl::If),
                Token::Lit(1),
                Token::Ctl(Ctl::Then),
            ]
        );
    }

    #[test]
    fn reduce_constant_pick() {
        // 0/1 pick -> dup/over ; 2 pick -> Pick(2) ; runtime pick stays PickWord
        assert_eq!(
            reduce(&[Token::Lit(0), Token::PickWord]),
            vec![Token::Stack(StackOp::Dup)]
        );
        assert_eq!(
            reduce(&[Token::Lit(1), Token::PickWord]),
            vec![Token::Stack(StackOp::Over)]
        );
        assert_eq!(
            reduce(&[Token::Lit(2), Token::PickWord]),
            vec![Token::Pick(2)]
        );
        // folded index: 1 1 + pick -> 2 pick -> Pick(2)
        assert_eq!(
            reduce(&[
                Token::Lit(1),
                Token::Lit(1),
                Token::Inline(Fop::Add),
                Token::PickWord
            ]),
            vec![Token::Pick(2)]
        );
        // runtime pick (no preceding literal) is not deferrable
        assert!(!is_deferrable(&[Token::PickWord]));
        // Pick(k) assembles
        let asm = lower(&[Token::Pick(3)], "wf66_t_pick").unwrap();
        wfasm::rasm::assemble(&asm).unwrap_or_else(|e| panic!("pick: {e:#}\n{asm}"));
    }

    #[test]
    fn reduce_collapses_two_drops() {
        // drop drop -> 2drop ; drop drop drop -> 2drop drop
        assert_eq!(
            reduce(&[Token::Stack(StackOp::Drop), Token::Stack(StackOp::Drop)]),
            vec![Token::Stack(StackOp::TwoDrop)]
        );
        assert_eq!(
            reduce(&[
                Token::Stack(StackOp::Drop),
                Token::Stack(StackOp::Drop),
                Token::Stack(StackOp::Drop)
            ]),
            vec![Token::Stack(StackOp::TwoDrop), Token::Stack(StackOp::Drop)]
        );
    }

    #[test]
    fn reduce_folds_constant_through_self_op() {
        // 2 dup *  ->  Lit 4   (dup-fuse then const-fold-through-DupOp)
        assert_eq!(
            reduce(&[Token::Lit(2), Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)]),
            vec![Token::Lit(4)]
        );
    }

    #[test]
    fn reduce_subsumes_const_fold_and_dce() {
        assert_eq!(
            reduce(&[Token::Lit(5), Token::Lit(7), Token::Inline(Fop::Add)]),
            vec![Token::Lit(12)]
        );
        assert_eq!(reduce(&[Token::Lit(5), Token::Stack(StackOp::Drop)]), vec![]);
        // cmp -> branch fusion still happens through the unified engine
        assert_eq!(
            reduce(&[Token::Cmp(CmpOp::ZeroEq), Token::Ctl(Ctl::If)]),
            vec![Token::CmpCtl(CmpOp::ZeroEq, Ctl::If)]
        );
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

    // ---- control flow (Phase 4a) ---------------------------------------

    #[test]
    fn ctl_if_else_then_assembles() {
        // ( flag -- n ) : if 1 else 2 then
        let asm = lower(
            &[
                Token::Ctl(Ctl::If),
                Token::Lit(1),
                Token::Ctl(Ctl::Else),
                Token::Lit(2),
                Token::Ctl(Ctl::Then),
            ],
            "wf66_t_if",
        )
        .unwrap();
        wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("if/else/then rejected: {e:#}\nasm:\n{asm}"));
    }

    #[test]
    fn ctl_if_then_no_else_assembles() {
        // ( n flag -- n|n+100 ) : if 100 + then
        let asm = lower(
            &[
                Token::Ctl(Ctl::If),
                Token::ImmOp { op: Fop::Add, k: 100 },
                Token::Ctl(Ctl::Then),
            ],
            "wf66_t_ift",
        )
        .unwrap();
        wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("if/then rejected: {e:#}\nasm:\n{asm}"));
    }

    #[test]
    fn ctl_nested_assembles() {
        // nested if inside if
        let asm = lower(
            &[
                Token::Ctl(Ctl::If),
                Token::Ctl(Ctl::If),
                Token::Lit(1),
                Token::Ctl(Ctl::Then),
                Token::Ctl(Ctl::Then),
            ],
            "wf66_t_nest",
        )
        .unwrap();
        wfasm::rasm::assemble(&asm)
            .unwrap_or_else(|e| panic!("nested if rejected: {e:#}\nasm:\n{asm}"));
    }

    #[test]
    fn cmp_branch_fuses_and_assembles() {
        // 0= if  ->  CmpCtl(ZeroEq, If); same for until/while.
        assert_eq!(
            fold_cmp_branch(&[Token::Cmp(CmpOp::ZeroEq), Token::Ctl(Ctl::If)]),
            vec![Token::CmpCtl(CmpOp::ZeroEq, Ctl::If)]
        );
        // a comparison NOT followed by a flag-consumer is left alone
        assert_eq!(
            fold_cmp_branch(&[Token::Cmp(CmpOp::ZeroLt), Token::Ctl(Ctl::Then)]),
            vec![Token::Cmp(CmpOp::ZeroLt), Token::Ctl(Ctl::Then)]
        );
        // fused forms assemble (with their control frames)
        for c in [CmpOp::ZeroEq, CmpOp::ZeroLt] {
            let ifd = lower(
                &[Token::CmpCtl(c, Ctl::If), Token::Lit(1), Token::Ctl(Ctl::Then)],
                "wf66_t_cbif",
            )
            .unwrap();
            wfasm::rasm::assemble(&ifd).unwrap_or_else(|e| panic!("{c:?} if: {e:#}\n{ifd}"));
            let utl = lower(
                &[Token::Ctl(Ctl::Begin), Token::CmpCtl(c, Ctl::Until)],
                "wf66_t_cbu",
            )
            .unwrap();
            wfasm::rasm::assemble(&utl).unwrap_or_else(|e| panic!("{c:?} until: {e:#}\n{utl}"));
        }
    }

    #[test]
    fn binary_compares_assemble() {
        let all = [
            CmpOp::ZeroNe,
            CmpOp::ZeroGt,
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Gt,
            CmpOp::Le,
            CmpOp::Ge,
            CmpOp::ULt,
            CmpOp::UGt,
        ];
        for c in all {
            // materialized
            let m = lower(&[Token::Cmp(c)], "wf66_t_bc").unwrap();
            wfasm::rasm::assemble(&m).unwrap_or_else(|e| panic!("{c:?} mat: {e:#}\n{m}"));
            // fused with IF
            let f = lower(
                &[Token::CmpCtl(c, Ctl::If), Token::Lit(1), Token::Ctl(Ctl::Then)],
                "wf66_t_bcf",
            )
            .unwrap();
            wfasm::rasm::assemble(&f).unwrap_or_else(|e| panic!("{c:?} fused: {e:#}\n{f}"));
        }
    }

    #[test]
    fn cmp_and_loops_assemble() {
        for op in [CmpOp::ZeroEq, CmpOp::ZeroLt] {
            let asm = lower(&[Token::Cmp(op)], "wf66_t_cmp").unwrap();
            wfasm::rasm::assemble(&asm).unwrap_or_else(|e| panic!("{op:?}: {e:#}\nasm:\n{asm}"));
        }
        // begin 1- dup 0= until
        let until = lower(
            &[
                Token::Ctl(Ctl::Begin),
                Token::ImmOp { op: Fop::Sub, k: 1 },
                Token::Stack(StackOp::Dup),
                Token::Cmp(CmpOp::ZeroEq),
                Token::Ctl(Ctl::Until),
            ],
            "wf66_t_until",
        )
        .unwrap();
        wfasm::rasm::assemble(&until).unwrap_or_else(|e| panic!("until: {e:#}\n{until}"));
        // begin dup while 1- repeat
        let whilel = lower(
            &[
                Token::Ctl(Ctl::Begin),
                Token::Stack(StackOp::Dup),
                Token::Ctl(Ctl::While),
                Token::ImmOp { op: Fop::Sub, k: 1 },
                Token::Ctl(Ctl::Repeat),
            ],
            "wf66_t_while",
        )
        .unwrap();
        wfasm::rasm::assemble(&whilel).unwrap_or_else(|e| panic!("while: {e:#}\n{whilel}"));
        // begin again (infinite; assemble only)
        let again = lower(&[Token::Ctl(Ctl::Begin), Token::Ctl(Ctl::Again)], "wf66_t_again").unwrap();
        wfasm::rasm::assemble(&again).unwrap();
    }

    #[test]
    fn ctl_mismatch_errors() {
        // BEGIN closed by THEN, or IF closed by UNTIL -> unbalanced.
        assert_eq!(
            lower(&[Token::Ctl(Ctl::Begin), Token::Ctl(Ctl::Then)], "x"),
            Err(LowerError::UnbalancedControl)
        );
        assert_eq!(
            lower(&[Token::Ctl(Ctl::If), Token::Ctl(Ctl::Until)], "x"),
            Err(LowerError::UnbalancedControl)
        );
    }

    #[test]
    fn ctl_unbalanced_errors() {
        assert_eq!(
            lower(&[Token::Ctl(Ctl::Then)], "x"),
            Err(LowerError::UnbalancedControl)
        );
        assert_eq!(
            lower(&[Token::Ctl(Ctl::If)], "x"),
            Err(LowerError::UnbalancedControl)
        );
    }

    // ---- memory ops (Phase 2.1) ----------------------------------------

    #[test]
    fn mem_ops_assemble() {
        for op in [MemOp::Fetch, MemOp::Store, MemOp::CFetch, MemOp::CStore] {
            let asm = lower(&[Token::Mem(op)], "wf66_t_mem").unwrap();
            wfasm::rasm::assemble(&asm)
                .unwrap_or_else(|e| panic!("{op:?} rejected: {e:#}\nasm:\n{asm}"));
        }
    }

    #[test]
    fn mem_body_is_deferrable() {
        // : @1+ @ 1 + ;  ( addr -- *addr+1 )
        assert!(is_deferrable(&[
            Token::Mem(MemOp::Fetch),
            Token::Lit(1),
            Token::Inline(Fop::Add),
        ]));
    }

    // ---- DCE (Phase 1.3) -----------------------------------------------

    #[test]
    fn dce_cancels_pure_push_then_drop() {
        assert_eq!(dce(&[Token::Lit(5), Token::Stack(StackOp::Drop)]), vec![]);
        assert_eq!(
            dce(&[Token::Stack(StackOp::Dup), Token::Stack(StackOp::Drop)]),
            vec![]
        );
        assert_eq!(
            dce(&[Token::Stack(StackOp::Over), Token::Stack(StackOp::Drop)]),
            vec![]
        );
    }

    #[test]
    fn dce_reaches_fixpoint_in_one_pass() {
        // 5 dup drop drop -> nothing
        let ir = [
            Token::Lit(5),
            Token::Stack(StackOp::Dup),
            Token::Stack(StackOp::Drop),
            Token::Stack(StackOp::Drop),
        ];
        assert_eq!(dce(&ir), vec![]);
    }

    #[test]
    fn dce_leaves_consuming_op_before_drop() {
        // 5 + drop consumes the entry value — not removable.
        let ir = [Token::Lit(5), Token::Inline(Fop::Add), Token::Stack(StackOp::Drop)];
        assert_eq!(dce(&ir), ir.to_vec());
    }

    // ---- dup+op fusion (regression fix) --------------------------------

    #[test]
    fn dup_op_fuses() {
        // dup * -> DupOp(Mul); the lone-token cases too.
        assert_eq!(
            fold_dup_op(&[Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)]),
            vec![Token::DupOp(Fop::Mul)]
        );
        // a non-dup op is left alone
        assert_eq!(
            fold_dup_op(&[Token::Inline(Fop::Add)]),
            vec![Token::Inline(Fop::Add)]
        );
    }

    #[test]
    fn dup_op_lowerings_assemble() {
        for op in [Fop::Add, Fop::Sub, Fop::Mul, Fop::And, Fop::Or, Fop::Xor] {
            let asm = lower(&[Token::DupOp(op)], "wf66_t_dupop").unwrap();
            wfasm::rasm::assemble(&asm)
                .unwrap_or_else(|e| panic!("dup {op:?}: {e:#}\nasm:\n{asm}"));
        }
    }

    #[test]
    fn dup_mul_is_compact() {
        // The regression case: dup * must fuse to a tiny body (imul rax,rax; ret).
        let bytes =
            compile_body_bytes(&[Token::Stack(StackOp::Dup), Token::Inline(Fop::Mul)]).unwrap();
        assert!(bytes.len() <= 8, "dup * should be tiny, got {} bytes: {bytes:02x?}", bytes.len());
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
            StackOp::Tuck,
            StackOp::Rot,
            StackOp::NegRot,
            StackOp::TwoDup,
            StackOp::TwoDrop,
            StackOp::TwoSwap,
            StackOp::TwoOver,
            StackOp::TwoNip,
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
