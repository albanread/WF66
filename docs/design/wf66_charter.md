# WF66 - IR-Builder Optimizing Forth Compiler

Status: charter / design. WF66 keeps WF65's working Forth runtime and replaces the compilation product: compile-time words build per-definition IR instead of emitting final bytes immediately.

Full codegen / optimizer / register-allocation design: [wf66_compiler.md](wf66_compiler.md).

## Thesis

Forth has no clean offline parse phase. Immediate words are executable compiler extensions: `IF`, `LITERAL`, `POSTPONE`, `;`, and user-defined immediate words run while source is being consumed. WF66 therefore does not replace the outer interpreter. The outer interpreter remains the front end, and the compile-time vocabulary becomes the IR-builder API.

## Performance goal

WF66 is not just a cleaner compiler than WF65 — the target is to out-run the field (STC peephole Forths, VM-hosted Forths, and VFX/SwiftForth-class native Forths). Three factors, multiplied:

1. **Native impedance.** Every primitive lowers to its own instruction — `@`→`mov`, `<`→flags, `>r`→`push`, floats→xmm — never boxed, no VM-call tax. (This is what beats Factor / VM-hosted Forths; cf. the mandelbrot result.)
2. **Cross-block register allocation.** The data stack lives in registers *across* `IF`/`THEN`/`BEGIN`/`DO`, reconciled to memory only at spills and word boundaries — the category STC peephole (WF65/WF32/most Forths) structurally cannot reach.
3. **Inline-and-fold across word boundaries** (token-splice small callees, then fold/CSE/strength-reduce over the former boundary).

The moat: WF65 is a frozen **differential oracle** — same source must produce the same *observable Forth state* (data stack, program-defined memory, output), even though WF66 emits different, faster bytes and uses registers differently. It is a **cross-check, not the spec**, and it compares semantics, not machine incidentals (see *Test Strategy*): a too-strict oracle that pinned scratch registers, spill memory, or instruction count would forbid the very register allocation that is the point — a false oracle pins you to WF65's wrong places, not just its right ones. Honest ceiling: per-definition, not whole-program — Forth's mutable, runtime-patched dictionary forbids whole-program analysis, a ceiling VFX shares. Full thesis in [wf66_compiler.md](wf66_compiler.md).

## Carries Over

- JASM/Rasm native assembler and loader.
- STC runtime: every primitive is a `proc ... endp` body; CPU `call`/`ret` is dispatch.
- Register conventions: RAX=TOS, RBP=DSP, RBX=UP, RSP=rstack, R12=save slot.
- Dictionary, vocabularies, `CREATE`/`DOES>`, live REPL, `lib/core.f`, and the Rust harness.
- `CODE:` and raw code emission as explicit opaque escape hatches.

## Two Sources of Truth

The kernel `proc … endp` body is the **canonical semantics** of every primitive. WF66 reaches it two ways:

- **Non-inlined `Word`** — *calls the same kernel*, byte-for-byte. Same kernel, same dispatch.
- **Inlined hot primitive** — spliced from a per-primitive **emit template** (`@`→`mov dst,[src]`, `<`→flags, …) with allocated registers.

The template is a second *representation*, never a second *semantics*: it must stay behavior-equivalent to the `proc` body, enforced by a golden test that runs both and compares observable state (per *Test Strategy*). One source of truth — the kernel — two ways to reach it. Each `Inline(fop)` also declares its `RegEffect{ins, outs, clobbers}` so the allocator can schedule around the template.

## Replaced

WF65's compiler emits final bytes as soon as each compiling word runs. WF66 redirects those words to build IR:

| Compile-time word | WF66 behavior |
| --- | --- |
| `LITERAL` | append an IR literal node |
| `COMPILE,` / `POSTPONE` | append an IR compile-this node or compile hook call |
| `IF` / `ELSE` / `THEN` | build CFG branches and control markers |
| `BEGIN` / `UNTIL` / `AGAIN` / `DO` / `LOOP` | build loop/control-flow IR |
| `;` | close the definition and run the back end |

The replay substrate disappears: `LAST_LIT_*`, `LAST_DUP_END`, `LAST_CMP_*`, `LAST_ADDR_*`, `OPT_FENCE`, `try_fold_literal`, and the rewind tails of the old fold words become unnecessary because optimization sees the completed definition IR.

