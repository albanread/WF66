# Multitasking in Forth

*How WF66 lets one single-threaded Forth run many things at once — a console you
can type into, a Mandelbrot quietly painting itself, a clock ticking in a third
window — all without threads, locks, or a single data race.*

---

## The oldest trick in the book

Forth has been multitasking since before it was fashionable. The classic Forth
multitasker is a handful of words and a circular list of tasks. Each task runs
until it voluntarily calls `PAUSE`, which saves its stacks and hands the CPU to the
next task in the ring. Round and round it goes. No operating system, no preemption,
no interrupts required — just tasks that are polite enough to yield.

That politeness is the whole secret. Because a task only ever switches at a `PAUSE`
*it chose*, it knows exactly what state the world is in on both sides of the switch.
There is no moment where another task barges in halfway through updating a variable.
**Cooperative multitasking is multitasking without the races.** You give up
preemption — a runaway task can hog the machine — and in return you delete an entire
universe of locking bugs.

WF66 keeps that bargain and modernises the machinery. Its tasks are called
**agents**, and they are how the Forth IDE stays responsive: every window the IDE
opens is driven by its own agent, and they take turns on one thread.

---

## The big idea: agents

An **agent** is a cooperative green thread. It has its own data stack, return stack,
locals stack, and floating-point stack — a complete private Forth machine — but it
shares the one OS thread, the one dictionary, and the one object heap with every
other agent. Agents switch by choice, never by force.

You create one from an execution token:

```forth
: greeter   ." hello from an agent" cr ;
' greeter agent   ( -- aid )
```

`agent ( xt -- aid )` spawns the word as a new agent and returns its **agent id**
(*aid*). That's it — the agent is now alive and the scheduler will run it.

Under the hood each agent is a Win32 *fiber*, which is the OS's name for "a stack I
switch by hand." Fibers keep the thread's guard pages and exception-handling state
straight across a switch, so an agent can do anything ordinary Forth can — deep
calls, floating point, `catch`/`throw` — on its own stack. But you never see the
fibers. You see agents, and three small verbs.

---

## The three moves

Everything an agent does between "born" and "done" is built from three cooperative
primitives.

### `pause` — yield, but stay runnable

```forth
: counter   10 0 do  i .  pause  loop ;
```

`pause ( -- )` hands control back to the scheduler and puts the agent at the back of
the run queue. The agent will be resumed on the next scheduling round, right where
it left off. Drop a `pause` inside any long loop and you turn a CPU hog into a good
citizen: the console — and every other agent — gets a turn between iterations.

### `receive` — wait for a message

```forth
: worker   begin  receive  process-it  again ;
```

`receive ( -- msg )` blocks the agent until a message lands in its mailbox, then
returns it. "Blocks" here is the cooperative kind: the agent yields to the scheduler
and is simply *not run* again until a message wakes it. While it waits, it costs
nothing — the thread is free to do everything else.

### `(post)` — send a message

```forth
   42  worker-aid  (post)   ( msg aid -- )
```

`(post) ( msg aid -- )` drops a value into another agent's mailbox and marks it
runnable. (It's spelled `(post)`, not `send`, because in WF66 `send` is reserved for
object-message dispatch.) A message is just a cell — a number, or a tagged pointer
to a statically-allocated object. Mailboxes are ordered queues, so messages arrive
in the order they were posted.

Together, `receive`/`(post)` give you the classic producer/consumer pattern with no
locks anywhere:

```forth
\ A consumer agent that sums whatever it is sent, until it gets 0.
variable total
: summer
   0 total !
   begin  receive  dup  while   total +!   repeat   drop ;

' summer agent value sum-aid

\ ... elsewhere, the producer just posts numbers:
3 sum-aid (post)   4 sum-aid (post)   5 sum-aid (post)
0 sum-aid (post)   \ sentinel: summer finishes
```

No mutex protects `total`, and none is needed: only `summer` ever touches it, and it
only runs between cooperative switches. The race can't exist.

