\ WF66 on the BOX: the SWI words, as MF66's (MF66 design/BOX_PORT.md,
\ section 3). regs is R0..R9 and the flags; (swi) and (swi#) are kernel
\ primitives (kernel/io.masm) that call the host.
create regs 11 cells allot
: swi-flags ( -- addr ) regs 10 cells + ;
: r0 ( -- x ) regs @ ;
: r1 ( -- x ) regs 1 cells + @ ;
: r2 ( -- x ) regs 2 cells + @ ;
: r3 ( -- x ) regs 3 cells + @ ;
: r4 ( -- x ) regs 4 cells + @ ;
: r5 ( -- x ) regs 5 cells + @ ;
: r6 ( -- x ) regs 6 cells + @ ;
: r7 ( -- x ) regs 7 cells + @ ;
: s>32 ( n -- u ) 4294967295 and ;
: 32>s ( u -- n ) 4294967295 and dup 2147483648 and if 4294967296 - then ;
: swi# ( c-addr u -- n ) (swi#) ;
: xswi ( n -- err|0 ) regs swap (swi) ;
: .error ( err -- ) 4 + zcount type ;
: swi ( n -- ) xswi ?dup if cr .error cr -2 throw then ;
: >regs ( x0 .. xk-1 k -- ) regs 10 cells 0 fill  dup 0 ?do dup i - pick regs i cells + ! loop  0 ?do drop loop ;
: sys ( x0 .. xk-1 k n -- ) >r >regs r> swi ;
: xsys ( x0 .. xk-1 k n -- err|0 ) >r >regs r> xswi ;
variable zlen
: (z,) ( c-addr u -- addr ) dup zlen ! here dup >r swap move  0 r@ zlen @ + c!  zlen @ 1+ allot r> ;
: [char] ( "c" -- c ) char state @ if postpone literal then ; immediate
: -lead ( c-addr u -- c-addr' u' ) begin dup if over c@ 32 = else 0 then while 1- swap 1+ swap repeat ;
: z" ( "text" -- c-addr ) [char] " parse -lead (z,) state @ if postpone literal then ; immediate
: swi" ( "name" -- n ) [char] " parse -lead (swi#) state @ if postpone literal then ; immediate
\ s" strings in data space, so a SWI can be given them, as in MF66
: (s,) ( c-addr u -- addr u ) dup >r here dup >r swap move r> r> dup allot ;
: s" ( "text" -- addr u ) [char] " parse -lead (s,) state @ if swap postpone literal postpone literal then ; immediate
