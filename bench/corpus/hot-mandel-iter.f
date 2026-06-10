\ bench/corpus/hot-mandel-iter.f
\ Register-pinning workload: the integer fixed-point Mandelbrot escape count
\ (same maths as real-mandel-iter.f) but with the running state declared
\ `hotvariable`, so the inner ?do loop keeps the hottest values in registers
\ (r9/r10/r11) instead of memory. Five hot vars, three register slots: the
\ first three by first-use (zx, zy, cy) pin; the rest stay inline+memory.
\ Pin-on vs pin-off (WF64_NO_PIN) isolates the register-pinning win.
\
\ Verdict word: hot-mandel-iter ( cx cy maxiter -- count ).

hotvariable hmi-cx
hotvariable hmi-cy
hotvariable hmi-zx
hotvariable hmi-zy
hotvariable hmi-cnt

: hot-mandel-iter ( cx cy maxiter -- count )
  >r                                    \ R: maxiter ;  stack ( cx cy )
  hmi-cy !  hmi-cx !
  0 hmi-zx !  0 hmi-zy !  0 hmi-cnt !
  r> 0 ?do
    hmi-zx @ dup *  256 /                ( zx2 )
    hmi-zy @ dup *  256 /                ( zx2 zy2 )
    2dup +  1024 >  if  2drop  leave  then
    hmi-zx @ hmi-zy @ *  256 /  2*  hmi-cy @ +   ( zx2 zy2 zynew )
    >r                                  ( zx2 zy2 )  R: zynew
    -  hmi-cx @ +                       ( zxnew )
    hmi-zx !  r> hmi-zy !
    1 hmi-cnt +!
  loop
  hmi-cnt @ ;

\ load-time self-check: a small bounded instance, leave the stack balanced.
-64 0 16 hot-mandel-iter drop
