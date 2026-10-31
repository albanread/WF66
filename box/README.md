# WF66 on the Intel BOX

The BOX is our 64-bit RISC OS translated to C over Linux (A7232ToolChain,
rosgd). Its F66 module (rosgd/modules/forth) runs MF66 on the Arm box; on
the Intel box it runs WF66 through the same C interface, MF66's
`box/mf66_box.h`. MF66's design/BOX_PORT.md describes the module, the
commands, the SWI words and the console; this file says what is WF66's.

## Build

    sh box/build.sh

builds this crate with the box's pinned Rust (1.97.1) for
x86_64-unknown-linux-musl and makes `BOX/caches/wf66/out-x86_64/libwf66-box.a`
(every symbol but `mf66_box_*` local) and copies MF66's header beside it.
rosgd's Makefile links it into the Intel `/init`. It needs the MF66 checkout
beside this one (`../MF66`: the f66-host crate and the header) and JASM
(`../JASM`), as checked out in BOX/src.

### The exact commands

From the checkouts in /Volumes/xc/BOX/src (the ordinary ones, not
worktrees), on the Studio:

    cd /Volumes/xc/BOX/src
    git -C JASM pull --ff-only origin main
    git -C MF66 pull --ff-only origin main
    git -C NewGC pull --ff-only origin main
    git -C WF66 pull --ff-only origin master
    rustup target add x86_64-unknown-linux-musl --toolchain 1.97.1   # once
    cd WF66 && sh box/build.sh

It needs LLVM's `ld.lld` and `llvm-objcopy` (Homebrew's llvm@22). NewGC is
github.com/albanread/GC-, checked out as `src/NewGC`. rosgd's Makefile
(`MF66_GUEST` for ARCH x86_64) finds the library in
`BOX/caches/wf66/out-x86_64`; `tools/build-intel-mac.sh` links it into the
Intel ROM.

## Rebuilding the library

From the checkouts in BOX/src (WF66, with JASM, MF66 and NewGC beside it):

    cd /Volumes/xc/BOX/src/WF66
    git pull --ff-only origin master
    (cd ../JASM && git pull --ff-only origin main)
    (cd ../MF66 && git pull --ff-only origin main)
    (cd ../NewGC && git pull --ff-only origin main)
    sh box/build.sh

This writes `BOX/caches/wf66/out-x86_64/libwf66-box.a` and MF66's
`mf66_box.h`, which rosgd's Makefile links when it builds the Intel box
(`tools/build-intel-mac.sh`). NewGC is github.com/albanread/GC-, checked
out as `NewGC`. `WF66_BOX_OUT=<dir>` writes elsewhere.

## How WF66 runs in /init

- **Loader.** JASM's `NativeJit` on Linux: the kernel in an anonymous
  mapping, read-execute once placed. The session's 128 MB region (stacks,
  user area, dictionary and compiled code) and the 32 MB code arena are
  mapped within rel32 reach of it (`jitmem::map_near`), never in the low
  4 GB, which is RISC OS's.
- **Data space** (`here`, `allot`, `create`, variables, `s"` and `z"`
  strings, compiled string literals) and the **stacks and user area**
  (`pad`, the parse buffers) are in the region the host gives: the dynamic
  area "F66 Forth", 16 MB and 768 KB. Their addresses are RISC OS addresses.
- **Control structures outside a definition** (`3 0 do i . loop` at the
  prompt or at a file's top level, included files too) are run as
  `:noname ... ; execute`, by f66-host's `wrap`, which MF66 uses the same way.
- **Calls to the host** keep the Win64 convention: `forth_main` and every
  `rt_*` function are `extern "win64"` (the same as `extern "C"` on
  Windows), so the kernel and the code generator are the same on both.
- **The kernel's Win32 imports** (KERNEL32's heap, files, Sleep, time;
  msvcrt's maths) are bound on Linux to Rust functions of the same shape in
  `src/winshim.rs`, which use the host's hooks: `allocate` takes RMA
  blocks, files are RISC OS files, `ms` is the host's wait.
- **Output, key, key?, include** go through the hooks. Under the box an
  undefined word is an error (THROW -13), quit's own " ok" is left out
  (the module prints its own) and the library's output at boot is dropped.
- **Faults and Escape** leave Forth code by the module's long jump;
  `mf66_box_recover` resets the stacks and the interpreter's state.

`box/examples/run.rs` is a driver for Linux x86-64 without a host:
`wf66-run file.fth`.
