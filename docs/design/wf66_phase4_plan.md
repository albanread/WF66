# WF66 Phase 4 — CFG + cross-block register allocation (implementation plan)

Status: **plan.** The VFX-class jump. Assumes Phases 0–3 done (token IR, straight-
line const-fold / strength-reduce / shuffle-elim, inlining via token-splice; JASM
/Rasm back end; Rust-side optimizer; WF65 as the differential state oracle). Extends
[`wf66_charter.md`](wf66_charter.md) §"Back End" and [`wf66_compiler.md`](wf66_compiler.md) §§3–5.

## 0. Where Phase 4 starts, what it adds

**Start (end of Phase 3):** per-definition token IR; straight-line spans
optimized; **control flow flushes to canonical state** — at every `IF`/`THEN`/
`BEGIN`/`DO` the abstract stack is materialized to the ABI (TOS in RAX, rest in
`[RBP]`) and a fresh span begins. Good straight-line code; still STC-class across
control flow.

**Add:** a real CFG, SSA across it, and a register allocator that keeps data-stack
values **in registers across branches and loops** — reconciling to memory only at
spills and word boundaries. That cross-block residency is the entire win.

## 1. The guiding invariant — settle-to-canonical is always the fallback

Phase 3's behavior — materialize the data stack to the canonical ABI at a
boundary — is the **safe baseline**. Phase 4 *optimizes the boundaries it can
handle and settles at the ones it can't.* Consequences:

- **Correctness is never worse than Phase 3.** Worst case at any boundary = a
  settle, i.e. exactly what ships today.
- **Every Phase-4 step is a strict widening** of "what stays in registers,"
  independently oracle-gated. You can stop after any step with a real, correct win.
- **Three boundaries always settle, non-negotiably:** definition exit (`;`),
  non-inlined calls, and opaque/dynamic regions (`EXECUTE`, raw poke, unknown
  immediate words). This is what keeps Forth's stack semantics and WF65
  observable-state equivalence intact.
- **Exceptions need no extra data-stack settling.** `CATCH` settles via `EXECUTE`
  and `THROW` restores the data-stack pointer from the catch frame, discarding
  register-resident values above it — so faulting fops are *not* settle boundaries
  for anonymous values. The one obligation: a **promoted named cell**
  (`VARIABLE`/`VALUE` — the subsumed `hotvariable`) is written back before any
  THROW-capable op in its range, so program-defined memory stays canonical on
  `THROW` (charter *Exceptions*).

## 2. IR additions (Rust-side)

- `BasicBlock { params: Vec<ValueId>, tokens: Vec<Token>, term: Terminator }`
- `Terminator = Fallthrough(bb) | Cond{val, taken, untaken} | LoopBack(bb) | Leave(target) | Call{xt, cont} | Opaque{effect, cont} | Return`
- `Value` = SSA value; producers are tokens; **phis** are join-block `params`.
- **`StackShape`** at each block boundary = `Vec<ValueId>` (depth + which value
  occupies each cell). Joins require equal depth → the **stack-balance check**;
  one phi per cell.
- Each `Inline(fop)` carries a **`RegEffect{ins, outs, clobbers}`** so the
  allocator schedules around it. This extends the charter's **Two Sources of
  Truth** contract: the fop template both emits bytes *and* declares its register
  interface, checked against the kernel `proc` body by a golden test.

## 3. Sub-phase 4a — CFG + SSA, **zero codegen change** (the safe substrate)

Build the CFG and lift to SSA; lower by settling at every boundary (Phase-3
behavior). Behavior-identical → the oracle passes trivially. This isolates the
substrate from the risk.

1. Add the CFG/SSA IR types (§2).
2. Reimplement the control-flow immediate words (`IF ELSE THEN BEGIN WHILE UNTIL
   AGAIN REPEAT DO ?DO LOOP +LOOP LEAVE EXIT`) to build blocks/edges and push
   **IR-level marks (block ids), not `HERE`**. The existing control-stack
   discipline (`mark`/`resolve`/`qpairs`) maps directly onto block ids — same
   structure, deferred target.
3. `stack-flow → SSA`: abstract-interpret each block's tokens (symbolic stack →
   SSA values; `swap`/`dup`/`over`/`rot` are renames); entry cells are block
   params; at joins insert phis and **require equal depth** (emit a clear compile
   error on mismatch — a new static bug-catch WF65 lacked).
4. Lower: each block settles to canonical at entry/exit (Phase-3 per-span
   lowering). **Registers do not cross blocks yet.**
