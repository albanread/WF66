\ gfx-diag.f — minimal gpane diagnostic, driven by a COOPERATIVE AGENT.
\
\ Opens a pane, paints once, waits (cooperatively) for the close event, exits.
\ No locals, no case dispatch, no rect.  If the pane shows up with a circle
\ inside, the basic gpane wiring works.  If not, open/begin/clear/fill-circle/
\ present is broken.
\
\ A controller AGENT drives the pane so the Forth console stays live: the entry
\ word opens the pane and spawns the agent, then returns immediately; the agent
\ binds itself to the pane, paints once, then WAITS for the pane's close event
\ cooperatively (close the pane and the agent exits, console live throughout).

variable gfx-diag-id    \ the graphics pane id (so the no-arg agent can reach it)

\ The pane controller agent: bind to the pane, paint once, then cooperatively
\ wait for the pane's close.  Runs as an agent (no args; reads gfx-diag-id).
: gfx-diag-agent  ( -- )
    gfx-diag-id @ (set-pane)        \ bind this agent to its pane (event routing)

    gfx-diag-id @ gpane-begin
    0x202020 gpane-clear
    160 120 60 0xFFCC66 gpane-fill-circle
    gpane-present

    cr ." gfx-diag: painted, waiting for close" cr

    wait-close                      \ cooperatively wait until the pane closes
    ." gfx-diag: done" cr
;

\ Open the pane and spawn the controller agent; returns immediately so the
\ console stays interactive while the agent runs.
: gfx-diag  ( -- )
    cr ." gfx-diag: opening pane" cr

    320 240 S" ∴ Diag" gpane-open
    dup 0= if drop ." (no UI substrate — demo skipped)" cr exit then

    cr ." gfx-diag: pane id = " dup . cr

    gfx-diag-id !
    ['] gfx-diag-agent agent drop
;
