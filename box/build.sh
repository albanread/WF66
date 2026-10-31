#!/bin/sh
# build.sh -- WF66 for the Intel BOX: libwf66-box.a, which rosgd's Makefile
# links into the Intel /init (A7232ToolChain modules/forth), as MF66's
# libmf66-box.a is linked into the Arm one.
#
#   sh box/build.sh
#
# cargo (the box's pinned Rust, 1.97.1) builds the box crate (the Forth,
# JASM's front end, x86-64 encoder and loader, Rust's std) for
# x86_64-unknown-linux-musl as a static library. Its objects are then linked
# into one (ld.lld -r) and every symbol but the C API's (mf66_box_*, MF66's
# names: the module is the same on both boxes) made local, so Rust's runtime
# and unwinder never meet another library's in /init. Debug information is
# dropped.
#
# Output: BOX/caches/wf66/out-x86_64/libwf66-box.a, and MF66's mf66_box.h
# (the interface is MF66's; its checkout is beside this one).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)              # BOX
out=${WF66_BOX_OUT:-$root/caches/wf66/out-x86_64}
target=x86_64-unknown-linux-musl
rust=${RUST_TOOLCHAIN:-1.97.1}
CARGO=${CARGO:-$(rustup which --toolchain "$rust" cargo)}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
export RUSTC
OBJCOPY=${OBJCOPY:-$(ls /opt/homebrew/opt/llvm@22/bin/llvm-objcopy 2>/dev/null || command -v llvm-objcopy)}
LLD=${LLD:-$(command -v ld.lld)}
AR=${AR:-$(dirname "$OBJCOPY")/llvm-ar}
[ -x "$AR" ] || AR=ar
header=${MF66_HEADER:-$here/../../MF66/box/mf66_box.h}
[ -f "$header" ] || { echo "build.sh: no MF66 checkout with box/mf66_box.h beside this one (MF66_HEADER)" >&2; exit 1; }

cd "$here"
"$CARGO" build --release --target $target
lib="$here/target/$target/release/libwf66_box.a"

mkdir -p "$out/obj"
rm -f "$out/obj/"*.o
echo "link: libwf66-box.a"
# Rust's own libunwind goes in too (made local with the rest): /init has none.
unwind="$("$RUSTC" --print sysroot)/lib/rustlib/$target/lib/self-contained/libunwind.a"
"$LLD" -r -o "$out/obj/wf66_all.o" --whole-archive "$lib" --no-whole-archive "$unwind"
printf '%s\n' mf66_box_set_hooks mf66_box_boot mf66_box_end mf66_box_running \
    mf66_box_line mf66_box_eval mf66_box_compiling mf66_box_recover \
    mf66_box_depth mf66_box_stack mf66_box_base mf66_box_words mf66_box_word_help >"$out/obj/exports.txt"
"$OBJCOPY" --strip-debug --keep-global-symbols="$out/obj/exports.txt" \
    "$out/obj/wf66_all.o" "$out/obj/wf66_box.o"
rm -f "$out/libwf66-box.a" "$out/obj/wf66_all.o"
"$AR" rcs "$out/libwf66-box.a" "$out/obj/wf66_box.o"
cp "$header" "$out/mf66_box.h"
ls -l "$out/libwf66-box.a"
