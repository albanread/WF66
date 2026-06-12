# WF66 — the dual-level reducer (as built)

Status: **implemented.** This is the actual design of the WF66 optimizer, as it
ships. It supersedes the early (deleted) charter/compiler/roadmap plans, which
proposed a CFG + stack-flow→SSA + cross-block register allocator. WF66 took a
different, lighter path that got most of the win for a fraction of the machinery:

> **One idea, applied at two levels: recognize a pattern, replace it with a
> cheaper equivalent that computes the same thing, repeat to a fixpoint.**

There is no SSA form, no CFG dataflow, and no register allocator. Both levels are
pattern-driven rewriters (a *reducer* each), and the whole optimizer is a pure
function `tokens -> bytes` that runs Rust-side. All code references below are in
[`src/wf66/mod.rs`](../../src/wf66/mod.rs) unless noted.

```
Forth source
   │  outer interpreter drives the IR builder (capture; see §1)
   ▼
[Token]                                    ── per-definition token IR
   │  reduce()            ← LEVEL 1: token-IR reducer (Forth level, §2)
   ▼
[Token]  (reduced)
   │  fold_fp_abs_mem() ; lower()          ── tokens → MC-flavour Intel asm text
   ▼
asm text
   │  parse_instrs()                       ── lex asm into instruction records
   ▼
[Instr]
   │  fp_coalesce → coalesce_dsp →         ← LEVEL 2: instruction-buffer reducer
   │  window_fuse → promote_hot_cells         (machine level, §3)
   ▼
[Instr]  (reduced)
   │  render() ; wfasm::rasm::assemble()   ── re-render → native encoder
   ▼
machine code bytes  → patched over the eagerly-compiled body
```

The two entry points: `compile_definition()` runs level 1 + lowering;
`compile_body_bytes()` runs the whole pipeline including level 2.

---

## 1. Capture — how the token IR is built

WF66 does not parse Forth. The existing WF65 **outer interpreter** drives an IR
builder: as it compiles a `:`…`;` body, kernel convergence hooks call into the
Rust recorder (`src/runtime.rs`) instead of (well, *alongside*) emitting final
bytes. The eager WF65 compile still runs underneath, so there is always a correct
body to fall back to.

- `rt_ir_begin` at `:` starts a span; `rt_ir_finalize` at `;` ends it.
- Each compiled word reaches the recorder by **xt** (`rt_ir_word`): a known
  arithmetic/stack/control primitive becomes a structured `Token`; a literal
  becomes `Lit` (`rt_ir_lit`) or `FpLit` (`rt_ir_flit`); a `variable` reference
  becomes a literal address; a *known* word (libm, or another WF66-optimized
  word) becomes a settle-barrier `Call`; anything unrecognized **taints** the
  span (`rt_ir_taint` → an `Opaque` token).
- Inline-emitted things that bypass the convergence point are hooked directly:
  local fetches/stores (`rt_ir_local_fetch`/`_store`/`_ffetch`/`_fstore`) and the
  `{:` prologue (`rt_ir_open_locals`).

The result is a `Vec<Token>` (the `Token` enum). `is_deferrable()` decides if the
span is fully modelled; if any `Opaque` (or other non-Phase-0 token) is present,
`compile_body_bytes` returns `NotDeferrable` and the eager body stands. This is
the **two-sources-of-truth** safety net: WF66 only ever *replaces* a body it fully
understands.

---

## 2. Level 1 — the token-IR reducer (Forth level)

`reduce(tokens)` is a **shift-reduce fixpoint engine**. It shifts each token onto
an output vector and then reduces the *tail* (`reduce_tail`) until no rule fires:

```rust
for &t in tokens { out.push(t); reduce_tail(&mut out); }
```

Because every reduction only *shortens* the tail and the new tail is immediately
re-checked, a single forward sweep reaches a fixpoint — including cascades a
fixed-order pipeline would miss (`7 + 3 + → +10`, `1+ 1+ 1+ 1+ → +4`,
`2 dup * → 4`). Whole-definition visibility is what makes this legal: the entire
body is present as data, so there are no rewind fences or "have we seen enough
yet" heuristics.

`reduce_tail` applies two windows:

1. **Window-3 whole-span constant fold:** `Lit a, Lit b, Inline(op)` → `Lit(a op b)`.
2. **Window-2 rule table** (`reduce_pair`) — the catalog of "common idiom → cheaper
   equivalent". Each arm returns 0, 1, or 2 replacement tokens:

| Pattern | → | Why |
|---|---|---|
| `Lit\|Dup\|Over , Drop` | *(nothing)* | DCE: side-effect-free producer then drop |
| `Drop Drop` | `2drop` | one op instead of two |
| `Swap Drop` | `Nip` | common idiom → rarer single op |
| `Over Over` | `2dup` | likewise |
| `Swap Swap` | *(nothing)* | identity |
| `Lit 0 , Cmp c` | `Cmp c.zero_form()` | `0<` etc. — then fuses with `if`/`until` |
| `Lit 0\|1 , pick` | `Dup`/`Over` | constant pick is dup/over |
| `Lit k , pick` | `Pick(k)` | direct cell copy (if disp fits imm32) |
| `Lit k , Inline op` | `ImmOp{op,k}` | register-immediate op (if k fits imm32) |
| `Dup , Inline op` | `DupOp(op)` | self-combining op (`a+a`, `a*a`) |
| `Cmp c , if/until/while` | `CmpCtl(c, ctl)` | branch off the operand — no materialized flag |
| `Lit k , DupOp op` | `Lit(op.eval(k,k))` | const through a self-op |
| `Lit k , ImmOp{op,j}` | `Lit(op.eval(k,j))` | const through an immediate op |
| `ImmOp{o,k1} , ImmOp{o,k2}` | `ImmOp{o, k1∘k2}` | merge same-op immediates (`combine_imm`, if it still fits imm32) |

The catalog is **ordered by a frequency miner** (`cargo run --bin seq_freq` over
real Forth + the kernel MASM) so the hottest sequences reduce first.

After `reduce`, `fold_fp_abs_mem` does one more peephole: `Lit(addr) FpMem(f@/f!)`
→ `FpFetchAbs`/`FpStoreAbs` (an `fvariable` access becomes a direct absolute FP
load/store, no data-stack traffic for the address).

`lower()` then turns the reduced token stream into MC-flavour Intel asm **text**,
under the settle-everywhere ABI (§4). Control words (`If/Else/Then`,
`Begin/Until/Again/While/Repeat`, `Exit`) lower to label/branch text against a
small control stack; `CmpCtl` lowers a compare directly into a conditional jump.

---

## 3. Level 2 — the deferred-assembly instruction-buffer reducer (machine level)

"We have our own assembler, so we defer assembly." Instead of handing `lower()`'s
text straight to the encoder, WF66 lexes it into a buffer of **instruction
records** and reduces *those* with the same recognize→replace philosophy, then
re-renders. The buffer's contract is an identity round-trip:
`render(parse_instrs(x)) == x` — introducing the buffer changes nothing until a
pass is added.

`parse_instrs()` recognizes only what the passes need; everything else rides as
`Raw` (verbatim) and is promoted to a structured variant only as a pass requires:

```rust
enum Instr {
    AdjustDsp(i64),                                   // add/sub rbp, n  — the DSP adjust
    LoadCell  { dst, disp },                          // mov dst, [rbp+disp]
    StoreCell { disp, src },                          // mov [rbp+disp], src
    RegMove   { dst, src },                           // mov dst, src  (rbp-independent)
    CellAlu   { mnem, reg, disp, cell_is_dest },      // a Fop combining TOS with [rbp+disp]
    Raw(String),                                      // anything else, verbatim
}
```

`Raw` lines — labels, jumps, calls, `ret`, program-memory `[rax]` access, the FP
preamble — are **barriers**: passes never reorder across them or assume anything
about state through them. A maximal run with no `Raw` and no `AdjustDsp` is a
**barrier-free window**: rbp is fixed, so every `[rbp+disp]` names one stable
cell. The passes run in this order:

### 3a. `fp_coalesce` — cache the FP stack pointer across a run
Each FP op in `lower()` brackets its work with `mov rcx,[rbx+FSP]` … `mov [rbx+FSP],rcx`.
Adjacent ops therefore store-then-reload the same pointer. `fp_coalesce` drops the
redundant `store; load` pair so `rcx` holds `user_FSP` across a whole FP run —
the kernel reloads it from memory every op; WF66 keeps it in a register.

### 3b. `coalesce_dsp` — defer the rbp adjusts to each window edge
Walks the buffer carrying a running `delta`. Each `AdjustDsp(n)` is *deferred*
(`delta += n`); each cell access (`LoadCell`/`StoreCell`/`CellAlu`) has its disp
rewritten by `delta`; at every `Raw` barrier the deferred adjust is *flushed*
(`add rbp, delta`). The N interspersed `add/sub rbp` of a shuffle run collapse to
one adjust at the edge — or vanish when they cancel.
**Sound** because nothing observes rbp mid-defer: every instruction that depends
on rbp's logical position (a Fop reading `[rbp]`, a jump, a label, a call, `ret`)
is a `Raw` line, and `flushed + delta` always equals the total adjust so the
rewritten address is the original one. Removing the interior adjusts also turns
relative cell accesses into absolute ones, exposing more redundant reloads for the
next pass.