---

## The operator and the pump

Someone has to decide which agent runs next. That someone is the **operator** —
agent 0, the scheduler itself. In the IDE the operator *is* the worker thread that
owns your Forth session. Its whole job is the **pump**:

```
loop forever:
    drain any GUI events  →  route each to the agent that owns it
    run every runnable agent once   (a "slice")
    flush each agent's output to its window
    if nobody is runnable, sleep until the next event wakes us
```

You almost never think about the pump. You write agents and spawn them; the IDE
operator drives them between your keystrokes. The important consequence:

> **The operator runs the slices, so the operator is never the thing that's busy.**
> Your console stays alive because typing a command and rendering a Mandelbrot are
> two different agents, and the operator gives each a turn.

(If you drive the scheduler yourself — in a headless script or a test — you call
`run-slice` to run one round, or `run-until-idle` to run until every agent has
finished or parked. The IDE does this for you.)

---

## Wait, but never block

There is one rule that makes the whole IDE feel alive, and it is worth saying out
loud:

> **A task may *wait*. A task may never *block*.**

*Waiting* is cooperative: a paused or `receive`-ing agent has stepped aside and
costs nothing; the thread keeps serving everyone else. *Blocking* is the opposite: a
task that calls a slow operation which parks the **whole OS thread** freezes every
other agent and the console with it. One blocked task and the IDE is dead.

So WF66 has no blocking calls in agent code. The two places a naïve port would block
— waiting for window input, and asking the GUI to measure some text — are both
turned into cooperative waits:

- input becomes `pane-event` (below), which yields until an event is routed to you;
- a synchronous GUI query becomes `await-reply`, which posts the request and
  `receive`s the answer, so other agents run while the GUI works.

If you remember one thing, remember this rule. Long loop? Sprinkle `pause`. Need
something from outside? `receive` it. Never sit on the thread.

---

## Pane-agents: the app model

This is where multitasking becomes *apps*. In WF66 the unit of a graphical program
is a **pane-agent**: one window ⇄ one agent ⇄ one `child_id`. The agent owns the
pane — it paints it and it handles its input — and because it's an agent, owning a
window costs the rest of the IDE nothing.

Three words turn an ordinary agent into a pane-agent:

| Word | Stack | What it does |
|---|---|---|
| `(set-pane)` | `( child_id -- )` | Bind the running agent to a pane. Its output and its input are now that window's. |
| `pane-event` | `( -- p4 p3 p2 p1 kind )` | Cooperatively get this pane's next input event (key, mouse, resize, close…), yielding until one arrives. The agent replacement for the old blocking `gpane-next-event`. |
| `wait-close` | `( -- )` | For a draw-once pane: wait (cooperatively) until the window is closed. |

When you open a window with `gpane-open`, the IDE gives it a `child_id`. You spawn
an agent, the agent calls `(set-pane)` to claim that id, and from then on the pump
routes that window's events — `ev-key`, `ev-mouse`, `ev-resize`, `ev-close`, … — into
*that agent's* queue, where `pane-event` drains them. Ten windows, ten agents, ten
private event streams, all multiplexed onto one thread.

The event kinds and their parameters (from `lib/core.f`):

```
ev-key    ( vkey mods down repeat )      ev-resize ( width height 0 0 )
ev-char   ( codepoint mods 0 0 )         ev-close  ( 0 0 0 0 )
ev-mouse  ( x y op mods|button<<8 )      ev-tick   ( time_ms 0 0 0 )
ev-focus  ( gained 0 0 0 )
```

`pane-event` returns the four parameters and the kind, with the kind on top —
exactly the shape your handler wants.

---

## Worked example 1 — a live Mandelbrot

Here is the canonical pane-agent: a Mandelbrot set that paints itself while you keep
working in the console. It is `demos/gfx-mandel-live.f`, trimmed to the essentials.

