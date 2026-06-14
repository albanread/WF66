\ WF66 cooperative agent scheduler (Forth side).
\
\ Green-thread multitasking for single-threaded Forth. The kernel primitives
\ ((spawn)/(switch-to)/(post)/(mailbox-*)/(ready-*), see kernel/agents.masm)
\ provide the fiber switch + ready queue + mailboxes; this file is the policy:
\ spawn/yield/receive/run-slice. The host (IDE worker, or a test) calls
\ (agent-init) once on the scheduler thread, then drives run-slice between event
\ pumps. See docs/design/wf66_agents.md.
\
\ Model: the operator (aid 0) drives run-slice and is never in the ready queue.
\ Agents yield with `pause` (stay runnable) or `receive` (block until a message),
\ both switching back to the operator. Step-2 agents may use FP and `catch`
\ (FSP + the handler chain are swapped per agent).

forth-wordlist set-current

\ Create a runnable agent from an execution token.  ( xt -- aid )
: agent  ( xt -- aid )  (spawn)  dup (ready-push) ;

\ Cooperative yield: stay runnable, hand control back to the operator.
: pause  ( -- )  (agent-self) (ready-push)  0 (switch-to) ;

\ Block until a message arrives in my mailbox, then return it.  ( -- msg )
: receive  ( -- msg )
    begin
        (mailbox-len) 0=
    while
        0 (switch-to)              \ blocked: yield WITHOUT re-queueing self
    repeat
    (mailbox-pop) ;

\ Run one scheduling slice: switch to each currently-runnable agent once.
\ Agents that `pause` re-queue themselves for the next slice; agents that
\ `receive` (and find nothing) drop out until a `(post)` wakes them.
: run-slice  ( -- )
    (ready-count)  0 ?do
        (ready-pop)
        dup -1 = if  drop  else
            dup (agent-done?) if  drop  else  (switch-to)  then
        then
    loop ;

\ Drive slices until no agent is runnable (useful headless / for batch work).
: run-until-idle  ( -- )
    begin  (ready-count) 0>  while  run-slice  repeat ;

\ Await an async GUI reply WITHOUT blocking the thread: bind this request id to
\ self, then `receive` (a cooperative yield) until the host pump posts the reply
\ back. Other agents keep running while we wait. ( request_id -- reply )
: await-reply  ( request_id -- reply )  (req-bind)  receive ;

\ ── Pane input for pane-agents (cooperative; the agent replacement for the
\ ── blocking gpane-next-event). The host pump routes this pane's decoded events
\ ── into the running agent's queue; here we drain one, yielding when none waits —
\ ── so a pane WAITS for its input without BLOCKING the console or other panes.

\ Get this pane-agent's next input event, yielding to the operator until one
\ arrives. Same 5-value shape as gpane-next-event. ( -- p4 p3 p2 p1 kind )
: pane-event  ( -- p4 p3 p2 p1 kind )
    begin
        0 0 gpane-next-event        \ agent path: poll own queue (args ignored)
        dup 0= while                \ EV_NONE = empty?
        drop 2drop 2drop            \ discard the 5 zero cells
        0 (switch-to)               \ yield — the console + other panes run
    repeat ;

\ Block (cooperatively) until this pane is asked to close. Render-only panes use
\ this after drawing; the console stays fully live while we wait. ( -- )
: wait-close  ( -- )
    begin  pane-event  nip nip nip nip  ev-close =  until ;
