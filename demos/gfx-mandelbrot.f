\ gfx-mandelbrot.f — Mandelbrot set via gpane + fractal-iter.
\
\ 480 × 360 window; 2 × 2-pixel blocks → 240 × 180 iteration grid.
\ Each pixel evaluates  z ← z² + c  from z₀ = (0,0), c = (px, py).
\ The hot inner loop runs entirely in XMM registers inside the MASM
\ primitive `fractal-iter`; Forth manages the coordinate mapping,
\ colour lookup, and rendering framework.
\
\ FP stack protocol for fractal-iter  ( maxiter -- n )
\   ( F: z0x z0y cx cy -- ):
\     push z0x z0y  (Mandelbrot: both 0.0)
\     push cx cy    (the pixel's complex coordinate)
\     push maxiter  onto the DATA stack
\
\ Driven by a COOPERATIVE PANE-AGENT so the Forth console stays live: the
\ controller agent binds the pane, draws the whole set into one batch
\ (`pause`ing each row so the console keeps its turn), presents once, then
\ WAITS for the pane's close event cooperatively. See lib/agents.f.
\ See lib/core.f for gpane-* and ev-* definitions.

64 constant mb-maxiter
2  constant mb-blk

\ Step sizes: real [−2.5 .. 1.0] across 240 cols; imag [−1.25 .. 1.25] across 180 rows.
3.5e  240e f/  fconstant mb-dx
2.5e  180e f/  fconstant mb-dy

variable mb-id        \ the graphics pane id (so the no-arg agent can reach it)
variable mb-row
variable mb-rgb

\ 16-step escape-time palette: deep navy → ice-white → amber → black.
: mb-colour ( n -- rgb )
    dup mb-maxiter = if drop 0x000000 exit then
    15 and case
        0  of 0x0D1540 endof
        1  of 0x102B80 endof
        2  of 0x1558C8 endof
        3  of 0x3A8CF5 endof
        4  of 0x7DC8FF endof
        5  of 0xB8EEFF endof
        6  of 0xFFFFFF endof
        7  of 0xFFF4A8 endof
        8  of 0xFFCC57 endof
        9  of 0xFFA000 endof
        10 of 0xFF6800 endof
        11 of 0xE83800 endof
        12 of 0xAA1200 endof
        13 of 0x650000 endof
        14 of 0x280000 endof
        15 of 0x080010 endof
    endcase
;

\ The pane controller agent: bind to the pane, render the complete set into ONE
\ batch (`pause`ing each row so the console keeps its turn), present once, then
\ WAIT for the pane's close event cooperatively. Runs as an agent (no args;
\ reads mb-id). While it waits the worker keeps serving the console.
: mb-agent ( -- )
    mb-id @ (set-pane)              \ bind this agent to its pane (event routing)
    mb-id @ gpane-begin             \ one batch for the whole image
    0x000000 gpane-clear
    180 0 do
        i mb-row !
        240 0 do
            \ Push z₀=(0,0) then c=(cx,cy) onto FP; maxiter onto data.
            0e  0e
            i        s>d d>f  mb-dx f*  -2.5e  f+
            mb-row @ s>d d>f  mb-dy f*  -1.25e f+
            mb-maxiter fractal-iter    \ ( -- n )
            mb-colour                  \ ( n -- rgb )
            mb-rgb !
            i mb-blk *   mb-row @ mb-blk *   mb-blk mb-blk   mb-rgb @
            gpane-fill-rect            \ ( -- )
        loop
        pause                       \ <- yield each row so the console stays live
    loop
    mb-id @ gpane-present           \ submit the complete image once
    wait-close                      \ cooperatively wait until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: gfx-mandelbrot
    cr ." rendering Mandelbrot set ..." cr
    480 360  S" ∴ Mandelbrot"  gpane-open  mb-id !
    mb-id @ 0= if  ." (no UI substrate — demo skipped)" cr  exit then
    ['] mb-agent agent drop
    ." Mandelbrot rendering in the background — keep using the console." cr
;