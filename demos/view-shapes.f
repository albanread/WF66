\ view-shapes.f — a render-only window in a dozen lines, on the View class.
\
\ A render-only pane overrides just `draw`.  The View base class draws it once,
\ then sits in its cooperative loop WAITING for the close event — repainting on
\ resize — while the console and every other pane stay fully live.  There is no
\ event handling to write and no agent loop to manage: subclass View, paint, and
\ `show-view`.
\
\ Close the pane or the IDE frame to exit.  lib/view.f loads at boot.

View subclass shapes-view
  :m draw ( -- )
     0x0E1116 gpane-clear

     \ A row of filled circles across the top — a little spectrum.
     120  80 36  0xF24C4C gpane-fill-circle
     240  80 36  0xF2A632 gpane-fill-circle
     360  80 36  0x4CD94C gpane-fill-circle
     480  80 36  0x4C8CF2 gpane-fill-circle

     \ Two overlapping translucent-looking rectangles (outlined + filled).
     90 170 200 150        0x243A66 gpane-fill-rect
     90 170 200 150  3     0x6FA8FF gpane-stroke-rect
     230 220 240 130       0x66243A gpane-fill-rect
     230 220 240 130  3    0xFF6FA8 gpane-stroke-rect

     \ A diagonal accent line.
     90 360  540 200  4    0xF2EB32 gpane-line ;m
end-class

shapes-view new my-shapes

: view-shapes ( -- )
    cr ." opening View shapes ..." cr
    640 400  S" ∴ View Shapes"  my-shapes show-view
    ." drawn — resize repaints; close to exit. console stays live." cr ;
