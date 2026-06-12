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
/// Offset of `user_FSP` (the FP stack pointer) in the user area (kernel
/// `@assign user_FSP = 0x1218`). The FP stack pointer lives in memory at
/// `[rbx + FSP_OFF]` (rbx = UP), not a register, so each FP op loads/stores it.
const FSP_OFF: i64 = 0x1218;

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

impl FpOp {
    /// Settle-everywhere FP lowering (FTOS in `xmm15`, FNOS at the FP stack
    /// pointer `[rbx + FSP_OFF]`, `xmm0`/`rcx` scratch). Verbatim mirror of
    /// kernel `f+ f- f* f/`.
    fn emit(self, out: &mut String) {
        out.push_str(&format!("    mov rcx, [rbx + {FSP_OFF}]\n"));
        match self {
            // commutative: op FTOS with FNOS in place, then pop.
            FpOp::Add => out.push_str("    addsd xmm15, qword ptr [rcx]\n"),
            FpOp::Mul => out.push_str("    mulsd xmm15, qword ptr [rcx]\n"),
            // non-commutative: FNOS (r1) <op> FTOS (r2) -> via xmm0, then pop.
            FpOp::Sub => {
                out.push_str("    movsd xmm0, qword ptr [rcx]\n");
                out.push_str("    subsd xmm0, xmm15\n");
                out.push_str("    movsd xmm15, xmm0\n");
            }
            FpOp::Div => {
                out.push_str("    movsd xmm0, qword ptr [rcx]\n");
                out.push_str("    divsd xmm0, xmm15\n");
                out.push_str("    movsd xmm15, xmm0\n");
            }
        }
        out.push_str(&format!("    add rcx, {CELL}\n"));
        out.push_str(&format!("    mov [rbx + {FSP_OFF}], rcx\n"));
    }
}

impl FpStackOp {
    /// Settle-everywhere FP-stack shuffle. Verbatim mirror of kernel `fdup`
    /// `fdrop` `fswap` `fover`.
    fn emit(self, out: &mut String) {
        out.push_str(&format!("    mov rcx, [rbx + {FSP_OFF}]\n"));
        match self {
            // ( r -- r r ): spill FTOS to new FNOS, FSP -= cell.
            FpStackOp::FDup => {
                out.push_str(&format!("    movsd qword ptr [rcx - {CELL}], xmm15\n"));
                out.push_str(&format!("    sub rcx, {CELL}\n"));
                out.push_str(&format!("    mov [rbx + {FSP_OFF}], rcx\n"));
            }
            // ( r -- ): reload FTOS from FNOS, FSP += cell.
            FpStackOp::FDrop => {
                out.push_str("    movsd xmm15, qword ptr [rcx]\n");
                out.push_str(&format!("    add rcx, {CELL}\n"));
                out.push_str(&format!("    mov [rbx + {FSP_OFF}], rcx\n"));
            }
            // ( r1 r2 -- r2 r1 ): FTOS <-> [FSP], FSP unchanged.
            FpStackOp::FSwap => {
                out.push_str("    movsd xmm0, qword ptr [rcx]\n");
                out.push_str("    movsd qword ptr [rcx], xmm15\n");
                out.push_str("    movsd xmm15, xmm0\n");
            }
            // ( r1 r2 -- r1 r2 r1 ): push a copy of FNOS, FSP -= cell.
            FpStackOp::FOver => {
                out.push_str("    movsd xmm0, qword ptr [rcx]\n");
                out.push_str(&format!("    movsd qword ptr [rcx - {CELL}], xmm15\n"));
                out.push_str(&format!("    sub rcx, {CELL}\n"));
                out.push_str(&format!("    mov [rbx + {FSP_OFF}], rcx\n"));
                out.push_str("    movsd xmm15, xmm0\n");
            }
        }
    }
}

