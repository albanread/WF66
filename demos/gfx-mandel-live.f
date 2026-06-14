\ gfx-mandel-live.f — Mandelbrot drawn by a COOPERATIVE AGENT.
\
\ Same image as gfx-mandelbrot.f, but rendered one row per scheduler slice by an
\ agent that `pause`s between rows. Because the IDE worker is a cooperative pump
\ (see docs/design/wf66_agents.md), the Forth console stays fully responsive while
\ the set draws — type and run other words while the Mandelbrot fills in.
\
\ Usage (in the IDE console):   gfx-mandel-live
\ Then keep typing in the console — e.g.  2 3 + .   — as it renders.

64 constant lm-maxiter
2  constant lm-blk

3.5e  240e f/  fconstant lm-dx
2.5e  180e f/  fconstant lm-dy

variable lm-id        \ the graphics pane id (so the no-arg agent can reach it)
variable lm-row
variable lm-rgb

\ 16-step escape-time palette (deep navy -> ice-white -> amber -> black).
: lm-colour ( n -- rgb )
    dup lm-maxiter = if drop 0x000000 exit then
    15 and case
        0  of 0x0D1540 endof   1  of 0x102B80 endof
        2  of 0x1558C8 endof   3  of 0x3A8CF5 endof
        4  of 0x7DC8FF endof   5  of 0xB8EEFF endof
        6  of 0xFFFFFF endof   7  of 0xFFF4A8 endof
        8  of 0xFFCC57 endof   9  of 0xFFA000 endof
        10 of 0xFF6800 endof   11 of 0xE83800 endof
        12 of 0xAA1200 endof   13 of 0x650000 endof
        14 of 0x280000 endof   15 of 0x080010 endof
    endcase ;

\ The pane controller agent: bind to the pane, draw the whole set into ONE batch
\ (so rows accumulate — `gpane-present`/submit REPLACES the pane's batch, it does
\ not stack), `pause`ing each row so the console keeps its turn, present once, then
\ WAIT for the pane's close event cooperatively. Runs as an agent (no args; reads
\ lm-id). While it waits the worker keeps serving the console — wait, never block.
: lm-agent  ( -- )
    lm-id @ (set-pane)              \ bind this agent to its pane (event routing)
    lm-id @ gpane-begin             \ one batch for the whole image
    0x000000 gpane-clear
    180 0 do
        i lm-row !
        240 0 do
            0e  0e
            i        s>d d>f  lm-dx f*  -2.5e  f+
            lm-row @ s>d d>f  lm-dy f*  -1.25e f+
            lm-maxiter fractal-iter
            lm-colour lm-rgb !
            i lm-blk *   lm-row @ lm-blk *   lm-blk lm-blk   lm-rgb @
            gpane-fill-rect
        loop
        pause                       \ <- yield each row so the console stays live
    loop
    lm-id @ gpane-present           \ submit the complete image once
    begin  receive  ev-close =  until   \ wait (cooperatively) until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: gfx-mandel-live  ( -- )
    480 360  S" ∴ Mandelbrot (live)"  gpane-open  lm-id !
    lm-id @ 0= if  ." (no UI substrate — demo skipped)" cr  exit then
    ['] lm-agent agent drop
    ." Mandelbrot rendering in the background — keep using the console." cr ;

\ NOTE: this demo needs the cooperative agent pump on the IDE worker thread, which
\ is gated behind the WF66_AGENTS environment variable. Launch the IDE with
\ WF66_AGENTS=1, then load this demo: lm-agent draws one row per scheduler slice and
\ `pause`s, so the Forth console stays interactive while the set renders. The
\ fiber/SEH boot blocker that previously disabled this is RESOLVED — the worker boots
\ clean under the pump (the old "register kernel procs with SEH" report was the benign
\ 0x406D1388 thread-naming exception, not a real fault). See
\ docs/design/wf66_pane_agent_gui.md.
