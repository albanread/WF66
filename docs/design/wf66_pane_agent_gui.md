# WF66 — pane-agent GUI configuration (responsive panes, wait-never-block)

Status: **design** (synthesized + adversarially audited, 2026-06-14). The concrete
IDE wiring that realizes [wf66_agents.md](wf66_agents.md): every pane the Forth IDE
opens stays responsive to Forth by multiplexing GUI events to that pane's
cooperative **agent**. One pane ⇄ one agent ⇄ one `child_id`.

> **The invariant.** A pane may **WAIT** — its agent cooperatively yields to the
> operator (`0 (switch-to)` without re-queue) and is re-readied when its wake
> arrives — but a pane must never **BLOCK**: no operation may park the OS worker
> thread (`recv`/`recv_timeout`/`Sleep`/`SendMessageW`) where doing so freezes the
> other panes. *Wait good, block bad.*

This document is grounded in the session's headless proofs: the fiber switch,
scheduler, mailbox, `forth_main` RSP-reroute under a fiber, `SwitchToFiber` from a
separate eval, SEH recovery on an agent fiber, and D2D-from-a-fiber **all work**
([fiber-seh-repro], [gfx-render]). The substrate is sound; only the GUI integration
assumptions were wrong. Code refs use `file:line` at the time of writing.

## 0. Two structural decisions that make the rest fall out

