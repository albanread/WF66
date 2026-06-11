# WF66 — Token-IR Optimizing Forth

WF66 is the successor to **WF65** (a complete, working 64-bit STC Forth,
JIT-compiled via the JASM assembler). It reuses WF65's proven runtime substrate
— JASM/Rasm native encoding, the STC kernel primitives, the dictionary, the
live REPL, and the Rust test harness — and **replaces the compiler**.

## The change in one line

WF65 emits code eagerly and then *replays* (rewind + rewrite) to optimize. WF66
captures each definition as a **token IR, optimizes the tokens as data, and only
then generates code** — a real front-end → optimizer → back-end split.

See **[docs/design/wf66_charter.md](docs/design/wf66_charter.md)** for the full
charter: the token IR, the span-flush rule, the three passes, inlining via
token-splicing, the phased plan, and the open decision (where the optimizer
runs — Forth-side vs Rust-side).

## Status

Day 0. This tree is a verbatim fork of the WF65 baseline (it builds and tests
identically); the compiler rewrite proceeds from here per the charter's phased
plan. **WF65 is WF66's correctness oracle** — identical source must give
identical results, and WF66's codegen must be ≥ as optimized.

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
