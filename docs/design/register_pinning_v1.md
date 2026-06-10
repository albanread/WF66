# Register pinning (hot-variable scalar replacement) — Design v1

Status: design / build plan.
Scope: keep a `hotvariable`'s **value** in a register across an eligible loop,
so `@`/`!`/`+!` inside the loop become register moves with zero memory traffic.
Builds the loop-scoped op-buffer that agenda item #2 (the general `FopOp`
optimizer) will reuse.

This supersedes the address-only meaning of `hotvariable`. Today
`hotvariable hv` only inlines the *address push* (`lea rax,[rip+disp32]`,
`kernel/compile.masm` `inline_var_comp`); every `hv @` still loads memory. v1
makes `hotvariable` a **hint**: the compiler may promote the variable's value
into a register for the duration of a loop it judges worthwhile.

---

## 0. Decisions (locked)

- **`hotvariable` is a hint, not a directive.** The declaration marks a variable
  as a candidate; the compiler decides whether and where to pin it. No new
  syntax; no per-loop annotations.
- **Hot-variable values are stale after an exception.** If a pinned loop body
  `THROW`s, the unwind skips the write-back and the variable's *memory* keeps
  its pre-loop value while the register copy is lost. This is documented
  behaviour, not a bug: a thrown hot loop is already exceptional. Code that must
  observe a hot variable after a possible throw should not rely on a pinned
  loop's partial progress.
- **Foundation first.** Build the record → analyze → replay op-buffer properly;
  it is the substrate, not a one-off peephole.

---

## 1. The optimization

Scalar replacement of a memory cell across a loop:

- **Prologue** (loop entry, after `do` setup): load the body into its register —
  `mov <reg>, [hv-body]`. Skipped for write-first variables.
- **Body**: `hv @` → push `<reg>`; `hv !` → `mov <reg>, rax` + drop;
  `hv +!` → `add <reg>, rax` + drop. No memory traffic.
- **Write-back** at every exit (normal `loop` fallthrough, each `leave`, any
  `exit`/`;` inside the body): `mov [hv-body], <reg>` for read-write /
  write-only vars. Read-only-in-loop vars need no write-back.

Per-variable classification falls out of scanning the body:

| Class            | Load on entry | Write-back on exit | Example (mandel) |
|------------------|---------------|--------------------|------------------|
| read-only        | yes           | no                 | `mi-cx`, `mi-cy` |
| write-only       | no            | yes                | (rare)           |
| read-write       | yes           | yes                | `mi-zx`, `mi-cnt`|

---

## 2. The register budget (audited)

Registers that survive a call-free loop body, from reading the inline emitters:

- **Always reserved:** `rax`(TOS), `rbp`(DSP), `rbx`(UP), `rsp`(rstack),
  `r12`(Win64 RSP save), `r15`(LP).
- **`loop` back-edge** (`inline_loop_comp`, compile.masm:2320): `add [rsp],1; jno`
  — no GP register touched. Pins survive the back-edge.
- **`i`/`j`** (compile.masm:1341/1369): `rax`,`rbp`,`rsp` only.
- **`do` setup** (compile.masm:1398/1424): clobbers `rdx`,`rcx`,`r8` — once, at
  entry, before the prologue load runs.
- **Stack/mem/r-stack inline ops** (`swap`,`!`,`-`,`rot`,`c@/c!`,`>r`/`r>`):
  scratch in **`rcx`**.
- **`/ mod /mod */`**: `idiv`/full `imul` clobber **`rdx`** (and `rax`).
- **`+loop`/`-loop`**: `rax`,`rbp`,`rsp`.

So the body's clobber set is `{rax,rbp,rcx}` ∪ `{rdx}` (if division) ∪
`{rdx,r8}` (if a nested `do`). The pinnable pool is the complement.

**Pool: `r9, r10, r11`** (fixed — not widened). The inline op bodies clobber
only `{rax,rbp,rcx}` ∪ `{rdx}` (division) ∪ `{r8}` (`do` setup), so the pool
survives any call-free body, including nested loops.

**Pool-safety audit (Phase 3).** The danger is not the inline op bodies but the
runtime *helpers a loop body still calls*: a standalone literal compiles to
`call do_lit`, and `do_lit`/`do_flit`/`do_slit`/`do_clit` originally used
`r9`/`r10`/`r11` as scratch — clobbering the pool on every literal. They were
moved to `rcx`/`rdx`/`r8`/`rsi` so a pinned loop may contain literals (and
nested loops, whose `do` bounds are literals). `unloop` (`two_rdrop`, emitted at
loop close) uses only `rcx`/`rsp` — already pool-safe. Any *other* runtime call
is a `compile_comma`, which the analysis already disqualifies. Widening the pool
to `rsi/rdi` was considered and declined: it buys little and those registers are
used as scratch by string/move helpers.

---

## 3. Eligibility

A loop pins variable `hv` only if **all** hold:

1. **Call-free body** — no `E8` call to a non-inlined word. Caller-saved pins
   die across a call, so one user-word call disqualifies the whole loop.
