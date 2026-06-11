# WF66 Roadmap — phases & sprints

Status: **plan.** The single authoritative phase/sprint list for the WF66 compiler
rewrite. The *why* is [`wf66_charter.md`](wf66_charter.md); the *architecture* is
[`wf66_compiler.md`](wf66_compiler.md); Phase 4 is expanded in
[`wf66_phase4_plan.md`](wf66_phase4_plan.md). When those and this disagree, **this
file owns the sequencing** — keep it the only place phases are numbered.

## How to read this

- **Phase** = a shippable capability level. **Sprint** = a numbered task inside a
  phase, independently landable and gated.
- **Every sprint is gated by the oracle contract** (charter *Test Strategy*):
  observable Forth state matches WF65; scratch registers / spill memory /
  instruction count are *not* compared. WF66 may emit different, faster bytes.
- **Settle-to-canonical is always the fallback.** Until a sprint teaches the
  back end to keep values in registers across a boundary, it materialises the
  data stack to the WF65 ABI there. So every phase is correct by construction and
  ≥ the previous one.
- Phases 0–3 build the per-definition token IR and optimise straight-line code;
  **Phase 4 is the cross-block register-allocation jump** (the VFX-class step);
  Phase 5 finishes locals/floats.

---

## Phase 0 — Token IR + capture + const-fold + lower

Prove the three-stage split (capture → optimise → lower) end-to-end on
`Lit` + arithmetic; everything else flushes to canonical state.

- **0.1** IR-builder object on the compiling state; capture hook at the
  interpreter's convergence point appends typed tokens instead of emitting bytes.
- **0.2** Redirect `LITERAL`, `COMPILE,`, `POSTPONE`, `;` to the builder /
  finalizer. `CODE:` and raw byte pokes → opaque nodes.
- **0.3** const-fold (`Lit … Inline(arith)` → `Lit`); back end lowers a closed
  definition via JASM/Rasm.

**Exit:** `5 7 +` → `Lit 12`; `: bar 5 * 2 + ;` lowers; suite + value-oracle
fuzzer at WF65 parity.

## Phase 1 — straight-line scheduler

Optimise the deferrable span fully.

- **1.1** `dup/drop/swap/over/rot` as SSA renames; the symbolic-stack scheduler
  (per-span register window) deciding registers/memory with full-span lookahead.
- **1.2** strength-reduce annotations (`* 2`→`add`, `* 2ⁿ`→`shl`, `* 3/5/9`→`lea`,
  `*0/1/-1`).
- **1.3** DCE (value pushed then dropped before any settle emits nothing).

**Exit:** straight-line corpus instruction count down vs Phase 0; fuzzer green.

## Phase 2 — memory ops, whole-span

- **2.1** `@`/`!`/`c@`/`c!` tokens + the memory-effect ordering barrier, decided
  over the whole span.
- **2.2** two-literal store (`42 var !` → `mov [addr], 42`) and compile-time `/`,
  which fall out once the whole token array is visible.

**Exit:** memory ops correct and folded; differential state fuzzer (with memory
writes) green.

## Phase 3 — inlining via token-splice

- **3.1** keep small word *bodies* as token sequences; splice a callee's tokens
  into the caller below a size/all-deferrable threshold (recursive/large stay a
  `Word` call).
- **3.2** re-run const-fold / strength-reduce across the former call boundary.

**Exit:** `bl +` → `add rax, 32` across the call; `optinline.fs`-class cross-word
fold; fuzzer green. *(With Phases 0–2 this completes the **peephole subsumption**:
the WF65 peephole/replay layer — `try_fold_literal`, the `LAST_*` tails, `OPT_FENCE`,
the `bl`/`true`/`false` ordering tricks — is now reimplemented as whole-definition IR
passes and removed wholesale; WF66 runs no peephole pass alongside the optimizer.
See charter *Replaced*.)*

## Phase 4 — CFG + cross-block register allocation  *(the performance jump)*

Full detail and gates in **[wf66_phase4_plan.md](wf66_phase4_plan.md)**. Sprints:

- **4a** structured control flow builds a CFG (blocks/edges, IR-level marks);
  stack-flow → SSA with phis at joins; static stack-balance check. **Codegen
  unchanged (settle at every boundary) → a pure refactor, byte-identical
  observable state.**
- **4b.1** keep values in registers across fallthrough chains.
- **4b.2** across `if/then/else` joins (phi resolution / parallel-copy);
  compare→branch fusion falls out.
- **4b.3** across loops: loop-carried accumulators live in registers;
  `hotvariable` pinning subsumed; call-surviving values in audited
  callee-preserved registers.
- **4c** LICM + CSE/GVN over the CFG.

**Exit:** `hot-mandel-iter` / `fib-iter` / `dot-prod` fewer instructions per
iteration; fuzzer green on random control-flow nests; pin differential tests pass.

## Phase 5 — locals & floats

- **5.1** locals (`r15` frame) as IR values, allocated like data-stack values.
- **5.2** float values get xmm allocation across loops (the `hot-fmandel` win,
  now automatic — no manual float pinning).

**Exit:** locals/floats optimised; float pin tests subsumed; `hot-fmandel`
instruction-per-iteration down.

---

## Stop-anywhere property

Because settle-to-canonical is the fallback and every sprint is oracle-gated,
**the project is shippable after any sprint** — each one is a strict, correct
improvement over the last. Phases 0–3 already beat WF65 on straight-line and
cross-word code; Phase 4 is where WF66 stands with VFX; Phase 5 closes the
numeric/locals gap. None of it is load-bearing for correctness — only for speed.
