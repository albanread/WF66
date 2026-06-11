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

**ABI** (inherited from WF65): settle-everywhere — TOS in `rax`, the rest of the
data stack in memory at `[rbp]` (grows down by 8), canonical at every call,
control edge, and `;`. Spare registers (`rsi`, `rdi`, `r8`…) are used as
parallel-move temporaries within a window.

**Measured** (eager WF65 vs WF66 body bytes, `wf66_size_report`): const-fold −8,
const-chain −12, inline −9, inc-chain −12, conditional body −13, loop body −16,
shuffle chains −8; never larger than eager on the bench corpus.

## Status

Forked from the WF65 baseline; the LLVM backend was removed. The token-IR
compiler and the deferred-assembly optimizer above are **implemented and on by
opt-in** (`set_wf66_enabled`), with the eager WF65 path as the fallback for any
span WF66 cannot fully defer.

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
- focused unit tests for every reduction rule and instruction-buffer pass.

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
