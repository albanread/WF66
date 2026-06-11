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

The moat: WF65 is a frozen, byte-for-byte **state correctness oracle** — same source must produce the same final stack, touched memory, `rbp`, and scratch state, even when WF66 emits different, faster bytes. Honest ceiling: per-definition, not whole-program — Forth's mutable, runtime-patched dictionary forbids whole-program analysis, a ceiling VFX shares. Full thesis in [wf66_compiler.md](wf66_compiler.md).

## Carries Over

- JASM/Rasm native assembler and loader.
- STC runtime: every primitive is a `proc ... endp` body; CPU `call`/`ret` is dispatch.
- Register conventions: RAX=TOS, RBP=DSP, RBX=UP, RSP=rstack, R12=save slot.
- Dictionary, vocabularies, `CREATE`/`DOES>`, live REPL, `lib/core.f`, and the Rust harness.
- `CODE:` and raw code emission as explicit opaque escape hatches.

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

## Non-Goals

WF66 is not a whole-program batch compiler. That would require amputating the user-programmable compile-time semantics that make Forth Forth. WF66 is an optimizing interactive compiler: per-definition, mutable-dictionary aware, and REPL-friendly.

## Test Strategy

- Preserve WF65 behavior with the existing harness and data-driven tests.
- Add pure IR tests for immediate words and user-defined IR macros.
- Add backend golden tests for generated JASM/MASM text and native bytes.
- Add differential definition tests: source in, byte-for-byte equivalent final stack/touched-memory/register state out.

## First Slice

1. Add an IR builder object to the compiling state.
2. Redirect `LITERAL`, `COMPILE,`, `POSTPONE`, and `;` to the builder/finalizer.
3. Keep structured control (`IF`/`ELSE`/`THEN`, loops) on the settle-to-canonical compatibility path for Phases 0-3; define the IR-level mark API, but land CFG block/edge construction in Phase 4.
4. Lower a small closed definition through JASM/Rasm.
5. Treat `CODE:` and raw byte pokes as opaque.
