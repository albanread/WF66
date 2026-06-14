\ view-counter.f — the click counter, rebuilt on the View class.
\
\ Compare gfx-click.f: there you wrote the (set-pane) call, the pane-event loop,
\ the event-kind IF chain, AND the initial render by hand.  Here you subclass
\ View, override `draw` + `on-mouse` + `on-char`, and call `show-view` — the base
\ class owns the controller agent, the pane binding, the event loop, and the
\ dispatch.  "Your object is the window."
\
\ Left-click the square to cycle its colour and bump the counter; press SPACE to
\ reset.  The Forth console (and every other pane) stays live the whole time —
\ the pane WAITS for input without BLOCKING.  Close the pane or the IDE frame to
\ exit.  lib/view.f loads at boot, so there is nothing to `require` here.
\
\ See lib/view.f for the View base class and docs/multitasking-in-forth.md for
\ the cooperative-agent model underneath.

\ Six bright hues; (count mod 6) picks one.
: vc-colour ( n -- rgb )
    6 mod case
        0 of 0xF24C4C endof   \ red
        1 of 0xF2A632 endof   \ orange
        2 of 0xF2EB32 endof   \ yellow
        3 of 0x4CD94C endof   \ green
        4 of 0x4C8CF2 endof   \ blue
        5 of 0xCC66F2 endof   \ violet
    endcase ;

View subclass counter-view
  cell ivar: clicks                       \ how many times the square was clicked

  \ Paint the scene.  redraw (the base class) brackets this in gpane-begin /
  \ gpane-present, so `draw` is pure painting — no begin/present here.
  :m draw ( -- )
     0x10131A gpane-clear
     50 60 200 200  clicks vc-colour  gpane-fill-rect ;m

  \ Is (x, y) inside the square at (50,60)-(250,260)?
  :m hit? ( x y -- ? )
     >r  50 251 within  r> 60 261 within  and ;m

  \ Left-press inside the square: bump the counter and repaint.
  :m on-mouse ( x y op btn|mods -- )
     drop                                 \ button/mods unused
     mouse-left-down <> if  2drop  exit  then
     self -> hit? if  clicks 1+ to clicks  self -> redraw  then ;m

  \ SPACE resets the counter (back to red).
  :m on-char ( codepoint mods -- )
     drop                                 \ mods unused
     bl = if  0 to clicks  self -> redraw  then ;m
end-class

counter-view new my-counter               \ one instance — "the window is an object"

: view-counter ( -- )
    cr ." opening View click counter ..." cr
    480 360  S" ∴ View Counter"  my-counter show-view
    ." running — click the square, SPACE resets. console stays live." cr ;
