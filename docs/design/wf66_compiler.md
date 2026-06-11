# WF66 Compiler — front-end, IR, cross-block optimizer, JASM/Rasm codegen

Status: **design.** Extends [`wf66_charter.md`](wf66_charter.md). The charter fixes
the outer-interpreter/IR-builder contract, Rust-side optimizer decision,
per-definition scope, and WF65-as-oracle. This document fills in the full
front-end, CFG, optimizer, register-allocation, and codegen architecture:

1. **Codegen: native Rasm only.** Lowering is IR -> JASM asm -> **Rasm native
  encoder** -> bytes in the RWX arena. WF66 has one assembler/loader path.
2. **Control flow is first-class IR (a CFG), not a permanent span-flush boundary.**
   This is the change that lets WF66 keep the data stack *in registers across
   `if`/`then`/loops* — the single thing that separates it from every STC Forth,
  any settle-everywhere baseline.

The mandate: a front-end Forth-IR compiler that **preserves full Forth
semantics** (`create`/`does>`, user-defined immediate words, interactive
redefinition) and **out-performs the field** (STC Forths, Factor, VFX/SwiftForth).

---

## 0. The performance thesis — how WF66 beats the field

Three factors *multiplied*. No competitor has all three:

1. **Perfect impedance match.** Every primitive lowers to its native instruction
   — `@`→`mov`, `<`→flags, `>r`→`push`, floats→xmm — never boxed, never a VM
   call boundary. (This is what beats Factor/NewFactor; cf. the mandelbrot
   result — boxed floats + GC vs native xmm.)
2. **Per-definition cross-block register allocation.** The data stack lives in
   registers *across* `if`/`then`/`begin`/`do`, reconciled to memory only at
   spills and word boundaries. This is the category STC peephole — WF65, WF32,
  and any settle-everywhere baseline — structurally **cannot** reach.
3. **Inline-and-fold across word boundaries.** Small words splice their token
   body into the caller; const-fold / strength-reduce / CSE then run across the
   former call (the `optinline.fs`/`nseopt` generality, now whole-definition).

Plus the moat nobody else has: **a complete, frozen predecessor (WF65) as a
byte-for-byte state correctness oracle.** You can optimize aggressively because
the fuzzer proves final stack, touched memory, `rbp`, and scratch state did not
change (§9), even when generated code bytes do. Hand-tuned commercial optimizers
are least trusted exactly here.

**Honest ceiling:** per-definition, not whole-program. Forth's mutable,
incremental dictionary and runtime `does>` patching forbid whole-program
analysis — but VFX shares that ceiling, so it is competitive at the high bar,
not a concession below it.

---

## 1. The load-bearing constraint — the compiler is user-programmable

Immediate words and `create`/`does>` run user code *at compile time* that
participates in compilation. There is no offline parse. The resolution (from the
charter, made precise here):

- **The outer interpreter is unchanged**; it drives an **IR builder** instead of
  emitting bytes.
- **The compile-time vocabulary *is* the IR-builder API.** `literal`,
  `compile,`, `postpone`, `[char]`, and the control-flow markers are redefined to
  construct IR nodes. Any user immediate word written against them participates
  and is optimized for free — `if`/`then`/`do` are just library immediate words
  over that API.
- **Three classes of compile-time word, three treatments:**

  | class | examples | treatment |
  |---|---|---|
  | **(a) deferrable** | `lit`, `+ - * / and @ ! dup swap …` | append a token to the current block |
  | **(b) structured control** | `if then else begin while until do loop +loop leave exit` | **build CFG blocks/edges in the IR** — push an *IR-level* mark, not a `HERE` address. Resolved at codegen, after regalloc. |
  | **(c) opaque / dynamic** | `execute`, runtime xt, raw `here c,` poking, `postpone` of an unknown, any user immediate word that escapes the vocabulary | **flush** the live region to canonical stack state, emit an **opaque node** with a declared/conservative effect, resume fresh |

The crux is **(b)**: in WF65's eager model, `if` reads `HERE` and emits a `jz`, so
it *had* to be a flush boundary. In WF66 it reads the IR builder's control stack
and appends a branch node — so structured control flow is **first-class IR**.
Only genuinely-opaque compile-time code (c) needs the charter's flush. That
distinction is the entire performance unlock.

**`create`/`does>`** needs no new mechanism: the `compiles-me` hook installs an
IR-builder on children. A `constant` child emits a `Lit`; a `does>` word emits
*push-body* + inline-or-call the does-body. Folding falls out structurally.

---

## 2. The IR — two levels

