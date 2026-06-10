# Rasm migration — work journal

Running log of the LLVM→Rasm migration (replacing LLVM-MC + MCJIT in the
`wfasm`/JASM toolchain with a native Rust assembler + loader). Newest entries at
the bottom. Plan: [rasm-replace-llvm.md](rasm-replace-llvm.md).

Two git repos move together: **WF65** (`e:/WF65`) and **JASM/wfasm**
(`E:/JASM/rust`).

---

## Sprint 0 — oracle + seam (DONE)

- **Golden oracle** (WF65 `efd83f3`). `src/golden.rs` + `golden-capture` bin +
  `bench/golden/kernel.json`: 514 kernel symbols' bytes captured from the live
  LLVM build, ASLR-normalized (only out-of-region/extern displacements zeroed),
  409 with extern-free `raw` goldens. `with_kernel` appends a
  `__rasm_kernel_end__` sentinel so every symbol has a known extent.
  `tests/golden_oracle.rs` re-captures + asserts a byte match (faithful +
  deterministic) and checks the self-inspected leaves (`dup_`, `plus`) carry a
  raw golden ending in `c3`. Irreplaceable — captured before any LLVM removal.
- **Backend seam** (JASM `bb61dba`). `src/backend.rs`: `Encoder` (text →
  `EncodedModule{code,symbols,relocs,externs}`) + `Loader` traits + `Reloc`/
  `RelocKind`. `Jit` implements `Loader` (the "LlvmJit").
- **Feature gate** (JASM `243ebe0`). LLVM behind default-on `llvm` feature;
  `cargo build --no-default-features --lib` compiles wfasm with zero LLVM dep.
  `bind_externs` and `register_jit_procs` generalized onto `Loader`.
- All green throughout: 369 harness + 146 lib + gc + 2 golden + static gate.

### Key facts banked for Sprint 1
- Loader contract (mirror of `Jit`): `add_asm` / `declare_fn` /
  `define_extern_fn(name,argc,addr)` / `lookup_addr` / `lookup_fn`.
- 3 reloc kinds, complete: `BranchRel32` (call/jmp/jcc), `RipRel32`
  (`lea [rip+label]`, ~41 sites), `Abs64` (data, ~unused). Externs reached via
  `call rel32`. User-area globals are `[UP+off]` (register-relative, no reloc).
- Each `proc()` emits an 8-byte `.quad 0` xt-metadata cell before its label
  (`XT_META_OFFSET=8`), written at boot by `write_primitive_xt_backref`.
- ~205 distinct (opcode, operand-form) encodings; hardest = SSE2+REX.R/xmm15
  island (FTOS), branch relaxation, RIP-rel disp32, ModRM `[rbp]`/`[rsp]` traps,
  imm8-vs-imm32 group-1 ALU.
- Self-inspection byte contract (behavioral, not cosmetic): `inline_leaf_comp`
  (compile.masm:980) byte-copies leaf bodies; T3 peephole (216-231) matches
  `48 89 45 F8`/`48 83 ED 08` to fuse `dup +`.

---

## Sprint 1 — native loader + iced encoder (IN PROGRESS)

### Decision: de-risk the loader first, driven by EncodedModule directly
The front-end exposes a resolved `Vec<Token>` (via `asm::emit`) but it's
token-level, not instruction-level — any encoder still needs a parser. Since the
owned `RasmEncoder` is the real target, an iced *text*-parser is throwaway. So I
brought up the genuinely-new **loader** first, driven by hand-built
`EncodedModule`s, before touching any encoder. This isolates the scariest risk
(executing relocated JIT code that calls back into Rust) on a controlled input.

