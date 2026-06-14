\ demos/hot-fmandel-canvas.f — floating-point Mandelbrot via the CANVAS,
\ computed in Forth with a per-pixel FP escape loop.
\
\ gfx-canvas-mandelbrot.f calls the native FP `fractal-iter`; here every pixel
\ runs a Forth FP escape loop whose running state (zx, zy, cx, cy) is held in
\ `hotfvariable`s. (Float register pinning is disabled — not useful; see
\ docs/design/fp-stack-register-cache.md. hotfvariable still inlines the body
\ address, so f@/f! skip the variable-stub CALL.)
\ Leave-free: once escaped the body stops counting but the bounded loop runs on.
\
\ Driven by a COOPERATIVE pane-agent: the controller binds the pane, renders the
\ image one row per scheduler slice (`pause` each row so the Forth console stays
\ live), blits the canvas, then WAITS for the pane's close event cooperatively.
\ The entry word opens the pane, spawns the agent, and returns immediately.

480 constant hfc-w
360 constant hfc-h
128 constant hfc-maxiter

hfc-w hfc-h *  4 *   buffer: hfc-fb

\ Complex-plane step per pixel. Real [-2.5 .. 1.0] over hfc-w; imag
\ [-1.25 .. 1.25] over hfc-h. The denominators MUST equal hfc-w / hfc-h.
3.5e 480e f/   fconstant hfc-dx
2.5e 360e f/   fconstant hfc-dy

hotfvariable hfc-zx
hotfvariable hfc-zy
hotfvariable hfc-cx
hotfvariable hfc-cy
variable hfc-cnt
variable hfc-live

variable hfc-id       \ the graphics pane id (so the no-arg agent can reach it)

\ Register-pinned FP escape count (same maths as bench hot-fmandel).
: hfc-iter ( maxiter -- count ; F: cx cy -- )
  hfc-cy f!  hfc-cx f!
  0e hfc-zx f!  0e hfc-zy f!  0 hfc-cnt !  -1 hfc-live !
  0 ?do
    hfc-zx f@ fdup f*                 ( F: zx2 )
    hfc-zy f@ fdup f*                 ( F: zx2 zy2 )
    fover fover f+  4e f<  hfc-live @ and  if
      hfc-zx f@ hfc-zy f@ f*  2e f*  hfc-cy f@ f+  hfc-zy f!
      f-  hfc-cx f@ f+  hfc-zx f!
      1 hfc-cnt +!
    else
      fdrop fdrop  0 hfc-live !
    then
  loop
  hfc-cnt @ ;

\ 16-step escape-time palette (same as gfx-canvas-mandelbrot); interior black.
: hfc-colour ( n -- argb )
    dup hfc-maxiter = if drop 0xFF000000 exit then
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
    0xFF000000 or
;

variable hfc-yrow

\ The pane controller agent: bind to the pane, render the whole set into the
\ canvas framebuffer one row per scheduler slice (`pause` each row so the Forth
\ console keeps its turn), blit the canvas, then WAIT for the pane's close event
\ cooperatively. Runs as an agent (no args; reads hfc-id) — wait, never block.
: hot-fmandel-canvas-agent  ( -- )
    hfc-id @ (set-pane)              \ bind this agent to its pane (event routing)
    hfc-h 0 do
        i hfc-yrow !
        hfc-w 0 do
            i        s>d d>f  hfc-dx f*  -2.5e  f+    \ cx  ( F: cx )
            hfc-yrow @ s>d d>f  hfc-dy f*  -1.25e f+  \ cy  ( F: cx cy )
            hfc-maxiter hfc-iter                      \ ( -- n )
            hfc-colour
            hfc-yrow @ hfc-w *  i +  4 *  hfc-fb +  L!
        loop
        pause                       \ <- yield each row so the console stays live
    loop
    hfc-id @ hfc-fb hfc-w hfc-h canvas-blit
    wait-close                      \ cooperatively wait until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: hot-fmandel-canvas
    cr ." rendering " hfc-w . ." x " hfc-h .
    ."  FP (register-pinned) Mandelbrot via canvas ..." cr
    hfc-w hfc-h  S" ∴ Hot FP Mandelbrot (xmm-pinned)"  gpane-open  hfc-id !
    hfc-id @ 0= if  ." (no UI substrate — demo skipped)" cr  exit then
    ['] hot-fmandel-canvas-agent agent drop
    ." Mandelbrot rendering in the background — keep using the console." cr
;