- **Token spans** (charter's IR): per-basic-block arrays of typed tokens —
  `Lit(i64)`, `FLit(bits)`, `Local(off,rw)`, `Word(xt,flags)`, `Inline(fop)`,
  `Body(addr)` (a `does>`/var address push). The unit of straight-line
  optimization.
- **CFG** (new): basic blocks (token spans) joined by typed edges —
  `fallthrough`, `cond(value)`, `loop-back`, `leave`/`exit`. Built by the
  class-(b) words. The live data stack at each block boundary is a vector of SSA
  values (block parameters); joins carry **phis**.

`stack-flow` lifts each block's token span to SSA values and the CFG carries them
across blocks. After stack-flow, *within a definition the stack is gone* — it is
ordinary SSA, and every standard optimization applies.

---

## 3. stack-flow → SSA (generalizes the v2 StackCache to the CFG)

- **Intra-block:** the v2 `StackCache` / symbolic stack — each push is a fresh
  SSA value; `swap`/`dup`/`over`/`rot` become **renames** (zero cost);
  `force_settle` is the lowering postcondition. Reuse v2 wholesale here.
- **Inter-block:** at a join, reconcile incoming stacks — they must have **equal
  depth** (the stack-balance check, a real bug-catcher, §9) → insert phis. At a
  loop header, fix a register convention for loop-carried values so the back-edge
  doesn't reshuffle.
- **Dynamic boundary (class c):** `force_settle` to canonical memory state,
  opaque node, resume. The escape hatch; correctness over speed.

---

## 4. Optimizer — pure `IR → IR`, whole-definition, unit-tested

The charter's pass (b), now over the whole CFG instead of one span:

- **const-fold / propagate** — `5 7 +` → `Lit 12`; through phis when both arms
  are constant. Two-literal stores and compile-time `/` fall out (the whole CFG
  is visible; the one-slot watermark ceiling is gone).
- **strength-reduce** — `* 2`→`add`, `* 2ⁿ`→`shl`, `* 3/5/9`→`lea`, `*0`→`xor`,
  `*1`→nop, `*-1`→`neg` (WF32's `imul-immed`, as an SSA rewrite).
- **inline** — splice a small `Word`'s token body in, then re-fold across the
  boundary. Heuristic: token-count threshold or all-deferrable body; recursive /
  large stay `Word` (a call).
- **shuffle elimination** — `swap dup over rot nip tuck` vanish; they are SSA
  renames.
- **CSE / GVN** within the definition — repeated address loads, common
  subexpressions.
- **DCE** — push-then-drop, dead phis, unused values.
- **LICM** — hoist loop-invariant SSA out of loop bodies. *VFX-class; impossible
  under a permanent span-flush model because it has no loop CFG.*
- **compare→branch fusion** — **falls out for free**: a compare SSA value feeding
  a `cond` edge lowers to `cmp; jcc`, no materialized boolean. (This session's
  hand-written fusion becomes a non-special codegen case.)
- **TCO** — a `Word` on a tail edge lowers to `jmp`.

---

## 5. Register allocation — the VFX-beating core

The thing STC structurally cannot do. Linear-scan (or SSA-coloring) over the
whole-definition CFG; live ranges cross blocks and loops.

- **Interior:** values live in the GP scratch pool / xmm (floats); the allocator
  is free. **No data-stack memory traffic** for a value that stays in a register
  across its live range — including across `if`/`then` and around loops.
- **The data stack in memory** is the **spill space** *and* the **cross-word
  ABI**.
- **Cross-word ABI** (the calling convention between definitions): TOS in `RAX`
  (WF65 convention), the rest of the live data stack in memory at `[RBP]`. At a
  non-inlined `Word` call: reconcile — spill live SSA values to the data stack in
  canonical order, `call`, reload the result. **Inlining avoids this for hot
  callees.** So Forth's stack semantics hold *exactly* at every word boundary
  while the interior is register-allocated. `r15`=LP, `rsp`=rstack, `rbx`=UP
  unchanged.
- **`hotvariable` pinning is subsumed**: a variable whose live range spans a loop
  simply gets a register from the allocator. The manual pin machinery
  (`register_pinning_v1`) folds into general allocation — or remains a hint.

This is the headline: VFX-class allocation, the difference vs every STC Forth.

---

## 6. Back-end — JASM / Rasm

- Lower SSA -> JASM asm text **with allocated registers substituted** -> **Rasm
  native encoder** -> bytes into the RWX JIT arena. This is WF66's only codegen
  path.
- **Per-primitive emit templates.** Each `Inline(fop)` carries a JASM template
  parameterized by its allocated src/dst registers — the impedance match
  (`@`→`mov dst,[src]`, `<`→`cmp`+fused branch, …). The **kernel primitive bodies
  remain the semantic source of truth and the template source** (charter §5's
  two-sources-of-truth, kept honest by a golden test that the template and the
  `proc` body agree).
- **`CODE:` words = opaque nodes**: the hand-MASM body is emitted verbatim and
  called or inlined as an opaque block with a declared effect — the hand-asm
  escape hatch (the mandelbrot `iter`) preserved intact.
- **`force_settle`** at region end = the lowering postcondition (canonical stack
  state), reused from v2.

---

## 7. Interactivity — keeping `;` fast (the honest tax)

The whole pipeline runs per-definition at `;`. Because scope is **one definition
(+ inlined callees)**, SSA + linear-scan is microseconds-to-low-ms — fine for a
REPL, unlike a whole-program optimizer. This is the NewFactor "doesn't feel
faster" tax, **bounded by per-definition scope**. If a definition is pathological,
tier: emit competent non-allocated code immediately, re-optimize when the word
goes hot. (Scope-bounding likely makes tiering unnecessary; keep it in reserve.)

---

## 8. Where the optimizer lives

The charter locks the optimizer implementation **Rust-side**. For the CFG / SSA /
liveness / linear-scan machinery, that is deliberate: SSA, liveness, and
allocation are type-heavy, test-heavy, and fiddly — precisely what Rust enums +
cargo tests + the borrow checker make safe and fast, and what would be painful
and slow in Forth.

> **Recommendation: Rust-side IR + optimizer + allocator. Forth-side stays the
> *surface*** — immediate words, `create`/`does>`, and the compile-time
> vocabulary that builds the IR through FFI hooks.

The *language* stays extensible (you define immediate words); the *optimizer* is
compiled Rust. This trades "live-extensible optimizer" for "a correct, fast,
testable one" — the right trade for the performance mandate. Keep the door open
to expose optimizer hooks to Forth later, but don't gate Phase 0 on it.

---

## 9. Verification — WF65 is the oracle (carried + extended)

- **Differential oracle:** identical source -> identical results, *and* WF66
  codegen **≥ as optimized** (byte / instruction / call metrics, never wrong).
- **Pure optimizer unit tests** — `IR → IR`, no JIT, no session.
- **Whole-touched-region differential fuzzer** (v2 §8 / charter §7): compare the
  result **and** the full data-stack region + `rbp` + scratch memory, byte-for-
  byte, against WF65. Plus the **value-oracle fuzzer** (random pure expressions
  vs a Rust evaluator — proven in WF65 to catch the consumption-bug class).
- **Static stack-balance / effect check** — falls out of stack-flow; a *new* bug
  class WF65 caught only dynamically.
- **Golden-byte lowering tests** + the **BLAKE3 metrics gate** over the bench
  corpus (byte/call/instruction goldens; wrong-direction move fails CI).

---

## 10. Phased plan (extends charter §6; every phase oracle-gated)

- **Phase 0** — token buffer + capture hook + const-fold + lower (`Lit` +
  arithmetic), JASM/Rasm emit. Parity with WF65 on the suite + fuzzer.
  *(front-end swap, same-or-better codegen.)*
- **Phase 1** — `dup/drop/swap/over` + the StackCache scheduler + strength-reduce
  + DCE — straight-line spans fully optimized.
- **Phase 2** — `@`/`!` + memory-ordering, decided over the whole span.
- **Phase 3** — inlining via token-splice; cross-word fold.
- **Phase 4 — the CFG / regalloc layer (the performance jump):**
  - **4a** control-flow words build CFG blocks/edges instead of flushing;
    stack-flow reconciles at joins (phis); compare→branch fusion + TCO fall out.
  - **4b** per-definition register allocation across blocks/loops; the cross-word
    ABI reconciliation; `hotvariable` subsumed.
  - **4c** LICM + CSE/GVN over the CFG. **WF66 passes VFX-class here.**
- **Phase 5** — locals, floats. Float values get xmm allocation automatically
  (the `hot-fmandel` win, no manual pinning).

`CODE:` opaque nodes and the class-(c) flush-fallback preserve full Forth
semantics at *every* phase.

---

## 11. Risks / honest limits

- **Register allocation is the hard, subtle part.** Well-trodden, but bugs are
  silent → the differential fuzzer + WF65 oracle are the safety net. This is
  *why* the frozen predecessor matters.
- **Cross-word ABI spills** at non-inlined calls cost if inlining is too
  conservative → tune the heuristic; the data stack *is* the spill space, so it's
  cheap.
- **Dynamic Forth** (`execute`, variable arity, raw-poke immediate words) →
  flush + opaque, bounded optimization there. Rare in hot code; acceptable.
- **Per-definition ceiling** (not whole-program) → Forth's nature; VFX shares it.
- **Complexity vs "hold it in your head"** → mitigated by a Rust-side type-safe
  IR + the verification discipline; the *surface* stays plain Forth.

---

## 12. Why this credibly kicks the field (summary)

- **vs WF65 / WF32 / STC peephole** — cross-block register allocation + LICM +
  whole-definition CSE/inlining: a category one-step-lookback / span-local
  optimizers cannot reach.
- **vs Factor / NewFactor** — native impedance (no boxing, no VM tax) + `CODE:`
  hand-asm kernels: the mandelbrot result, generalized.
- **vs VFX / SwiftForth (the bar)** — match the per-definition optimizing-compiler
  design, and *add* differential-oracle + fuzzer + metrics verification
  (provably-correct aggression) with full codegen transparency.
- **The moat** — a complete, frozen WF65 as a byte-for-byte state oracle for every
  optimization. Be more aggressive, because you can prove you didn't break it.
