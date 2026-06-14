\ demos/hot-mandel-canvas.f — Mandelbrot via the CANVAS, computed in Forth.
\
\ Unlike gfx-canvas-mandelbrot.f (which calls the native FP `fractal-iter`),
\ every pixel here runs an INTEGER fixed-point escape loop written in Forth
\ whose running state is `hotvariable` — so the compiler keeps the hot values
\ in registers (r9/r10/r11) across the inner ?do loop (register pinning, on by
\ default; WF64_NO_PIN turns it off). This is the same pinned loop the
\ hot-mandel-iter bench measures, exercised in a real render.
\
\ Rendered by a COOPERATIVE PANE-AGENT: the entry word opens the pane and spawns
\ a controller agent, then returns immediately so the Forth console stays live.
\ The agent computes the whole framebuffer (pausing once per row so the console
\ keeps its turn), uploads it in one blit, then WAITS for the pane's close event
\ cooperatively. See docs/design/wf66_pane_agent_gui.md.
\
\ Fixed point: 8 fractional bits (1.0 = 256).
\   real [-2.5 .. 1.0]   -> cx in [-640 .. 256]  (span 896)
\   imag [-1.25 .. 1.25] -> cy in [-320 .. 320]  (span 640)

480 constant hmc-w
360 constant hmc-h
128 constant hmc-maxiter

\ Framebuffer: hmc-w * hmc-h pixels, 4 bytes each (0xAARRGGBB; little-endian
\ B,G,R,A — the B8G8R8A8 the blit wants). Lives in the dictionary.
hmc-w hmc-h *  4 *   buffer: hmc-fb

\ Register-pinned inner state. Five hot vars, three register slots: the first
\ three by first use (zx, zy, cy) pin; cx and cnt stay inline+memory.
hotvariable hmc-cx
hotvariable hmc-cy
hotvariable hmc-zx
hotvariable hmc-zy
hotvariable hmc-cnt

\ Integer fixed-point escape count. Identical maths to bench hot-mandel-iter.
: hmc-iter ( cx cy maxiter -- count )
  >r
  hmc-cy !  hmc-cx !
  0 hmc-zx !  0 hmc-zy !  0 hmc-cnt !
  r> 0 ?do
    hmc-zx @ dup *  256 /                ( zx2 )
    hmc-zy @ dup *  256 /                ( zx2 zy2 )
    2dup +  1024 >  if  2drop  leave  then
    hmc-zx @ hmc-zy @ *  256 /  2*  hmc-cy @ +   ( zx2 zy2 zynew )
    >r
    -  hmc-cx @ +                        ( zxnew )
    hmc-zx !  r> hmc-zy !
    1 hmc-cnt +!
  loop
  hmc-cnt @ ;

\ 16-step escape-time palette (same as gfx-canvas-mandelbrot); interior is
\ opaque black, all entries forced opaque (alpha 0xFF).
: hmc-colour ( n -- argb )
    dup hmc-maxiter = if drop 0xFF000000 exit then
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

variable hmc-id        \ the canvas pane id (so the no-arg agent can reach it)
variable hmc-yrow

\ Compute every pixel into the framebuffer (no drawing yet). One `pause` per row
\ keeps the cooperative pump serving the console while the image computes.
: hmc-render ( -- )
    hmc-h 0 do
        i hmc-yrow !
        hmc-w 0 do
            i        896 *  hmc-w /  -640 +          \ cx = -640 + i*896/w
            hmc-yrow @  640 *  hmc-h /  -320 +       \ cy = -320 + row*640/h
            hmc-maxiter hmc-iter                     \ ( -- n )
            hmc-colour                               \ ( n -- argb )
            hmc-yrow @ hmc-w *  i +  4 *  hmc-fb +  L!
        loop
        pause                       \ <- yield each row so the console stays live
    loop
;

\ The pane controller agent: bind to the pane, compute the whole framebuffer
\ (pausing per row), upload it in one bulk blit, then WAIT cooperatively for the
\ pane's close event. Runs as an agent (no args; reads hmc-id).
: hmc-agent ( -- )
    hmc-id @ (set-pane)             \ bind this agent to its pane (event routing)
    hmc-render
    hmc-id @ hmc-fb hmc-w hmc-h canvas-blit   \ one bulk upload
    ." done — close the window to exit" cr
    wait-close                      \ cooperatively wait until the pane closes
;

\ Open the pane and spawn the drawing agent; returns immediately so the console
\ stays interactive while the agent renders under the cooperative pump.
: hot-mandel-canvas
    cr ." rendering " hmc-w . ." x " hmc-h .
    ."  integer (register-pinned) Mandelbrot via canvas ..." cr
    hmc-w hmc-h  S" ∴ Hot Mandelbrot (pinned)"  gpane-open
    dup 0= if drop ." (no UI substrate — demo skipped)" cr exit then
    hmc-id !
    ['] hmc-agent agent drop
    ." Mandelbrot rendering in the background — keep using the console." cr
;