2. **`hv` used only as `hv @` / `hv !` / `hv +!`** — never as a bare address
   that escapes (passed to another word, `hv 4 + @`, stored, `execute`d). A
   bare-address use means memory could be aliased behind the register's back →
   that variable is not pinned (others in the loop may still be).
3. **A pinnable register is free.** Otherwise the variable falls back (§7).

`hotvariable` is the programmer's contract that (2) is intended; we still verify
and fall back safely.

---

## 4. Architecture: record → analyze → replay

The kernel compiler is single-pass and byte-emitting, but at `do` the body is
unknown. We therefore **buffer the loop body, analyze it, then emit** — the
v1-optimizer span collector, scoped to a loop (single entry, the loop construct
delimits it).

Division of labour (keeps the brittle logic in Rust, the mechanical work in the
kernel where emission already lives):

```
?do / do  ──►  kernel: begin recording (buffer xt/lit stream; suppress emit)
   body   ──►  kernel: every compiled word/literal is appended, not emitted
loop/+loop ─►  kernel: stop recording at the matching loop
              kernel ──win64_call──► rt_pin_analyze(up, buf, len)
                                       Rust: eligibility, escape, classify,
                                             allocate regs, enumerate exits
                                       ──► writes a PIN PLAN to the user area
              kernel: emit load prologue (from plan)
              kernel: REPLAY the buffer through the normal compile dispatch,
                      with pinned-var accesses rewritten to register moves
              kernel: emit write-backs at each exit (from plan)
```

Replay re-uses **all** existing compile logic (inline helpers, folds,
if/then/loop resolution) — no duplication. The only injected behaviour is
(a) pinned-access substitution and (b) load/write-back insertion, both driven by
the plan.

### 4.1 The dispatch hook

In compile state the dispatch reaches `.compile_ct` (interp.masm:144): xt in
`rdx`, compile action in `rcx`, then `call rcx`. The literal path is
`.got_number`; immediate words take `.compile_exec_*`. We add one gate at these
points:

```
if (user_PIN_RECORDING) {
    append (kind, xt|value) to user_PIN_BUF      ; do NOT call rcx / emit
    adjust loop-nesting counter for do/loop family
    goto .after_word
}
```

Recording is **all-or-nothing**: in record mode immediate words (if/then/leave,
and do/loop themselves) are recorded too, never executed, so the data-stack
control marks are not disturbed until replay reproduces them. The matching
`loop` is found by a nesting counter (`?do`/`do` +1, `loop`/`+loop`/`-loop` −1);
reaching depth 0 ends the recording. `;`/EOF while recording = malformed
(do-without-loop) → flush as a plain replay with no pins.

### 4.2 The op buffer (user area)

A flat array of records; minimal because replay re-dispatches:

```
record = { u8 kind ; i64 payload }      ; kind: 0=Lit(payload=value)
                                         ;       1=Word(payload=xt)
```

User-area cells: `user_PIN_RECORDING` (flag), `user_PIN_BUF` (base),
`user_PIN_BUF_LEN`, `user_PIN_BUF_CAP`, `user_PIN_NEST`. The buffer lives in the
RW data region (it is compile-time scratch, never executed).

### 4.3 The pin plan (Rust → user area)

```
plan = {
  u32 count ;
  entries[count] = { i64 body_addr ; u8 reg ; u8 class /*RO/WO/RW*/ } ;
  // exits are handled structurally during replay (see §5); the plan only
  // needs the per-var reg + class. The replay knows the exit sites because
  // it is the one emitting leave/loop/exit.
}
```

`rt_pin_analyze` returns count=0 (⇒ identity replay, no pins) whenever the loop
is ineligible.

---

## 5. Emission (replay)

1. **Prologue:** for each RO/RW entry, emit `mov <reg>, [body_addr]`
   (RIP-relative when in range, else abs — same range check as `inline_var_comp`).
2. **Body replay:** re-dispatch each record. For a pinned `hv`, its access pair
   is rewritten:
   - `Word(hv) Word(@)`  → `mov [rbp-8],rax; sub rbp,8; mov rax,<reg>`  (push reg)
   - `Word(hv) Word(!)`  → `mov <reg>,rax; mov rax,[rbp]; add rbp,8`     (store+drop)
   - `Word(hv) Word(+!)` → `add <reg>,rax; mov rax,[rbp]; add rbp,8`     (add+drop)
   Implementation: during replay the pinned variable's `dh_comp` is the
   pin-aware helper (it reads the plan to find `<reg>` and peeks the next record
   to confirm `@`/`!`/`+!`, consuming both).
3. **Write-back at exits** — the replay emits these because it emits the exit
   words:
   - replaying `loop`/`+loop`/`-loop` (normal exit): emit write-backs **after**
     the loop resolves, on the fallthrough path, for RW/WO entries;
   - replaying `leave`: emit write-backs **before** the `leave`'s branch;
   - replaying `exit`/`;` inside the body: emit write-backs first.

---

## 6. Correctness invariant

