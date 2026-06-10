# WF66 — Token-IR Optimizing Forth Compiler (project charter)

Status: **charter / design.** Successor to WF65. Supersedes the WF65 optimizer
line (`jasm_forth_optimizer_v1.md`, `jasm_forth_optimizer_v2.md`) by replacing
its *architecture*, not patching it. WF65 is frozen and complete; it is WF66's
correctness oracle (§7).

## 0. Thesis (one line)

WF65 emits code eagerly and then **replays** (rewind + rewrite) to fold; WF66
captures each definition as a **token IR, optimizes the tokens as data, and only
then generates code.** Optimization moves from a pile of stateless one-step
peephole rewrites to a pure, testable, multi-pass data transform.

## 1. Carries over vs. replaced

**Carries over (proven in WF65, reused as-is):**

- JASM macro assembler + LLVM-MC encoding + MCJIT.
- The STC runtime: every primitive a `proc…endp` body; CPU `call`/`ret` *is*
  the dispatch.
- Register conventions — RAX=TOS, RBP=DSP, RBX=UP, RSP=rstack, R12=save slot.
- The kernel primitive bodies (`+ * dup @ ! …`): they stay the source of truth
  for semantics **and** supply the byte recipes the back-end lowers to.
- Dictionary, headers, `create`/`does>`, the live REPL + `lib/core.f` growth.
- The Rust harness pattern (`Wf64Session`, data-driven `.t`/`.in`/`.out`).

**Replaced (the WF65 compiler/optimizer):**

- The single-forward-pass colon compiler's *immediate* codegen.
- The entire **replay substrate**: `LAST_LIT_*`, `LAST_DUP_END`, `LAST_CMP_*`,
  `LAST_ADDR_*`, `OPT_FENCE`, `try_fold_literal`, and the rewind tail of every
  `fold_*_comp`.
- The v2 streaming `StackCache` (designed, never built) — superseded by the
  token-IR buffer, which is more powerful and easier to test.

## 2. The token IR

While compiling a straight-line span, a definition body is captured as an array
of typed tokens:

| Token            | Source                                            |
|------------------|---------------------------------------------------|
| `Lit(i64)`       | integer (caught at interp `.got_number`, *before* `literal` runs — the v1 "literal can't reach the compiler" gap, gone by construction) |
| `FLit(bits)`     | float literal                                     |
| `Local(off,rw)`  | local read/write                                  |
| `Word(xt,flags)` | a call to a non-inlined word                      |
| `Inline(fop)`    | curated primitive with a known emit-template / algebraic identity (`+ - * / and or xor dup drop swap over @ ! < = …`) |

A folding constant is simply a `Lit` (its value), so the `constant` machinery
and the `bl`/`true`/`false` ordering hack disappear (§4).

**Span-flush rule (intrinsic to Forth, not a design choice).** Immediate words
and control-flow words run *at compile time* and read `HERE` / the control
stack; they cannot be inert tokens. A **span** is a maximal run of deferrable
tokens; every immediate / control-flow / `postpone` / `[` / `;` word **flushes**
the buffer (optimize → lower → canonical state) and then runs against that
state. This is the same boundary WF65 already respects — here it is simply where
the IR buffer empties.

## 3. The three passes

**(a) Front-end / capture.** The lowering hook at the interpreter's convergence
point appends typed tokens to the per-definition span buffer instead of
compiling each token immediately.

**(b) Optimizer — pure `tokens → tokens`, no codegen, unit-testable:**

- *const-fold*: evaluate `Lit … Inline(arith)` symbolically (`5 7 +` → `Lit
  12`). Two-literal stores (`42 var !`) and compile-time `/` fall out because the
  whole array is visible — the one-slot watermark ceiling is gone.
- *strength-reduce*: annotate `Inline(*) imm` with the cheap form. (WF65's
  `fold_times` strength reduction becomes a token annotation, not a rewrite.)
- *inline*: splice a small `Word`'s token body into the stream (§4), then re-run
  fold across the former boundary.
- *schedule*: assign the abstract stack to registers/memory with full-span
  lookahead (the StackCache's job, done over the whole span at once).
- *dead-code*: a value pushed then dropped before any settle emits nothing.

**(c) Back-end / lower.** Walk the optimized tokens, emit canonical STC bytes
(the kernel byte recipes), and force the span to canonical state at its end —
the WF65 `force_settle` invariant, reused as the lowering postcondition.

## 4. Inlining = token splicing (the `optinline.fs` payoff)

If word *bodies* are kept as token sequences, using a word can splice its tokens
into the caller before optimizing:

- `: bl 32 ;` ≡ `[Lit 32]`; `bl +` → `[Lit 32, Inline(+)]` → folds. **Item 2
  falls out structurally — no `constant` trick, no core.f reordering.**
- Small colon words inline by splice; const-fold / strength-reduce then run
  across the call boundary that used to block them.

Heuristic: inline below a token-count threshold or when the body is
all-deferrable; otherwise emit `Word(xt)` (a call). Recursive / large bodies
stay calls.

## 5. The one open decision — where the optimizer runs

- **Forth-side (recommended).** The kernel provides the token buffer + a fixed
  repertoire of lowering/emit primitives; the passes are Forth code in a lib
  file, live-extensible in the REPL. Fits the charter's "grow Forth in Forth"
  and matches the WF32 precedent (`optliterals32.fs`/`optinline.fs` were Forth).
  A fixed emit repertoire is **not** a Forth-side assembler, so "no metacompile"
  holds.
- **Rust-side.** Type-safe enum IR, cargo unit tests, reuses MCJIT directly —
  but the language's optimizer lives *outside* the language and is not
  live-extensible.

Must be decided before Phase 0.

## 6. Phased plan

- **Phase 0** — token buffer + capture hook + const-fold + lower, for `Lit` +
  arithmetic only; everything else flushes. Proves the three-stage split
  end-to-end on `: bar 5 * 2 + ;` and `5 7 +` → `12`.
- **Phase 1** — `dup/drop/swap/over` + the register-window scheduler +
  strength-reduce annotations.
- **Phase 2** — `@`/`!` + memory-ordering barrier, decided over the whole span.
- **Phase 3** — inlining via token-splice; cross-word fold.
- **Phase 4** — locals, floats.

## 7. Test strategy — WF65 is the oracle

WF65 is complete and correct, so it is WF66's **differential oracle**: identical
source must give identical results, and WF66's codegen must be **≥ as optimized**
(fewer/cheaper instructions, never wrong). Plus:

- pure optimizer unit tests (`tokens → tokens`, no JIT, no session);
- golden-byte lowering tests (the back-end's recipes);
- the whole-touched-region differential fuzzer from v2 §8 (compare result **and**
  the full stack region + `rbp` + scratch memory, byte-for-byte).

## 8. What WF65 taught us (carried as constraints)

- Replay's ceiling is **one-step lookback** (one watermark slot per kind) → the
  two-literal folds are structurally impossible. The IR removes the ceiling.
- `OPT_FENCE` was a band-aid for premature emission; deferral + span-flush
  removes the wound, not just the symptom.
- `force_settle`'s canonical-state contract is the real correctness crux and
  survives intact as the back-end's per-span postcondition.
- Span boundaries (immediate / control-flow) are a property of **Forth**, not of
  the optimizer — WF65 and WF66 draw them in exactly the same place.
