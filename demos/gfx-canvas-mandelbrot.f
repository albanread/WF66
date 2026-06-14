\ gfx-canvas-mandelbrot.f — high-resolution Mandelbrot via the CANVAS,
\ driven by a COOPERATIVE AGENT so the Forth console stays live.
\
\ Where gfx-mandelbrot.f draws a 240 x 180 grid as one gpane-fill-rect
\ per 2 x 2 block, this renders at FULL per-pixel resolution
\ (640 x 480 = 307200 pixels) by filling a packed BGRA framebuffer in
\ memory and shipping the whole frame across in a SINGLE call:
\
\     canvas-blit  ( child-id addr w h -- )
\
\ That is the "canvas" fast path (rt_canvas_blit -> SurfaceCmd::Blit):
\ Forth owns a `w*h` array of 0xAARRGGBB words and stores into it with
\ native 32-bit writes (L!), so there is ONE boundary crossing per
\ frame instead of one draw command per pixel.  No gpane-begin /
\ gpane-present — canvas-blit submits and repaints on its own.
\
\ Pixel words are 0xAARRGGBB (alpha in the high byte).  In memory,
\ little-endian, that is B,G,R,A — exactly the B8G8R8A8 the blit wants.
\
\ FP stack protocol for fractal-iter ( maxiter -- n ), see igui_gfx.masm:
\   push z0x z0y (both 0 for Mandelbrot), then cx cy, maxiter on data.
\
\ A controller agent renders one row per scheduler slice (`pause`s
\ between rows), blits the finished frame, then WAITS for the pane's
\ close event cooperatively — so the console stays responsive while the
\ HD set draws, and the agent exits when the pane is closed.

640 constant cm-w
480 constant cm-h
128 constant cm-maxiter

\ Framebuffer: cm-w * cm-h pixels, 4 bytes each.  Lives in the
\ dictionary (stable address); every pixel is written before blit,
\ so it never needs zeroing.
cm-w cm-h *  4 *   buffer: cm-fb

\ Complex-plane step per pixel.  The `e` literals MUST equal cm-w / cm-h.
\ Real span  [-2.5 .. 1.0]  = 3.5 across cm-w columns.
\ Imag span  [-1.25 .. 1.25] = 2.5 across cm-h rows.
3.5e 640e f/   fconstant cm-dx
2.5e 480e f/   fconstant cm-dy

variable cm-id        \ the graphics pane id (so the no-arg agent can reach it)
variable cm-row

\ 16-step escape-time palette: deep navy -> ice-white -> amber -> black,
\ made opaque (alpha 0xFF).  Interior points (n = maxiter) are opaque black.
: cm-colour ( n -- argb )
    dup cm-maxiter = if drop 0xFF000000 exit then
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

\ Compute every pixel into the framebuffer (no drawing yet).  `pause`s once
\ per row so the cooperative pump keeps the console live as the frame fills.
: cm-render ( -- )
    cm-h 0 do
        i cm-row !
        cm-w 0 do
            0e 0e
            i        s>d d>f  cm-dx f*  -2.5e  f+    \ cx
            cm-row @ s>d d>f  cm-dy f*  -1.25e f+    \ cy
            cm-maxiter fractal-iter                  \ ( -- n )
            cm-colour                                \ ( n -- argb )
            cm-row @ cm-w *  i +  4 *  cm-fb +  L!   \ store pixel
        loop
        pause                                        \ <- yield each row so the console stays live
    loop
;

\ The pane controller agent: bind to the pane, render the full frame (pausing
\ each row), upload it in ONE bulk canvas-blit, then WAIT for the pane's close
\ event cooperatively. Runs as an agent (no args; reads cm-id). While it waits
\ the worker keeps serving the console — wait, never block.
: cm-agent  ( -- )
    cm-id @ (set-pane)                  \ bind this agent to its pane (event routing)
    cm-render
    cm-id @ cm-fb cm-w cm-h canvas-blit \ one bulk upload — submits + repaints
    wait-close                          \ cooperatively wait until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: gfx-canvas-mandelbrot  ( -- )
    cr ." rendering " cm-w . ." x " cm-h . ." Mandelbrot via canvas ..." cr
    cm-w cm-h  S" ∴ Mandelbrot HD"  gpane-open  cm-id !
    cm-id @ 0= if  ." (no UI substrate — demo skipped)" cr  exit then
    ['] cm-agent agent drop
    ." Mandelbrot rendering in the background — keep using the console." cr
;
