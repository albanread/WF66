# WF66 — cooperative agents (sane now, shaped for multi-Forth)

Status: **design.** The plan for making single-threaded Forth multiplex IDE panes
via events, built so it can grow — without an API rewrite — into **multiple Forth
instances over a shared-memory bus** (parallelism by *N single-threaded Forths*,
not one multi-threaded Forth). Companion to the as-built optimizer doc
([wf66_dual_level_reducer.md](wf66_dual_level_reducer.md)). Code refs are in
`e:\WF66\src\igui\` and `e:\WF66\kernel\` unless noted.

## 0. Two horizons, one API

- **Sane (now):** one Forth instance, one OS thread, cooperative **agents** (green
  threads) with mailboxes, supervised, on the shared static-object heap. Solves
  the stated problem: a "running" pane (Mandelbrot) no longer stalls the console.
- **Extreme (later, not foreclosed):** several Forth instances — one per core —
  each single-threaded and cooperative internally, communicating over a
  **shared-memory message bus**. Parallelism with *no* shared mutable object graph
  (each instance owns its objects; cross-instance is messages only), so the
  race-free property of cooperative single-thread is preserved at scale.

The whole point of this doc is the **shaping principle** that lets the first grow
into the second by swapping a transport, not rewriting agents:

> **Agents are addressed by opaque, location-transparent IDs and communicate by
> messages — never by assuming they share an address space.** Co-located agents
> *may* be handed a static object by pointer as a transport *optimization*, but the
> contract is message-passing. An agent written today works unchanged when it
> later lives on another instance across the bus.

**This is explicitly NOT BEAM.** We are building a cooperative pane multiplexer —
not an actor platform, not OTP supervision trees, not link/monitor process graphs,
not distribution. The "extreme" horizon and the bus exist in this doc **only to
justify two cheap, reversible shaping choices** (location-transparent AIDs +
copied messages). We keep those two; we build none of the rest unless and until
there is a concrete reason. The voice in your head that wants a Rust/LLVM Elixir
gets a polite nod and the door, closed.

## 1. What we build on (today's reality)

- **Two threads** (`window.rs` run/worker): a Win32 GUI pump and one **worker**
  that owns the single `Wf64Session`. The worker loop is `next_event()` →
  `session.eval(source)` — and that `eval` **blocks** in a long Forth loop with no
  yield point, which is *exactly* why other panes stall.
- **Per-pane routing already exists:** `child_id: i64` (`registry.rs`) routes
  output to a pane; `IGuiEvent` (`channels.rs`) is the GUI→worker mailbox;
  `INTERRUPT_HOOK` (defined, unused) is the precedent for a synchronous callback
  into running Forth.
- **Object model:** oop.f objects are **statically allocated** — fixed address,
  never moved, never collected. Only *new strings* are GC-managed.
- **GC:** NewGC is a **moving generational** collector (minor/major, evacuation,
  dirty cards) driven by a host `visit_roots` callback + a "static" root region.
  Treated here as **redesignable** (it is not well exercised yet), so the design
  states what the GC must provide rather than bending to its current shape.

The static-object model is decisive: object references are safe to hold across a
pause and to share between co-located agents (cooperative ⇒ no concurrent field
write). The only thing the GC moves is strings.

## 2. The model

- **Agent** — a cooperative green thread bound to a purpose (a pane, a supervisor,
  a port). Has: an **AID** (id), a **mailbox**, private **stacks** (data, return,
  FP, locals), a state (`runnable` / `blocked` / `done`), and a current
  continuation (captured by `pause`/`receive`). One pane ⇄ one agent ⇄ one
  `child_id`.
- **Scheduler** — per Forth instance. A ready queue + the agent table. `run-slice`
  runs runnable agents round-robin until each yields or blocks, then returns. The
  host (IDE worker) drives slices and only delivers external events at slice
  boundaries — "wait until Forth is ready."
- **Mailbox** — an ordered message queue per agent. *Pluggable transport:* an
  in-instance queue today; a shared-memory ring across the bus tomorrow. The agent
  never knows which.
- **Message** — a self-contained value (numbers, copied byte blobs, or a
  **shared-region handle**), chosen so it can cross the bus. Co-located delivery
  may pass a static-object pointer zero-copy as an optimization; that is the
  transport's choice, not the agent's contract.
- **Error isolation** (sane, lightweight — *not* OTP). Each agent runs under a
  top-level `catch`; a Forth fault terminates only that agent and notifies whoever
  spawned it, which may restart the pane from a known state. That's the whole
  resilience story — no link/monitor graphs, no supervision trees.

## 3. Core API (location-transparent from day one)

```
spawn   ( xt -- aid )          \ create an agent running xt; returns its AID
self    ( -- aid )             \ the running agent's AID
send    ( msg aid -- )         \ enqueue msg to aid's mailbox (local or remote)
receive ( -- msg )             \ block until a message arrives (yields the agent)
?receive ( -- msg true | false ) \ non-blocking poll
pause   ( -- )                 \ cooperative yield (no message needed)
exit    ( reason -- )          \ terminate self; the spawner is told the reason
```

That's the whole surface. Notably **absent on purpose** (the BEAM-ward features we
are not building): `link`/`monitor` process graphs, supervision trees, selective-
receive pattern matching, and any distribution primitive. If a real need appears
they can be added later — the AID/message contract wouldn't change — but they are
not in scope.

`receive`/`pause` are the yield points. A compute-heavy agent (Mandelbrot) drops a
`pause` in its loop; an idle agent (console) lives in `begin … receive handle …
again`. The **AID is opaque** — today `(instance=self, index)`, tomorrow
`(instance=N, index)` for a remote agent — so `send` never changes.

## 4. The kernel mechanism — fibers + Forth-register save/restore

The call-stack half uses **Win32 fibers** (`ConvertThreadToFiber` / `CreateFiber`
/ `SwitchToFiber`), *not* a hand-rolled `RSP` swap. Fibers let the OS keep the
TEB's `StackBase`/`StackLimit`, guard pages, and SEH/crash-handler stack walks
correct per agent — the sharp edges of relocating the native stack by hand. Each
agent is a fiber.

Fibers don't know about Forth's registers, so the switch primitive saves/restores
the **Forth half** around `SwitchToFiber`:

| State | Reg / cell |
|---|---|
| Data stack pointer (DSP) | `RBP` |
| TOS | `RAX` |
| Locals pointer (LP) | `R15` |
| FP TOS | `XMM15` |
| FP stack pointer | `[UP + user_FSP]` value |

(Non-volatile `xmm6..14`/GP non-volatiles are preserved by the normal call ABI
around the switch.) The **TCB** holds those saved values + the agent's stack-region
bases + state + AID + mailbox. `(spawn)` lays out the regions and a fake initial
return frame so the first switch-in enters the entry word with clean stacks and a
trampoline that calls `exit` when the entry word returns.

## 5. Scheduling + IDE host binding

The worker loop changes from "block in one eval" to a **pump**:

```
loop {
  drain IGuiEvents → translate to messages, `send` to the target pane's agent
  session.eval("scheduler run-slice")     ← runnable agents run until they yield
  flush each agent's output to its pane (child_id)   ← partial output = free progress
  if nothing runnable and no events → block on next_event()   (sleep until woken)
}
```

- Each pane registers an agent; the host keeps a `child_id ⇄ AID` map (extends
  `registry.rs`).
- **Per-agent output:** the `Io` layer must route the *current agent's* output to
  *its* pane buffer (today `session.eval` captures to one buffer — that becomes
  per-agent, keyed by the running AID). This is the one non-trivial runtime change.
- **Synchronous GUI replies** (`replies.rs` text-measure round-trip) currently
  block the whole worker; from an agent they must become async (the agent `await`s
  the reply message) so one agent's query can't freeze the rest.

## 6. Error isolation (lightweight)

Single-thread makes this cheap and OTP-free: wrap each agent's top-level in the
existing `catch`/throw machinery. On throw, the agent transitions to `done` with a
reason and the spawner is notified; the IDE may respawn that pane from a known
state. Result: a pane's Forth bug kills only that pane's agent, not the IDE. No
`link`/`monitor`, no restart strategies, no trees — just per-agent error
boundaries and a respawn-the-pane affordance.

## 7. The extreme path — multi-Forth over a shared-memory bus (NOT planned)

**We are not building this.** This section exists only to show that the two cheap
shaping choices (location-transparent AIDs, copied messages) are enough to keep
the door open — so that nothing in the sane build has to be undone if the option
is ever taken. Each abstraction was chosen to extend without changing agent code:

- **AID already carries an instance id.** A `send` to a remote AID routes to the
  bus instead of the local queue. Agents don't change.
- **Mailbox transport becomes a shared-memory ring.** Per (remote mailbox) a
  single-producer/single-consumer (or lock-free MPSC) ring in the shared region.
  `send` serializes the message into the ring; the owning instance drains it into
  the agent's local mailbox at a slice boundary. The **bus is the only shared
  mutable state**, and it's a disciplined channel, not a shared object graph.
- **Messages are already bus-safe** (copied values / shared-region handles). A big
  payload two instances must share lives in the shared region and travels as a
  handle (offset), valid in every instance that maps the bus.
- **Each instance stays single-threaded and cooperative**, so the race-free
  property holds *within* an instance; parallelism is N instances on N cores. No
  preemption, no locks on object fields — because objects are never shared across
  instances (only messaged), and within an instance there's no concurrency.
- **Per-instance heaps/GC.** Each instance owns its string heap and collects
  independently and locally — no stop-the-world, no cross-bus rooting. This is the
  Erlang-per-process-heap win, and it makes the GC story *simpler* at scale, not
  harder.

What does *not* cross the bus: raw object pointers (static addresses are
per-instance), and shared mutable object graphs. That's the deliberate boundary
that keeps the model sound.

## 8. GC integration

- **Sane (one instance):** the moving collector's `visit_roots` must enumerate
  **every agent's** data/return stacks and saved TCB registers (not just the
  running agent), and apply evacuation pointer-updates to all of them. Because
  collection is cooperative (it only runs when the *running* agent allocates, with
  everyone else parked at a known pause boundary recorded in their TCB), the
  visitor can walk the TCB list safely. Static objects need no fixups (they don't
  move); only string references on the stacks do.
- **Extreme:** per-instance heaps remove the cross-agent question from the hot path
  — each instance roots only its own agents.
- Since NewGC is redesignable, the contract is simply: *the collector takes a
  root-visitor that the scheduler implements over the TCB list.* If a future GC
  prefers a handle table for managed strings, the cross-agent rooting question
  disappears entirely.

## 9. Discipline / constraints

- **Don't `pause` mid-compilation** (between `:` and `;`): `HERE`/`STATE` are
  shared, and cooperative timing keeps them consistent only at yield boundaries.
  Any auto-yield safepoint (below) must suppress yielding while `STATE != 0`.
- **Don't `pause` mid-invariant** across a multi-field object update that other
  agents read — use a short critical section (defer scheduling) for those. (This
  is a *logical* hazard only; there are no memory races under cooperation.)
- Synchronous GUI round-trips become `await`-style (see §5).

## 10. Deliberately NOT done (non-goals)

- **Not BEAM.** No actor *platform*: no `link`/`monitor` graphs, no OTP supervision
  trees, no selective-receive matching, no distribution/nodes. The only things kept
  from that world are the two cheap shaping choices (location-transparent AIDs +
  copied messages); everything else is out of scope until a concrete need forces it.
- No preemption; no M:N (one instance = one thread, cooperative).
- No shared mutable object graph across instances (message instead).
- The multi-Forth shared-memory bus (§7) is **documented, not built**.

## 11. Staged build (sane → extreme)

1. **Kernel:** fiber-based context switch + TCB + `(spawn)`. Prove headless: 2–3
   agents that `pause` in a loop and demonstrably round-robin. No IDE yet.
2. **Forth agent/scheduler/event service** on oop.f: `spawn/send/receive/self/
   pause/link/monitor`, mailboxes (in-instance queue), `run-slice`, supervisor.
   Messages copied-by-value from day one so they're bus-ready. Headless tests for
   blocking/wake/round-robin/supervision.
3. **IDE host binding:** swap the worker's blocking eval for the pump; per-agent
   `Io` routing by `child_id`; async GUI replies; bind each pane to an agent.
4. **Auto-yield** (optional): wire `INTERRUPT_HOOK` as a time-slice safepoint
   (respecting `STATE`) so tight loops yield without explicit `pause`.
5. **Extreme (NOT planned):** shared-memory bus transport for remote AIDs;
   per-instance heap/GC; a second Forth instance on another core. Listed only to
   confirm steps 1–4 wouldn't need rework if it were ever taken.

## 12. Open items

- Confirm the managed-string root-visitor surface and make it TCB-aware (or move
  strings to a handle table) — small, contained, and gated by the GC redesign.
- Per-agent stack sizing + overflow guards (fibers give guard pages); fiber count
  ceiling vs. expected pane count.
- Whether the operator/REPL agent is special (it's the one that compiles) or just
  an agent that happens not to `pause` mid-definition.
