\ lib/view.f — View: a base class for pane-agent apps.
\
\ "Your object is the window." One View instance owns one graphical pane, driven
\ by its own cooperative agent (green thread), so it stays responsive while the
\ console and other panes keep running. Subclass View, override `draw` and the
\ on-* event hooks; the base class owns the agent loop, the pane binding, and the
\ event dispatch — you never write an agent loop.
\
\ A minimal app:
\
\     View subclass clock-view
\       cell ivar: ticks
\       :m draw   ( -- )  0x101820 gpane-clear  ... ;m
\       :m on-tick ( ms -- )  drop  ticks 1+ to ticks  self -> redraw ;m
\     end-class
\
\     clock-view new my-clock
\     480 360  S" ∴ Clock"  my-clock show-view      \ opens + spawns; returns at once
\
\ See docs/multitasking-in-forth.md for the cooperative-agent model underneath,
\ and lib/agents.f for the agent words ((set-pane), pane-event, (spawn-recv)).

forth-wordlist set-current

object subclass View
  cell ivar: pane-id          \ the child_id this view owns (its gpane window)

  \ ── Overridable hooks. Defaults are safe: they consume their arguments and do
  \ ── nothing (on-resize repaints). Override the ones your app cares about. The
  \ ── parameter order matches gpane's events (see ev-* in lib/core.f).
  :m draw      ( -- )                          ;m   \ paint the pane (override me)
  :m on-key    ( vkey mods down repeat -- )    2drop 2drop ;m
  :m on-char   ( codepoint mods -- )           2drop ;m
  :m on-mouse  ( x y op btn|mods -- )          2drop 2drop ;m
  :m on-tick   ( time-ms -- )                  drop ;m
  :m on-focus  ( gained? -- )                  drop ;m
  :m on-close  ( -- )                          ;m   \ called once, after the loop ends

  \ ── Accessors / setters (rarely overridden).
  :m set-pane-id ( child_id -- )  to pane-id ;m
  :m pane        ( -- child_id )  pane-id ;m

  \ ── Repaint: bracket the subclass's `draw` in one batch and present it. A
  \ ── canvas/blit view (which submits on its own) overrides this to just `draw`.
  :m redraw  ( -- )
     pane-id gpane-begin
     self -> draw
     gpane-present ;m

  \ on-resize default: repaint to fill the new size. (Defined after `redraw` so
  \ there is no forward selector reference.)
  :m on-resize ( width height -- )  2drop  self -> redraw ;m

  \ ── Decode one pane event and call the matching hook. Returns true on close.
  \ pane-event yields ( p4 p3 p2 p1 kind ) with the parameters reversed; we flip
  \ them back to natural order ( p1 p2 p3 p4 ) once, then route on `kind`.
  :m dispatch ( p4 p3 p2 p1 kind -- done? )
     >r  swap 2swap swap  r>                  \ -> ( p1 p2 p3 p4 kind )
     dup ev-close  = if  drop 2drop 2drop                    true   exit then
     dup ev-mouse  = if  drop              self -> on-mouse  false  exit then
     dup ev-key    = if  drop              self -> on-key    false  exit then
     dup ev-char   = if  drop 2drop        self -> on-char   false  exit then
     dup ev-resize = if  drop 2drop        self -> on-resize false  exit then
     dup ev-tick   = if  drop 2drop drop   self -> on-tick   false  exit then
     dup ev-focus  = if  drop 2drop drop   self -> on-focus  false  exit then
     drop 2drop 2drop  false                  \ unknown kind: ignore, keep running
  ;m

  \ ── The controller agent's body: bind the pane, draw once, then run the event
  \ ── loop cooperatively (pane-event yields, so the console stays live), and call
  \ ── on-close when the window is asked to close.
  :m run ( -- )
     pane-id (set-pane)
     self -> redraw
     begin  pane-event self -> dispatch  until
     self -> on-close ;m
end-class

\ Generic agent entry point: send `run` to whichever View the agent carries.
: view-run  ( -- )  (recv) -> run ;

\ Open a window for a View instance and spawn its controller agent, then return
\ immediately so the console stays interactive. ( w h c-addr u view -- )
: show-view  ( w h c-addr u view -- )
   >r                                          \ stash the view object
   gpane-open                                  \ ( -- child_id )
   dup 0= if  drop  r> drop
              ." (no UI substrate — view skipped)" cr  exit then
   r@ -> set-pane-id                           \ tell the view its pane id
   r> ['] view-run (spawn-recv)                \ spawn its agent (carrying the view)
   dup (ready-push) drop ;                     \ ...and make it runnable