5. **Gate:** the differential state fuzzer + the suite must match Phase 3 exactly
   — 4a is a pure refactor with zero codegen change, so output is identical to its
   own predecessor (a stronger self-check than the WF65 observable-state oracle).

**Deliverable:** CFG+SSA substrate, behavior-identical, plus the static stack-
balance check (already a user-visible correctness win).

## 4. Sub-phase 4b — cross-block register allocation (the value), widened in 3 steps

Replace settle-at-boundary lowering with an allocator, expanding scope
incrementally. Each step keeps strictly more values in registers; settle-to-
canonical everywhere it can't; **differential observable-state oracle gate each step.**

### 4b.1 — extended blocks (fallthrough chains, no joins)

- A value produced in block A and used in B across an A→B fallthrough stays in a
  register; don't settle at fallthrough edges.
- Linear-scan over the linear chain; spill to the value's data-stack slot under
  pressure. Settle still at joins, calls, opaque, exit.
- **Gate:** fuzzer green; straight-line corpus instruction count drops (Phase 3
  was settling unnecessarily).

### 4b.2 — diamonds (`if/then/else` joins, no loops)

- Allocate across cond-branches and joins. **Phi resolution:** if a value sits in
  different registers on the two predecessor edges, insert a parallel-copy (moves)
  on those edges — via a correct **parallel-copy sequencer** (handle swaps/cycles
  with a temp).
- **compare→branch fusion falls out:** a `Cond{val}` whose `val` is a compare
  lowers to `cmp; jcc` with no materialized boolean. (This session's hand-written
  fusion becomes a non-special codegen case.)
- Settle still at loops, calls, opaque, exit.
- **Gate:** fuzzer with random `if/then/else` nests (the value-oracle fuzzer
  already generates these).

### 4b.3 — loops (the hard one)

- Allocate across back-edges. **Loop-carried** data-stack values get a fixed
  register at the loop header that the back-edge restores (a move on the back-edge
  if needed); live ranges span the whole loop body.
- **The DO counter stays on the rstack** — we deliberately keep it in memory rather
  than pinning it; `i`/`j` read `[rsp]`/`[rsp+8]` as today. This also keeps `THROW`'s
  return-stack restore unchanged (charter *Exceptions*). The win is the *other*
  loop-carried cells — accumulators (the fib pair, mandelbrot `zx/zy`) — living in
  registers across iterations.
- **`hotvariable` pinning is subsumed:** a variable live across the loop just gets
  a register from the allocator. If it is a **named cell** (`VARIABLE`/`VALUE`), the
  allocator writes it back before any THROW-capable op in the loop so its memory is
  canonical on `THROW` (charter *Exceptions*); anonymous accumulators carry no such
  obligation.
- **Values that must survive a call inside the loop** go only in registers proven
  callee-preserved and free in compiled-body context. Candidate GP pair: R13/R14;
  floats: xmm6–15, mirroring the float-pin precedent (xmm6–9 held across
  `f*`/`f+` calls).
- **Gate:** explicit register-reservation audit for call-surviving values; tests
  with calls inside loops prove loop-carried values survive; fuzzer with random
  loops; the existing pin differential tests; the `hot-mandel-iter` /
  `fib-iter` / `dot-prod` benches show fewer instructions per iteration and stay
  correct.

## 5. Sub-phase 4c — SSA optimizations the residency now pays off

SSA→SSA passes, run before allocation, *enabled* after 4b works (they only
manifest once values stay in registers):

- **GVN/CSE** — common subexpressions, repeated address loads.
- **LICM** — hoist loop-invariant SSA to a preheader (now pays: the hoisted value
  stays in a register across iterations instead of being respilled).
- cross-block **const-prop through phis**; **DCE** of dead phis/values.
- Each gated; each strictly reduces work.

## 6. The cross-word ABI (the correctness spine)

- Between definitions: TOS in RAX, rest of the live stack in `[RBP]`, canonical
  depth (WF65 convention).
- **`Call` terminator (non-inlined):** spill live data-stack SSA values to their
  canonical `[RBP]` slots, TOS in RAX, `call`; on return the result is RAX (+
  `[RBP]`), reload as needed. Scratch is caller-clobbered anyway. Inlining (Phase
  3) avoids this for hot small callees, so the ABI cost only hits real (large /
  recursive) calls where it's negligible.
- **Definition exit (`;`):** settle to canonical — this is precisely what makes
  WF66's observable state (the data-stack region + depth) match WF65, which the
  oracle gate depends on.

## 7. Register budget & the fop register interface

