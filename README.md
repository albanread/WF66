# WF66 — Token-IR Optimizing Forth

WF66 is the successor to **WF65** (a complete, working 64-bit STC Forth,
JIT-compiled via the JASM assembler). It reuses WF65's proven runtime substrate
— JASM/Rasm native encoding, the STC kernel primitives, the dictionary, the
live REPL, and the Rust test harness — and **replaces the compiler**.

## The change in one line

WF65 emits code eagerly and then *replays* (rewind + rewrite) to optimize. WF66
captures each definition as a **token IR, optimizes the tokens as data, and only
then generates code** — a real front-end → optimizer → back-end split.

Design set:

- **[docs/design/wf66_charter.md](docs/design/wf66_charter.md)** — the contract:
  outer-interpreter/IR-builder split, the three compile-time word classes,
  two-sources-of-truth, exceptions, and the WF65 differential oracle.
- **[docs/design/wf66_compiler.md](docs/design/wf66_compiler.md)** — the
  architecture: token IR + CFG, stack-flow → SSA, the optimizer passes,
  cross-block register allocation, and JASM/Rasm codegen.
- **[docs/design/wf66_roadmap.md](docs/design/wf66_roadmap.md)** — the authoritative
  phase/sprint sequence (Phases 0–5).
- **[docs/design/wf66_phase4_plan.md](docs/design/wf66_phase4_plan.md)** — Phase 4,
  the CFG + register-allocation jump, in detail.

The optimizer runs **Rust-side**; Forth stays the surface (outer interpreter,
immediate words, `CREATE`/`DOES>`). That choice is settled in the charter.

## The optimizer (as built)

One idea, applied at two levels: **recognize a pattern, replace it with a
cheaper equivalent, repeat to fixpoint.** There is no SSA form and no register
allocator — both stages are pattern-driven rewriters.

**1. Token-IR reduction (Forth level).** Each `:`…`;` body is captured as a token
stream and reduced by a shift-reduce fixpoint engine — recognize the longest
reducible run, replace, re-scan, until nothing reduces. Rules include whole-span
constant folding (`2 3 * 4 +` → push 10), strength reduction (`5 *` → shift/lea),
immediate-operand folding (`7 + 3 +` → `+10`), dup-op fusion (`dup *`), `n pick`
folding, and compare→branch fusion (`0< if` emits one conditional jump, no
materialized flag). The rule catalog is ordered by a frequency miner
(`cargo run --bin seq_freq`) over real Forth and the kernel MASM.

**2. Deferred-assembly instruction buffer (machine level).** Rather than assemble
the lowered text directly, WF66 lexes it into instruction records and reduces
*those* with the same philosophy, then re-renders:

```
lower → parse_instrs → coalesce_dsp → window_fuse → render → assemble
```

- **coalesce_dsp** defers the data-stack-pointer (`rbp`) adjusts to each
  barrier-free window's edge, rewriting cell displacements by the running delta —
  so paired push/pop adjusts cancel and a shuffle run's adjusts collapse to one.
- **window_fuse** ("auto-pick instead of stack ops"): after coalescing a window
  is pure fixed-offset slot addressing. It symbolically simulates the window to
  its *net* permutation/duplication map and emits the **minimal parallel-move**
  that realizes it, instead of replaying each stack op. `rot rot` collapses from
  8 memory accesses to the 4 of its net `-rot`; redundant reloads vanish.
- **promote_hot_cells**: a read-only data-stack cell read ≥2× in a run is loaded
  into a reserved register once and reused — registers for hot values with a
  clear home, no spills, stop when the pool is empty (no liveness analysis).

**3. Reaching past the leaf — calls, variables, inlining.** A call no longer has
to taint its caller:

- **Settle-barrier calls.** A call to a *known* word (the libm math words, or
  any WF66-optimized word) settles the stacks to canonical and calls it, while
  the optimizer keeps optimizing the windows around it. No stack-effect analysis
  is needed — settle-everywhere already keeps TOS/FTOS/DSP/FSP canonical, so it's
  just *settle, call, resume*.
- **Variables are address pushes, not calls.** A `variable`/`fvariable`
  reference is captured as a literal address (`create`'s stub bakes the body
  address in as `mov rax,imm64`; the recorder reads it back), and `var f@`/`f!`
  fuse to a direct absolute load/store.
- **Optimized words inline.** A WF66-optimized word is a leaf word by
  construction, so a caller inlines it (small) or settle-barrier-calls it
  (large) — never taints.