### 3c. `window_fuse` — "auto-pick instead of stack ops"
The heart of the optimizer. After coalescing, a barrier-free window is pure
fixed-offset slot addressing. Instead of *replaying* each stack op's moves,
`window_fuse` symbolically simulates the whole window to its **net map** — final
`rax` and each changed slot expressed as `Val::{Rax, Slot(i)}` of the window's
*entry* state — then emits the **minimal parallel move** that realizes that map.
A pure-movement window never synthesizes values, only shuffles them, so the net
permutation/duplication is all that matters. `rot rot` (8 memory accesses)
collapses to its net `-rot` (4); redundant reloads disappear; nothing is
replayed. Fusion uses a scratch pool (`rsi rdi r8 r9 rcx rdx`) for the parallel
move and a live-out guard (`reg_family`) so it never clobbers a sub-register the
following barrier reads.

### 3d. `promote_hot_cells` — registers for hot read-only values, no spills
Within an rbp-stable run, a data-stack cell **read ≥ 2× and never written** is
loaded into a reserved register **once** and every read rewritten to use it. No
write-back, no liveness analysis: the cell's memory home is never touched, so the
register is a pure read-cache that dies at the run's end. The reserved pool
(`r10`, `r11`) is disjoint from fusion's scratch; when it's exhausted, promotion
simply stops. **Sound** because a run has no `Raw` (no opaque aliasing) and no
`AdjustDsp` (rbp fixed), so a read-only cell is constant across the run.

`render()` turns the reduced buffer back into asm text; `wfasm::rasm::assemble`
(the same native encoder the LET path uses) produces the bytes, which are patched
over the eager body.

---

## 4. What makes the windows possible — settle-everywhere ABI

Inherited from WF65: **TOS in `rax`**, the rest of the data stack in memory at
`[rbp]` (grows down by 8); FTOS in `xmm15`, the rest at `user_FSP`. State is
canonical (settled) at **every call, control edge, and `;`**. That invariant is
exactly what lets level 2 treat each `Raw` barrier as a clean boundary and each
in-between region as a self-contained window it can rewrite freely. It's also why
a **settle-barrier call** needs no stack-effect analysis: settle, `call`, resume —
the callee preserves every Forth invariant, and the optimizer keeps optimizing the
windows around it.

Spare registers: `rsi rdi r8 r9 rcx rdx` are parallel-move temporaries; `rcx`
additionally caches `user_FSP` across an FP run; `r10`/`r11` are the read-only
promotion pool; `r15` is the locals pointer (LP).

---

## 5. Why two levels

Each level catches what the other structurally cannot:

- **Token level** sees Forth *meaning*: that `dup *` is a self-multiply, that
  `7 + 3 +` is one add, that `0< if` is a branch with no flag. These are
  algebraic identities over Forth operations — invisible once lowered to moves.
- **Instruction level** sees *machine* redundancy that only exists after lowering:
  that a shuffle run's rbp adjusts cancel, that `rot rot` is a net `-rot`, that an
  FP pointer needn't be reloaded every op, that a slot read twice can live in a
  register. The token IR has no notion of `[rbp+disp]` or `rcx`, so it can't.

Running both, each to a fixpoint, is the whole optimizer.

---

## 6. Reach: beyond the leaf

Three things keep a call from tainting its caller, so optimization reaches past a
single leaf word:

- **Settle-barrier calls** to known words (libm, or any WF66-optimized word).
- **Variables as literal address pushes**, not calls (`create`'s stub bakes the
  body address in as `mov rax, imm64`; the recorder reads it back).
- **Inlining**: a WF66-optimized word is a leaf by construction, so a caller
  splices its token body (`splice`) and the spliced tokens join the caller's
  reduction — folding across the former call boundary. Locals words splice as a
  balanced `[OpenLocals … CloseLocals]` nested frame.

---

## 7. Correctness

WF65 is the **differential oracle**: identical source must produce identical
*observable Forth state* (data stack, program memory, output), even though WF66
emits different, faster bytes. It is a semantic cross-check, not a byte spec.
Verified continuously by: a 600-program differential fuzzer; the ANS Forth-2012
core suite run both eager *and* WF66-enabled; FP/libm/variable/locals/inlining
differential tests; and focused unit tests for every reduction rule and buffer
pass (the `render(parse(x)) == x` identity, each pass in isolation, and the full
`compile_body_bytes` path).

---

## 8. Status

The optimizer is **feature-complete** (v0.2.0). It lands ~3.7× faster than the
eager STC baseline and ~2.6× off hand-written MASM on the Mandelbrot inner loop.
The one lever left unused — promoting loop-carried locals into registers *across*
a loop back-edge (the `begin` back-edge is a `Raw` barrier, so `promote_hot_cells`
resets per iteration) — is a deliberate stopping point, not a deficiency. Any
future optimization should be driven by a real corpus to analyze, not speculative
passes against micro-benchmarks.