**Peephole subsumption (assumption).** Compiling through the IR *subsumes the WF65 peephole/replay layer wholesale* — WF66 never runs a peephole pass alongside the IR optimizer; there is no dual path. Every WF65 one-step-lookback rewrite is reimplemented as a whole-definition IR pass (*Back End* step 3; [`wf66_compiler.md`](wf66_compiler.md) §4): constant folding (`try_fold_literal`), two-literal stores, compile-time `/`, the `bl`/`true`/`false` ordering tricks, `imul`-immediate strength reduction, the `LAST_DUP`/`LAST_CMP` compare-and-branch tails, and `LAST_ADDR` load coalescing. Whole-definition visibility makes each strictly stronger than the watermark-limited predecessor it replaces.

## Compile-Time Vocabulary Contract

User immediate words keep working if they are written against the standard compile-time vocabulary. That vocabulary is the IR-builder surface, and compile-time words fall into three classes:

| Class | Examples | Treatment |
| --- | --- | --- |
| deferrable | literals, arithmetic, stack shuffles, memory ops with known effects | append tokens to the current IR block |
| structured control | `IF`/`ELSE`/`THEN`, `BEGIN`/`UNTIL`, `DO`/`LOOP`, `LEAVE`, `EXIT` | build CFG blocks/edges with IR-level marks, not `HERE` addresses |
| opaque/dynamic | `EXECUTE`, runtime xts, unknown `POSTPONE`, raw `HERE c,` pokes, immediates that escape the vocabulary | settle to canonical state, emit an opaque node with a declared/conservative effect, resume fresh |

The IR-builder surface includes:

- literal construction
- compile-this / compile-hook construction
- postpone semantics
- branch markers and branch resolution
- control-flow stack operations
- definition close/finalize

A user immediate word that calls those words becomes a user-extensible IR macro and is optimized for free. A user immediate word that reads or writes physical code layout directly (`HERE`, `,`, `c,`, raw branch patching) bypasses structured IR unless it goes through a defined builder operation; the safe treatment is an opaque/dynamic boundary.

## CREATE/DOES>

`CREATE`/`DOES>` uses the same mechanism WF65 already has: defining words install compile behavior on their children through the compile hook. In WF66 that hook builds IR for the child instead of emitting bytes directly. A `CREATE`d child can emit a `Body(addr)` / push-body token; a `DOES>` child emits push-body plus inline-or-call of the does-body. Inlining is allowed only when the target's execution semantics are stable for this compiled definition; mutable, deferred, vectored, or otherwise dynamic behavior stays as a call or opaque boundary.

## Optimizer Implementation: Rust-Side

The IR, the optimization passes, the stack-flow → SSA lift, and the register allocator live in **Rust**, not Forth. SSA, liveness, and linear-scan allocation are type-heavy and test-heavy — exactly what Rust's enums + cargo unit tests make safe and fast, and what would be slow and fragile written in Forth. This follows the **LET** precedent: LET is already a Rust-native compile path on the Rasm encoder, and it is easier to build and verify there.

The trade is deliberate: the optimizer is **not** live-extensible from the REPL, in exchange for being correct, fast, and unit-testable — which the performance mandate needs. **Forth stays the surface**: the outer interpreter, immediate words, `CREATE`/`DOES>`, and the compile-time vocabulary that drives the IR builder all remain Forth-facing. Optimizer hooks may be exposed to Forth later, but no phase gates on it.

## Back End

At `;`, WF66 runs a per-definition pipeline (light enough for the REPL because the scope is one definition + inlined callees):

1. stack-flow analysis (symbolic stack → SSA values; `swap`/`dup`/`over` become renames)
2. CFG construction + cleanup; reconcile stacks at joins (phis), check stack-balance
3. constant folding, strength reduction, algebraic simplification, CSE/DCE
4. inlining of already-known, stable callees by token-splice, then re-fold across the boundary
5. **whole-definition register allocation** — data-stack values held in registers *across branches and loops*; the data stack in memory is the spill space and the cross-word ABI (TOS in RAX; reconcile live values at non-inlined calls). This cross-block allocation is the primary speed lever and the thing STC peephole cannot do.
6. loop-invariant code motion over the CFG (a win the span-flush model could not reach)
7. MASM/JASM text generation with allocated registers substituted
8. native Rasm assembly and placement

This is per-definition and incremental by design. The dictionary is mutable, `DOES>` can patch behavior at runtime, and later definitions can change the world. WF66 optimizes what is closed and known at definition finalization time. Settle-to-canonical is always the fallback: definition exit (`;`), non-inlined calls, and opaque/dynamic regions materialize the data stack to the WF65 ABI before control crosses the boundary.

## Opaque Regions