impl FpMemOp {
    /// FP memory access (`f@ f!`): address in TOS (`rax`), value in FTOS
    /// (`xmm15`); both top cells consumed. Verbatim mirror of kernel `f@`/`f!`.
    fn emit(self, out: &mut String) {
        match self {
            // f@ ( f-addr -- ) ( F: -- r ): push [addr] onto the FP stack,
            // drop the data-stack address.
            FpMemOp::FFetch => {
                out.push_str("    mov rcx, rax\n");
                out.push_str(&format!("    mov rdx, [rbx + {FSP_OFF}]\n"));
                out.push_str(&format!("    movsd qword ptr [rdx - {CELL}], xmm15\n"));
                out.push_str(&format!("    sub rdx, {CELL}\n"));
                out.push_str(&format!("    mov [rbx + {FSP_OFF}], rdx\n"));
                out.push_str("    movsd xmm15, qword ptr [rcx]\n");
                out.push_str("    mov rax, [rbp]\n");
                out.push_str(&format!("    add rbp, {CELL}\n"));
            }
            // f! ( f-addr -- ) ( F: r -- ): store FTOS to [addr], pop the FP
            // stack, drop the data-stack address.
            FpMemOp::FStore => {
                out.push_str("    mov rcx, rax\n");
                out.push_str("    movsd qword ptr [rcx], xmm15\n");
                out.push_str(&format!("    mov rdx, [rbx + {FSP_OFF}]\n"));
                out.push_str("    movsd xmm15, qword ptr [rdx]\n");
                out.push_str(&format!("    add rdx, {CELL}\n"));
                out.push_str(&format!("    mov [rbx + {FSP_OFF}], rdx\n"));
                out.push_str("    mov rax, [rbp]\n");
                out.push_str(&format!("    add rbp, {CELL}\n"));
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

/// A binary floating-point op on the FP stack (`f+ f- f* f/`). Mirrors [`Fop`]
/// for the separate FP stack (FTOS cached in `xmm15`, FNOS+ in memory at the FP
/// stack pointer `[rbx + user_FSP]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// An FP-stack shuffle (`fdup fdrop fswap fover`). Mirrors [`StackOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpStackOp {
    FDup,
    FDrop,
    FSwap,
    FOver,
}

/// An FP memory access (`f@ f!`): the address is a data-stack cell (TOS), the
/// value an FP-stack cell (FTOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpMemOp {
    FFetch,
    FStore,
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
    /// A binary floating-point op (`f+ f- f* f/`) on the FP stack.
    FpBin(FpOp),
    /// `fnegate` — negate FTOS (FP depth unchanged).
    FpNeg,
    /// An FP-stack shuffle (`fdup fdrop fswap fover`).
    FpStack(FpStackOp),
    /// An FP memory access (`f@ f!`): data-stack address, FP-stack value.
    FpMem(FpMemOp),
    /// A settle-barrier call to a known word at absolute address `xt` (F2). The
    /// stacks are settled to canonical before the call (TOS in rax, FTOS in
    /// xmm15, DSP/FSP in memory), so the callee — which preserves every Forth
    /// invariant — runs exactly as under eager; the optimizer keeps optimizing
    /// the windows around it. Used for libm math words (`fsqrt fsin ...`) so FP
    /// code that calls them still optimizes. Needs no stack-effect analysis.
    Call(u64),
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

    pub fn fp_bin(&mut self, op: FpOp) {
        self.tokens.push(Token::FpBin(op));
    }

    pub fn fp_neg(&mut self) {
        self.tokens.push(Token::FpNeg);
    }

    pub fn fp_stack(&mut self, op: FpStackOp) {
        self.tokens.push(Token::FpStack(op));
    }

    pub fn fp_mem(&mut self, op: FpMemOp) {
        self.tokens.push(Token::FpMem(op));
    }

    pub fn call(&mut self, xt: u64) {
        self.tokens.push(Token::Call(xt));
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
            // ── floating point (settle-everywhere; mirrors kernel/float.masm) ──
            // FTOS in xmm15, FNOS+ in memory at the FP stack pointer
            // [rbx + FSP_OFF], xmm0 scratch. Patterns copied verbatim from the
            // kernel so WF66's FP body interoperates with the eager FP path.
            Token::FpBin(op) => op.emit(&mut s),
            Token::FpNeg => {
                s.push_str("    xorpd xmm0, xmm0\n");
                s.push_str("    subsd xmm0, xmm15\n");
                s.push_str("    movsd xmm15, xmm0\n");
            }
            Token::FpStack(op) => op.emit(&mut s),
            Token::FpMem(op) => op.emit(&mut s),
            // Settle-barrier call: at this point the data + FP stacks are settled
            // to canonical (the call is a Raw barrier, so coalesce flushed the
            // rbp adjust, TOS is in rax, FTOS in xmm15, FSP in memory). Call the
            // word absolutely (position-independent target) via a scratch reg.
            Token::Call(xt) => {
                s.push_str(&format!("    movabs rcx, {xt}\n"));
                s.push_str("    call rcx\n");
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
    /// `mov <dst>, [rbp+disp]`: load a data-stack cell into a register.
    LoadCell { dst: String, disp: i64 },
    /// `mov [rbp+disp], <src>`: spill a register to a data-stack cell.
    StoreCell { disp: i64, src: String },
    /// `mov <dst>, <src>`: a register-to-register move (rbp-independent, so it
    /// neither moves the stack pointer nor bounds a window).
    RegMove { dst: String, src: String },
    /// An arithmetic op with a data-stack cell operand — a Fop combining TOS with
    /// `[rbp+disp]`. `cell_is_dest=false`: `<mnem> <reg>, [rbp+disp]` (reads the
    /// cell, e.g. `add`/`imul`/`and`/`or`/`xor`). `cell_is_dest=true`:
    /// `<mnem> [rbp+disp], <reg>` (reads *and* writes the cell, e.g. `sub`).
    /// Carrying the disp lets coalesce hold rbp stable across arithmetic.
    CellAlu {
        mnem: String,
        reg: String,
        disp: i64,
        cell_is_dest: bool,
    },
    /// Any other emitted line, verbatim (without its trailing newline).
    Raw(String),
}

/// True when `s` is a bare register operand (not memory, not an immediate).
fn is_reg(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()) && s.parse::<i64>().is_err()
}

/// Format an `[rbp+disp]` operand exactly as the emitters do.
fn mem_rbp(disp: i64) -> String {
    if disp == 0 {
        "[rbp]".to_string()
    } else if disp > 0 {
        format!("[rbp + {disp}]")
    } else {
        format!("[rbp - {}]", -disp)
    }
}

/// Parse an `[rbp]` / `[rbp + N]` / `[rbp - N]` operand to its displacement.
/// Returns None for any other memory operand (e.g. `[rax]`, indexed) so those
/// stay `Raw`.
fn parse_rbp_mem(s: &str) -> Option<i64> {
    let inner = s.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner == "rbp" {
        Some(0)
    } else if let Some(r) = inner.strip_prefix("rbp + ") {
        r.trim().parse::<i64>().ok()
    } else if let Some(r) = inner.strip_prefix("rbp - ") {
        r.trim().parse::<i64>().ok().map(|v| -v)
    } else {
        None
    }
}

/// Lex lowered asm text into instruction records. The rbp adjust and
/// `[rbp+disp]` cell loads/stores are recognized; everything else (program
/// memory `[rax]`, labels, the preamble, lea, arithmetic-to-memory) is `Raw`.
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
            // `mov [rbp+disp], <src>` (store) or `mov <dst>, [rbp+disp]` (load).
            // Only [rbp]-relative movs structure; [rax]/imm/reg-reg stay Raw.
            if let Some(rest) = t.strip_prefix("mov ") {
                if let Some((a, b)) = rest.split_once(", ") {
                    if let Some(disp) = parse_rbp_mem(a) {
                        return Instr::StoreCell {
                            disp,
                            src: b.to_string(),
                        };
                    }
                    if let Some(disp) = parse_rbp_mem(b) {
                        return Instr::LoadCell {
                            dst: a.to_string(),
                            disp,
                        };
                    }
                    // `mov <reg>, <reg>`: a register-to-register move.
                    if is_reg(a) && is_reg(b) {
                        return Instr::RegMove {
                            dst: a.to_string(),
                            src: b.to_string(),
                        };
                    }
                }
            }
            // Arithmetic op against a data-stack cell (a Fop). `op reg,[rbp+d]`
            // reads the cell; `op [rbp+d],reg` reads+writes it. (`add/sub rbp`
            // and immediate forms were handled above / fall through to Raw.)
            for mnem in ["add", "sub", "imul", "and", "or", "xor"] {
                if let Some(rest) = t.strip_prefix(mnem).and_then(|r| r.strip_prefix(' ')) {
                    if let Some((a, b)) = rest.split_once(", ") {
                        if let Some(disp) = parse_rbp_mem(a) {
                            return Instr::CellAlu {
                                mnem: mnem.to_string(),
                                reg: b.to_string(),
                                disp,
                                cell_is_dest: true,
                            };
                        }
                        if let Some(disp) = parse_rbp_mem(b) {
                            return Instr::CellAlu {
                                mnem: mnem.to_string(),
                                reg: a.to_string(),
                                disp,
                                cell_is_dest: false,
                            };
                        }
                    }
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
            Instr::LoadCell { dst, disp } => {
                s.push_str(&format!("    mov {dst}, {}\n", mem_rbp(*disp)))
            }
            Instr::StoreCell { disp, src } => {
                s.push_str(&format!("    mov {}, {src}\n", mem_rbp(*disp)))
            }
            Instr::RegMove { dst, src } => s.push_str(&format!("    mov {dst}, {src}\n")),
            Instr::CellAlu {
                mnem,
                reg,
                disp,
                cell_is_dest,
            } => {
                if *cell_is_dest {
                    s.push_str(&format!("    {mnem} {}, {reg}\n", mem_rbp(*disp)));
                } else {
                    s.push_str(&format!("    {mnem} {reg}, {}\n", mem_rbp(*disp)));
                }
            }
            Instr::Raw(l) => {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    s
}

/// FP-stack-pointer coalescing (F1): the kernel reloads and stores `user_FSP`
/// from memory on *every* FP op (it isn't a register like rbp). But within a run
/// of consecutive FP ops FSP stays in `rcx`, so an op's trailing
/// `mov [rbx+FSP], rcx` immediately followed by the next op's
/// `mov rcx, [rbx+FSP]` is redundant — drop both. FSP is then loaded once at the
/// run start and stored once before the next barrier, killing the per-op pointer
/// traffic that makes Forth FP slow. Sound: the pair is adjacent (nothing reads
/// FSP between), and the run's final store (unpaired) still settles memory
/// before any call/control edge.
fn fp_coalesce(instrs: Vec<Instr>) -> Vec<Instr> {
    let load = format!("mov rcx, [rbx + {FSP_OFF}]");
    let store = format!("mov [rbx + {FSP_OFF}], rcx");
    let mut out: Vec<Instr> = Vec::with_capacity(instrs.len());
    for ins in instrs {
        if let Instr::Raw(l) = &ins {
            if l.trim() == load
                && matches!(out.last(), Some(Instr::Raw(p)) if p.trim() == store)
            {
                out.pop(); // drop the preceding store...
                continue; // ...and skip this load (rcx already holds FSP)
            }
        }
        out.push(ins);
    }
    out
}

/// rbp-coalescing (Step 2.3): defer the data-stack-pointer adjusts to the end of
/// each barrier-free window, rewriting intervening cell displacements by the
/// running delta. The N interspersed `add/sub rbp` of a shuffle run collapse to a
/// single adjust at the window edge — or vanish entirely when they cancel. Runs
/// *before* [`forward_loads`]: removing the interior adjusts turns relative cell
/// accesses into absolute ones, which exposes more redundant reloads to drop.
///
/// Sound because the deferred adjust is *flushed* before every `Raw` barrier, and
/// every instruction that depends on rbp being at its logical position — a Fop
/// reading `[rbp]`, a jump, a label, a call, `ret`, the binary-compare
/// `lea rbp,[rbp+16]` — is a `Raw` line. So nothing ever observes rbp mid-defer;
/// only the structured cells (whose disp we rewrite) and the adjust itself move.
/// Displacement bookkeeping: at any access, `flushed + delta` equals the total
/// adjust seen so far, so `[rbp_phys + (disp + delta)]` is the original address.
fn coalesce_dsp(instrs: Vec<Instr>) -> Vec<Instr> {
    let mut out: Vec<Instr> = Vec::with_capacity(instrs.len());
    let mut delta: i64 = 0; // adjust deferred past the cells rewritten so far
    for ins in instrs {
        match ins {
            Instr::AdjustDsp(n) => delta += n, // defer
            Instr::LoadCell { dst, disp } => out.push(Instr::LoadCell {
                dst,
                disp: disp + delta,
            }),
            Instr::StoreCell { disp, src } => out.push(Instr::StoreCell {
                disp: disp + delta,
                src,
            }),
            Instr::RegMove { dst, src } => out.push(Instr::RegMove { dst, src }),
            // A Fop reads (and maybe writes) a data-stack cell but leaves rbp
            // alone — so it does NOT settle rbp; rewrite its cell disp by the
            // running delta, exactly like a Load/Store. This keeps rbp stable
            // across arithmetic so a whole basic block is one run.
            Instr::CellAlu {
                mnem,
                reg,
                disp,
                cell_is_dest,
            } => out.push(Instr::CellAlu {
                mnem,
                reg,
                disp: disp + delta,
                cell_is_dest,
            }),
            Instr::Raw(l) => {
                if delta != 0 {
                    out.push(Instr::AdjustDsp(delta));
                    delta = 0;
                }
                out.push(Instr::Raw(l));
            }
        }
    }
    if delta != 0 {
        out.push(Instr::AdjustDsp(delta));
    }
    out
}

/// An abstract value flowing through a barrier-free window: it traces back either
/// to the register `rax` at window entry, or to the entry content of some slot.
/// (Pure movement windows never synthesize new values — only shuffle these.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Val {
    Rax,
    Slot(i64),
}

/// Sub-register aliases to also test in the live-out guard (so a window that
/// loads `rcx` is held back when the following barrier reads `cl`, etc.).
fn reg_family(r: &str) -> &'static [&'static str] {
    match r {
        "rcx" => &["ecx", "cx", "cl", "ch"],
        "rdx" => &["edx", "dx", "dl", "dh"],
        _ => &[],
    }
}

/// Window fusion (Step 2.4): the heart of "auto-pick instead of stack ops".
/// After coalescing, a barrier-free window is pure fixed-offset slot addressing.
/// Rather than replaying each stack op's moves, recognize the *whole window* as a
/// permutation/duplication pattern: symbolically simulate it to the net map
/// (final rax + each changed slot) <- (entry rax + entry slots), then emit the
/// MINIMAL parallel-move that realizes it. `rot rot` (8 accesses) collapses to its
/// net `-rot` (4); redundant reloads vanish; nothing is replayed.
fn window_fuse(instrs: Vec<Instr>) -> Vec<Instr> {
    let mut out: Vec<Instr> = Vec::with_capacity(instrs.len());
    let mut i = 0;
    while i < instrs.len() {
        // CellAlu (a Fop) and Raw both bound a pure-movement window: a Fop
        // computes (it isn't a value shuffle) so fusion can't model it; it passes
        // through untouched. (It reads only rax + its cell, so it can't read a
        // window's scratch reg — the live-out guard needn't see it.)
        if matches!(instrs[i], Instr::Raw(_) | Instr::CellAlu { .. }) {
            out.push(instrs[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < instrs.len() && !matches!(instrs[i], Instr::Raw(_) | Instr::CellAlu { .. }) {
            i += 1;
        }
        let window = &instrs[start..i];
        // text of the immediately-following Raw run, for the live-out guard
        let mut follow = String::new();
        let mut k = i;
        while let Some(Instr::Raw(l)) = instrs.get(k) {
            follow.push_str(l);
            follow.push('\n');
            k += 1;
        }
        match fuse_window(window, &follow) {
            Some(em) => out.extend(em),
            None => out.extend(window.iter().cloned()), // unfusible: emit verbatim
        }
    }
    out
}

/// Fuse one barrier-free window to its minimal parallel-move, or `None` to keep
/// it verbatim when fusion would be unsafe (unknown source register, a load/store
/// after the coalesced adjust, a defined scratch register read by the following
/// barrier, or more live temporaries than the spare pool holds).
fn fuse_window(window: &[Instr], follow: &str) -> Option<Vec<Instr>> {
    use std::collections::HashMap;
    // 1. split movement ops from the (single, trailing) coalesced adjust
    let mut net_delta = 0i64;
    let mut moves: Vec<&Instr> = Vec::new();
    let mut adjust_seen = false;
    for ins in window {
        match ins {
            Instr::AdjustDsp(n) => {
                net_delta += n;
                adjust_seen = true;
            }
            Instr::LoadCell { .. } | Instr::StoreCell { .. } | Instr::RegMove { .. } => {
                if adjust_seen {
                    return None; // movement after the adjust: not coalesced as expected
                }
                moves.push(ins);
            }
            Instr::Raw(_) | Instr::CellAlu { .. } => return None,
        }
    }
    // 2. symbolic simulation -> net register/slot map
    let mut reg: HashMap<&str, Val> = HashMap::new();
    reg.insert("rax", Val::Rax);
    let mut slot: HashMap<i64, Val> = HashMap::new();
    let mut defined: Vec<&str> = Vec::new(); // non-rax registers this window writes
    for ins in &moves {
        match ins {
            Instr::LoadCell { dst, disp } => {
                let v = slot.get(disp).copied().unwrap_or(Val::Slot(*disp));
                reg.insert(dst.as_str(), v);
                if dst != "rax" && !defined.contains(&dst.as_str()) {
                    defined.push(dst.as_str());
                }
            }
            Instr::StoreCell { disp, src } => {
                let v = *reg.get(src.as_str())?;
                slot.insert(*disp, v);
            }
            Instr::RegMove { dst, src } => {
                let v = *reg.get(src.as_str())?;
                reg.insert(dst.as_str(), v);
                if dst != "rax" && !defined.contains(&dst.as_str()) {
                    defined.push(dst.as_str());
                }
            }
            Instr::Raw(_) | Instr::AdjustDsp(_) | Instr::CellAlu { .. } => unreachable!(),
        }
    }
    // 3. live-out scratch guard: a scratch reg this window defines must not be
    //    read by the following barrier (settle keeps scratch dead across windows,
    //    so this only fires inside a single op's lowering, e.g. `!`/`c!`).
    for r in &defined {
        if follow.contains(*r) || reg_family(r).iter().any(|n| follow.contains(n)) {
            return None;
        }
    }
    // 4. assignments: realize every CHANGED slot (no liveness filter -> sound
    //    across pushes/barriers) plus rax. Unchanged slots keep their value.
    let final_rax = *reg.get("rax").unwrap();
    let mut slot_assigns: Vec<(i64, Val)> = slot
        .iter()
        .filter(|(d, v)| **v != Val::Slot(**d))
        .map(|(d, v)| (*d, *v))
        .collect();
    slot_assigns.sort_by_key(|(d, _)| *d);
    // 5. emit the minimal parallel-move, holding hot/snapshotted source slots in
    //    registers.  A source slot is loaded into a register ONCE when it is
    //    either written (must be snapshotted before the overwrite) or read more
    //    than once ("many reads, few writes" -> one load, reuse for every read).
    //    A source read exactly once and never overwritten is read straight from
    //    memory via a shared transient.
    let dest_slots: std::collections::HashSet<i64> =
        slot_assigns.iter().map(|(d, _)| *d).collect();

    // How many times each source slot is read (slot writes + the rax move).
    let mut read_count: HashMap<i64, usize> = HashMap::new();
    for (_, v) in &slot_assigns {
        if let Val::Slot(s) = v {
            *read_count.entry(*s).or_insert(0) += 1;
        }
    }
    if let Val::Slot(s) = final_rax {
        *read_count.entry(s).or_insert(0) += 1;
    }

    // Fusion's scratch pool. r10/r11 are reserved for the block-level read-only
    // hot-cell promoter (promote_hot_cells), so the two passes never collide.
    let pool = ["rsi", "rdi", "r8", "r9", "rcx", "rdx"];
    // Candidate source slots, hottest first: a written source ("mandatory" — it
    // must be snapshotted) outranks a read-only one, then ranked by read count.
    let mut srcs: Vec<i64> = read_count.keys().copied().collect();
    srcs.sort_by(|a, b| {
        dest_slots
            .contains(b)
            .cmp(&dest_slots.contains(a))
            .then(read_count[b].cmp(&read_count[a]))
            .then(a.cmp(b))
    });
    // Every written source must be held; bail only if even those can't fit.
    let mandatory = srcs.iter().filter(|s| dest_slots.contains(s)).count();
    if mandatory + 1 > pool.len() {
        return None;
    }
    let mut held: Vec<i64> = srcs
        .iter()
        .copied()
        .filter(|s| dest_slots.contains(s) || read_count[s] >= 2)
        .collect();
    // Keep one register free as the transient for direct (read-once) reads;
    // if the hot set overflows, the coldest promotions fall back to direct.
    if held.len() + 1 > pool.len() {
        held.truncate(pool.len() - 1);
    }

    // Emit structured records (not Raw text) so a later pass can still read the
    // cell traffic; render() turns these into the exact same bytes.
    let mut emitted: Vec<Instr> = Vec::new();
    let mut tmp_of: HashMap<i64, &str> = HashMap::new();
    for (idx, s) in held.iter().enumerate() {
        emitted.push(Instr::LoadCell {
            dst: pool[idx].to_string(),
            disp: *s,
        });
        tmp_of.insert(*s, pool[idx]);
    }
    let transient = pool.get(held.len()).copied();
    // changed slots (rax still holds entry value; written last)
    for (d, v) in &slot_assigns {
        match v {
            Val::Rax => emitted.push(Instr::StoreCell {
                disp: *d,
                src: "rax".to_string(),
            }),
            Val::Slot(s) => {
                if let Some(&t) = tmp_of.get(s) {
                    emitted.push(Instr::StoreCell {
                        disp: *d,
                        src: t.to_string(),
                    });
                } else {
                    let t = transient?;
                    emitted.push(Instr::LoadCell {
                        dst: t.to_string(),
                        disp: *s,
                    });
                    emitted.push(Instr::StoreCell {
                        disp: *d,
                        src: t.to_string(),
                    });
                }
            }
        }
    }
    // rax last
    if let Val::Slot(s) = final_rax {
        if let Some(&t) = tmp_of.get(&s) {
            emitted.push(Instr::RegMove {
                dst: "rax".to_string(),
                src: t.to_string(),
            });
        } else {
            emitted.push(Instr::LoadCell {
                dst: "rax".to_string(),
                disp: s,
            });
        }
    }
    if net_delta != 0 {
        emitted.push(Instr::AdjustDsp(net_delta));
    }
    Some(emitted)
}

/// Read-only hot-cell promotion (the "registers for hot values, no spills" pass).
/// Within an rbp-stable run, a data-stack cell that is read >= 2 times and never
/// written is loaded into a reserved register ONCE and every read is rewritten to
/// use it. No write-back and no liveness analysis: the cell's memory home is
/// never touched, so the register is purely a read cache that dies at the run's
/// end. The reserved pool (`r10`, `r11`) is disjoint from fusion's scratch, so
/// the two never collide; when it's exhausted we simply stop promoting.
///
/// Sound because a run has no `Raw` (no opaque aliasing) and no `AdjustDsp` (rbp
/// is fixed, so each `[rbp+disp]` is one stable cell); a read-only cell's value
/// is therefore constant across the run, and its memory copy stays authoritative.
fn promote_hot_cells(instrs: Vec<Instr>) -> Vec<Instr> {
    const PROMO_POOL: [&str; 2] = ["r10", "r11"];
    let mut out: Vec<Instr> = Vec::with_capacity(instrs.len());
    let mut i = 0;
    while i < instrs.len() {
        if matches!(instrs[i], Instr::Raw(_) | Instr::AdjustDsp(_)) {
            out.push(instrs[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < instrs.len() && !matches!(instrs[i], Instr::Raw(_) | Instr::AdjustDsp(_)) {
            i += 1;
        }
        out.extend(promote_run(&instrs[start..i], &PROMO_POOL));
    }
    out
}

/// Promote read-only, multiply-read cells in one rbp-stable run to the reserved
/// registers; returns the run unchanged when there is nothing worth promoting.
fn promote_run(run: &[Instr], pool: &[&str]) -> Vec<Instr> {
    use std::collections::HashMap;
    let mut reads: HashMap<i64, usize> = HashMap::new();
    let mut writes: HashMap<i64, usize> = HashMap::new();
    for ins in run {
        match ins {
            Instr::LoadCell { disp, .. } => *reads.entry(*disp).or_insert(0) += 1,
            Instr::StoreCell { disp, .. } => *writes.entry(*disp).or_insert(0) += 1,
            Instr::CellAlu {
                disp, cell_is_dest, ..
            } => {
                *reads.entry(*disp).or_insert(0) += 1;
                if *cell_is_dest {
                    *writes.entry(*disp).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }
    // candidates: read >= 2 times, never written -> read-only with a clear home
    let mut cands: Vec<i64> = reads
        .iter()
        .filter(|(d, &c)| c >= 2 && *writes.get(d).unwrap_or(&0) == 0)
        .map(|(d, _)| *d)
        .collect();
    if cands.is_empty() {
        return run.to_vec();
    }
    cands.sort_by(|a, b| reads[b].cmp(&reads[a]).then(a.cmp(b))); // hottest first
    cands.truncate(pool.len()); // out of registers -> stop promoting the rest
    let reg_of: HashMap<i64, &str> = cands
        .iter()
        .enumerate()
        .map(|(idx, d)| (*d, pool[idx]))
        .collect();

    let mut out: Vec<Instr> = Vec::with_capacity(run.len() + cands.len());
    // load each promoted cell once, up front (rbp is fixed; value is constant)
    for d in &cands {
        out.push(Instr::LoadCell {
            dst: reg_of[d].to_string(),
            disp: *d,
        });
    }
    for ins in run {
        match ins {
            Instr::LoadCell { dst, disp } if reg_of.contains_key(disp) => {
                out.push(Instr::RegMove {
                    dst: dst.clone(),
                    src: reg_of[disp].to_string(),
                });
            }
            Instr::CellAlu {
                mnem,
                reg,
                disp,
                cell_is_dest: false,
            } if reg_of.contains_key(disp) => {
                out.push(Instr::Raw(format!("    {mnem} {reg}, {}", reg_of[disp])));
            }
            other => out.push(other.clone()),
        }
    }
    out
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
                | Token::FpBin(_)
                | Token::FpNeg
                | Token::FpStack(_)
                | Token::FpMem(_)
                | Token::Call(_)
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

// ── Compile-time metrics (accumulated across every WF66 compile) ───────────
//
// The compiler keeps a running tally so a full test run reports min/max/avg per
// word. Cheap (one mutex lock per definition) and always on; `reset_metrics`
// before a measured run, `metrics()` after.

/// min / max / average of one per-word quantity.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    n: u64,
    min: u32,
    max: u32,
    sum: u64,
}

impl Stat {
    const fn new() -> Self {
        Self {
            n: 0,
            min: u32::MAX,
            max: 0,
            sum: 0,
        }
    }
    fn record(&mut self, v: u32) {
        self.n += 1;
        self.sum += v as u64;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
    }
    pub fn count(&self) -> u64 {
        self.n
    }
    pub fn min(&self) -> u32 {
        if self.n == 0 {
            0
        } else {
            self.min
        }
    }
    pub fn max(&self) -> u32 {
        self.max
    }
    pub fn avg(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum as f64 / self.n as f64
        }
    }
}

/// Per-word compile metrics, accumulated over a run.
#[derive(Clone, Copy, Debug)]
pub struct CompileMetrics {
    /// Definitions WF66 optimized (fully deferrable).
    pub deferrable: u64,
    /// Definitions that fell back to the eager compiler (not deferrable).
    pub non_deferrable: u64,
    /// Of the deferrable words, how many used a read-only promotion register.
    pub promoted: u64,
    /// Input token count per word.
    pub tokens: Stat,
    /// Emitted instruction count per word (excludes directives/labels).
    pub instrs: Stat,
    /// Machine-code body size in bytes per word.
    pub bytes: Stat,
    /// Data-stack memory accesses (`[rbp]` loads/stores + Fop cell operands).
    pub mem: Stat,
    /// GP registers allocated per word beyond rax/TOS (the spare + promotion pool).
    pub regs: Stat,
}

impl CompileMetrics {
    const fn new() -> Self {
        Self {
            deferrable: 0,
            non_deferrable: 0,
            promoted: 0,
            tokens: Stat::new(),
            instrs: Stat::new(),
            bytes: Stat::new(),
            mem: Stat::new(),
            regs: Stat::new(),
        }
    }
    /// Fraction of definitions WF66 optimized (vs eager fallback).
    pub fn coverage(&self) -> f64 {
        let total = self.deferrable + self.non_deferrable;
        if total == 0 {
            0.0
        } else {
            self.deferrable as f64 / total as f64
        }
    }
}

static METRICS: std::sync::Mutex<CompileMetrics> = std::sync::Mutex::new(CompileMetrics::new());

/// Snapshot the accumulated per-word compile metrics.
pub fn metrics() -> CompileMetrics {
    METRICS.lock().map(|m| *m).unwrap_or(CompileMetrics::new())
}

/// Reset the accumulated metrics (call before a measured run).
pub fn reset_metrics() {
    if let Ok(mut m) = METRICS.lock() {
        *m = CompileMetrics::new();
    }
}

/// True if `r` appears as a whole register token in `asm`.
fn reg_present(asm: &str, r: &str) -> bool {
    asm.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == r)
}

fn record_metrics(tokens: usize, instrs: &[Instr], asm: &str, byte_len: usize) {
    const SPARE: [&str; 8] = ["rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11"];
    let regs = SPARE.iter().filter(|r| reg_present(asm, r)).count() as u32;
    let promoted = reg_present(asm, "r10") || reg_present(asm, "r11");
    let instr_n = instrs
        .iter()
        .filter(|i| match i {
            Instr::Raw(l) => {
                let t = l.trim_start();
                !t.starts_with('.') && !t.ends_with(':')
            }
            _ => true,
        })
        .count() as u32;
    let mem = instrs
        .iter()
        .filter(|i| {
            matches!(
                i,
                Instr::LoadCell { .. } | Instr::StoreCell { .. } | Instr::CellAlu { .. }
            )
        })
        .count() as u32;
    if let Ok(mut m) = METRICS.lock() {
        m.deferrable += 1;
        if promoted {
            m.promoted += 1;
        }
        m.tokens.record(tokens as u32);
        m.instrs.record(instr_n);
        m.bytes.record(byte_len as u32);
        m.mem.record(mem);
        m.regs.record(regs);
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
        if let Ok(mut m) = METRICS.lock() {
            m.non_deferrable += 1;
        }
        return Err(CompileError::NotDeferrable);
    }
    let asm = compile_definition(tokens, "wf66_body").map_err(CompileError::Lower)?;
    // Deferred assembly: lex to instruction records, reduce, re-render.
    // fp_coalesce (cache the FP stack pointer in a register) -> coalesce rbp
    // adjusts -> fuse each window to its minimal parallel-move (auto-pick) ->
    // promote read-only hot cells to registers.
    let instrs =
        promote_hot_cells(window_fuse(coalesce_dsp(fp_coalesce(parse_instrs(&asm)))));
    let asm = render(&instrs);
    let module = wfasm::rasm::assemble(&asm).map_err(|e| CompileError::Assemble(format!("{e:#}")))?;
    let fn_off = *module
        .symbols
        .get("wf66_body")
        .ok_or(CompileError::MissingSymbol)?;
    let bytes = module.code[fn_off..].to_vec();
    record_metrics(tokens.len(), &instrs, &asm, bytes.len());
    Ok(bytes)
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
            // Fop forms: Sub lowers to `sub [rbp], rax` (cell-dest CellAlu);
            // And to `and rax, [rbp]` (cell-source). Exercise the round-trip.
            vec![Token::Lit(10), Token::Lit(3), Token::Inline(Fop::Sub)],
            vec![Token::Lit(12), Token::Lit(5), Token::Inline(Fop::And)],
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

    #[test]
    fn instr_structures_rbp_cells_and_leaves_program_mem_raw() {
        // `over` ( a b -- a b a ): spill TOS below, reload NOS -> both a
        // StoreCell and a LoadCell must appear (else 2.2 forwarding sees nothing).
        let asm = lower(&[Token::Stack(StackOp::Over)], "rt").unwrap();
        let instrs = parse_instrs(&asm);
        assert!(
            instrs.iter().any(|i| matches!(i, Instr::StoreCell { .. })),
            "over should structure a StoreCell: {instrs:?}"
        );
        assert!(
            instrs.iter().any(|i| matches!(i, Instr::LoadCell { .. })),
            "over should structure a LoadCell: {instrs:?}"
        );
        // `@` ( a -- *a ) touches program memory [rax], which must stay Raw —
        // never misread as a data-stack cell.
        let fetch = parse_instrs(&lower(&[Token::Mem(MemOp::Fetch)], "rt").unwrap());
        assert!(
            !fetch
                .iter()
                .any(|i| matches!(i, Instr::LoadCell { .. } | Instr::StoreCell { .. })),
            "[rax] program memory must stay Raw: {fetch:?}"
        );
    }

    #[test]
    #[ignore = "diagnostic: cargo test -- --ignored --nocapture wf66_before_after_asm"]
    fn wf66_before_after_asm() {
        // Before/after for longer words: the settle-everywhere lowering of the
        // RAW captured tokens (what the eager path emits per-token) vs the full
        // WF66 pipeline (token-reduce -> lower -> coalesce -> window-fuse).
        use CmpOp as C;
        use Ctl as K;
        use Fop::*;
        use StackOp::*;
        let cases: &[(&str, &str, Vec<Token>)] = &[
            (
                "poly",
                "( x -- 3x^2+2x+7 )   dup dup * 3 * swap 2 * + 7 +",
                vec![
                    Token::Stack(Dup),
                    Token::Stack(Dup),
                    Token::Inline(Mul),
                    Token::Lit(3),
                    Token::Inline(Mul),
                    Token::Stack(Swap),
                    Token::Lit(2),
                    Token::Inline(Mul),
                    Token::Inline(Add),
                    Token::Lit(7),
                    Token::Inline(Add),
                ],
            ),
            (
                "sumsq",
                "( a b -- a*a+b*b )   dup * swap dup * +",
                vec![
                    Token::Stack(Dup),
                    Token::Inline(Mul),
                    Token::Stack(Swap),
                    Token::Stack(Dup),
                    Token::Inline(Mul),
                    Token::Inline(Add),
                ],
            ),
            (
                "clamp0",
                "( n -- n|0 )   dup 0< if drop 0 then",
                vec![
                    Token::Stack(Dup),
                    Token::Cmp(C::ZeroLt),
                    Token::Ctl(K::If),
                    Token::Stack(Drop),
                    Token::Lit(0),
                    Token::Ctl(K::Then),
                ],
            ),
            (
                "countdown",
                "( n -- )   begin 1 - dup 0= until drop",
                vec![
                    Token::Ctl(K::Begin),
                    Token::Lit(1),
                    Token::Inline(Sub),
                    Token::Stack(Dup),
                    Token::Cmp(C::ZeroEq),
                    Token::Ctl(K::Until),
                    Token::Stack(Drop),
                ],
            ),
            (
                "rot rot",
                "( a b c -- c a b )   == -rot",
                vec![Token::Stack(Rot), Token::Stack(Rot)],
            ),
        ];

        fn body(asm: &str) -> Vec<String> {
            asm.lines()
                .map(|l| l.trim())
                .filter(|t| {
                    !(t.starts_with('.') || t.ends_with(':') || *t == "ret" || t.is_empty())
                })
                .map(|t| t.to_string())
                .collect()
        }
        let mem = |ls: &[String]| ls.iter().filter(|l| l.contains("[rbp")).count();

        for (name, eff, toks) in cases {
            let before = body(&lower(toks, "w").unwrap());
            let after = body(&render(&window_fuse(coalesce_dsp(parse_instrs(
                &lower(&reduce(toks), "w").unwrap(),
            )))));
            eprintln!("\n== {name}   {eff}");
            eprintln!(
                "   BEFORE  {} instrs, {} stack mem-accesses",
                before.len(),
                mem(&before)
            );
            for l in &before {
                eprintln!("       {l}");
            }
            eprintln!(
                "   AFTER   {} instrs, {} stack mem-accesses",
                after.len(),
                mem(&after)
            );
            for l in &after {
                eprintln!("       {l}");
            }
        }
        eprintln!();
    }

    #[test]
    #[ignore = "diagnostic: run with --ignored --nocapture to inspect 2.4 headroom"]
    fn wf66_traffic_probe() {
        // Diagnostic: the current optimized body for a few shuffle-chains, with
        // the count of data-stack memory accesses ([rbp]). Shows that post-
        // coalesce the window is pure offset addressing (auto-pick), and that
        // replaying each op's moves is redundant across chains (e.g. rot rot
        // replays 8 accesses for a permutation realizable in ~4).
        use StackOp::*;
        let cases: &[(&str, Vec<Token>)] = &[
            ("dup over", vec![Token::Stack(Dup), Token::Stack(Over)]),
            ("over over", vec![Token::Stack(Over), Token::Stack(Over)]),
            ("swap over", vec![Token::Stack(Swap), Token::Stack(Over)]),
            (
                "rot rot",
                vec![Token::Stack(Rot), Token::Stack(Rot)],
            ),
            (
                "2dup 2dup",
                vec![Token::Stack(TwoDup), Token::Stack(TwoDup)],
            ),
            (
                "swap swap",
                vec![Token::Stack(Swap), Token::Stack(Swap)],
            ),
            (
                "over swap drop",
                vec![Token::Stack(Over), Token::Stack(Swap), Token::Stack(Drop)],
            ),
        ];
        eprintln!("\n  --- 2.4 traffic probe (optimized body) ---");
        for (name, toks) in cases {
            let reduced = reduce(toks);
            let asm = lower(&reduced, "p").unwrap();
            let opt = window_fuse(coalesce_dsp(parse_instrs(&asm)));
            let mem = opt
                .iter()
                .filter(|i| {
                    matches!(i, Instr::LoadCell { .. } | Instr::StoreCell { .. })
                        || matches!(i, Instr::Raw(l) if l.contains("[rbp"))
                })
                .count();
            eprintln!("  {name:<16} data-stack mem-accesses = {mem}");
            for i in &opt {
                let line = render(std::slice::from_ref(i));
                let l = line.trim_end();
                let t = l.trim_start();
                if t.starts_with('.') || t == "p:" || t.contains("ret") || t.is_empty() {
                    continue; // skip preamble/label/ret noise
                }
                eprintln!("      {l}");
            }
        }
        eprintln!();
    }

    #[test]
    #[ignore = "diagnostic: cargo test -- --ignored --nocapture wf66_register_stats"]
    fn wf66_register_stats() {
        // How many GP registers WF66 allocates per word. rax is the permanent TOS
        // (always present); rcx/rdx/rsi/rdi/r8/r9 are fusion scratch; r10/r11 are
        // the read-only promotion pool. rbp (DSP) is not counted.
        use CmpOp as C;
        use Ctl as K;
        use Fop::*;
        use StackOp::*;
        let st = |o| Token::Stack(o);
        let corpus: Vec<(&str, Vec<Token>)> = vec![
            ("a b +", vec![Token::Inline(Add)]),
            ("dup *", vec![st(Dup), Token::Inline(Mul)]),
            ("over", vec![st(Over)]),
            ("swap", vec![st(Swap)]),
            ("nip", vec![st(Nip)]),
            ("tuck", vec![st(Tuck)]),
            ("rot", vec![st(Rot)]),
            ("-rot", vec![st(NegRot)]),
            ("over over", vec![st(Over), st(Over)]),
            ("swap over", vec![st(Swap), st(Over)]),
            ("rot rot", vec![st(Rot), st(Rot)]),
            ("2dup", vec![st(TwoDup)]),
            ("2dup 2dup", vec![st(TwoDup), st(TwoDup)]),
            ("2over", vec![st(TwoOver)]),
            ("2swap", vec![st(TwoSwap)]),
            ("@", vec![Token::Mem(MemOp::Fetch)]),
            ("!", vec![Token::Mem(MemOp::Store)]),
            (
                "poly",
                vec![
                    st(Dup), st(Dup), Token::Inline(Mul), Token::Lit(3), Token::Inline(Mul),
                    st(Swap), Token::Lit(2), Token::Inline(Mul), Token::Inline(Add),
                    Token::Lit(7), Token::Inline(Add),
                ],
            ),
            (
                "sumsq",
                vec![
                    st(Dup), Token::Inline(Mul), st(Swap), st(Dup), Token::Inline(Mul),
                    Token::Inline(Add),
                ],
            ),
            (
                "2pick + 2pick +",
                vec![
                    Token::Pick(2), Token::Inline(Add), Token::Pick(2), Token::Inline(Add),
                ],
            ),
            (
                "clamp0",
                vec![
                    st(Dup), Token::Cmp(C::ZeroLt), Token::Ctl(K::If), st(Drop), Token::Lit(0),
                    Token::Ctl(K::Then),
                ],
            ),
            (
                "countdown",
                vec![
                    Token::Ctl(K::Begin), Token::Lit(1), Token::Inline(Sub), st(Dup),
                    Token::Cmp(C::ZeroEq), Token::Ctl(K::Until), st(Drop),
                ],
            ),
        ];

        const REGS: [&str; 9] = ["rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11"];
        let has = |body: &str, r: &str| {
            body.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|tok| tok == r)
        };

        let mut sum_extra = 0usize;
        let mut max_extra = 0usize;
        let mut hist = [0usize; 9]; // extra regs (beyond rax) -> word count
        let mut promo_words = 0usize;
        let mut per_reg = [0usize; 9];

        eprintln!("\n  word                 #regs (excl rax)  registers used");
        for (name, toks) in &corpus {
            let body =
                render(&promote_hot_cells(window_fuse(coalesce_dsp(parse_instrs(
                    &lower(&reduce(toks), "w").unwrap(),
                )))));
            let used: Vec<&str> = REGS.iter().copied().filter(|r| has(&body, r)).collect();
            let extra = used.iter().filter(|r| **r != "rax").count();
            for (i, r) in REGS.iter().enumerate() {
                if used.contains(r) {
                    per_reg[i] += 1;
                }
            }
            if used.contains(&"r10") || used.contains(&"r11") {
                promo_words += 1;
            }
            sum_extra += extra;
            max_extra = max_extra.max(extra);
            hist[extra] += 1;
            eprintln!("  {name:<20} {extra:^16}  {}", used.join(" "));
        }

        let n = corpus.len();
        eprintln!("\n  --- summary over {n} words ---");
        eprintln!(
            "  avg extra regs/word = {:.2}   max = {max_extra}",
            sum_extra as f64 / n as f64
        );
        eprint!("  distribution (extra regs -> words): ");
        for (k, c) in hist.iter().enumerate() {
            if *c > 0 {
                eprint!("{k}:{c}  ");
            }
        }
        eprintln!();
        eprint!("  per-register use: ");
        for (i, r) in REGS.iter().enumerate() {
            if per_reg[i] > 0 {
                eprint!("{r}:{}  ", per_reg[i]);
            }
        }
        eprintln!("\n  words using a promotion reg (r10/r11) = {promo_words}\n");
    }

    // ---- rbp-coalescing (Step 2.3) --------------------------------------

    #[test]
    fn coalesce_cancels_paired_adjusts() {
        // push then pop within a window: sub rbp,8 / add rbp,8 cancel; the load's
        // [rbp] becomes [rbp-8] because rbp never physically moved.
        let buf = vec![
            Instr::StoreCell {
                disp: -8,
                src: "rax".into(),
            },
            Instr::AdjustDsp(-8),
            Instr::LoadCell {
                dst: "rax".into(),
                disp: 0,
            },
            Instr::AdjustDsp(8),
            Instr::Raw("    ret".into()),
        ];
        assert_eq!(
            coalesce_dsp(buf),
            vec![
                Instr::StoreCell {
                    disp: -8,
                    src: "rax".into()
                },
                Instr::LoadCell {
                    dst: "rax".into(),
                    disp: -8
                },
                Instr::Raw("    ret".into()),
            ]
        );
    }

    #[test]
    fn coalesce_flushes_before_a_raw_barrier() {
        // a Fop reading [rbp] must see rbp settled: the deferred adjust is emitted
        // before it, never deferred past it.
        let buf = vec![
            Instr::AdjustDsp(-8),
            Instr::Raw("    add rax, [rbp]".into()),
        ];
        assert_eq!(
            coalesce_dsp(buf),
            vec![
                Instr::AdjustDsp(-8),
                Instr::Raw("    add rax, [rbp]".into()),
            ]
        );
    }

    #[test]
    fn coalesce_merges_a_shuffle_run() {
        // two adjusts in a structured run collapse to one at the window edge.
        let buf = vec![
            Instr::StoreCell {
                disp: -8,
                src: "rax".into(),
            },
            Instr::AdjustDsp(-8),
            Instr::StoreCell {
                disp: -8,
                src: "rax".into(),
            },
            Instr::AdjustDsp(-8),
            Instr::Raw("    ret".into()),
        ];
        assert_eq!(
            coalesce_dsp(buf),
            vec![
                Instr::StoreCell {
                    disp: -8,
                    src: "rax".into()
                },
                Instr::StoreCell {
                    disp: -16,
                    src: "rax".into()
                },
                Instr::AdjustDsp(-16),
                Instr::Raw("    ret".into()),
            ]
        );
    }

    #[test]
    fn coalesce_then_fuse_shrinks_dup_over() {
        // composed: coalesce removes interior adjusts, fusion then realizes only
        // the changed cells -> fewer instrs and a single rbp adjust.
        let asm = lower(
            &[Token::Stack(StackOp::Dup), Token::Stack(StackOp::Over)],
            "rt",
        )
        .unwrap();
        let base = parse_instrs(&asm);
        let opt = window_fuse(coalesce_dsp(base.clone()));
        assert!(
            opt.len() < base.len(),
            "coalesce+fuse should shrink: {base:?} -> {opt:?}"
        );
        assert!(
            opt.iter()
                .filter(|i| matches!(i, Instr::AdjustDsp(_)))
                .count()
                <= 1,
            "at most one rbp adjust should remain: {opt:?}"
        );
        assert!(wfasm::rasm::assemble(&render(&opt)).is_ok());
    }

    // ---- floating point (settle-everywhere, mirrors kernel/float.masm) ---

    #[test]
    fn fp_bodies_are_deferrable_and_compile() {
        use FpOp::*;
        use FpStackOp::*;
        let bodies: Vec<Vec<Token>> = vec![
            vec![Token::FpBin(Add)],
            vec![Token::FpBin(Sub)],
            vec![Token::FpBin(Mul)],
            vec![Token::FpBin(Div)],
            vec![Token::FpNeg],
            vec![Token::FpStack(FDup)],
            vec![Token::FpStack(FDrop)],
            vec![Token::FpStack(FSwap)],
            vec![Token::FpStack(FOver)],
            vec![Token::FpMem(FpMemOp::FFetch)],
            vec![Token::FpMem(FpMemOp::FStore)],
            // leaf FP words a caller would write to get the optimizer's reach:
            vec![Token::FpBin(Mul), Token::FpBin(Add)], // f* f+
            vec![Token::FpStack(FOver), Token::FpBin(Mul), Token::FpBin(Add)],
            vec![Token::FpStack(FDup), Token::FpBin(Mul)], // square: fdup f*
        ];
        for b in &bodies {
            assert!(is_deferrable(b), "FP body should be deferrable: {b:?}");
            let bytes = compile_body_bytes(b);
            assert!(
                matches!(&bytes, Ok(v) if !v.is_empty()),
                "FP body failed to compile: {b:?} -> {:?}",
                bytes.err()
            );
        }
    }

    #[test]
    fn fp_settle_barrier_call_compiles() {
        use FpOp::*;
        use FpStackOp::*;
        // FP arithmetic followed by a settle-barrier call (like `... fsqrt`):
        // must be deferrable and assemble to a `movabs rcx, addr ; call rcx`.
        let toks = [
            Token::FpStack(FDup),
            Token::FpBin(Mul),
            Token::FpStack(FSwap),
            Token::FpStack(FDup),
            Token::FpBin(Mul),
            Token::FpBin(Add),
            Token::Call(0x2573_6920_0000),
        ];
        assert!(is_deferrable(&toks));
        let asm = lower(&toks, "rt").unwrap();
        assert!(asm.contains("movabs rcx, 41177615171584"));
        assert!(asm.contains("call rcx"));
        let bytes = compile_body_bytes(&toks);
        assert!(
            matches!(&bytes, Ok(v) if !v.is_empty()),
            "Call body failed to assemble: {:?}",
            bytes.err()
        );
    }

    #[test]
    fn fp_coalesce_removes_redundant_fsp_traffic() {
        // f+ f+ : op1's FSP store + op2's FSP load are redundant -> dropped, so
        // FSP is loaded once and stored once across the run.
        let asm = lower(&[Token::FpBin(FpOp::Add), Token::FpBin(FpOp::Add)], "rt").unwrap();
        let before = parse_instrs(&asm);
        let after = fp_coalesce(before.clone());
        let count = |is: &[Instr], pat: &str| {
            is.iter()
                .filter(|i| matches!(i, Instr::Raw(l) if l.contains(pat)))
                .count()
        };
        assert_eq!(count(&before, "mov rcx, [rbx + 4632]"), 2, "before: 2 FSP loads");
        assert_eq!(count(&after, "mov rcx, [rbx + 4632]"), 1, "after: 1 FSP load");
        assert_eq!(count(&after, "mov [rbx + 4632], rcx"), 1, "after: 1 FSP store");
        assert!(wfasm::rasm::assemble(&render(&after)).is_ok());
    }

    #[test]
    fn fp_add_matches_kernel_pattern() {
        // settle-everywhere f+ must match kernel/float.masm verbatim (FTOS=xmm15,
        // FSP at [rbx + 0x1218] = [rbx + 4632]).
        let asm = lower(&[Token::FpBin(FpOp::Add)], "rt").unwrap();
        for needle in [
            "mov rcx, [rbx + 4632]",
            "addsd xmm15, qword ptr [rcx]",
            "add rcx, 8",
            "mov [rbx + 4632], rcx",
        ] {
            assert!(asm.contains(needle), "f+ lowering missing {needle:?}:\n{asm}");
        }
    }

    // ---- window fusion / auto-pick (Step 2.4) ---------------------------

    /// Full deferred-assembly pipeline on a token body, as the compile path runs
    /// it: reduce -> lower -> parse -> coalesce -> window_fuse.
    fn fused(toks: &[Token]) -> Vec<Instr> {
        let asm = lower(&reduce(toks), "t").unwrap();
        window_fuse(coalesce_dsp(parse_instrs(&asm)))
    }

    fn mem_accesses(is: &[Instr]) -> usize {
        is.iter()
            .filter(|i| {
                matches!(i, Instr::LoadCell { .. } | Instr::StoreCell { .. })
                    || matches!(i, Instr::Raw(l) if l.contains("[rbp"))
            })
            .count()
    }

    #[test]
    fn fuse_collapses_rot_rot_to_negrot_cost() {
        use StackOp::*;
        // rot rot == -rot: a 3-cycle realizable in 4 mem-accesses, not 8 replayed.
        let rr = fused(&[Token::Stack(Rot), Token::Stack(Rot)]);
        assert_eq!(mem_accesses(&rr), 4, "rot rot should fuse to ~4: {rr:?}");
        // ...and it equals the cost of the single primitive -rot it's equivalent to.
        let nr = fused(&[Token::Stack(NegRot)]);
        assert_eq!(mem_accesses(&rr), mem_accesses(&nr));
        assert!(wfasm::rasm::assemble(&render(&rr)).is_ok());
    }

    #[test]
    fn fuse_drops_redundant_reload_in_dup_over() {
        use StackOp::*;
        // dup over reloads a cell rax already holds; fusion realizes only the two
        // changed cells (2 stores), no reload.
        let f = fused(&[Token::Stack(Dup), Token::Stack(Over)]);
        assert_eq!(mem_accesses(&f), 2, "dup over: {f:?}");
        assert!(wfasm::rasm::assemble(&render(&f)).is_ok());
    }

    #[test]
    fn fuse_swap_over_saves_an_access() {
        use StackOp::*;
        // swap over: 4 replayed accesses -> 3 (and rax is never pointlessly reloaded).
        let f = fused(&[Token::Stack(Swap), Token::Stack(Over)]);
        assert_eq!(mem_accesses(&f), 3, "swap over: {f:?}");
        assert!(wfasm::rasm::assemble(&render(&f)).is_ok());
    }

    #[test]
    fn fuse_preserves_a_push_spill() {
        // : w 5 ;  pushes 5: the spill of the old TOS to [rbp-8] must survive
        // even though the rbp adjust that makes it live sits past the `mov eax,5`
        // barrier (the no-liveness-filter soundness case).
        let f = fused(&[Token::Lit(5)]);
        let txt = render(&f);
        assert!(
            txt.contains("mov [rbp - 8], rax"),
            "push spill must not be dropped: {txt}"
        );
        assert!(wfasm::rasm::assemble(&txt).is_ok());
    }

    #[test]
    fn fuse_keeps_mem_store_live_out_scratch_verbatim() {
        use StackOp::*;
        // `!` loads NOS into rcx then `mov [rax], rcx` (a Raw reading rcx). The
        // window defining rcx must NOT be fused away (rcx is live into the barrier).
        let f = fused(&[Token::Stack(Over), Token::Mem(MemOp::Store)]);
        let txt = render(&f);
        assert!(txt.contains("mov [rax], rcx"), "store body intact: {txt}");
        // the rcx-defining load survives (coalesce may rebase its disp); it is not
        // fused away, because rcx is read by the following `mov [rax], rcx`.
        assert!(txt.contains("mov rcx, [rbp"), "rcx load preserved: {txt}");
        assert!(wfasm::rasm::assemble(&txt).is_ok());
    }

    #[test]
    fn fuse_promotes_multiply_read_slot_to_one_load() {
        use StackOp::*;
        // `2dup 2dup` reads the deep cell `a` twice and never writes it: it must
        // be loaded into a register once and reused, not re-read from memory.
        // (A memory read renders as an operand after a comma: `, [rbp`.)
        let txt = render(&fused(&[Token::Stack(TwoDup), Token::Stack(TwoDup)]));
        let reads = txt.matches(", [rbp").count();
        assert_eq!(reads, 1, "multiply-read slot should load once:\n{txt}");
        assert!(wfasm::rasm::assemble(&txt).is_ok());
    }

    #[test]
    fn fuse_reads_a_single_use_source_directly() {
        use StackOp::*;
        // `swap` reads NOS once -> no promotion needed, just the one direct read.
        let txt = render(&fused(&[Token::Stack(Swap)]));
        assert!(wfasm::rasm::assemble(&txt).is_ok());
        // rot rot (a permutation) keeps its 4 accesses — promotion must not
        // regress the already-minimal permutation realizer.
        let rr = fused(&[Token::Stack(Rot), Token::Stack(Rot)]);
        assert_eq!(mem_accesses(&rr), 4);
    }

    #[test]
    fn promote_caches_a_read_only_multiply_read_cell() {
        // `2 pick + 2 pick +` reads the same deep cell twice, once in each of two
        // fusion windows split by the `+` (a Fop), and never writes it. Fusion's
        // per-window pass can't see across the Fop; the block-level promoter loads
        // the cell once into a reserved register and reuses it, cutting reads.
        let toks = vec![
            Token::Pick(2),
            Token::Inline(Fop::Add),
            Token::Pick(2),
            Token::Inline(Fop::Add),
        ];
        let asm = lower(&reduce(&toks), "t").unwrap();
        let fused = window_fuse(coalesce_dsp(parse_instrs(&asm)));
        let promoted = promote_hot_cells(fused.clone());
        let before = render(&fused).matches(", [rbp").count();
        let after = render(&promoted).matches(", [rbp").count();
        assert!(
            after < before,
            "promotion should cut memory reads: {before} -> {after}\n{}",
            render(&promoted)
        );
        assert!(
            render(&promoted).contains("r10"),
            "a reserved register should be used:\n{}",
            render(&promoted)
        );
        assert!(wfasm::rasm::assemble(&render(&promoted)).is_ok());
    }

    #[test]
    fn promote_leaves_written_cells_alone() {
        // A cell that is written must NOT be promoted (read-only only): `swap`
        // writes its cells, so promotion is a no-op here.
        let asm = lower(&[Token::Stack(StackOp::Swap)], "t").unwrap();
        let fused = window_fuse(coalesce_dsp(parse_instrs(&asm)));
        assert_eq!(promote_hot_cells(fused.clone()), fused);
    }

    #[test]
    fn fuse_window_unknown_source_bails() {
        // a StoreCell whose src was never defined in the window -> keep verbatim.
        let win = [Instr::StoreCell {
            disp: -8,
            src: "rbx".into(),
        }];
        assert_eq!(fuse_window(&win, ""), None);
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
