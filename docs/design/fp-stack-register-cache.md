# FP stack design — cache the top in a register (and why hotfloat was the wrong lever)

Status: **IMPLEMENTED** (single-FTOS cache shipped). Triggered by disabling
`hotfvariable` register pinning (`src/pin.rs ENABLE_FLOAT_PINNING = false`).
Section 1 below describes the *old* memory-based stack; sections 4–6 are what
now ships. See "Measured" and "Verdict" at the end.

## 1. What we have today: a fully memory-based FP stack

The floating-point stack is a separate stack living entirely in the user area:

| cell | addr | role |
|---|---|---|
| `user_FP0` | 0x1210 | empty-stack FP pointer (base) |
| `user_FSP` | 0x1218 | current FP stack pointer (grows down) |
| `user_FP_TMP` | 0x1220 | float-parser scratch (raw IEEE bits) |
| `user_FP_STACK` | 0x1300 | 256-byte stack area = 32 doubles |

Every FP primitive in `kernel/float.masm` follows the same shape — load the
pointer, pull the operands **out of memory** into `xmm0`/`xmm1`, compute, write
the result **back to memory**, store the pointer:

```asm
; f+  ( F: r1 r2 -- r3 )
proc(f_plus)
    mov     rcx, [UP + user_FSP]
    movsd   xmm0, qword ptr [rcx + cell]     ; load r1 from memory
    addsd   xmm0, qword ptr [rcx]            ; + r2 from memory
    movsd   qword ptr [rcx + cell], xmm0     ; store r3 to memory
    add     rcx, cell
    mov     [UP + user_FSP], rcx             ; store FSP
    next()
endp()
```

`xmm0`/`xmm1` are **transient scratch, reloaded on every single op**. There is
no cached top-of-stack. Every float value the program ever touches —
`fvariable`/`hotfvariable` bodies, `fconstant` (read via `f@`), `do_flit`
literals, and the stack slots themselves — is a memory-resident IEEE-754 double.

## 2. The cost, and why hotfloat pinning barely moved it

A `mulsd`/`addsd` is ~3–5 cycles. The work *around* it in each primitive — a
call, an FSP load, one or two operand loads, a result store, an FSP store — is
the bulk of the per-op cost. A tight float loop is **memory- and call-bound, not
arithmetic-bound**.