> **Boundary materialization.** At every point control may leave the loop —
> normal exit, each `leave`, any `exit`/`;`, and (modulo the documented throw
> caveat) any observer — the variable's memory at `body_addr` must equal the
> value the STC build would have stored. Within the loop there is no observer:
> it is call-free and touches only `rax`/`rbp`/`rcx`/the pinned regs.

Why sufficient: a call-free loop has no inner observer of the data/var memory;
all exits are enumerable because replay emits them; read-only vars never diverge
(memory is never written). The one acknowledged gap is `THROW` unwinding past
the write-backs — covered by the locked stale-after-exception decision and
documented for users.

Mandatory safety net: **differential testing against the STC oracle** (compile
each test word both pinned and unpinned, run over a value table, assert
identical final stack + variable memory), plus randomized fuzzing of
pin-eligible loop bodies. This gates default-on.

---

## 7. Fallback tiers

Per variable, best applicable wins:

- **Tier 2 — value-pinned** (this design): no loop memory traffic. Needs
  call-free + no-escape + a free register.
- **Tier 1 — address-pinned:** `lea <reg>,[body]` once at entry; `@`/`!` use
  `[<reg>]`. Saves the per-access address materialization; still hits memory.
  Safe even if the address escapes within the loop. Used when value-pinning is
  unsafe but accesses are frequent, or registers are scarce.
- **Tier 0 — today:** inline `lea` + load/store per access. The unconditional
  fallback.

Out-of-register variables drop to Tier 1 or Tier 0; the loop still pins what
fits.

---

## 8. Build order (phased, each independently testable)

**Phase 0 — record/replay identity substrate (de-risk the dispatch surgery).**
User-area buffer + `user_PIN_RECORDING` + nesting counter; the §4.1 hook;
`?do`/`do` begins recording, the matching `loop` ends it and **replays with no
pinning**. Replay must be an identity transform.
*Test:* compile loop-containing words and assert the dict bytes /
`opt-bench` `byte_length` are **identical** to today, and behaviour is
unchanged. This proves record/replay reproduces the compiler exactly — the
scariest piece — before any optimization logic exists.

**Phase 1 — analysis shim + pin plan (no emission change).**
`rt_pin_analyze`: eligibility, escape detection, RO/WO/RW classification,
register allocation from `{r9,r10,r11}`, write plan to the user area. Replay
still identity; plan only logged (debug env var).
*Test:* Rust unit tests over synthetic buffers (classification, escape, pool
exhaustion); eyeball the plan for `real-mandel-iter`.

**Phase 2 — emit with pinning (the payoff), flag-gated.**
Prologue load, access substitution, write-back at all exits per §5.
*Test:* behavioural parity vs STC oracle for `mandel-iter` across inputs;
extend the `hotvar_inline` eval test; `opt-bench` shows the call/byte/memory
drop on `hotvar.f`/`real-mandel-iter`; differential fuzzing of random
pin-eligible loops.

**Phase 3 — widen the loops (done).**
`begin…until` / `begin…while…repeat` / `begin…again` recording triggers
(`begin` opens, `until`/`again`/`repeat` close — same back-target-at-`[DSP]`
structure as `do`, no `?do` skip). Nested loops work unchanged: the pool is
never clobbered by inline op bodies or the `do`/`loop` machinery. The pool stays
`r9/r10/r11` (not widened, by decision); the literal helpers `do_lit`/`do_flit`/
`do_slit`/`do_clit` were moved off the pool so a pinned loop may contain literals
and nested loops. Tier-1 (address-pin) fallback deferred — out-of-register vars
simply stay unpinned. Verified by differential tests (do/?do/begin/while/repeat/
nested, RW/RO/WO, +!, multi-var, zero-iteration ?do).

**Phase 4 — measure, document, default-on (done).**
Measured: `hot-sum` (2-access `?do`) drops ~10% (168→152 cyc/iter) pinned vs
unpinned; the win scales with access density. Broad differential is green: 8
hand-written cases + an 80-case deterministic fuzzer (random do/?do bodies vs
the STC oracle). Documented the stale-after-exception contract in
`docs/memory-map.md`. **Default-on**: pinning is enabled by default
(`WF64_NO_PIN` disables) — `hotvariable` is the programmer's opt-in hint, the
blast radius is limited to hotvariable-declared vars, and boot/`lib/core.f` use
no hotvariable loops.

---

## 9. Risks

- **Dispatch surgery** is in the most delicate code. Mitigation: Phase 0's
  byte-identity test before any optimization logic.
- **Pool correctness** rests on the inline-body register audit. Mitigation:
  start with `{r9,r10,r11}` (rock-solid), widen only after the audit; let the
  analyzer union actual per-op clobbers.
- **Compile-time cost**: eligible loops compile their body twice (record +
  replay). Compile-time only, runtime unaffected; acceptable.
- **Silent stack corruption** from a mis-emitted write-back or substitution.
  Mitigation: the differential oracle + fuzzing gate default-on; until then the
  feature is flag-gated.
```