**Locals.** A `{:`…`:}` definition is captured and WF66-compiled. Locals live in
a per-word frame (`R15`/LP, byte-offset slots); `{:` records its own prologue
(frame alloc + the stack-init arg stores) into the IR via `(wf66-open-locals)` —
it has to, because `{:` *postpones* the frame ops and `postpone` compiles past
the recorder's capture point — and `;` appends the paired teardown so the body is
a **balanced `[OpenLocals … CloseLocals]` unit**. That balance is what lets a
locals word inline correctly: a caller splices the whole unit as a *nested frame*
(`R15` dips and pops, the caller's own locals untouched while the inner frame is
open). Local fetches (and `to`-stores) are inline-emitted by the kernel, so the
kernel calls the recorder directly for them. `to`-stores currently taint (a `to`
is an unrecognized immediate, and it's polymorphic over values vs locals), so a
word that assigns a local falls back to the eager body for now.

Why locals matter for the endgame below: a local is word-private and can't be
addressed, so it **can't alias** — making it the ideal register-promotion target,
and (the user's rule) locals take priority over globals. The plan is to promote a
leaf word's locals into registers for the word's duration and elide the frame
entirely when they're all register-resident — which is precisely how the
hand-MASM owns its loop state. Inlining a locals word then composes: the inlined
unit's locals become register-resident in the caller's call-free body and the
nested frame evaporates.

**Floating point.** The FP stack mirrors the data stack (FTOS in `xmm15`, the
rest in memory at `user_FSP`). `f+ f- f* f/ fnegate fdup fdrop fswap fover f@ f!`
all lower as a verbatim mirror of the kernel; **libm** (`fsqrt fsin fcos …`) is
reached via settle-barrier calls; and `fp_coalesce` caches the FP stack pointer
in a register across a run instead of the kernel's reload-from-memory every op.

**ABI** (inherited from WF65): settle-everywhere — TOS in `rax`, the rest of the
data stack in memory at `[rbp]` (grows down by 8), canonical at every call,
control edge, and `;`. Spare registers (`rsi`, `rdi`, `r8`…) are
parallel-move temporaries; `r10`/`r11` are the read-only promotion pool.

**Measured** (eager WF65 vs WF66 body bytes, `wf66_size_report`): const-fold −8,
const-chain −12, inline −9, inc-chain −12, conditional body −13, loop body −16,
shuffle chains −8; never larger than eager on the bench corpus.

## A benchmark, and what it says about scope

A pure-Forth Mandelbrot inner loop (`z = z² + c` over fvariables, 5M iterations,
`wf66_mandel_inner_bench`):

```
Forth, optimizer OFF:  ~149 ms        (eager: per-op kernel calls + FP-stack traffic)
Forth, optimizer ON :   ~41 ms        (3.6× faster than off)
hand-rolled MASM    :   ~15 ms        (loop state in xmm registers, all in one word)
```

So the optimizer recovers about **three-quarters of the hand-tuning gap**
automatically (eager is ~9.6× slower than MASM; optimized Forth is ~2.7×).

The residual 2.7× is one thing: **loop-carried register residency.** The MASM
keeps `zx`/`zy`/count in registers *across iterations*; WF66 keeps `zr`/`zi` in
fvariables (memory) and the counter on the data stack, settling them at the loop
back-edge. (The MASM even runs an escape test WF66 omits and is *still* faster —
the win is register-vs-memory for the loop state, not the arithmetic.)

**This frames the scope deliberately.** The hand-MASM isn't doing whole-program
magic, and it isn't a privilege the MASM has and a compiler doesn't: it keeps
`zx`/`zy` in `xmm0`/`xmm1` across the loop **only because it makes no calls**, so
the Win64 volatile registers are genuinely free there. Register residency is only
ever valid inside a **call-free region** — the moment any code makes a call, it
cannot own those registers across it (the callee may trash the caller-saved set
and won't restore the caller-saved ones). So *no* general-purpose program and *no*
compiler can claim global register state; the call-free span is the universal
boundary, full stop.

That makes WF66's scope the *correct* one, not a limitation. WF66 optimizes
**leaf words** (and inlined hot paths) — exactly the call-free regions where
owning the registers is legal in the first place — which is the same boundary the
hand-MASM exploits. Whole-program register allocation across calls is not a thing
to chase: it's unachievable (calls clobber registers) and unnecessary (the hot
~10% of a program lives in leaf words). The remaining work is to own the registers
*within a leaf word's loop* the way the MASM does — loop register allocation across
the back-edge of a call-free loop — which is the next frontier the benchmark now
measures.

## Status

Forked from the WF65 baseline; the LLVM backend was removed. The token-IR
compiler and the deferred-assembly optimizer above — including **floating point**
(FP ops, libm via settle-barrier calls, FP-stack-pointer caching),
**variable-reference-as-literal**, and **locals** (`{:`…`:}` captured and
WF66-compiled, balanced frames that inline as nested frames) — are **implemented
and on by opt-in** (`set_wf66_enabled`; default-on in the IDE, toggle under
Forth → WF66 Optimizer), with the eager WF65 path as the fallback for any span
WF66 cannot fully defer (`do`/`loop`, I/O, return-stack, FP comparisons, `to`-on-
locals — the current taint set).

**WF65 is WF66's differential oracle** — identical source must produce identical
*observable Forth state* (data stack, program-defined memory, output), even though
WF66 emits different, faster bytes. It is a semantic cross-check, not a byte-for-byte
spec; "≥ as optimised" is a bench-corpus tripwire, not a per-word gate (see the
charter's *Test Strategy*).

Verified continuously by:

- a **600-program differential fuzzer** (`wf66_differential_fuzzer`) — random
  deferrable programs, WF66 output checked against the WF65 oracle;
- the **ANS Forth-2012 core test suite**, run both eager *and* with WF66 enabled
  (`m7_ans_core_tests_pass{,_with_wf66}`);
- FP, libm, variable-reference, locals (incl. nested-frame inlining and
  `|`-uninitialized slots), and inlining **differential tests** (WF66 output vs
  the eager kernel);
- focused unit tests for every reduction rule and instruction-buffer pass.

Diagnostics worth knowing (`cargo test … -- --ignored --nocapture`):
`wf66_compile_metrics` (coverage + per-word min/max/avg of regs, bytes, instrs,
stack accesses), `wf66_register_stats`, `wf66_mandel_inner_bench` (Forth off/on
vs hand MASM), `wf66_before_after_asm`.

## Build

```powershell
cargo build              # native JASM/Rasm build
cargo test --test harness
```

Path deps (`../JASM/rust`, `../NewGC/...`) resolve the same as in WF65; no Cargo
changes were needed for the fork.

## Carried-over baseline still named `wf64`

The package/binary are still named `wf64` from the fork. Renaming to `wf66`
(Cargo manifest + `use wf64::` sites + the jasm-forth skill + `get-started.md`)
is a deliberate follow-up, kept separate so the baseline stays green first.
