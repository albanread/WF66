//! WF66 for the Intel BOX: the C API the native module (A7232ToolChain
//! rosgd/modules/forth/forth.c) calls. It is MF66's (MF66 box/mf66_box.h,
//! which this crate's build copies), so the module and its console are the
//! same on Arm and Intel; the hooks table and the console's lists are MF66's
//! too (the f66-host crate). See box/README.md.
//!
//! One session at a time, owned here. Every entry point catches a Rust panic
//! and reports it as an error, so a panic never unwinds into C.

use std::panic::{catch_unwind, AssertUnwindSafe};

use f66_host::{console, Hooks};
use wf64::Wf64Session;

/// The words the box adds in Forth: the register block and the SWI words.
pub const PRELUDE: &str = include_str!("prelude.fs");

static mut SESSION: Option<Wf64Session> = None;

#[allow(static_mut_refs)]
fn session() -> Option<&'static mut Wf64Session> {
    unsafe { SESSION.as_mut() }
}

fn say(s: &str) {
    match f66_host::hooks().and_then(|h| h.write) {
        Some(w) => w(s.as_ptr(), s.len()),
        None => eprint!("{s}"),
    }
}

fn guarded(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            let m = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            say(&format!("\nF66: internal error: {m}\n"));
            -1
        }
    }
}

/// The text of an error, without the output already printed.
fn message(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or("").to_string()
}

/// Install the host's hooks (a table that lives as long as the program).
#[no_mangle]
pub extern "C" fn mf66_box_set_hooks(h: *const Hooks) {
    f66_host::set_hooks(h);
}

/// Boot a fresh session (ending any other): 0, or -1 with the reason printed.
#[no_mangle]
pub extern "C" fn mf66_box_boot() -> i32 {
    guarded(|| {
        unsafe { SESSION = None };
        let mut s = match Wf64Session::builtin() {
            Ok(s) => s,
            Err(e) => {
                say(&format!("F66: the Forth did not start: {e:#}\n"));
                return -1;
            }
        };
        if let Err((line, e)) = s.eval_source(PRELUDE) {
            say(&format!("F66: prelude line {line}: {}\n", message(&e)));
            return -1;
        }
        console::defined().clear();
        note_definitions(PRELUDE);
        unsafe { SESSION = Some(s) };
        0
    })
}

/// End the session.
#[no_mangle]
pub extern "C" fn mf66_box_end() {
    guarded(|| {
        unsafe { SESSION = None };
        0
    });
}

/// Is there a session?
#[no_mangle]
pub extern "C" fn mf66_box_running() -> i32 {
    session().is_some() as i32
}

fn bytes<'a>(p: *const u8, n: usize) -> &'a str {
    if p.is_null() || n == 0 {
        return "";
    }
    let b = unsafe { std::slice::from_raw_parts(p, n) };
    std::str::from_utf8(b).unwrap_or_else(|e| {
        // Latin-1 text: keep what is valid; RISC OS files are mostly ASCII.
        std::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or("")
    })
}

/// Interpret one line (the REPL): 0 done, 1 `bye`, -1 an error (printed, the
/// input state reset). A definition may stay open across lines.
#[no_mangle]
pub extern "C" fn mf66_box_line(p: *const u8, n: usize) -> i32 {
    guarded(|| {
        let Some(s) = session() else { return -1 };
        let line = bytes(p, n).to_string();
        note_definitions(&line);
        match s.eval_line(&line) {
            Ok(_) => s.bye_requested() as i32,
            Err(e) => {
                say(&format!(" ? {}\n", message(&e)));
                -1
            }
        }
    })
}

/// Interpret a whole source text: an error names its line
/// (`name:line: error: ...`). 0, 1 at `bye`, -1 at the first error.
#[no_mangle]
pub extern "C" fn mf66_box_eval(p: *const u8, n: usize, name: *const u8, nlen: usize) -> i32 {
    guarded(|| {
        let Some(s) = session() else { return -1 };
        let name = bytes(name, nlen).to_string();
        // Copy: the text may be in a buffer the program itself reuses.
        let src = bytes(p, n).to_string();
        note_definitions(&src);
        match s.eval_source(&src) {
            Ok(_) => s.bye_requested() as i32,
            Err((line, e)) => {
                say(&format!("\n{name}:{line}: error: {}\n", message(&e)));
                -1
            }
        }
    })
}

/// Is a definition open (the REPL prints ` compiled` rather than ` ok`)?
#[no_mangle]
pub extern "C" fn mf66_box_compiling() -> i32 {
    session().map(|s| s.compiling() as i32).unwrap_or(0)
}

/// After a fault escaped from Forth code: drop any half-made definition and
/// clear the stacks.
#[no_mangle]
pub extern "C" fn mf66_box_recover() {
    guarded(|| {
        if let Some(s) = session() {
            s.recover();
        }
        0
    });
}

// ── For a console (the module's Wimp REPL window) ─────────────────────────

/// The data stack's depth.
#[no_mangle]
pub extern "C" fn mf66_box_depth() -> i64 {
    session().map(|s| s.depth() as i64).unwrap_or(0)
}

/// Up to `max` stack cells into `out`, top first: how many were written.
#[no_mangle]
pub extern "C" fn mf66_box_stack(out: *mut i64, max: usize) -> i64 {
    let Some(s) = session() else { return 0 };
    if out.is_null() {
        return 0;
    }
    let v = s.stack();
    let n = v.len().min(max);
    for (i, x) in v.iter().take(n).enumerate() {
        unsafe { out.add(i).write(*x) };
    }
    n as i64
}

/// The number base (`base`).
#[no_mangle]
pub extern "C" fn mf66_box_base() -> i64 {
    session().map(|s| s.base() as i64).unwrap_or(10)
}

/// The words a session knows, newest first, separated by spaces, into buf
/// (zero-terminated, cut at max): the length written.
#[no_mangle]
pub extern "C" fn mf66_box_words(buf: *mut u8, max: usize) -> usize {
    let t = console::words(wf64::PRIMITIVES.iter().map(|p| p.0));
    console::put_cstr(&t, buf, max)
}

/// A word's stack comment, as `( a b -- c )`, into buf (zero-terminated):
/// its length, 0 if none is known.
#[no_mangle]
pub extern "C" fn mf66_box_word_help(name: *const u8, n: usize, buf: *mut u8, max: usize) -> usize {
    match console::word_help(bytes(name, n)) {
        Some(c) => console::put_cstr(&c, buf, max),
        None => 0,
    }
}

/// An included file's text, for the console's word list (through the
/// host's files: RISC OS names in the box).
fn read_included(name: &str) -> Option<String> {
    let h = f66_host::hooks()?;
    let (open, read, close) = (h.file_open?, h.file_read?, h.file_close?);
    let fh = open(name.as_ptr(), name.len(), 1);
    if fh == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fh, buf.as_mut_ptr(), buf.len());
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fh);
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn note_definitions(text: &str) {
    console::note_definitions(text, &read_included);
}