Raw byte poking, unknown stack effects, dynamic xt execution, and `CODE:` bodies are represented as opaque IR nodes or force opaque compilation for that definition. Before an opaque boundary, live SSA stack values are settled to canonical state (`RAX` for TOS, rest in `[RBP]`); after the boundary, the compiler resumes from the declared stack effect. If no sound effect can be declared, the whole definition falls back to opaque/settled compilation or a clear compile-time error.

The contract is binary:

- use the compile-time IR-builder vocabulary: participates in optimization
- cross an opaque/dynamic boundary: settle first, preserve semantics, block optimization across it

## Exceptions (CATCH / THROW)

Register residency must not change exception semantics. Two cases:

- **Anonymous data-stack values are exception-transparent** and need no extra settling. `CATCH` reaches its protected code through `EXECUTE` — an opaque/dynamic boundary that already settles — so the data stack is canonical when the catch frame records its depth. `THROW` restores the data-stack pointer from that frame; any register-resident values in the unwound region necessarily live *above* the restored depth and are discarded — exactly ANS `CATCH`/`THROW` behavior. The return stack and `DO` loop counters stay in memory (never register-allocated; see [`wf66_phase4_plan.md`](wf66_phase4_plan.md) §4b.3), so `THROW`'s return-stack restore is unchanged. **No faulting primitive** (`/`, `mod`, an address-faulting `@`) needs to become a data-stack settle boundary.
- **Promoted named cells *do* need a write-back.** A `VARIABLE`/`VALUE`/`CREATE` body held in a register (the subsumed `hotvariable` case) is *program-defined memory*, which the oracle compares (*Test Strategy*). Its backing memory must be canonical wherever a `THROW` is observable, so the allocator writes a promoted named cell back to memory **before any THROW-capable operation in its live range** — integer `/`/`mod`, an address-faulting `@`/`!`/`c@`/`c!`, or a non-inlined call. Anonymous values are exempt. This **supersedes** the [`register_pinning_v1`](register_pinning_v1.md) "values stale after exception" caveat: under the stricter observable-memory oracle a stale promoted variable is a divergence, not documented behavior. The promotion heuristic may simply decline to promote a named cell whose range contains THROW-capable ops.

## Non-Goals

WF66 is not a whole-program batch compiler. That would require amputating the user-programmable compile-time semantics that make Forth Forth. WF66 is an optimizing interactive compiler: per-definition, mutable-dictionary aware, and REPL-friendly.

## Test Strategy

WF65 is a **cross-check, not the specification.** The differential oracle compares the **observable Forth state and nothing else**:

- **Compared (semantic):** the data stack — every live cell and the depth; the return-stack / locals net effect; memory the program *defines* (`VARIABLE`/`VALUE`/`CREATE`/`ALLOT` cells, dictionary writes); and emitted output.
- **Not compared (implementation freedom):** scratch / caller-clobbered registers, memory below the stack pointer or in spill / scratch regions, padding, uninitialised cells, instruction bytes, and instruction *count*. Pinning any of these is a **false oracle** — it would forbid the register allocation and restructuring that are the whole point.
- **Not gospel.** Where WF65 disagrees with Forth-2012 (or has a known bug), the standard wins and the divergence is flagged for a human; the oracle never enshrines a WF65 quirk as the spec.
- **"≥ as optimised" is a tripwire, not a gate** — measured on the bench corpus (throughput / instruction trend). A single word emitting differently-shaped, even locally larger, code is fine when the bench holds. WF66 is free to restructure.

Test layers: the existing harness + data-driven tests; pure IR / optimiser unit tests; backend golden tests (template vs `proc` body, per *Two Sources of Truth*); the **value-oracle fuzzer** (random pure expressions vs an independent Rust evaluator); the **differential state fuzzer** (the contract above) over random control-flow nests; and the static stack-balance check from the front end.

## First Slice

The full phase / sprint plan is **[wf66_roadmap.md](wf66_roadmap.md)** (Phase 4 detail in **[wf66_phase4_plan.md](wf66_phase4_plan.md)**). The first slice is Phase 0:

1. Add an IR builder object to the compiling state.
2. Redirect `LITERAL`, `COMPILE,`, `POSTPONE`, and `;` to the builder/finalizer.
3. Keep structured control (`IF`/`ELSE`/`THEN`, loops) on the settle-to-canonical compatibility path for Phases 0-3; define the IR-level mark API, but land CFG block/edge construction in Phase 4.
4. Lower a small closed definition through JASM/Rasm.
5. Treat `CODE:` and raw byte pokes as opaque.