`hotfvariable` register pinning attacked the wrong term. It kept a few user
*variables* in xmm registers so `f@`/`f!` on them became register moves. Measured
on the FP Mandelbrot that removed 8 ops/iteration (`call_count 42 → 34`) for
**~2%** — because every `f+ f* f- f<` in the body still paid the full FP-stack
memory round-trip. Pinning the variables can't touch the stack traffic, which is
where the time goes. (Contrast the integer `hot-sum`: ~20%, because a call-free
integer loop's bottleneck *is* the values, and r9–r11 hold them.)

## 3. The asymmetry with the data stack

The data stack already does the right thing: **TOS is cached in RAX.** `+` is
`add rax, [rbp]` + a pointer bump — one arithmetic op, operand already in
register, result already in register for the next word. The FP stack has no
equivalent: FTOS is always in memory, so every op round-trips it.

That asymmetry is the whole story. The fix is to give the FP stack the same
treatment as the data stack.

## 4. Proposal: cache FTOS in a fixed xmm register

Reserve **`xmm15` = FTOS** (the float top-of-stack), mirroring `RAX` = TOS.
`xmm6–xmm15` are nonvolatile in the Win64 ABI (callee-saved), already spilled by
`forth_main`, and untouched by every primitive — so `xmm15` threads across word
calls and Windows callouts for free, exactly like RAX threads across STC calls.
`FSP` points at FNOS (the second item); an N-deep stack is `xmm15` + (N−1) in
memory. This is the same "pointer-at-NOS, top-in-register" discipline the data
stack uses with `RBP`/`RAX`.

The ops collapse (single-cache (FTOS) variant):

| word | today (mem-based) | with FTOS in xmm15 |
|---|---|---|
| `f+` | load FSP, 2× movsd, addsd, movsd, store FSP | `addsd xmm15,[FSP]` ; `FSP+=8` |
| `f*` | …mulsd… | `mulsd xmm15,[FSP]` ; `FSP+=8` |
| `fdup` | movsd, movsd, store FSP | `FSP-=8 ; movsd [FSP],xmm15` |
| `fdrop` | load/add/store FSP | `movsd xmm15,[FSP] ; FSP+=8` |
| `f@` | movsd, movsd, store FSP | `FSP-=8 ; movsd [FSP],xmm15 ; movsd xmm15,[addr]` |
| `f!` | movsd, movsd, store FSP | `movsd [addr],xmm15 ; movsd xmm15,[FSP] ; FSP+=8` |

The binary arithmetic ops drop from ~5 memory touches to **one read**, and — the
bigger win — the result stays in `xmm15`, so a chained expression like
`a f@ b f@ f* c f@ f+` flows register-to-register instead of bouncing off memory
between every step. `f-`/`f/` need operand order handled (`xmm15` is the second
operand) with one scratch movsd; still far cheaper than today.

A 2-deep cache (`xmm15`=FTOS, `xmm14`=FNOS) makes `fdup`/`fover`/`fswap` pure
register shuffles, but binary ops then must refill FNOS from memory, so the net
gain over the single cache is small for the extra bookkeeping. **Recommend the
single-FTOS cache** — most of the win, least complexity.

This subsumes hotfloat pinning: with FTOS hot and arithmetic flowing through
registers, keeping a user variable in a register too is redundant.

## 5. Costs and the one tricky invariant

- **Every FP primitive is rewritten.** `user_FSP` is touched at ~71 sites across
  8 kernel files: `float.masm` (36, the core), `fmath.masm` (6, transcendental),
  `compile.masm` (6, `do_flit`/`fliteral`), `interp.masm` (4, the float parser),
  `strings_managed.masm` (6, `f.`/formatting), `igui_gfx.masm` (5, float graphics
  args), `gc.masm` (4), `macros.masm` (4, defs). Mechanical but broad — this is
  the same scale as the RAX=TOS threading, done once.
- **The phantom top slot.** FTOS lives in `xmm15`, valid only when `fdepth ≥ 1`
  (just as RAX is only meaningful when the data stack is non-empty). `fdepth`,
  the empty/underflow checks, and any code that inspects the FP stack from
  outside (`f.s`, the GC if it ever scans FP, save/restore) must account for the
  in-register top. Get the off-by-one right against `FP0` and the rest is
  mechanical. This is the part to design carefully and test first.
- **Boundary code.** `forth_main` already preserves `xmm6–15`. Float callouts
  into Win64 are safe (nonvolatile). The float parser and `f.` printing read/
  write through the FTOS convention instead of poking `[FSP]` directly.

## 6. What shipped

The single-FTOS-in-`xmm15` cache, exactly as in sections 4–5:

- `xmm15` = FTOS; `user_FSP` → FNOS; `fdepth = (FP0 - FSP)/cell` (unchanged).
- Every FP primitive in `float.masm` rewritten; the `do_flit`/`fliteral`/pin
  float-literal paths (`compile.masm`, `interp.masm`), the `fmath` call macros,
  the float parser + `f.`/builder formatting (`strings_managed.masm`),
  `fractal-iter` (`igui_gfx.masm`), and `vec-f@`/`vec-f!` (`gc.masm`) all updated.
- The LET native-word trampoline (`emit_let_trampoline`, `runtime.rs`)
  materialises `xmm15` just below FSP before the call so the LET body still sees
  a contiguous in-memory FP stack, then reloads `xmm15` after.
- FTOS is parked in `user_FTOS_SAVE` across `forth_main` boundaries (the host's
  `xmm15` is restored on exit), so a value left on the FP stack survives between
  REPL lines / direct primitive calls — mirror of how RAX round-trips through
  `[DSP]`.

## Measured

Controlled A/B (same machine, `git stash`), a tight arithmetic loop
`2e f* 0.5e f* 1e f+ 1e f-` × 4M, rdtsc:

| build | cycles | per-iter |
|---|---|---|
| memory-based FP stack | ~331M | ~83 |
| FTOS cache (`xmm15`) | ~306M | ~76 |

**~6–8% on arithmetic-bound float code** — intermediates flow through `xmm15`
register-to-register instead of round-tripping memory between every op. On
`f@`/`f!`-bound code (e.g. `hot-fmandel`, which reads hotfvariables every
iteration) it is roughly neutral: `f@`/`f!` move memory→memory regardless, so the
cache can't help them.

## Verdict — and the honest limit of the stack model

FTOS raises the floor for *incidental* FP (the odd `f+`/`f*` sprinkled through
otherwise-integer code), and it's the right baseline. But it does not — cannot —
make a stack machine good at floating point. A stack model forces every value
through a single top-of-stack; there is no register allocation *across* an
expression, so `a*b + c*d` still shuffles partial results to/from memory between
the sub-products. One cache register removes one level of that traffic; it can't
remove the model.

For FP-*dense* kernels the real answer is to leave the stack model entirely:

- **LET** compiles a `(a b) -> (c) = …` body to native SSE with the compiler
  allocating `xmm` registers freely across the whole expression — no stack
  shuffling at all. `pi * r * r` becomes a couple of `mulsd`s.
- **CODE:** drops to hand-written assembly for full manual control.

So: FTOS for the common case, LET/CODE for the hot case. The stack is convenient,
not fast; when FP performance actually matters, use the tools that bypass it.

The `emit_f*` movsd machinery in `src/pin.rs` stays dormant (float pinning is
disabled and now redundant) but is kept — its RIP-relative encoders are reusable.
