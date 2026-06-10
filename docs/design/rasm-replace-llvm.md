# Rasm — replace LLVM (MC assembler + MCJIT) with a native Rust assembler/loader

Status: **NATIVE IS THE DEFAULT** (2026-06). A plain `cargo build`/`cargo test`
in both `wfasm` and WF65 is LLVM-free: the native RasmEncoder + NativeJit boot
the kernel and run CODE: and LET. `--features llvm` is now an opt-in that builds
the second (LLVM) release binary and, more importantly, drives the differential
**oracles** that keep RasmEncoder byte-honest (the LET oracle and the kernel
golden's live-build check). **We ship both binaries every release; native is the
standard.** Native SEH unwind is done (Forth-centric crash dump); the only open
items are CLI auto-respawn and eventually deleting LLVM once the oracles are no
longer wanted.

Mission (original): dump the LLVM dependency from the JASM (`wfasm`) toolchain —
"JASM becomes Rasm" — by replacing LLVM-MC (assembly text → machine code) and
MCJIT (load → executable + symbol resolution) with our own Rust assembler and
loader. Borrowing from LLVM-MC source is permitted (Apache-2.0-with-LLVM-
exception), so porting its X86 encoder tables/logic into Rust is on the table.

### Build / test / release matrix (the standard)

| command | backend | what runs |
|---|---|---|
| `cargo build` / `cargo test` | **native** (default) | full functional suite, LLVM-free |
| `cargo build --features llvm` | LLVM | the second release binary |
| `cargo test --features llvm` | both | + LET differential oracle, MCJIT path, let_lang integration |
| `cargo test --features opt-metrics` | native | + kernel golden (native live build vs LLVM golden) |
| `cargo test --features llvm,opt-metrics` | both | everything |

Releases always package **both** the native and the LLVM build (native is the
primary). The oracles (`--features llvm` / `opt-metrics`) are the regression net
that lets the native default stay byte-identical to what LLVM-MC produced.

This doc is the output of a 10-agent design study (scope → 3 designs → 3
adversarial challenges → synthesis) plus my moderation. The agents read the
actual code; the key findings below are verified against the tree, not assumed.

---

## 1. How WF65 reaches LLVM today (the coupling to sever)

`wfasm` front-end (**keep**): `src/asm/` — `lex` → `expr` → `expand` (~4.5k
lines of macro expansion) → `emit` (~213 lines, currently emits Intel-syntax
**text**). This parser + macro layer is LLVM-independent and stays.

Backend (**replace**): `src/llvm.rs` (FFI to LLVM-C), `src/jit.rs` (the JIT/
loader), and the text→LLVM seam. LLVM is reached as a hand-rolled `#[link]`
in `build.rs` (links `LLVM-C.lib`, ships `LLVM-C.dll`); there is **no
`llvm-sys` crate** — `anyhow` is `wfasm`'s only real crate dep. LLVM does two
jobs:

1. **Assemble** — assembly text is handed over via `LLVMAppendModuleInlineAsm`
   and encoded by MCJIT's integrated assembler.
2. **Load/JIT** — `LLVMCreateMCJITCompilerForModule` + a custom memory manager
   allocate code/data sections; `finalize()` applies W^X; host symbols (Rust
   `rt_*` externs, user-area globals) bind via `LLVMAddGlobalMapping`; addresses
   come back via `LLVMGetFunctionAddress` / `LLVMGetGlobalValueAddress`.

## 2. The decisive discovery — byte-identity is *behavioral*, but only narrowly

The study verified (not in my original brief) that **the kernel inspects its own
emitted machine code**:

- `proc(inline_leaf_comp)` (`kernel/compile.masm:980`) byte-copies an
  assembler-emitted leaf primitive body verbatim (`src = xt`, copy until `0xC3`)
  into compiled colon words.
- The T3 peephole (`kernel/compile.masm:216-231`) then pattern-matches those
  exact copied bytes — e.g. `48 89 45 F8` / `48 83 ED 08` (`mov [rbp-8],rax ;
  sub rbp,8`) — to fuse `dup +` → `add rax,rax`.

So for the **~10–15 self-inspected leaf forms**, a legal-but-*different*
encoding silently kills the optimization — byte-identity is a **behavioral
requirement**, not cosmetic.

Conversely, the opt-gate `body_hash` is **report-only** (the static gate fails
only on `Regressed`; a fingerprint change is a `Stale`/re-bless prompt, not a CI
failure). So for everything *except* the self-inspected forms, byte-identity
with LLVM buys only a one-time `opt-bench --bless`.

**Implication:** we need byte-stability for ~10–15 sequences (hard), and at most
one audited re-bless for the rest (easy). A from-scratch **MC-derived** encoder
gives byte-identity *everywhere by construction* — which makes the self-inspection
contracts safe for free and avoids the re-bless. That is the cleanest way to
satisfy the one hard constraint.

## 3. What is genuinely new vs low-risk (verified)

- **The loader is the real new work** and is **shared by every option**. The
  boot-kernel placement + two-pass layout + relocation + `VirtualProtect` RX
  path lived *inside* MCJIT's memory manager — there is no in-tree replacement.
  WF65 has `alloc_forth_region`/`alloc_jit_arena` (RWX) to reuse for the runtime
  arena, but nothing for the immutable RX kernel path.
- **SEH / unwind is low-risk.** `src/seh.rs` is a pure VEH crash-dumper with
  **zero LLVM symbols**; its only touchpoint is `lookup_addr` (preserved).
  `install_runtime_unwind_table` is dict-header-driven and region-relative
  (LLVM-independent). THROW/CATCH is pure-asm RSP restore. The boot kernel had
  **no** unwind info under MCJIT, so there's no regression to reproduce.
- **The relocation model is small and complete:** branch `rel32`
  (call/jmp/jcc), RIP-relative `rel32` to internal labels (`lea [rip+label]`,
  the kernel's sole address-taking idiom, ~41 sites), and `abs64`-in-data
  (supported, ~unused). No external *data*-symbol relocations; user-area globals
  are register-relative (`[UP+off]`), not relocs.
- **`CODE:` is open-scope.** `rt_code_compile_body` runs the *full* assembler on
  **arbitrary** user x86-64 at runtime with no whitelist. The encoder must be a
  reentrant library, and must keep emitted bytes stable (for `body_hash`).

## 4. Decision (moderated)

**End state: own a from-scratch, MC-derived Rust encoder as the default**, reached
via a **trait-based, risk-sequenced migration** (the study's hybrid "C"
sequencing). This honors the mission's explicit intent — "implement assembler in
Rust", "our own compatible version", "borrowing from MC is valid" — which reads
as *owning* the encoder, not swapping LLVM for another third-party encoder. The
MC-derived path also delivers full byte-identity, which is the cleanest answer to
the §2 self-inspection constraint.

The study's own recommendation (iced-x86 as the *permanent* production encoder)
is the lower-risk, faster path and remains the fallback if ownership is judged
not worth the extra weeks (see §10 Q1). I diverge from it on the end-state only;
I adopt its sequencing wholesale because it de-risks the shared loader.

The migration uses two seams:

- **`Encoder` trait**: post-expansion instruction stream → `{bytes, relocs,
  symbols}`. Implementations: `TextEncoder` (today's text→LLVM, kept behind a
  feature as the **oracle**), `IcedEncoder` (a **bootstrap** to prove the loader
  without also debugging a new encoder), `RasmEncoder` (the owned MC-derived
  target that becomes default).
- **`Loader` trait**: mirrors the current `Jit` API (`new`/`add_asm`/`declare_fn`
  /`define_extern_fn`/`lookup_addr`/`lookup_fn`). Implementations: `LlvmJit`
  (today's MCJIT, oracle-only) and `NativeJit` (the new VirtualAlloc/reloc/W^X
  loader — the permanent one).

iced-x86's **decoder** (already in-tree) stays permanently as a boot-time decode
round-trip self-check; iced's **encoder** is a throwaway bootstrap, dropped once
`RasmEncoder` is the default. LLVM is dropped entirely at the end.

### Comparison

| Dimension | A — From-scratch MC-derived (chosen end-state) | B — iced-x86 (permanent) | C — Hybrid sequencing (chosen path) |
|---|---|---|---|
| **Encoder** | Port `X86MCCodeEmitter` logic + the ~205-row instr table from `X86.td`/TSFlags. Logic ports cleanly; the **table** is the bug surface (not mechanically extractable without TableGen). | iced `BlockEncoder`; token→`Instruction` builder is the only new code. Full ISA incl. open-scope `CODE:`. Diverges from MC on imm8/imm32 + accumulator short forms. | iced as *bootstrap* to prove the loader; `RasmEncoder` brought up behind the trait, gated to byte-identity by the LLVM oracle, then made default. |
| **Loader** | New two-pass + 3-reloc + `VirtualProtect` RX for the immutable kernel; reuse `CodeArena` (RWX) for runtime words. **Genuinely new.** | Same native loader (shared). | Same loader, built **first** and proven with iced before any byte-identity work — lowest-risk path through the shared hazard. |
| **SEH/unwind** | Untouched (VEH dumper is LLVM-free; `install_runtime_unwind_table` survives). | Identical. | Identical; one int3/THROW-CATCH smoke per phase. |
| **Byte-identity** | Full, by construction → self-inspection safe, **no re-bless**. | Not pursued → audit/pin ~10–15 self-inspected forms + one re-bless. **Unsafe as framed** without that audit. | Full identity where it's behavioral (proven vs golden), one audited re-bless elsewhere. |
| **Effort** | High: ~7–11 wk to a byte-identical kernel; SSE2+REX.R island + relaxation fixpoint are the long tail. | Lowest: ~2–3 wk + one re-bless. | Medium front-loaded: ~2–3 wk loader/iced bring-up, then `RasmEncoder` over ~4–6 wk off the critical boot path. |
| **Risk** | Highest *encoder* risk (hand-transcribed table = silent miscompiles), mitigated by the oracle. | Medium (builder mis-picks + the self-inspection trap). | **Lowest overall**: oracle mechanically diffs bytes; loader proven with a trusted encoder first; ownership reached incrementally. |

## 5. ISA scope — ~205 distinct (opcode, operand-form) encodings

The closed kernel set is bounded and small, but has sharp corners. Hardest cases
(the encoder test suite must nail every one):

- **SSE2 + REX.R / xmm15 island** — and our recent FTOS work made this *worse*:
  FTOS is now pinned in **xmm15**, so `movsd/addsd/subsd/mulsd/divsd` carry
  `REX.R=1` on nearly every float op. Byte order `F2`(mandatory)→`REX`→`0F`→
  opcode→ModRM must be exact (REX **after** the mandatory prefix).
- **Three SSE prefix regimes in one file**: `F2` (addsd/movsd), `66`
  (ucomisd/xorpd/movq), none (movups) — easy to cross-wire.
- **`movq` gpr↔xmm**: `66` + `REX.W` + (`REX.R` for xmm15) + two-byte `0F 6E/7E`,
  direction by opcode.
- **`cvtsi2sd` / `cvttsd2si`**: `F2` + `REX.W` + xmm-REX, mixing a GPR and an XMM
  across the ModRM reg/rm split.
- **Branch relaxation**: ~700 `jcc` + ~127 `jmp` must pick rel8 vs rel32 and
  converge to a fixpoint; byte-identity requires replicating MC's
  start-short/relax-on-overflow algorithm and ordering.
- **RIP-relative disp32** to internal labels **and** externs — fixed up only
  after final layout (~41 `lea [rip+label]` sites).
- **ModRM traps**: `[rbp]`/`[r13]` with no disp → must emit `disp8=0` (mod=01);
  `[rsp]`/`[r12]` → must emit a SIB even with no index. Silent-miscompile traps.
- **imm8-vs-imm32 group-1 ALU** (opcode `83` when imm fits sign-extended i8,
  else `81`), incl. negatives like `add rbp,-8` — must match MC.
- **Shift-by-1** (`D1`, not `C1 imm=1`), shift-by-imm8 (`C1`), shift-by-cl
  (`D3`) — three opcodes, one mnemonic.
- **setcc + 8-bit-register REX trap**; **movzx/movsx/movsxd** source-size
  dispatch.

`pin.rs` (`src/pin.rs:357+`) already has known-good exact-byte expectations for
the SSE2+REX.R and RIP-rel cases — reuse them as encoder test seeds.

## 6. Sprint plan

### Sprint 0 — Capture the irreplaceable oracle + trait scaffolding (LLVM still default)
Goal: before removing anything, capture the golden bytes that cannot be recreated
after deletion, and introduce the seams so all later work is incremental/revertible.
- **Deliverables:** golden-capture harness over all ~700 `PRIMITIVES` +
  `KERNEL_HELPERS` (reuse `opt_metrics::decode_word`'s live region read +
  `zero_rel`/`is_ip_rel` ASLR normalizer) → name→bytes JSON; a **separate raw
  (non-normalized)** golden for the ~10–15 self-inspected sequences; `Encoder`
  trait with current text path wrapped as `TextEncoder`; `Loader` trait with
  MCJIT wrapped as `LlvmJit`; move the `LLVM-C` link in `build.rs` behind a
  non-default `llvm-oracle` feature.
- **Exit:** golden JSON committed + a test re-reads the live LLVM build and
  matches it; full behavioral suite green, unchanged, with traits in place;
  default build still links LLVM (no behavior change yet).

### Sprint 1 — Native loader + symbol/reloc, proven with iced (bootstrap encoder)
Goal: stand up the genuinely-new, encoder-independent loader and prove it
end-to-end with a *trusted* encoder before any byte-identity effort.
- **Deliverables:** `NativeJit` — kernel path = two-pass layout → `VirtualAlloc`
  a code region **near the host image** (so `rt_*` externs are rel32-reachable)
  → copy → relocate → `VirtualProtect` RX; runtime path = reuse `CodeArena`
  (RWX). 3-reloc applier (branch rel32, RIP-rel rel32 to internal label, abs64).
  `bind_externs` → `HashMap<String,u64>` (preserve lazy missing-extern
  tolerance). `IcedEncoder` (promote iced `encoder`+`block_encoder`+`decoder` to
  default deps) with a token→`Instruction` builder + a directive interpreter for
  the closed set (`.text/.globl/.quad/.byte/.ascii/.asciz/.zero/.align/.p2align`),
  preserving each proc's explicit `.quad 0` xt-cell with no implicit padding.
  Re-point `register_jit_procs` at the new `lookup_addr`.
- **Exit:** boot kernel assembles/loads/RX-protects/runs under
  `NativeJit`+`IcedEncoder`; full behavioral suite green; loader-invariant tests
  pass (every `rt_*` within ±2GB of kernel base; `CodeArena` within ±2GB of
  kernel + dict/var; proc entry preceded by exactly 8 bytes of `.quad 0`;
  `lookup_addr('forth_main')` inside the kernel region); **zero far-branch stubs**
  asserted for kernel + runtime words.

### Sprint 2 — `RasmEncoder` bring-up + differential oracle + self-inspection gate
Goal: bring up the **owned** MC-derived encoder behind the trait, prove it
byte-identical to LLVM via the in-process oracle, and **prove** the self-inspected
contracts survive.
- **Deliverables:** `RasmEncoder` (MC-derived tables + `X86MCCodeEmitter`-ported
  logic) covering the ~205 kernel forms; differential harness assembling the
  kernel with `RasmEncoder` **and** the Sprint-0 golden, diffing primitive-by-
  primitive with the normalizer + a divergence report; boot-time iced-**decode**
  round-trip self-check (debug builds); **raw golden-byte** tests for the
  self-inspected set (dup stub, add/sub accumulator forms, the inline
  `add rax,[rbp]`/`add rbp,8` bodies, the `does>`-patcher `0xE8`); behavioral
  tests proving `dup +`/`dup *` fuse and `does>`/`CREATE` patches.
- **Exit:** zero unexplained divergences (each is behaviorally-irrelevant →
  accepted re-bless, or self-inspected → proven byte-identical); dup-fusion +
  `does>` behavioral tests pass under `NativeJit`+`RasmEncoder`; decode round-trip
  green for the whole kernel; static optimizer gate green (no re-bless needed if
  identity is full; otherwise one audited re-bless committed with a reviewed diff).

### Sprint 3 — Runtime/`CODE:` open-scope + W^X + reentrancy/determinism
- **Deliverables:** runtime `CODE:`/`LET` tests (one SSE form + one branch,
  executed and checked); **compile-the-same-`CODE:`-twice → byte-identical**
  (no thread_local leakage); float `LET` trampoline (xmm15) behavioral test;
  out-of-205-subset instruction assembles+runs (open-scope preserved — `RasmEncoder`
  must either cover the full ISA on this path **or** fall back to `IcedEncoder`
  for `CODE:` — see §10 Q4); forced-far extern test (>2GB → correct far handling
  or a *clean* error); optional W^X hardening of the runtime arena (RW staging →
  per-word RX page).
- **Exit:** same-`CODE:`-twice identical; out-of-subset assembles; forced-far
  passes; `install_runtime_unwind_table` RVA-safety asserted; int3/AV crash dump +
  THROW/CATCH smoke green.

### Sprint 4 — Make Rasm the default, delete LLVM
- **Deliverables:** default selects `NativeJit`+`RasmEncoder`; delete `llvm.rs`,
  the MCJIT half of `jit.rs`, and the `LLVM-C` link + DLL-copy from `build.rs`
  (collapses to near-nothing); retain `llvm-oracle` feature only for the CI
  differential job (or delete if golden corpus is deemed sufficient — §10 Q5);
  residual-LLVM CI assertions (no `LLVM-C.dll` import in `wf64.exe`, no
  `rustc-link-lib=LLVM-C`, no shipped DLL, `grep` confirms `llvm.rs` gone).
- **Exit:** default `cargo build` + full suite green with **no LLVM compiled or
  linked**; no DLL shipped; binary smaller; all Sprint 2–3 gates remain green.

## 7. Extensive test plan (layers × must-pass gate)

| Layer | What it proves | How | Gate |
|---|---|---|---|
| **Differential vs LLVM golden** | Every kernel word's bytes match LLVM-MC (ASLR-normalized) — the exhaustive proof the 10-file corpus does **not** give. | Capture golden from live LLVM in Sprint 0 (irreplaceable); diff `RasmEncoder` per-primitive. | Sprint 2: zero unexplained diffs. |
| **Self-inspection byte contract** | The ~10–15 read-back sequences stay byte-identical so `dup+`→`add rax,rax` etc. still fire. | Raw (non-normalized) byte-equality + behavioral fusion tests. | Sprint 2+: hard blocker, no re-bless accepted until green. |
| **iced decode round-trip** | Every emitted instruction decodes back to the intended mnemonic+operands (catches `movq` direction, `cvt` REX.W-by-operand, movsd-SSE-vs-string, imm8/imm32). | Boot-time decode loop (debug) over kernel + a runtime `CODE:` word. | Sprint 2 (kernel), Sprint 3 (runtime). |
| **Per-form fuzz/coverage** | All ~205 forms — especially ones the corpus never exercises (SSE2+REX.R xmm15 island, RIP-rel, imm8/imm32, shift D1/C1/D3) — individually correct. | Dedicated per-form unit suite: bytes == golden AND decode agrees; seed from `pin.rs:357+` known-good expectations. | Sprint 2 (in-kernel forms); ongoing per new mnemonic. |
| **Full behavioral suite** | The toolchain actually works, encoder-agnostic. | The 382-test harness under the gate features (force byte-level checks on). | **Every** sprint exit — non-negotiable. |
| **SEH / unwind** | Crash dumper symbolicates; int3/AV dumps + advances RIP; THROW/CATCH works; runtime unwind RVAs stay u32-safe. | int3 + AV smoke asserting nearest-symbol; THROW/CATCH program; assert word ranges within region. | Sprint 1 (lookup_addr), Sprint 3 (full smoke). |
| **W^X / protection** | Kernel RX (not RWX) after finalize; data RW-noexec; no regression vs commit `edd3baa`. | Post-finalize `VirtualQuery` assertions. | Sprint 1 baseline; Sprint 3 if hardening adopted. |
| **Runtime-codegen reentrancy/determinism** | Same `CODE:` source → identical bytes (body_hash stability); float LET (xmm15) works; out-of-subset assembles. | Compile-twice-assert-identical; CODE: SSE+branch; LET trampoline; out-of-subset instr. | Sprint 3. |

## 8. Decisions

Locked (confirmed):

1. **Encoder end-state — OWN IT: from-scratch MC-derived.** `RasmEncoder`
   (ported `X86MCCodeEmitter` logic + the ~205-row table from `X86.td`/TSFlags)
   becomes the default encoder. Byte-identical to LLVM by construction → the §2
   self-inspection contracts are safe for free and no re-bless is needed.
   iced-x86's **encoder** is a throwaway Sprint-1 bootstrap only; its **decoder**
   stays as the permanent round-trip self-check.
2. **Build-time LLVM dropped too** (follows from #1) — Sprint 4 removes the
   `LLVM-C` link from `build.rs`; no `C:\Program Files\LLVM` requirement after
   cutover (except the oracle test job, #5).
5. **LLVM-oracle lifetime — KEEP behind a feature through cutover.** LLVM stays
   compiled-in behind a non-default `llvm-oracle` feature for the live in-process
   A/B byte-diff through Sprints 1–3 (the test box needs the LLVM install until
   then); deleted in Sprint 4.

Defaulted (override anytime):

3. **New default dependency** — only iced-x86's **decoder** stays permanently
   (already in-tree via opt-metrics); the encoder/block_encoder features are
   enabled for the Sprint-1 bootstrap and dropped once `RasmEncoder` is default.
   Net dependencies drop (LLVM-C.lib + the shipped DLL leave).
4. **`CODE:` open-scope** — keep `IcedEncoder` available as the `CODE:` fallback
   so arbitrary user x86-64 still assembles (no capability regression) even if
   `RasmEncoder` stays subset-only. Revisit if full-ISA `RasmEncoder` is wanted.
6. **Runtime-arena W^X** — defer; keep parity with today's RWX runtime arena.
   Tracked as a follow-up (close to per-word RX once the loader owns placement).