### Increment 1: NativeJit loader core (JASM, committed)
`src/native.rs` (`#[cfg(windows)]`, no LLVM): `NativeJit::new_near(anchor, cap)`
uses `VirtualAlloc2` + `MEM_ADDRESS_REQUIREMENTS` to reserve **RW** code space
within ±~1.75 GB of an anchor (so `call rel32` reaches host externs — the same
windowing as WF65's `alloc_forth_region`). `load_module(&EncodedModule)` copies
code (16-byte aligned) and records symbols at final addresses; `define_extern`
binds host fns; `finalize()` applies all relocs then flips the region to **RX**
(never RWX); `lookup` returns symbol addrs.
- 3-kind relocator: `BranchRel32`/`RipRel32` (rel = target+addend - (site+4),
  range-checked to i32) and `Abs64`.
- **Loader proof test** `load_relocate_execute_with_host_callback`: places a
  2-symbol module — `helper(x)=x+x` (leaf) and `entry(x)=helper(host_inc(x))`
  with an INTERNAL branch reloc (`call helper`) and an EXTERN branch reloc
  (`call host_inc`, a Rust `extern "C"` fn) — relocates, RX-protects, executes:
  `entry(10)==22`, `entry(0)==2`. Proves VirtualAlloc-near + copy + both branch
  reloc paths + W^X RX + execution with host callback.
- Not yet a `Loader` trait impl (that needs `add_asm(text)` → an encoder); for
  now it's the loader-proof API. WF65 default build + suite unchanged.

### Decision: go straight to the owned RasmEncoder (skip the iced bootstrap)
The doc's hybrid used iced as a throwaway bootstrap to de-risk the loader — but
increment 1 already de-risked the loader without iced. So I'm going straight to
the from-scratch `RasmEncoder` (the real deliverable, "implement assembler in
Rust"), using iced's *decoder* + the golden as the correctness oracle. No
throwaway iced-text-parser.

### Ground truth: the kernel's instruction set + syntax
Dumped the assembled kernel (`WF64_DUMP_ASM`). Clean MC Intel syntax:
`.intel_syntax noprefix`, `.text`, `.quad 0`, `.globl NAME`, `NAME:`,
`name$$local:` (proc-local label mangling). `call name` for BOTH internal and
externs (no `&` in the text — the loader resolves the name as symbol-or-extern).
`lea r8, [rip + label]`. `qword ptr [..]` size prefixes. Spaced operators
(`[rbp - 8]`, `[rbx + 4632]`). ~50 mnemonics by frequency: mov 2205, ret 731,
add 522, call 462, sub 428, cmp 299, and 207, jne 172, lea 169, test 155,
jmp 136, jz 134, je 95, xor 91, push 52, pop 48, inc 39, movsd 35, jae 33,
jnz 31, jc 24, sbb 22, movups 20, jb 20, movsx 19, ja 19, shl 17, or 17,
jbe 16, rep 14, neg 10, movq 9, sar 8, idiv 8, shr 7, dec 7, not 5, movzx 5,
cqo 5, cmova 5, xchg 4, ucomisd 4, repz 4, jl 4, imul 4, cmovg 4, addsd 4,
xorpd 3, subsd 3, std 3, + mulsd/divsd/cvt*/setcc/etc.

### Increment 2: RasmEncoder parser (JASM, committed)
`src/rasm/parse.rs`: text line → `Line` (Empty / Directive / Label / Insn).
Operand model: `Reg{class,num}` (R8/16/32/64/Xmm), `Mem{size,base,index,scale,
disp,rip_sym}`, `Imm(i64)`, `Sym(name)`. Handles `[base]`, `[base±disp]`,
`[base+index*scale(±disp)]`, `[rip + sym]`, `qword/dword/word/byte ptr`, hex/
decimal/signed/`_`-sep immediates, `.intel_syntax/.text/.globl/.quad/.byte/
.zero/.align/.p2align/.ascii/.asciz`, and `$$`-mangled local labels. 6 parser
tests green. Pure Rust (anyhow only), always-compiled.

### Increment 3: RasmEncoder encoder core (JASM, committed)
`src/rasm/encode.rs`: the REX/ModRM/SIB/disp/imm machinery + a foundational
instruction set → `Encoded{bytes, fixups}` (fixups = Rel32 branch / RipRel32,
resolved later by the two-pass driver). Coverage so far: `ret/nop/cqo/leave/
std/cld`; `mov` (r/m,r · r,r/m · r,imm32 via C7 · movabs r,imm64 via B8+r ·
m,imm); group-1 ALU add/or/adc/sbb/and/sub/xor/cmp (r/m,r `01` · r,m `03` ·
r/m,imm8 `83 /ext` · imm32 `81 /ext`); `lea`; `push`/`pop` r64 (+REX.B);
`call`/`jmp`/`jcc` (rel32 + fixup); `test`.
- **Byte-identical to LLVM-MC** on the golden's self-inspected leaves:
  `mov [rbp-8],rax` = 48 89 45 F8, `sub rbp,8` = 48 83 ED 08,
  `add rax,[rbp]` = 48 03 45 00, `add rbp,8` = 48 83 C5 08. The ModRM traps are
  right: `[rbp]`→disp8=0, `[rsp]`→SIB, `[r13]`→disp8+REX.B, `[r12]`→SIB+REX.B.
- Tests: exact-byte (vs golden) + iced-decoder round-trip (decode our bytes,
  format Intel, assert the intended instruction). iced added as a wfasm
  **dev-dependency only** (decoder+intel formatter) — not a runtime dep. 5 tests.
- TODO next: branch relaxation (rel8 vs rel32 to match MC), the SSE2+REX.R/xmm15
  family (movsd/addsd/movq/cvt*/ucomisd/movups/xorpd), shifts, mul/div,
  movzx/movsx/movsxd, inc/dec/neg/not, setcc/cmovcc, rep/repz string ops; then
  the two-pass driver → EncodedModule and the full-kernel golden diff.

### Increment 4: SSE/FTOS island + unary + shift families (JASM, committed)
`encode()` now dispatches to family helpers first. Added: the SSE2 island —
movsd (load F2 0F 10 / store F2 0F 11), movups (0F 10/11), addsd/subsd/mulsd/
divsd (F2 0F 58/5C/59/5E), ucomisd (66 0F 2E), xorpd (66 0F 57), movq xmm<->r64
(66 REX.W 0F 6E/7E), cvtsi2sd (F2 REX.W 0F 2A), cvttsd2si (F2 REX.W 0F 2C); the
one-operand F7/FF group (not/neg/mul/imul/div/idiv/inc/dec); shifts shl/sal/shr/
sar/rol/ror by 1 (D1), imm8 (C1), cl (D3). Byte-identical to MC on the FTOS
forms: `addsd xmm15,[rcx]` = f2 44 0f 58 39, `movsd [rcx-8],xmm15` =
f2 44 0f 11 79 f8 (REX.R for xmm15), `shl rax,1` = 48 d1 e0 (D1, not C1 imm=1).
13 rasm tests green.
TODO: movzx/movsx/movsxd, setcc, cmovcc, multi-operand imul, xchg, rep/repz
string ops, sete/setb; then branch relaxation + two-pass driver -> EncodedModule
+ full-kernel golden diff.

### Increment 5: movzx/movsx/setcc/cmov/imul2/xchg/xadd/rep (JASM, committed)
Added the remaining kernel families: movzx/movsx (src-size dispatch 0F B6/B7/BE/BF),
movsxd (63), setcc (0F 90+cc), cmovcc (0F 40+cc), 2-operand imul (0F AF), xchg
(87), xadd (0F C1), rep/repz string ops (movsq/stosb/...). Shared cc_code nibble
for jcc/setcc/cmovcc. Byte-identical: sete cl=0F 94 C1, rep movsq=F3 48 A5. The
per-instruction encoder now covers the whole kernel ISA. 14 rasm tests green.
NEXT: two-pass driver (label offsets + branch relaxation rel8/rel32 to match MC
+ relocs) -> EncodedModule; then the full-kernel golden diff.

### Increment 6: two-pass driver + RasmEncoder (JASM, committed)
`src/rasm/assemble.rs`: text -> Vec<Item> -> branch-relaxation fixpoint (internal
jmp/jcc start short rel8, grow to rel32 on i8 overflow, iterated; call always
rel32; externs always rel32 — mirrors MC start-short/relax-on-overflow) -> emit
EncodedModule{code, symbols(.globl only), relocs, externs}. Internal targets
resolved in-module (no reloc); refs to undefined names (host rt_*) -> Reloc +
extern. RIP-rel internal patched, extern -> reloc. `RasmEncoder` implements the
backend::Encoder trait. 18 rasm tests green (internal-call no-reloc, extern-call
reloc, short<->near relaxation, rip-rel internal+extern). FULL RasmEncoder
PIPELINE COMPLETE: text -> EncodedModule.
NEXT: full-kernel golden diff (assemble real kernel, compare every symbol vs
bench/golden/kernel.json) to gate byte-identity and surface form divergences.

### Increment 7: BYTE-IDENTICAL — RasmEncoder matches LLVM-MC on the whole kernel
Added `rasm-diff` (WF65 bin, opt-metrics): assembles the real kernel with
RasmEncoder and diffs every symbol vs bench/golden/kernel.json (both normalize
extern fields; cuts on the golden's symbol set since untracked .globl procs like
pin_begin_maybe get lumped). Drove the iterate-fix loop to ZERO divergences
(533 symbols):
- parser: constant-expr eval (2*8) in mem disp AND immediates; char literals.
- new insns: rcl/rcr, popcnt/lzcnt/tzcnt/bsr/bsf, int3/int, syscall/cpuid/rdtsc/
  cdqe/cwde/cdq/pause/clc/stc/cmc/sahf/lahf, indirect jmp/call (FF /4,/2),
  3-operand imul.
- OPERAND SIZES: full B8/B16/B32/B64 handling for mov/alu/test (8-bit opcode-1,
  66 prefix for 16-bit, REX.W for 64; mov-imm B0/B8/C6/C7 by size).
- byte-identity subtleties that MC does: [rbp+idx*8] forces mod=01 disp8=0 even
  with SIB; accumulator short forms (cmp al,imm=3C; rax,imm32=3D); 83 over 81
  when imm fits i8; xchg reg=first-operand.
rasm-diff: BYTE-IDENTICAL. The owned encoder reproduces MC exactly -> NO re-bless,
self-inspection contracts safe by construction. 18 rasm + 369 harness + lib + gc
+ golden + gate all green.
NEXT: gate byte-identity as a CI test; wire NativeJit+RasmEncoder into a
Loader-trait path; boot the kernel under the native backend.

### Increment 8: byte-identity locked as a CI test (WF65, committed)
`golden::rasm_divergent_symbols()` (shared by the binary + test) assembles the
kernel with RasmEncoder and reports per-symbol divergences vs the golden.
`tests/golden_oracle.rs::rasm_encoder_byte_identical_to_llvm` asserts ZERO
divergences — a regression gate so the from-scratch encoder can never silently
drift from MC. 3 golden tests green.

--- SPRINT 1 STATUS ---
DONE: NativeJit loader core (place+relocate+W^X, proven via host-callback exec);
RasmEncoder COMPLETE and BYTE-IDENTICAL to LLVM-MC across all 533 kernel symbols
(parser + full ISA + branch relaxation + two-pass driver), gated by a CI test.
The highest-risk piece of the whole mission (per the design study) is done.
REMAINING (Sprint 1 capstone): wire NativeJit+RasmEncoder into a Loader-trait
path in with_kernel (behind a feature), bind externs + register SEH procs, BOOT
the kernel under the native backend, and run the full 369-test behavioral suite
LLVM-free. Then Sprints 2-4 (differential gates, CODE: open-scope, delete LLVM).

### Increment 9: FORTH BOOTS + RUNS FROM OUR OWN ASSEMBLER (capstone)
Wired NativeJit+RasmEncoder into with_kernel behind env WF64_RASM. NativeJit now
implements the Loader trait (add_asm buffers text; lookup_addr lazily assembles
via RasmEncoder, places, relocates, RX-protects). Session.jit -> Box<dyn Loader>;
bind_externs/register_jit_procs take &mut dyn Loader; forth_main = transmute of
the resolved kernel_addr (no lookup_fn). Two fixes to boot the real kernel:
- FAR-CALL STUBS: kernel32 imports (Sleep etc.) are >2GB away; rel32 cannot
  reach, so finalize appends a 12-byte movabs rax,target/jmp rax stub per far
  target (like RTDyld) and points the rel32 at it.
- alloc the kernel region ANYWHERE roomy (not crowded against the host image);
  all externs go through stubs, so there's no rel32-to-host constraint, fixing
  the flaky alloc_forth_region/alloc_jit_arena under native.
LET/CODE: are PARKED (their runtime rt_*_compile still use LLVM Jit instances,
unaffected by the env).
RESULT: WF64_RASM=1 boots Forth on the native macro assembler (NO LLVM in the
kernel path) and runs everything -- recursion (fib=89), do-loops, char emit,
floats (FTOS), variables, strings -- robustly across runs. THE FULL 369-TEST
BEHAVIORAL SUITE PASSES under WF64_RASM=1 (byte-identical kernel => identical
behavior). Sprint 1 capstone DONE.
NEXT (Sprint 2-4): make native the default + run the suite both ways in CI;
un-park LET/CODE: onto RasmEncoder; SEH unwind for native; delete LLVM.

### Increment 10: un-park CODE: onto RasmEncoder ("code is just RASM")
rt_code_compile_body now branches on WF64_RASM: native path assembles the
wrapped CODE: body with wfasm::rasm::assemble and bump-allocates the result into
the rel32-near code arena (new CodeArena::alloc, no header reserve — the proc's
own .quad 0 is the xt-metadata cell at fn_addr-8). CODE: words are self-contained
asm so there are no relocs/externs to resolve; if any extern is referenced the
native path errors clearly (kernel-symbol resolution deferred). LET stays on LLVM
(needs a real Rust compiler — parked).
Verified native: CODE: add3=add rax,3 -> 43; sq=imul rax,rax -> 49; a colon word
`both` calling two CODE: words via rel32 -> 28. Full 369-test suite green under
WF64_RASM=1, all 5 CODE: tests now on the native path (defines, embed-in-colon,
macro vocab, invalid-asm-throws, unterminated-error). Default LLVM build green too.

### Increment 11: LLVM-FREE BUILD — 67MB smaller, self-contained 968KB exe
Gated all WF65 LLVM refs behind a default-on `llvm` feature (wfasm/llvm):
build_boot_loader (native-only without llvm), try_compile_let (LET errors
cleanly without llvm — parked), the CODE: LLVM branch (native place_native
shared), LET_JITS/CODE_JITS caches, let_lang integration_tests. Moved CodeArena
out of the llvm-gated jit.rs into wfasm::arena (LLVM-independent bump allocator;
the MCJIT callbacks stay in jit.rs).
RESULT (cargo build --no-default-features):
- wf64.exe LLVM-FREE: 968 KB, self-contained, NO LLVM-C import.
- wf64.exe LLVM: 1.08 MB + ships a 67.7 MB LLVM-C.dll.
- ~70x smaller shipping footprint; the 67.7 MB DLL dependency is GONE.
LLVM-free binary boots Forth and runs arithmetic/colon/CODE:/floats/strings
(all rt_* runtime calls work via stubs). No-LLVM harness: 357/369 pass; the only
12 failures are let_dsl_* (LET parked). The IDE (wf64-ui) also builds LLVM-free.
Default (LLVM) build + suite unregressed.

### Increment 12: native LLVM-free IDE release — confirmed working
Packaged release/wf64-native/ (wf64-ui.exe 9.3 MB LLVM-free + kernel/lib/demos/
docs + README; NO LLVM-C.dll). 12 MB total vs the existing LLVM release at 76 MB.
User confirmed the native IDE runs "exactly as before" — REPL, colon defs,
floats, strings, GC, graphics pane/canvas, CODE: all work; LET is the one gap
(parked). The Forth->Rust runtime bridge (rt_* via far-call stubs) works in the
GUI identically to the console. Milestone: WF64 + its IDE run on our own Rasm
assembler with zero LLVM.

## LET on Rasm — plan

Realisation: LET never needed the LLVM ORC compiler. `let_lang::codegen::lower`
already lowers a LET form to **MC-flavour Intel asm text** (a `String`); LLVM was
only the *assembler/loader* for that text — exactly the job Rasm now does for the
kernel and CODE:. So un-parking LET is the same migration as CODE:, plus a few
SSE instructions the kernel never used. Plan:

* **A. Instructions.** Add the SSE ops LET emits but the kernel didn't:
  `sqrtsd`, `minsd`, `maxsd`, `andpd`/`andnpd`/`orpd`, the `cmpCCsd` pseudo-ops
  (`cmpeqsd`/`cmpltsd`/`cmplesd`/`cmpneqsd` = `cmpsd` + imm8 predicate),
  `roundsd` (3-byte opcode + imm8), and `movabs` (forced imm64). Parser: the
  `xmmword ptr` size, multi-value `.quad a,b`, inline `label: directive`, and
  `#` end-of-line comments. Each with exact-byte unit tests.
* **B. Oracle.** A LET differential oracle (`let_oracle`) mirroring the kernel
  golden: lower one LET source to asm text, encode it **both** ways (LLVM-MC via
  `Jit`, and `rasm::assemble`), and byte-compare. LET output has no relocations
  to normalise — libm is baked as `movabs rax,addr; call rax` with an address
  identical in both encoders — so it is a straight memcmp. Drive divergences to 0.
* **C. Native load path.** In `try_compile_let`, a `place_native` that assembles
  the asm with Rasm and copies it into the rel32-near code arena (like CODE:),
  then the existing native `emit_let_trampoline`. No externs/relocs (libm baked,
  const pool reached by internal RIP-rel).
* **D. Un-park.** Make the 13 `let_dsl_*` REPL tests pass under
  `--no-default-features`.

### Increment 13: LET un-parked onto RasmEncoder — byte-identical, LLVM-free
**A.** Added to RasmEncoder: `sqrtsd`/`minsd`/`maxsd` (F2 0F 51/5D/5F),
`andpd`/`andnpd`/`orpd` (66 0F 54/55/56), `cmpeqsd`/`cmpltsd`/`cmplesd`/`cmpneqsd`
(F2 0F C2 /r ib, predicate 0/1/2/4), `roundsd` (66 0F 3A 0B /r ib), `movabs r64`
(forced REX.W B8+r imm64). Parser/assembler: `xmmword ptr` (`MemSize::Xmmword`),
multi-value `.quad a, b` (`Directive::Quad(Vec<i64>)`), inline `label: …` lines
(`split_leading_label` in the assemble loop), and `#`/`;` end-of-line comments
(`strip_comment`). Unit tests: `sse_let_island`, `multi_value_quad`.

One real find from the oracle: `.p2align` padding. LLVM-MC emits **canonical
multi-byte NOPs** (`0f 1f 44 00 00`, `66 2e 0f 1f 84 …`) for code-section
alignment, while Rasm emitted runs of `0x90`. Implemented LLVM's exact NOP table
(lengths 1..10, 11..15 via prepended `0x66`) in `write_nop_padding`. That single
fix took the oracle from 28/35 divergent to **0/35** — every LET source now
encodes byte-for-byte identically under Rasm and LLVM-MC. Kernel golden still
0 divergences (the multi-byte NOPs only ever match LLVM better).

**C/D.** `try_compile_let` now shares source-extraction + `let_lang::compile` +
`emit_let_trampoline` across both backends; `place_native` assembles with Rasm
and bump-allocates into the code arena (`WF64_RASM` forces it under the LLVM
build too, for parity). RESULT: all **13 `let_dsl_*`** tests pass under
`--no-default-features` (previously the *only* native failures). Full suite green
all three ways: native (369 harness + 117 lib + …, 0 fail), LLVM default (369 +
146 + `let_oracle`), and LLVM+`WF64_RASM=1` (native LET path, 13/13). `wfasm`
unit tests 163/163 both feature modes. Milestone: **the LLVM-free build now runs
LET too** — WF64 has no remaining functional gap without LLVM.

### Increment 14: Forth-centric crash dump (SEH situation)
First mapped the situation (multi-agent): STC makes OS/Win64 unwind impossible —
RSP *is* the Forth return stack (real return addresses interleaved with `>r`'d
data/loop cells), primitives are no-prologue leaves, RBP is the DSP not a frame
pointer. The kernel registers zero `.pdata`; a blanket leaf `RUNTIME_FUNCTION`
would make the OS unwinder produce confidently-wrong frames. LLVM and native are
identical here. So the right goal is **not** OS unwind but a Forth-aware dump +
a return-stack-walk backtrace (STC makes that a linear classify scan, no
metadata).

Built it in `wfasm::seh` as one shared renderer `format_forth_dump(CrashRegs,
code, access)` ordered Forth-first per the maintainer's spec: faulting word
(RIP symbolicated) → **DATA STACK** (TOS=RAX + in-memory cells from RBP, with
the cached-TOS off-by-one handled: empty DSP sits one cell above dstack_top) →
**RETURN STACK** word trace (walk RSP→rstack_top; each qword in a registered
code range = a `#k <word+off>` frame, else a `(data)` cell) → key **USER VARS**
(HERE/LATEST/STATE/…) → CPU registers last. Host wires it via
`register_code_range`/`set_forth_dump_info` at boot (kernel extent, CODE:/LET
arena, dict region; data/return-stack regions; var offsets). The GUI crash view
calls the SAME renderer from its captured registers (crashed worker leaks its
region via `ExitThread`, so page-guarded live reads resolve). Hardening: every
read goes through `read_qword` (VirtualQuery page-check → no recursive fault on a
corrupt SP/DSP/UP); offset bounded by successor symbol; exact `offset_of!(
CONTEXT,Rip)==0xF8` assert. Verified on a live `int 3`: the trace reads
`brk_word → interpret_source → quit → forth_main` with real user-var values.
wfasm 166/166 both modes; WF64 suite green both ways. Commits: JASM `cb818e1`,
WF65 `96c56cb`.

STILL OPEN (plan item #4): the CLI REPL still dies on a hard fault (it gets the
rich stderr dump first); only the GUI auto-respawns. Extending the GUI's
worker+supervisor frame-abandonment to the CLI is a separable follow-up.

### Increment 15: NATIVE IS THE DEFAULT
Flipped `default = ["llvm"]` → `default = []` in both `wfasm` and WF65
Cargo.toml. A plain `cargo build`/`cargo test` is now LLVM-free; `--features
llvm` is opt-in. The whole matrix is green and coherent:
- `cargo test` → native (369 harness + 117 lib + …).
- `cargo test --features llvm` → + LET oracle (35/35) + MCJIT path + let_lang
  integration.
- `cargo test --features opt-metrics` → kernel golden, now via the **native**
  live build (renamed `golden_matches_live_llvm_build` → `golden_matches_live_build`):
  it passes, which *proves* the native loader places bytes byte-identically to
  what LLVM-MC produced — the golden is now an LLVM oracle the native build is
  held to, not its source.
- `cargo test --features llvm,opt-metrics` → everything.
`wfasm` 166/166 native. The `llvm`/`opt-metrics` oracles are retained as the
regression net. Releases ship BOTH binaries; native is primary. Commits: JASM
+ WF65 (this increment). Roadmap now: CLI auto-respawn (deferred), then delete
LLVM once the oracles are no longer wanted.