1. **The worker thread becomes the permanent operator fiber and does *only* the
   pump.** It drives the Rust-side `agents::run_slice` ([agents.rs:284](../../src/agents.rs#L284)),
   so the operator stays on its native stack and never re-enters `forth_main`'s RSP
   reroute (the proven-safe property, [agents.rs:281-283](../../src/agents.rs#L281-L283)).
   The operator never calls `session.eval`, never `receive`s, never compiles, never
   makes a GUI round-trip.
2. **The REPL/compiler is its own agent fiber** (aid 1), not the operator. Today the
   worker runs user evals inline ([wf64_ui.rs:254/273](../../src/bin/wf64_ui.rs#L254));
   that is mutually exclusive with "operator on its native stack" and is the
   "compiling freezes every pane" hole. Making the REPL an agent means a long `:`…`;`
   or `include` yields via the safepoint (§6) and other panes keep running, and
   `pause` is valid (an agent yields *to the operator*; on aid 0 it would be a
   self-no-op).

Everything else is registry plumbing and turning each blocking call into a
cooperative wait.

## 1. Thread model (three threads, unchanged in shape)

| Thread | Role | Change |
|---|---|---|
| **GUI thread** | windows + D3D11/D2D/DirectWrite + Win32 pump ([window.rs:738](../../src/igui/window.rs#L738)); the only thread that touches DirectWrite layout. | Producer side unchanged. New: after answering a measure it `channels::push`es a `SurfaceReply` event instead of `replies::deliver`ing into a sync_channel (§5). |
| **`igui-language` supervisor** | `run_supervisor` ([wf64_ui.rs:62](../../src/bin/wf64_ui.rs#L62)). | Structurally unchanged; demoted to the Layer-2 SEH net (§8). |
| **`wf64-worker`** | Becomes the operator fiber (aid 0) via `(agent-init)` and runs the pump. **Never calls `session.eval` again.** | The home of this whole design. |

**The pump** (replaces `run_drain_loop`, [wf64_ui.rs:193](../../src/bin/wf64_ui.rs#L193)),
entirely on the worker, operator on its native Rust stack:

```
loop {
    for f in dead_fibers.drain(..) { DeleteFiber(f); }      // §2 deferred cleanup
    while let Some(ev) = channels::next_event(0) {           // drain, non-blocking
        if route_event(ev) == FrameClose { return; }         // §3
    }
    agents::run_slice();                                     // advance runnable agents once
    flush_pane_output();                                     // §7
    scan_reply_deadlines();                                  // §5 worker-side 5s liveness
    if agents::ready_count() == 0 {
        if let Some(ev) = channels::next_event(50) { route_event(ev); }  // bounded WAIT
    }
}
```

GUI↔worker stays the **proven async seam**: in via `channels` mailbox, out via
`batch::submit` → `PostMessage(WM_PAINT)` ([batch.rs:318](../../src/igui/batch.rs#L318)).
No new thread; no locks on Forth object fields (cooperation ⇒ no concurrent writes).

## 2. Pane ↔ agent ↔ child_id registry & lifecycle

Two worker-thread-owned maps (a `RefCell` in a `thread_local!`, mirroring
[agents.rs:71](../../src/agents.rs#L71)); they are only touched by the pump and by CP
exports, which always run on the worker — so no lock is needed. The GUI thread reads
the existing HWND registry, **never** the agent table.

```
PANE_AGENTS : child_id (i64) ⇄ aid (u64)         // bidirectional
REQ_AGENTS  : request_id (u32) → aid             // async GUI-reply routing (§5)
```

- **aid 0 = operator** (no `child_id`; pump only).
- **aid 1 = REPL/compiler** (`begin receive eval-and-route again`; runs `session.eval`
  on its own fiber).
- **Open.** Forth opens a pane (`gpane-open` → `child_id`) → worker `(spawn)`s the
  pane's controller word, records `child_id ↔ aid`. The controller stashes its
  `child_id` in its TCB (§7).
- **Close.** `Close{child_id}` → worker posts a shutdown message into the aid's
  mailbox (worker-local `(post)`, legal same-thread) → controller cleans up, returns,
  trampoline calls `rt_agent_done` ([agents.rs:334](../../src/agents.rs#L334)). Worker
  unbinds the maps, `batch::forget(child_id)`, and **defers** `DeleteFiber` onto
  `dead_fibers` (never delete the running fiber — UB). The pump deletes them next
  iteration.
- **Respawn-just-this-pane.** On a Layer-1 Forth fault (§8) the worker re-`(spawn)`s
  the same controller xt, rebinds `child_id`, drops the dead aid's `REQ_AGENTS`
  entries. Every other pane is untouched.

## 3. Event multiplexing

Every input `IGuiEvent` already carries `child_id` (channels.rs). `route_event` is a
lookup:

- **Per-pane** (`Key/Char/Mouse/Focus/Resize/Close/Tick/DpiChange`): `aid =
  PANE_AGENTS[child_id]`; encode the scalar fields into mailbox cells (tag + scalars,
  **never a GC string on the hot path**) and worker-local `(post)` (auto-readies the
  agent). `Tick`/`Resize` coalesce at the mailbox tail to bound growth.
- **`ReplSubmit`/`EvalBuffer`**: routed to the REPL agent (aid 1), carrying `child_id`
  so output lands in that transcript.
- **Globals** (`FrameClose/ForthRestart/SetWf66/ThemeChange/Menu`): handled by the
  host directly (today's `handle_worker_event` body). `FrameClose` ends the pump;
  `ForthRestart` reboots + re-`(agent-init)`s.
- **`SurfaceReply{request_id, reply}`** (new, §5): `aid = REQ_AGENTS.remove(request_id)`;
  drop if absent/done; else encode + worker-local `(post)`.

The pump sets **no** `channels` filter, so the `matches_filter`/`matches_target` stash
machinery can never lose a `SurfaceReply`; add a global-true arm for it anyway so
legacy `gpane-next-event` consumers are safe.

## 4. Wait-not-block: the discipline

| Blocking op today | Site | Cooperative-wait replacement |
|---|---|---|
| `replies::wait` `recv_timeout(5s)` (measure / hit-test) | [replies.rs:83](../../src/igui/replies.rs#L83) | **Async await** (§5): record `request_id→self` in `REQ_AGENTS`, `batch::submit`, then `receive`. GUI thread `channels::push`es the reply; pump wakes the agent. |
| Long compute word | [window.rs:937](../../src/igui/window.rs#L937) | Runs on the **REPL agent fiber**; cooperative loops carry explicit `pause`; un-annotated loops yield via the **auto-yield safepoint** (§6). |
| `receive` on an idle pane | [lib/agents.f:24](../../lib/agents.f#L24) | Already the canonical WAIT — no change. |
| **All** agent-reachable `SendMessageW` round-trips | §9 | Reply-returning → async-await; fire-and-forget → `PostMessage` with an **owned heap payload**. **No agent-reachable path may call `SendMessageW`** (debug-asserted). |
| `next_event` idle block | pump | Legal WAIT, and only when `ready_count()==0`; bounded `next_event(50)` so a paused-but-runnable agent is never starved. |

## 5. The async-reply transform (the one new mechanism)

DirectWrite layout lives on the GUI thread, so a measure cannot be answered on the
worker. Replace the synchronous channel with mailbox routing:

1. **New `IGuiEvent::SurfaceReply { request_id: u32, reply: Reply }`** (the
   `SURFACE_REPLY` kind tag already exists in channels.rs). Add the
   `matches_filter`/`matches_target` global-true arm.
2. **CP export (agent fiber):** `alloc_id()`; `REQ_AGENTS[id] = (self)`
   ([agents.rs:371](../../src/agents.rs#L371)); `batch::submit` the
   `MeasureTextRun{request_id}`; then a Forth `(await-reply)` whose body is
   structurally `receive`. **Drop `replies::install`/`replies::wait` on this path.**
3. **GUI side** (`child.rs run_measure`, the `replies::deliver` sites at child.rs
   ~666/717/1045): compute metrics, then `channels::push(SurfaceReply{…})`.
4. **Pump:** map `request_id → aid`, encode metrics into mailbox cells, worker-local
   `(post)`. `next_event` returns the moment the push lands, so the agent resumes the
   same pump-iteration class.
5. **Timeout:** `scan_reply_deadlines()` synthesizes a `Reply::Failed` `(post)` for
   `REQ_AGENTS` entries older than 5s, so a lost reply degrades that one pane to a
   not-found return, never a hang.

**Why the wake is correct — the decisive correctness point.** `rt_mailbox_send`
touches the `thread_local!` agent table ([agents.rs:71](../../src/agents.rs#L71)), so it
**cannot be called from the GUI thread** — a naive "GUI wakes the agent" is a silent
no-op. This design splits the wake: the cross-thread half is `channels::push`
(proven), the `(ready-push)` half runs on the worker inside the pump. **At no point
does the GUI thread call `rt_mailbox_send`.**

**[AUDIT FIX D1] `SurfaceReply` must not ride the lossy event channel.** `channels::push`
is `try_send` on a 1024-slot `sync_channel` and **drops on full**
([channels.rs:212-214](../../src/igui/channels.rs#L212-L214)). A `SurfaceReply` is
liveness-critical (an awaiting agent's only wake), unlike lossy `Tick`/`Mouse`. Give it
a **non-droppable side path**: a dedicated unbounded reply queue the pump drains first,
or push with a small bounded retry. The 5s `scan_reply_deadlines` is the backstop, not
the primary mechanism.

**[AUDIT FIX B3] Boot fast-path uses a latch, not `ready_count()`.** A synchronous
`replies::wait` is acceptable only during boot, when truly nothing else needs the CPU.
Gating on `aid==0 && ready_count()==0` is **wrong** — a set of panes all parked in
`receive` also has `ready_count()==0`, so an operator measure during steady state would
take the 5s blocking path and stall the parked panes' wakes. Gate instead on a one-shot
`BOOTING` latch cleared after the first pane agent spawns.

## 6. Auto-yield safepoint (real kernel work, not a policy swap)

The `INTERRUPT_HOOK` is currently never wired and a tight loop never polls — so this is
new kernel work:

- The inner interpreter's `NEXT` (or a dedicated safepoint primitive in
  `kernel/agents.masm`) checks a per-slice **deadline** (a user-area cell decremented
  per N words, or a wall-clock check every K branches). When the budget is exceeded
  **and `STATE==0`** (not mid-`:`…`;`) it `pause`s; when `STATE!=0` it suppresses
  (the mid-compilation HERE/STATE hazard, wf66_agents.md §9).
- `pause` from the REPL agent (aid 1) is valid: it switches to the operator. This is
  *why* the REPL must not be the operator.

**Residual limit (honest).** A long **leaf primitive** with no `NEXT` (`FILL`/`CMOVE`/
string scan) cannot hit the safepoint. Mitigation: add an in-loop budget check to the
handful of unbounded-count primitives in `kernel/`. A genuinely infinite non-yielding
leaf loop falls back to the `INTERRUPT_HOOK` **throw** (Ctrl+B) — a *user* action, not a
scheduler guarantee, so between loop-start and the keystroke the worker is blocked. This
is a scoped, documented limit, **not** a path we claim upholds the invariant.

## 7. Per-agent IO routing by child_id

- Add `child_id: i64` and `out: Vec<u8>` to `Slot` ([agents.rs:47](../../src/agents.rs#L47)).
- The `with_current_io` sink (src/runtime.rs) branches: when `rt_agent_self() != 0`,
  append to `AGENTS[current].out` instead of the shared session buffer. Cooperative
  single-threading makes "the current agent" unambiguous at every write — no lock, no
  interleave.
- `flush_pane_output()` (pump step) drains each dirtied `out` and routes by `child_id`
  (`repl_pane::append` / `fconsole::append` / `doc_pane`). Partial output between yields
  = free incremental progress.
- **Errors:** an agent's top-level `catch` (§8) formats the throw to *its* `child_id`
  (`AppendKind::Error`), never the global console.
- **Graphics** already route per-pane and async (`batch::submit(child_id)`) — unchanged.

## 8. Crash isolation (two layers, coexisting with the supervisor + VEH)

**Layer 1 — per-agent Forth faults (the 99% case).** Each agent's entry runs under
top-level `catch`. The per-agent `HANDLER` chain is **already** swapped at every switch
([agents.rs:209-222](../../src/agents.rs#L209-L222)), so a `throw` in pane 5 unwinds to
*its* `catch`; the agent marks itself done with a reason, the operator routes the error
to `child_id` and respawns just that pane. A Forth `throw` is not an SEH, so **this never
reaches the VEH** and the agent table stays intact. (Prerequisite: install a real
top-level catch frame per agent — today an agent's `HANDLER` inits to 0, so an *uncaught*
throw null-derefs; the per-agent `catch` closes that.)

**Layer 2 — hard SEH (access violation in JIT'd code).** A corrupt native stack cannot
be resumed at per-fiber granularity, so an SEH is a whole-worker event — but survivable,
and we fix the table loss:

- VEH stays gated on `WORKER_TID` ([crash_handler.rs:211](../../src/igui/crash_handler.rs#L211)),
  rewrites RIP → `ExitThread(2)`; the supervisor joins + respawns.
- **The fix:** the open-pane set survives in the HWND registry (it lives on the GUI
  thread, outliving the worker). On respawn the fresh worker re-`(agent-init)`s **and
  re-spawns one agent per still-open pane**, rebinding `child_id → fresh aid`. Panes
  survive a worker SEH as windows with fresh agents; only in-flight per-agent Forth state
  is lost — the same guarantee a REPL restart gives today, applied per-pane.
- **Attribution:** record the `CURRENT` aid where the VEH can read it; after respawn,
  mark the pane whose agent was running at fault time as "crashed" in its transcript.

Strictly better than today (one SEH kills one eval, user restarts): here the windows
persist and rebind automatically, and per-agent Forth bugs never trip Layer 2.

## 9. [AUDIT FIX B1/B2] The exhaustive `SendMessageW` sweep

The single most important correction: the blocking-call partition must be **exhaustive
and mechanical**, not per-call-site. Today *many* language-thread→GUI helpers are
blocking `SendMessageW` ([window.rs:7](../../src/igui/window.rs#L7) documents it), and the
naive design covered only `open_child`. Every agent-reachable wrapper must be classified:

| Wrapper | Site | Class | Conversion |
|---|---|---|---|
| `open_child` | window.rs:1386 | reply-returning | async-await (§5-style: `PostMessage` request → `SurfaceReply` carrying the new `child_id` → agent `receive`s) |
| `open_text_child` / `open_doc_child` / `open_help` | window.rs:1409/1432/… | reply-returning (and the GUI side nests `SendMessageW(WM_MDICREATE)` with deep D2D init) | async-await |
| `set_child_title` / `child::set_title` | window.rs:1610/1631; child.rs:2084/2096 | fire-and-forget | `PostMessage` with an **owned heap payload** (`Box::into_raw` on the worker, freed on the GUI side). The current stack-pointer marshalling only works *because* `SendMessageW` keeps the caller frame alive. |
| `set_menu` / `close_via_mdi` / `dispatch_mdi_verb` | window.rs/child.rs | fire-and-forget | same `PostMessage`-owned-payload conversion |

**Enforcement (land it in stage 3, not last):** `debug_assert!(rt_agent_self() == 0)` at
the top of every remaining blocking `SendMessageW` helper. Operator/host boot code (aid 0)
may still use them before any agent exists; any agent that reaches one trips the assert
immediately in tests. This guard is what would have caught B1/B2 mechanically.

## 10. Staged migration (each stage shippable + headless-testable; `WF66_AGENTS` gate kept until the end)

1. **Registry spine (inert).** Add `PANE_AGENTS`, `REQ_AGENTS`, and `child_id`+`out` on
   `Slot`. Headless test: two agents write interleaved lines to two `child_id` buffers.
2. **Per-agent IO.** Branch `with_current_io` on `rt_agent_self()`; route caught errors to
   `child_id`. Headless-testable.
3. **Async replies + the `SendMessageW` sweep + the guard.** Add `SurfaceReply` (with the
   non-droppable path, D1) + `(await-reply)` + `REQ_AGENTS`; rewrite the `run_measure`
   deliver sites; convert **all** agent-reachable blocking wrappers (§9) and add the
   `debug_assert!(self==0)` guard. Replace the boot gate with the `BOOTING` latch (B3).
   Headless: an agent measures text without parking the loop while another runs.
4. **Auto-yield safepoint.** `STATE`-aware deadline `pause` in `kernel/agents.masm`/`NEXT`
   + in-loop budget checks on unbounded leaf primitives. Headless: a tight `: spin begin
   1+ again ;` agent yields and the operator keeps slicing.
5. **The pump.** Rewrite `run_drain_loop` into §1; deferred `DeleteFiber` list. Keep the gate.
6. **Pane lifecycle + REPL-as-agent.** Spawn a controller agent per pane; route
   `ReplSubmit`/`EvalBuffer` to the dedicated REPL agent. Move the Mandelbrot step-3b demo
   to a pane agent. **Acceptance:** running Mandelbrot doesn't stall the console; a
   measure-heavy text pane stays responsive while another computes; a long `:`…`;` compile
   doesn't freeze other panes.
7. **Crash coexistence.** Per-agent top-level `catch` (Layer 1); supervisor respawn
   re-reads the registry and re-spawns an agent per open pane (Layer 2); record `CURRENT`
   for VEH attribution. **Acceptance:** a Forth `throw` in one pane kills only that pane
   (VEH never fires); an SEH respawns the worker and rebinds all panes.
8. **Flip the gate + release.** Remove `WF66_AGENTS`, delete the dead synchronous branch.
   ("release" = overwrite `E:\WF66\release\wf66\wf64-ui.exe`.)

## 11. Out of scope for *this* milestone (tracked)

- **GC root-visiting across parked agent stacks.** Mailbox messages carry only scalars
  (no GC strings on the hot path), so responsiveness doesn't need it — but a string-heavy
  multi-pane session needs the TCB-aware `visit_roots` walking every `Slot`'s stacks +
  saved regs (wf66_agents.md §8). Gated with the GC redesign.
- The multi-Forth shared-memory bus (wf66_agents.md §7) — documented, not built.

[fiber-seh-repro]: ../../src/bin/fiber_seh_repro.rs
[gfx-render]: ../../src/bin/gfx_render.rs
