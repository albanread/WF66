\ bench/corpus/hot-fmandel.f
\ Floating-point Mandelbrot escape count with hotfvariable state (zx, zy, cx, cy).
\ f@/f! move them to/from the FP stack for the f* / f+ / f- math.
\
\ NOTE: float register pinning is DISABLED (not useful — see
\ docs/design/fp-stack-register-cache.md and src/pin.rs ENABLE_FLOAT_PINNING).
\ When it was on it removed the 8 f@/f! per loop (call_count 42 -> 34) for only
\ ~2%, because f+/f*/f-/f< are themselves calls that round-trip the whole FP
\ stack through memory — pinning a few user vars can't touch that. This corpus
\ is kept as an unpinned float-loop codegen baseline.
\
\ Leave-free by design (a recorded FP loop with `leave` is not yet supported):
\ once escaped, the body stops counting but the bounded ?do still runs to
\ maxiter. The count is the escape iteration, same as the integer version.
\
\ Verdict word: hot-fmandel ( maxiter -- count ; F: cx cy -- ).

hotfvariable fm-zx
hotfvariable fm-zy
hotfvariable fm-cx
hotfvariable fm-cy
variable fm-cnt
variable fm-live          \ 1 while still bounded, 0 after escape

: hot-fmandel ( maxiter -- count ; F: cx cy -- )
  fm-cy f!  fm-cx f!                  \ pop cy, cx off the FP stack
  0e fm-zx f!  0e fm-zy f!  0 fm-cnt !  -1 fm-live !
  0 ?do
    fm-zx f@ fdup f*                  ( F: zx2 )
    fm-zy f@ fdup f*                  ( F: zx2 zy2 )
    fover fover f+  4e f<             ( F: zx2 zy2 ; -- bounded? )
    fm-live @ and  if                \ still bounded AND not yet escaped
      fm-zx f@ fm-zy f@ f*  2e f*  fm-cy f@ f+   ( F: zx2 zy2 zynew )
      fm-zy f!                        ( F: zx2 zy2 -- fm-zy := zynew )
      f-  fm-cx f@ f+                 ( F: zxnew )
      fm-zx f!                        ( F: )
      1 fm-cnt +!
    else                             \ escaped (or already escaped): stop counting
      fdrop fdrop  0 fm-live !
    then
  loop
  fm-cnt @ ;

\ load-time self-check: a bounded interior point; leave the stack balanced.
-0.5e 0e 16 hot-fmandel drop