- **GP:** RAX=TOS (at settled boundaries), RBP/RBX/RSP/R12/R15 reserved → ~10
  scratch (RCX RDX RSI RDI R8–R11 R13 R14) for data-stack values. R13/R14 become
  the callee-preserved-across-calls pair only after the 4b.3 register-reservation
  audit proves they are free in compiled-body context.
- **xmm:** 0–15; xmm6–15 callee-preserved for float values that survive calls (the
  `hot-fmandel` precedent).
- Each `Inline(fop)` declares `RegEffect`; the allocator schedules around its
  `clobbers`; the emit template uses the allocated regs. Golden test: template
  register effect == kernel `proc` body behavior.

## 8. Verification — the net that makes aggression safe

- **WF65 differential oracle** is the primary gate for *every* 4b step: same
  source → same **observable Forth state** (data stack + depth, program-defined
  memory, output). Scratch registers and spill memory are *not* compared — settle-
  at-exit makes the *observable* state exact while the allocator stays free inside.
  (Charter *Test Strategy* for the full contract.)
- **Pure allocator unit tests** — SSA in → allocation out; assert no live-range
  overlap on a register, correct phi resolution.
- **Parallel-copy sequencer property tests** — random permutations including
  cycles.
- **Value-oracle fuzzer** (random pure exprs) extended with random control flow
  (if / loop nests) for 4b, including calls inside loops for 4b.3.
- **Exception-path tests** — `CATCH`/`THROW`, division-by-zero, and faulting `@`/`!`
  inside register-allocated loops; assert the data stack restores correctly and any
  promoted named cell holds its canonical value after `THROW` (charter *Exceptions*).
- **Static stack-balance check** (from 4a) as a standing gate.
- **Bench metrics tripwire** — `hot-mandel-iter`, `fib-iter`, `dot-prod`: fewer
  instructions/iteration after 4b.3; a corpus-level regression warrants a look —
  not a per-word gate, codegen changes by design.

## 9. Milestones & exit criteria (each independently shippable)

| Step | Adds | Exit gate |
|---|---|---|
| **4a** | CFG + SSA, settle-at-boundary codegen, stack-balance check | fuzzer **byte-identical** to Phase 3 |
| **4b.1** | fallthrough residency | fuzzer green; straight-line instr-count ↓ |
| **4b.2** | diamond residency + phi resolution + compare-fusion | fuzzer green on if/then/else nests |
| **4b.3** | loop residency + loop-carried regs; hotvar subsumed | register audit; call-in-loop survival tests; fuzzer green on loops; hot-mandel/fib instr/iter ↓; pin tests pass |
| **4c** | LICM / CSE / GVN | each gated; bench wins, no regressions |

The settle-fallback guarantees ≥ Phase-3 correctness at every row, so you can stop
after any milestone with a real, correct improvement.

## 10. Risks & mitigations

- **Silent wrong-stack from allocation bugs** → the differential state fuzzer + WF65
  oracle is the net; settle-fallback bounds blast radius.
- **Phi swap/cycle bugs** → dedicated parallel-copy sequencer + property tests.
- **Loop-carried value across a call** → callee-preserved regs (R13/R14, xmm6–15),
  the float-pin precedent. *Verify early:* `get-started.md` notes some primitives use
  R13/R14 across Win64 callouts — the register-reservation audit (§7) must confirm
  they are free in compiled-body context before the 4b.3 budget commits to them.
- **Stale promoted variable after `THROW`** → write promoted named cells back before
  any THROW-capable op (charter *Exceptions*); the heuristic may decline promotion
  when the write-back cost outweighs the win.
- **Register pressure / spill thrash** → spill to the value's data-stack slot
  (cheap — it's where it'd live in the naive model); tune the heuristic.
- **`;` compile cost** → scope is one definition; linear-scan is fast; tier
  (emit competent-then-reoptimize) only if a pathological definition appears.
- **Complexity vs "hold in head"** → each sub-phase independently testable; the
  settle-fallback means you never *must* finish 4b.3 to ship 4b.1.

## 11. What it buys (concretely)

- Loop accumulators — the fib pair, mandelbrot `zx/zy` — live in registers across
  iterations: no per-iteration spill/reload. The VFX-class behavior.
- compare→branch fusion and TCO become free codegen properties, not peepholes.
- `hotvariable` pinning becomes automatic.
- LICM lifts invariant work out of loops.

Net: the **warm** band — Forth that's hot enough to matter but not worth hand-
`CODE:`, the band the optimizer actually exists for — approaches VFX. Hot kernels
stay `CODE:` MASM; cold glue stays competent; the warm middle is now register-
allocated across its control flow. That is the jump.