```forth
variable lm-id          \ the pane's child_id, so the no-arg agent can find it
variable lm-row

\ The controller agent: bind the pane, draw the whole set into one batch
\ (pausing each row so the console keeps its turn), present it, then wait
\ for the window to close.
: lm-agent  ( -- )
    lm-id @ (set-pane)              \ claim the pane (routes input + output here)
    lm-id @ gpane-begin             \ start one drawing batch
    0x000000 gpane-clear
    180 0 do
        i lm-row !
        240 0 do
            ... compute and fill one 2x2 cell ...
        loop
        pause                       \ ← yield after each row: console stays live
    loop
    lm-id @ gpane-present           \ show the finished image
    wait-close                      \ ← cooperatively wait until the pane closes
;

\ The entry word: open the window, remember its id, spawn the agent, return.
: gfx-mandel-live  ( -- )
    480 360  S" ∴ Mandelbrot (live)"  gpane-open  lm-id !
    lm-id @ 0= if  ." (no UI substrate)" cr  exit then
    ['] lm-agent agent drop
    ." rendering in the background — keep using the console." cr
;
```

Read the shape, because every render-pane app has it:

1. **The entry word opens the window and returns immediately.** It does *not* draw
   and it does *not* wait — it spawns the agent and gets out of the way, so the word
   you typed finishes instantly and your prompt comes back.
2. **The agent binds itself with `(set-pane)`**, so its drawing goes to the right
   window.
3. **It renders, `pause`-ing in the loop.** Each `pause` lets the operator pump a
   keystroke or run another pane between rows. That is *why* you can type while it
   draws.
4. **It `wait-close`s.** The agent doesn't exit when the picture is done — it parks,
   waiting for `ev-close`. The window stays, costing nothing, until you close it;
   then the agent wakes from `wait-close` and falls off the end, finished.

Load it from the **Demos** menu and type `2 3 + .` while the set fills in. The `5`
comes back instantly.

---

## Worked example 2 — an interactive pane

Render-once panes use `wait-close`. Interactive panes run an **event loop** with
`pane-event`. Here's the click counter (`demos/gfx-click.f`), abridged:

```forth
variable cc-id

\ Repaint the square in the colour for the current count.
: cc-paint ( id count -- ) ... ;

\ Handle one event. ( id count p4 p3 p2 p1 kind -- id count' done? )
: cc-handle ( id count ... kind -- id count' done? )
    dup ev-close = if  ...drop the event...  -1 exit  then     \ close → done
    dup ev-resize = if ...repaint...          0 exit  then     \ resize → repaint
    dup ev-mouse  = if ...bump count, repaint... 0 exit then   \ click → recolour
    ...drop the event...  0 ;                                  \ ignore the rest

: cc-agent ( -- )
    cc-id @ (set-pane)
    cc-id @ 0                       \ ( id count )
    2dup cc-paint                   \ initial render
    begin
        pane-event                  \ ( id count p4 p3 p2 p1 kind ) — yields until input
        cc-handle                   \ ( id count' done? )
    until                           \ loop until cc-handle says done
    2drop ;

: gfx-click  ( -- )
    480 360 S" ∴ Click Counter" gpane-open  cc-id !
    cc-id @ 0= if ." (no UI)" cr exit then
    ['] cc-agent agent drop ;
```

The loop is the heart of it: `pane-event` **waits cooperatively** for the next mouse
click or resize or close, `cc-handle` acts on it, and the agent keeps its own state
(`id count`) on its own data stack across the wait — exactly the way the old
single-task version kept it across `gpane-next-event`, but now without freezing the
rest of the IDE. Click the square: it recolours. Meanwhile the console, and any
other pane, are still running.

---

## The rules of the game

Cooperative single-threading buys you a race-free world. You keep it by honouring a
short discipline:

- **Don't `pause` mid-definition.** Between `:` and `;` the compiler is using shared
  state (`HERE`, `STATE`). Yield only when you've finished defining a word, not in
  the middle of compiling one. (Render loops and event loops are fine — they run
  *compiled* code.)
