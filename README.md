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

The optimizer runs **Rust-side** (IR, SSA, allocator); Forth stays the surface
(outer interpreter, immediate words, `CREATE`/`DOES>`). That choice is settled in
the charter, not open.

## Status

Forked from the WF65 baseline; the LLVM backend has been removed and the
token-IR compiler rewrite proceeds from here per the [roadmap](docs/design/wf66_roadmap.md).
**WF65 is WF66's differential oracle** — identical source must produce identical
*observable Forth state* (data stack, program-defined memory, output), even though
WF66 emits different, faster bytes. It is a semantic cross-check, not a byte-for-byte
spec; "≥ as optimised" is a bench-corpus tripwire, not a per-word gate (see the
charter's *Test Strategy*).

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
