\ gfx-julia.f — Julia set for c = −0.7269 + 0.1889i, drawn by a COOPERATIVE AGENT.
\
\ Same 480 × 360, 2 × 2-pixel-block, 64-iteration pattern as
\ gfx-mandelbrot.f.  The key difference: z₀ = (px, py) (the pixel),
\ c is the fixed constant above.  This c sits just outside the Mandelbrot
\ set's antenna filament, giving deep spiralling tendrils.
\
\ Rendered one row per scheduler slice by an agent that `pause`s between rows, so
\ the Forth console stays fully responsive while the set draws (see
\ docs/design/wf66_agents.md). Close the pane and the agent exits.
\
\ FP stack protocol for fractal-iter  ( maxiter -- n )
\   ( F: z0x z0y cx cy -- ):
\     push z0x z0y  (the pixel coordinate as starting z)
\     push jl-cr jl-ci  (the fixed c constant)
\     push maxiter  onto the DATA stack

64 constant jl-maxiter
2  constant jl-blk

\ Viewport: real [−1.5 .. 1.5] across 240 cols; imag [−1.2 .. 1.2] across 180 rows.
3.0e  240e f/  fconstant jl-dx
2.4e  180e f/  fconstant jl-dy

-0.7269e  fconstant jl-cr   \ Julia c: real part
 0.1889e  fconstant jl-ci   \ Julia c: imaginary part

variable jl-id        \ the graphics pane id (so the no-arg agent can reach it)
variable jl-row
variable jl-rgb

\ 16-step escape-time palette: dark violet → white → green → black.
: jl-colour ( n -- rgb )
    dup jl-maxiter = if drop 0x060012 exit then
    15 and case
        0  of 0x060012 endof
        1  of 0x16003D endof
        2  of 0x33007C endof
        3  of 0x6020C0 endof
        4  of 0x9040E8 endof
        5  of 0xC070FF endof
        6  of 0xE8A8FF endof
        7  of 0xFFE4FF endof
        8  of 0xFFFFFF endof
        9  of 0xCCFFD4 endof
        10 of 0x88FF9C endof
        11 of 0x44E860 endof
        12 of 0x12C030 endof
        13 of 0x008818 endof
        14 of 0x004408 endof
        15 of 0x001202 endof
    endcase
;

\ The pane controller agent: bind to the pane, render the whole Julia set into ONE
\ batch (so rows accumulate — `gpane-present` REPLACES the pane's batch, it does not
\ stack), `pause`ing each row so the console keeps its turn, present once, then WAIT
\ for the pane's close event cooperatively. Runs as an agent (no args; reads jl-id).
: jl-agent  ( -- )
    jl-id @ (set-pane)             \ bind this agent to its pane (event routing)
    jl-id @ gpane-begin            \ one batch for the whole image
    0x060012 gpane-clear
    180 0 do
        i jl-row !
        240 0 do
            \ Push z₀=(px,py) then c=(cr,ci) onto FP; maxiter onto data.
            i        s>d d>f  jl-dx f*  -1.5e f+
            jl-row @ s>d d>f  jl-dy f*  -1.2e f+
            jl-cr  jl-ci
            jl-maxiter fractal-iter    \ ( -- n )
            jl-colour                  \ ( n -- rgb )
            jl-rgb !
            i jl-blk *   jl-row @ jl-blk *   jl-blk jl-blk   jl-rgb @
            gpane-fill-rect            \ ( -- )
        loop
        pause                       \ <- yield each row so the console stays live
    loop
    jl-id @ gpane-present           \ submit the complete image once
    wait-close                      \ cooperatively wait until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: gfx-julia  ( -- )
    cr ." rendering Julia set ..." cr
    480 360  S" ∴ Julia  c = -0.7269 + 0.1889i"  gpane-open  jl-id !
    jl-id @ 0= if  ." (no UI substrate — demo skipped)" cr  exit then
    ['] jl-agent agent drop
    ." Julia set rendering in the background — keep using the console." cr
;