- **Send results to a pane, not the console.** An agent's output is routed to the
  window it bound with `(set-pane)`. A pane-agent can `." ..."` freely — it lands in
  its own transcript. A *background* agent (no pane) should compute into a variable
  or object, which the console can read, rather than printing between keystrokes.
- **Objects are safe to share.** WF66's `oop.f` objects live at fixed addresses and
  never move, so a pointer you hold across a `pause` is still valid afterward, and
  two co-located agents may share an object by pointer. Cooperation means there is no
  concurrent field write to worry about — but don't `pause` in the *middle* of a
  multi-field update another agent reads, or it will see the half-finished version.
- **Keep slices short.** A slice runs every runnable agent once; if one agent never
  `pause`s during a long computation, it delays the others until it does. Long work →
  `pause` periodically.

---

## Why cooperative, and why one thread

It is tempting to reach for OS threads and "real" parallelism. WF66 deliberately
doesn't, and the reason is the same one that made the classic Forth multitasker
beautiful: **a cooperative single thread is the simplest correct concurrency model
there is.** No locks, because there is no preemption. No data races, because only one
agent runs at a time and it chose the moment it stepped aside. The entire category of
"works on my machine, deadlocks in production" simply isn't reachable.

What you give up is using more than one core, and protection from a misbehaving
agent that never yields. For an interactive IDE — where the work is *latency*-bound
(stay responsive), not *throughput*-bound (saturate the CPU) — that's a fine trade.
You want the console to answer instantly far more than you want eight cores melting a
fractal.

And the door to real parallelism is left open without paying for it now. Agents are
addressed by opaque ids and talk by messages, never by assuming they share memory.
That is exactly the shape you'd need to one day run **several** single-threaded
Forths, one per core, trading copied messages over a shared-memory bus — parallelism
as *N* race-free Forths rather than one lock-tangled multi-threaded one. WF66 is
*shaped* for that future but doesn't build it: it is a cooperative pane multiplexer,
not an actor platform. There are no supervision trees, no process links, no
distribution. **This is not BEAM** — it is the old Forth multitasker, grown up enough
to run a windowing IDE.

---

## Quick reference

| Word | Stack | Meaning |
|---|---|---|
| `agent` | `( xt -- aid )` | Spawn `xt` as a cooperative agent; return its id. |
| `pause` | `( -- )` | Yield; stay runnable (resume next round). |
| `receive` | `( -- msg )` | Cooperatively wait for a mailbox message. |
| `(post)` | `( msg aid -- )` | Send `msg` to agent `aid`'s mailbox; wake it. |
| `(agent-self)` | `( -- aid )` | The running agent's id. |
| `(set-pane)` | `( child_id -- )` | Bind the running agent to a window. |
| `pane-event` | `( -- p4 p3 p2 p1 kind )` | Wait for and return this pane's next input event. |
| `wait-close` | `( -- )` | Wait until this pane is closed. |
| `await-reply` | `( request_id -- reply )` | Post a GUI query and cooperatively wait for its answer. |
| `run-slice` | `( -- )` | (Host) run every runnable agent once. |
| `run-until-idle` | `( -- )` | (Host) run slices until all agents finish or park. |

The Forth layer lives in [`lib/agents.f`](../lib/agents.f); the kernel primitives in
[`kernel/agents.masm`](../kernel/agents.masm); the design and its rationale in
[`docs/design/wf66_agents.md`](design/wf66_agents.md) and
[`docs/design/wf66_pane_agent_gui.md`](design/wf66_pane_agent_gui.md).

## Try it

1. Open the IDE. **Demos → gfx-mandel-live** — the set paints while you type
   `: sq dup * ;  9 sq .` in the console.
2. **Demos → gfx-click** — click the square to recolour it; the console stays live.
3. Open a second graphical demo *as well*. Two panes, two agents, one thread, and
   nothing waits on anything else.

That is multitasking in Forth: not threads bolted on, but the language's own oldest
habit — yield when you're ready — turned into a way to build responsive apps.
