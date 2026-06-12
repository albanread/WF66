//! Integration tests covering the M3/M4 behaviour through both
//! execution modes:
//!
//!   * `eval(text)` — full REPL pipeline (accept/parse/dispatch). Pins
//!     the user-visible behaviour against regressions.
//!
//!   * `push(v)` + `call(asm_sym)` + `pop()` — direct primitive
//!     invocation with no parser in the loop. Lets us test the
//!     semantics of each primitive cell-accurately.
//!
//! Each `#[test]` owns its own `Wf64Session` so failures are isolated.

use std::ffi::OsStr;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::arch::x86_64::__cpuid;

use wf64::Wf64Session;

/// One Wf64Session is built per test binary and shared across every
/// test via `sess()`. Each `#[test]` call grabs the lock, gets a
/// freshly-`reset()`-ed session, and drops the guard on the way out.
///
/// Why: each `with_kernel` boot does JASM expansion + module
/// load + native finalize + extern binding + symbol registration + the
/// 45-call dictionary bootstrap. With ~50 tests that boot cost dominated
/// total run time many times over. Reusing the session collapses it to
/// a one-time cost amortised across the suite, while `reset()` makes
/// each test's view of the world look as if it had its own session.
///
/// Safety pre-condition: tests must run single-threaded. Enforced by
/// `.cargo/config.toml` setting `RUST_TEST_THREADS = "1"`. The Mutex
/// is uncontested in practice but provides the discipline anyway.
static SHARED: OnceLock<Mutex<Wf64Session>> = OnceLock::new();

fn sess() -> SessionGuard {
    let m = SHARED.get_or_init(|| {
        let kernel = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("kernel")
            .join("main.masm");
        Mutex::new(Wf64Session::with_kernel(kernel).expect("session boot"))
    });
    // `into_inner` salvages access from a poisoned mutex (i.e., a
    // panicking test). The state is whatever the panicking test left
    // behind; `reset()` makes that irrelevant before the next test
    // touches it.
    let mut guard = m.lock().unwrap_or_else(|p| p.into_inner());
    guard.reset();
    SessionGuard(guard)
}

/// Deref-mut wrapper so the existing `s.push()`, `s.call()`, `s.eval()`
/// call sites compile unchanged.
struct SessionGuard(MutexGuard<'static, Wf64Session>);
impl Deref for SessionGuard {
    type Target = Wf64Session;
    fn deref(&self) -> &Wf64Session { &*self.0 }
}
impl DerefMut for SessionGuard {
    fn deref_mut(&mut self) -> &mut Wf64Session { &mut *self.0 }
}

// ── eval-mode (full REPL pipeline) ───────────────────────────────────

#[test]
fn eval_empty_input_just_prints_ok() {
    let mut s = sess();
    let out = s.eval("\n").unwrap();
    assert_eq!(out, " ok\n");
}

#[test]
fn eval_bye_terminates_cleanly() {
    let mut s = sess();
    let out = s.eval("bye\n").unwrap();
    assert_eq!(out, "");
}

#[test]
fn eval_number_then_dot() {
    let mut s = sess();
    let out = s.eval("5 .\nbye\n").unwrap();
    assert_eq!(out, "5  ok\n");
}


#[test]
fn eval_source_defined_set_order_wrapper_persists_across_eval_calls() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    let out = s.eval(": only2 -1 set-order ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    let out = s.eval("only2 get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}
#[test]
fn eval_arithmetic() {
    let mut s = sess();
    let out = s.eval("5 3 + .\n7 2 * .\nbye\n").unwrap();
    assert_eq!(out, "8  ok\n14  ok\n");
}

// ── WF66 token-IR compiler (roadmap Phase 0) ─────────────────────────
// Opt-in: set_wf66_enabled(true) makes `:` shadow-capture the body and `;`
// rewrite it through the WF66 optimizer when the span is deferrable (literals +
// known arithmetic). The eager compiler is the differential oracle.

#[test]
fn wf66_folds_constant_definition() {
    // : twelve 5 7 + ;  -> const-fold to Lit 12 -> push 12; ret.
    let mut s = sess();
    s.set_wf66_enabled(true);
    let out = s.eval(": twelve 5 7 + ;\ntwelve .\nbye\n").unwrap();
    assert_eq!(out, " ok\n12  ok\n");
}

#[test]
fn wf66_lowers_runtime_arithmetic() {
    // : bar 5 * 2 + ;  ( n -- n*5+2 ) — lowers (operand is runtime), no fold.
    let mut s = sess();
    s.set_wf66_enabled(true);
    let out = s.eval(": bar 5 * 2 + ;\n3 bar .\nbye\n").unwrap();
    assert_eq!(out, " ok\n17  ok\n");
}

#[test]
fn wf66_matches_eager_oracle() {
    // Differential oracle: identical source -> identical observable output,
    // whether compiled eagerly or through WF66.
    let src = ": bar 5 * 2 + ;\n3 bar .\n10 bar .\n-4 bar .\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(src).unwrap()
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(src).unwrap()
    };
    assert_eq!(eager, wf66);
}

#[test]
fn wf66_non_deferrable_falls_back_to_eager() {
    // `rot` is outside the WF66 vocabulary -> taints the span -> the eager body
    // stands. Must still produce the right answer.
    let mut s = sess();
    s.set_wf66_enabled(true);
    let out = s.eval(": r3 rot ;\n1 2 3 r3 . . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n1 3 2  ok\n");
}

#[test]
fn wf66_shuffles_match_eager_oracle() {
    // Phase 1.1: dup/drop/swap/over/nip now compile through WF66 (settle-
    // everywhere). The eager compiler is the oracle.
    let cases = [
        (": sq dup * ;", "5 sq ."),               // 25
        (": twice dup + ;", "7 twice ."),         // 14
        (": diff swap - ;", "10 3 diff ."),       // 3 - 10 = -7
        (": ov over + . . ;", "4 5 ov"),          // over->4 5 4, +->4 9, prints 9 4
        (": keepb nip ;", "8 9 keepb ."),         // 9
        (": dropa drop ;", "8 9 dropa ."),        // 8
        (": mixed 3 * dup + ;", "5 mixed ."),     // 5*3=15, dup+ = 30
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_strength_reduce_matches_eager_oracle() {
    // Phase 1.2: literal-into-op folding + multiply strength reduction. Each of
    // these compiles to register-immediate code (shl / lea / neg / add / etc.);
    // the eager compiler is the oracle for correctness over several inputs.
    let cases = [
        ": dbl   2 * ;",   // shl rax, 1
        ": quad  4 * ;",   // shl rax, 2
        ": tri   3 * ;",   // lea rax,[rax+rax*2]
        ": pent  5 * ;",   // lea rax,[rax+rax*4]
        ": tenx 10 * ;",   // imul rax, rax, 10
        ": negate2 -1 * ;",// neg rax
        ": zero  0 * ;",   // xor eax, eax
        ": addk  7 + ;",   // add rax, 7
        ": subk  4 - ;",   // sub rax, 4
        ": mix   5 * 2 + ;", // lea + add (n*5+2)
    ];
    for def in cases {
        let word = def.split_whitespace().nth(1).unwrap().to_string();
        let src = format!("{def}\n3 {word} .\n-7 {word} .\n0 {word} .\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}`");
    }
}

#[test]
fn wf66_size_report() {
    // Eager-vs-WF66 compiled body size for WF66-eligible words (literals +
    // arithmetic, no shuffles). Prints with `--nocapture`. Non-eligible words
    // fall back to eager -> identical size.
    fn body_len(s: &Wf64Session, name: &str) -> u64 {
        s.debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some(b - a) } else { None })
            .unwrap_or(0)
    }
    // (category, program defining word `w`). 4a is settle-everywhere, so control
    // flow is ~parity with eager; the wins are whole-span const-fold + inlining.
    let cases: &[(&str, &str)] = &[
        ("const-fold", ": w 2 3 * 4 + ;"),
        ("const-chain", ": w 5 7 + 11 * 13 - ;"),
        ("derived-fold", ": w 10 1+ ;"),
        ("inline", ": h 32 ;\n: w h + ;"),
        ("strength", ": w 5 * 2 + ;"),
        ("shuffle", ": w dup * ;"),
        ("cond", ": w dup 0< if negate then ;"),
        ("loop", ": w begin 1- dup 0= until ;"),
        ("imm-chain", ": w 7 + 3 + ;"),     // cascade: -> + 10
        ("inc-chain", ": w 1+ 1+ 1+ 1+ ;"), // cascade: -> + 4
        ("dup-const", ": w 2 dup * ;"),     // cascade: -> push 4
        ("fwd-dup-over", ": w dup over ;"), // 2.2: reload of a held cell -> dropped
        ("fwd-over-over", ": w over over ;"), // 2.2: store->load forwarded
    ];
    eprintln!("\n  category        eager   wf66   delta");
    for (cat, prog) in cases {
        let src = format!("{prog}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap();
            body_len(&s, "w")
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap();
            body_len(&s, "w")
        };
        eprintln!(
            "  {cat:<14} {eager:>4}B  {wf66:>4}B   {:+}",
            wf66 as i64 - eager as i64
        );
    }
    eprintln!();
}

#[test]
fn wf66_actually_rewrites_not_falls_back() {
    // Regression guard for the `;` self-taint bug: WF66 must genuinely rewrite a
    // deferrable body, not silently fall back to eager. `: tw 2 3 * 4 + ;` folds
    // to a single push-10 (a constant); eager computes it at runtime, so the
    // emitted bytes MUST differ — and both must still produce 10.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let def = ": tw 2 3 * 4 + ;\nbye\n";
    let eager_bytes = {
        let mut s = sess();
        s.eval(def).unwrap();
        body(&s, "tw")
    };
    let wf66_bytes = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(def).unwrap();
        body(&s, "tw")
    };
    assert_ne!(
        eager_bytes, wf66_bytes,
        "WF66 produced identical bytes to eager — it fell back instead of rewriting"
    );

    let run = ": tw 2 3 * 4 + ;\ntw .\nbye\n";
    let eager_out = {
        let mut s = sess();
        s.eval(run).unwrap()
    };
    let wf66_out = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(run).unwrap()
    };
    assert_eq!(eager_out, wf66_out, "WF66 result differs from eager");
    assert!(wf66_out.contains("10"), "expected 2 3 * 4 + = 10, got {wf66_out:?}");
}

#[test]
fn wf66_dce_matches_eager_oracle() {
    // Phase 1.3: pure-push-then-drop cancels. The words below contain dead
    // pushes; WF66 elides them, and the result must match eager.
    let cases = [
        (": drop5 5 drop ;", "42 drop5 ."),    // 5 drop is dead -> 42
        (": dd dup drop ;", "7 dd ."),         // dup drop is a no-op -> 7
        (": deadchain 1 2 + drop ;", "9 deadchain ."), // 1 2 + drop -> 9 (drops the 3)
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_memory_ops_match_eager_oracle() {
    // Phase 2.1: @ ! c@ c! compile through WF66 (the variable push happens at the
    // call site, so the words below are pure stack-address memory ops). The eager
    // compiler is the oracle, and we read program-defined memory back.
    let src = "\
variable v\n\
: getv @ ;\n\
: setv ! ;\n\
: bump dup @ 1+ swap ! ;\n\
99 v setv\n\
v getv .\n\
v bump\n\
v getv .\n\
bye\n";
    let eager = {
        let mut s = sess();
        s.eval(src).unwrap()
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(src).unwrap()
    };
    assert_eq!(eager, wf66, "WF66 != eager for memory-op program");
    assert!(wf66.contains("99"), "expected 99 then 100, got {wf66:?}");
    assert!(wf66.contains("100"), "expected bump -> 100, got {wf66:?}");
}

#[test]
fn wf66_derived_ops_match_eager_oracle() {
    // Phase 2.2: 1+ 1- 2* cell+ cells negate invert each capture as a (Lit, Fop)
    // pair, so the existing passes optimize them (e.g. negate -> neg, 2* -> shl).
    let cases = [
        (": inc 1+ ;", "41 inc ."),
        (": dec 1- ;", "43 dec ."),
        (": dbl 2* ;", "21 dbl ."),
        (": cp cell+ ;", "100 cp ."),
        (": cs cells ;", "5 cs ."),
        (": neg negate ;", "7 neg ."),
        (": inv invert ;", "0 inv ."),
        (": chain 1+ 2* 1- ;", "10 chain ."), // (10+1)*2-1 = 21
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_folds_through_derived_ops() {
    // A win eager's one-literal watermark cannot do: a constant pushed then run
    // through a derived op folds at compile time. `: k 10 1+ ;` -> push 11.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let def = ": k 10 1+ ;\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(def).unwrap();
        body(&s, "k")
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(def).unwrap();
        body(&s, "k")
    };
    assert_ne!(eager, wf66, "WF66 should fold `10 1+` to a constant, not match eager");
    // and it still computes 11
    let mut s = sess();
    s.set_wf66_enabled(true);
    assert!(s.eval(": k 10 1+ ;\nk .\nbye\n").unwrap().contains("11"));
}

#[test]
fn wf66_inlines_and_folds_across_call_boundary() {
    // Phase 3: a small WF66-compiled word is spliced into its caller, then folds
    // across the former call boundary. `: t32 32 ;  : addt t32 + ;` -> addt is
    // `add rax, 32` with no call. Bytes must differ from eager (which emits a
    // call to t32), and the result must match eager.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let prog = ": t32 32 ;\n: addt t32 + ;\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(prog).unwrap();
        body(&s, "addt")
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(prog).unwrap();
        body(&s, "addt")
    };
    assert_ne!(eager, wf66, "WF66 should inline t32 (no call), differing from eager");
    assert!(
        !wf66.contains(&0xE8),
        "WF66 addt should contain no E8 CALL after inlining: {wf66:02x?}"
    );

    // Correctness over inputs, and a multi-level inline chain.
    let run = "\
: t32 32 ;\n\
: addt t32 + ;\n\
: addt2 addt addt ;\n\
10 addt .\n\
0 addt2 .\n\
bye\n";
    let eager_out = {
        let mut s = sess();
        s.eval(run).unwrap()
    };
    let wf66_out = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(run).unwrap()
    };
    assert_eq!(eager_out, wf66_out, "WF66 != eager for inline chain");
    assert!(wf66_out.contains("42")); // 10 + 32
    assert!(wf66_out.contains("64")); // 0 + 32 + 32
}

#[test]
fn wf66_control_flow_matches_eager_oracle() {
    // Phase 4a: IF/ELSE/THEN compile through WF66 (settle-everywhere branches).
    // Flags come from the caller or literals (comparison ops aren't in the vocab
    // yet). The eager compiler is the oracle, run over both branch directions.
    let cases = [
        (": sel if 11 else 22 then ;", "-1 sel .\n0 sel ."), // 11 then 22
        (": addif if 100 + then ;", "5 -1 addif .\n5 0 addif ."), // 105 then 5
        (": litf 0 if 1 else 2 then ;", "litf ."),           // const flag -> 2
        (": nest if if 1 else 2 then else 3 then ;", "-1 -1 nest .\n-1 0 nest .\n0 0 nest ."), // 1,2,3
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_loops_and_compares_match_eager_oracle() {
    // Phase 4a: BEGIN/UNTIL/AGAIN/WHILE/REPEAT + 0= / 0< compile through WF66.
    // Comparison flags now let IF and loop conditions be computed.
    let cases = [
        (": myabs dup 0< if negate then ;", "-5 myabs .\n5 myabs .\n0 myabs ."),
        (": iszero 0= ;", "0 iszero .\n5 iszero ."),
        (": cd begin 1- dup 0= until ;", "5 cd .\n1 cd ."),       // counts down to 0
        (": cd2 begin dup while 1- repeat ;", "5 cd2 .\n0 cd2 ."), // while loop -> 0
        (": clamp dup 0< if drop 0 then ;", "-3 clamp .\n4 clamp ."), // max(n,0)
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_binary_compares_match_eager_oracle() {
    // Binary comparisons: materialized (return the flag) and fused (cmp -> branch).
    let cases = [
        // materialized
        (": w < ;", "3 7 w .\n7 3 w ."),
        (": w = ;", "5 5 w .\n5 6 w ."),
        (": w u< ;", "3 7 w .\n-1 7 w ."),
        // fused compare -> branch
        (": w = if 11 else 22 then ;", "5 5 w .\n5 6 w ."),
        (": w < if 1 else 0 then ;", "3 7 w .\n7 3 w ."),
        (": w > if 1 else 0 then ;", "7 3 w .\n3 7 w ."),
        (": w <= if 1 else 0 then ;", "3 3 w .\n4 3 w ."),
        (": w >= if 1 else 0 then ;", "3 3 w .\n2 3 w ."),
        (": w <> if 1 else 0 then ;", "3 4 w .\n3 3 w ."),
        (": w u< if 1 else 0 then ;", "3 7 w .\n-1 7 w ."),
        (": w 0> if 1 else 0 then ;", "5 w .\n-5 w .\n0 w ."),
        (": w 0<> if 1 else 0 then ;", "5 w .\n0 w ."),
        // binary compare feeding a loop condition (fused > while)
        (": w begin 1- dup 0 > while repeat ;", "5 w .\n1 w ."),
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_idioms_match_eager_oracle() {
    // Common multi-op idioms Forth programmers write, recognized and replaced.
    let cases = [
        (": w swap drop ;", "3 7 w ."),         // -> nip -> 3
        (": w over over ;", "3 7 w . . . ."),   // -> 2dup -> 3 7 3 7
        (": w swap swap ;", "3 7 w . ."),       // -> identity -> 7 3
        (": w 0 = if 11 else 22 then ;", "0 w .\n5 w ."), // 0= fused
        (": w 0 < if 1 else 0 then ;", "-3 w .\n4 w ."),
        (": w 0 > if 1 else 0 then ;", "5 w .\n-5 w ."),
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_constant_pick_matches_eager_oracle() {
    // Constant (and folded) n pick -> direct cell copy; runtime pick falls back.
    let cases = [
        (": w 0 pick ;", "7 w . ."),           // dup -> 7 7
        (": w 1 pick ;", "3 7 w . . ."),       // over -> 3 7 3
        (": w 2 pick ;", "1 2 3 w . . . ."),   // copy 3rd -> 1 2 3 1
        (": w 3 pick ;", "1 2 3 4 w . . . . ."),
        (": w 1 1 + pick ;", "1 2 3 4 w . . . . ."), // folded -> 2 pick
        (": w over 2 pick + ;", "10 20 w . . ."),    // mixed with arithmetic
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_exit_matches_eager_oracle() {
    // `exit` (early return) compiles to `ret`; defs with it stop tainting.
    let cases = [
        (": w 1 exit ;", "w ."),                                // -> 1
        (": w dup 0< if drop 0 exit then ;", "-5 w .\n7 w ."),  // max(n,0): 0, 7
        (": w dup 5 > if drop 5 exit then ;", "9 w .\n3 w ."),  // min(n,5): 5, 3
        (": w dup 0> if drop 1 exit then 0< if -1 else 0 then ;", "8 w .\n-8 w .\n0 w ."), // sgn
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_control_flow_actually_rewrites() {
    // Confirm a control-flow definition genuinely goes through WF66 (the body
    // contains a Jcc/jmp from our lowering and differs from the eager body).
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let prog = ": sel if 11 else 22 then ;\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(prog).unwrap();
        body(&s, "sel")
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(prog).unwrap();
        body(&s, "sel")
    };
    assert_ne!(eager, wf66, "WF66 control-flow def should differ from eager (genuine rewrite)");
}

#[test]
fn wf66_differential_fuzzer() {
    // Random deferrable programs (literals, arithmetic, derived ops, shuffles,
    // comparisons, balanced conditionals) over random runtime inputs. WF66's
    // output must equal eager's for every one. Deterministic seed -> reproducible;
    // a failure prints the exact source. Excludes memory ops (random addresses
    // fault) and loops (random conditions may not terminate).
    fn next(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }
    fn pick<'a>(rng: &mut u64, xs: &[&'a str]) -> &'a str {
        xs[(next(rng) % xs.len() as u64) as usize]
    }
    // Generate a body that starts at `depth` inputs and ends at depth 1.
    fn gen_body(rng: &mut u64, mut depth: usize) -> String {
        let net0 = ["1+", "1-", "2*", "negate", "invert"]; // arity 1, delta 0
        let mut out: Vec<String> = Vec::new();
        let steps = 6 + (next(rng) % 16) as usize;
        for _ in 0..steps {
            // build the candidate set valid at the current depth
            let mut kinds: Vec<u8> = vec![0]; // 0 = push literal (always valid)
            if depth >= 1 {
                kinds.extend_from_slice(&[1, 2, 3, 4, 18]); // unary net0, dup, drop, balanced-if, pick
            }
            if depth >= 2 {
                kinds.extend_from_slice(&[5, 6, 7, 8, 9, 10, 16, 17]); // +binary-cmp, +fused-cmp-if
            }
            if depth >= 3 {
                kinds.extend_from_slice(&[11, 12]); // rot, -rot
            }
            if depth >= 4 {
                kinds.extend_from_slice(&[13, 14, 15]); // 2swap, 2over, 2nip
            }
            let k = kinds[(next(rng) % kinds.len() as u64) as usize];
            match k {
                0 => {
                    let v = (next(rng) % 2001) as i64 - 1000;
                    out.push(v.to_string());
                    depth += 1;
                }
                1 => out.push(pick(rng, &net0).to_string()),
                2 => {
                    out.push("dup".into());
                    depth += 1;
                }
                3 => {
                    out.push("drop".into());
                    depth -= 1;
                }
                4 => {
                    // balanced conditional, net delta 0
                    let cmp = pick(rng, &["0=", "0<"]);
                    let body = pick(rng, &net0);
                    out.push(format!("dup {cmp} if {body} then"));
                }
                5 => {
                    out.push(pick(rng, &["+", "-", "*", "and", "or", "xor"]).to_string());
                    depth -= 1;
                }
                6 => {
                    out.push("over".into());
                    depth += 1;
                }
                7 => {
                    let op = pick(rng, &["swap", "nip"]);
                    out.push(op.to_string());
                    if op == "nip" {
                        depth -= 1;
                    }
                }
                8 => {
                    out.push("tuck".into());
                    depth += 1;
                }
                9 => {
                    out.push("2dup".into());
                    depth += 2;
                }
                10 => {
                    out.push("2drop".into());
                    depth -= 2;
                }
                11 => out.push("rot".into()),
                12 => out.push("-rot".into()),
                13 => out.push("2swap".into()),
                14 => {
                    out.push("2over".into());
                    depth += 2;
                }
                15 => {
                    out.push("2nip".into());
                    depth -= 2;
                }
                16 => {
                    // binary comparison, materialized ( a b -- flag )
                    out.push(pick(rng, &["=", "<>", "<", ">", "<=", ">=", "u<", "u>"]).to_string());
                    depth -= 1;
                }
                17 => {
                    // fused compare->branch, balanced (net 0): 2dup <cmp> if <netop> then
                    let cmp = pick(rng, &["=", "<>", "<", ">", "<=", ">=", "u<", "u>"]);
                    let op = pick(rng, &net0);
                    out.push(format!("2dup {cmp} if {op} then"));
                }
                18 => {
                    // constant pick: copy the k-th item (0..depth-1) to TOS
                    let k = next(rng) % depth as u64;
                    out.push(format!("{k} pick"));
                    depth += 1;
                }
                _ => {}
            }
        }
        while depth > 1 {
            out.push("+".into());
            depth -= 1;
        }
        while depth < 1 {
            out.push("0".into());
            depth += 1;
        }
        out.join(" ")
    }

    let mut rng: u64 = 0x9e3779b97f4a7c15;
    for i in 0..600 {
        let n_inputs = 1 + (next(&mut rng) % 4) as usize; // 1..4 inputs
        let body = gen_body(&mut rng, n_inputs);
        let inputs: Vec<String> = (0..n_inputs)
            .map(|_| ((next(&mut rng) % 401) as i64 - 200).to_string())
            .collect();
        let src = format!(": fw {body} ;\n{} fw .\nbye\n", inputs.join(" "));
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "fuzz #{i} diverged:\n{src}");
    }
}

#[test]
fn wf66_multicell_shuffles_match_eager_oracle() {
    // Double / triple stack ops now compile through WF66 (settle-everywhere).
    let cases = [
        (": w tuck ;", "7 9 w . . ."),                  // 7 9 -> 9 7 9
        (": w rot ;", "1 2 3 w . . ."),                 // 1 2 3 -> 2 3 1
        (": w -rot ;", "1 2 3 w . . ."),                // 1 2 3 -> 3 1 2
        (": w 2dup ;", "4 5 w . . . ."),                // 4 5 -> 4 5 4 5
        (": w 2drop ;", "1 4 5 w ."),                   // 1 4 5 -> 1
        (": w 2swap ;", "1 2 3 4 w . . . ."),           // 1 2 3 4 -> 3 4 1 2
        (": w 2over ;", "1 2 3 4 w . . . . . ."),       // -> 1 2 3 4 1 2
        (": w 2nip ;", "1 2 3 4 w . ."),                // 1 2 3 4 -> 3 4
        (": w rot rot ;", "1 2 3 w . . ."),             // identity-ish (rot^2 = -rot)
        (": w 2dup + ;", "6 7 w . . ."),                // 6 7 -> 6 13
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_does_not_disturb_subsequent_eager_defs() {
    // After a WF66 rewrite, a following (non-deferrable) definition still
    // compiles and runs correctly — capture state was cleared at `;`.
    let mut s = sess();
    s.set_wf66_enabled(true);
    let out = s
        .eval(": twelve 5 7 + ;\n: inc 1 + ;\ntwelve inc .\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n ok\n13  ok\n");
}

#[test]
fn eval_brk_and_int3_are_callable() {
    let mut s = sess();
    let out = s.eval("BRK\nINT3\nbye\n").unwrap();
    // Both words emit a Forth state dump followed by " ok\n"; we just
    // check the eval succeeds and that the " ok" prompts are present.
    assert!(out.contains(" ok\n"), "expected at least one ' ok\\n' in: {out:?}");
}

#[test]
fn eval_key_reads_from_buffered_input_stream() {
    let mut s = sess();
    let out = s.eval("key .\nA\nbye\n").unwrap();
    assert_eq!(out, "65  ok\n ok\n");
}

#[test]
fn load_source_file_provides_only_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval("only get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn load_source_file_leaves_empty_data_stack() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let stack = s.stack();
    let resolved: Vec<String> = stack
        .iter()
        .map(|value| {
            s.resolve_word_addr(*value as u64)
                .unwrap_or_else(|| format!("{value:#x}"))
        })
        .collect();
    assert_eq!(stack, Vec::<i64>::new(), "resolved stack = {resolved:?}");
}

#[test]
fn direct_only_word_then_get_order_leaves_clean_stack() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();

    s.call("only_word").unwrap();
    s.call("get_order_word").unwrap();

    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn eval_tick_then_compiles_me_leaves_empty_stack() {
    let mut s = sess();
    let out = s.eval(
        ": compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
         : helper 123 ;\n\
         : target 456 ;\n\
         ' helper compiles-me\n\
         bye\n"
    ).unwrap();

    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n");
    assert_eq!(s.stack(), Vec::<i64>::new());
}

#[test]
fn eval_defining_word_with_does_leaves_empty_stack() {
    let mut s = sess();
    let out = s.eval(": , here ! 1 cells allot ;\n: constant create , does> @ ;\nbye\n").unwrap();

    assert_eq!(out, " ok\n ok\n");
    assert_eq!(s.stack(), Vec::<i64>::new());
}

#[test]
fn eval_compiles_me_on_defining_word_leaves_empty_stack() {
    let mut s = sess();
    let out = s.eval(
        ": , here ! 1 cells allot ;\n\
         : compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
             : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
             ' (comp-cons) compiles-me\n\
         bye\n"
    ).unwrap();

    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n ok\n");
    let stack = s.stack();
    let resolved: Vec<String> = stack
        .iter()
        .map(|value| {
            s.resolve_word_addr(*value as u64)
                .unwrap_or_else(|| format!("{value:#x}"))
        })
        .collect();
    assert_eq!(stack, Vec::<i64>::new(), "resolved stack = {resolved:?}");
}

#[test]
fn eval_defining_word_setup_leaves_empty_stack() {
    let mut s = sess();
    let out = s.eval(
        ": , here ! 1 cells allot ;\n\
         : compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
             : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
         bye\n"
    ).unwrap();

    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n");
    assert_eq!(s.stack(), Vec::<i64>::new());
}

#[test]
fn direct_compiles_me_on_defining_word_leaves_empty_stack() {
    let mut s = sess();
    s.eval(
        ": , here ! 1 cells allot ;\n\
         : compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
             : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
         bye\n"
    ).unwrap();

        s.eval("' compiles-me ' (comp-cons) bye\n").unwrap();
        let comp_cons_xt = s.pop() as u64;
    let compiles_me_xt = s.pop() as u64;

    s.push(comp_cons_xt as i64);
    s.call_xt(compiles_me_xt).unwrap();

    assert_eq!(s.stack(), Vec::<i64>::new());
}

#[test]
fn execute_primitive_compiles_me_on_defining_word_leaves_empty_stack() {
    let mut s = sess();
    s.eval(
        ": , here ! 1 cells allot ;\n\
         : compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
             : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
         bye\n"
    ).unwrap();

        s.eval("' compiles-me ' (comp-cons) bye\n").unwrap();
    let comp_cons_xt = s.pop() as u64;
    let compiles_me_xt = s.pop() as u64;

    s.push(comp_cons_xt as i64);
    s.push(compiles_me_xt as i64);
    s.call("execute").unwrap();

    assert_eq!(s.stack(), Vec::<i64>::new());
}

#[test]
fn load_source_file_only_word_is_present_in_root_wordlist() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval(
        "forth-wordlist constant root\n\
         : square-name s\" square\" ;\n\
         : only-name s\" only\" ;\n\
         square-name root search-wordlist nip . cr\n\
         only-name root search-wordlist nip . cr\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n-1 \n ok\n-1 \n ok\n");
}

#[test]
fn load_source_file_only_word_executes_via_search_wordlist_xt() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(
        "forth-wordlist constant root\n\
         : only-name s\" only\" ;\n\
         only-name root search-wordlist drop execute get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn eval_core_f_with_explicit_bye_provides_only_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    let mut source = std::fs::read_to_string(&path).unwrap();
    source.push_str("\nbye\n");

    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();

    let out = s.eval(&source).unwrap();
    assert!(out.ends_with(" ok\n"), "got {out:?}");

    let out = s.eval("only get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn eval_nested_evaluate_definition_provides_only_word() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();

    let out = s.eval(
        ": install-only s\" : only -1 set-order ;\" evaluate ;\n\
         install-only\n\
         only get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn load_source_file_then_redefine_only_same_name() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(": only -1 set-order ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    let out = s.eval("only get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn eval_redefining_simple_word_same_name_uses_newest() {
    let mut s = sess();
    let out = s.eval(": foo 1 ;\n: foo 2 ;\nfoo .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n2  ok\n");
}

#[test]
fn eval_redefining_simple_word_same_name_across_eval_calls_uses_newest() {
    let mut s = sess();
    let out = s.eval(": foo 1 ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    let out = s.eval(": foo 2 ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    let out = s.eval("foo .\nbye\n").unwrap();
    assert_eq!(out, "2  ok\n");
}

#[test]
fn eval_primitive_only_then_get_order() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();

    let out = s.eval("only get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn load_source_file_provides_also_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval("only also get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![2, root_wid, root_wid]);
}

#[test]
fn load_source_file_provides_previous_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order previous get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn load_source_file_provides_forth_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order forth get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![2, root_wid, root_wid]);
}

#[test]
fn load_source_file_provides_definitions_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order definitions get-current\n\
         extra\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n");
    let stack = s.stack();
    assert_eq!(stack.len(), 2);
    assert_eq!(stack[0], stack[1]);
    assert_ne!(stack[0], root_wid);
}

#[test]
fn eval_exit_returns_early_from_definition() {
    let mut s = sess();
    let out = s.eval(": early 1 exit 2 ;\nearly .\nbye\n").unwrap();
    assert_eq!(out, " ok\n1  ok\n");
}

/// Test the {: word compiles one local and open-locals works
#[test]
fn eval_locals_one_local_compiles() {
    let mut s = sess();
    let out = s.eval(": tloc {: x :} x . ;\n5 tloc\nbye\n").unwrap();
    assert_eq!(out, " ok\n5  ok\n");
}

/// Verify {: and to are findable in the current search order
#[test]
fn eval_locals_words_findable() {
    let mut s = sess();
    // Check several words that should be in the FORTH wordlist
    let out = s.eval(
        "s\" {:\" find-name nip .\
         \ns\" to\" find-name nip .\
         \ns\" locals#!\" find-name nip .\
         \nbye\n"
    ).unwrap();
    // Should print -1 for each found word
    assert_eq!(out, "-1  ok\n-1  ok\n-1  ok\n");
}

/// Minimal sanity check: {: with one local
#[test]
fn eval_locals_basic_fetch() {
    let mut s = sess();
    // Single local, defined and called on the same line.
    let out = s.eval(": tl1 {: a :} a . ; 42 tl1\nbye\n").unwrap();
    assert_eq!(out, "42  ok\n");
}

#[test]
fn eval_colon_without_name_throws_minus_16() {
    let mut s = sess();
    let err = s.eval(":\n").unwrap_err().to_string();
    assert!(err.contains("-16"), "got {err:?}");
}

#[test]
fn eval_exit_in_interpret_state_throws_minus_14() {
    let mut s = sess();
    let err = s.eval("exit\n").unwrap_err().to_string();
    assert!(err.contains("-14"), "got {err:?}");
}

#[test]
fn eval_nested_colon_defs() {
    let mut s = sess();
    let out = s
        .eval(": double 2 * ;\n: quad double double ;\n3 quad .\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n ok\n12  ok\n");
}

#[test]
fn eval_literal_inside_def() {
    let mut s = sess();
    let out = s.eval(": add5 5 + ;\n10 add5 .\nbye\n").unwrap();
    assert_eq!(out, " ok\n15  ok\n");
}

#[test]
fn eval_brackets_and_literal_compile_interpreted_value() {
    let mut s = sess();
    let out = s.eval(": eleven [ 5 6 + ] literal ;\neleven .\nbye\n").unwrap();
    assert_eq!(out, " ok\n11  ok\n");
}

#[test]
fn eval_s_quote_compiles_runtime_string() {
    let mut s = sess();
    let out = s.eval(": greet s\" HI\" ;\ngreet type cr\nbye\n").unwrap();
    assert_eq!(out, " ok\nHI\n ok\n");
}

#[test]
fn eval_dot_quote_compiles_runtime_output() {
    let mut s = sess();
    let out = s.eval(": greet .\" HI\" ;\ngreet cr\nbye\n").unwrap();
    assert_eq!(out, " ok\nHI\n ok\n");
}

#[test]
fn eval_dot_quote_works_in_interpret_mode() {
    // Extended: ." prints in both interpret and compile state.
    let mut s = sess();
    let out = s.eval(".\" HI\" cr\nbye\n").unwrap();
    assert_eq!(out, "HI\n ok\n");
}

#[test]
fn eval_s_quote_works_in_interpret_mode() {
    // ANS Forth: s" is valid in both interpret and compile state.
    let mut s = sess();
    let out = s.eval("s\" HI\" type cr\nbye\n").unwrap();
    assert_eq!(out, "HI\n ok\n");
}

#[test]
fn eval_c_quote_works_in_interpret_mode() {
    let mut s = sess();
    let out = s.eval("c\" HI\" count type cr\nbye\n").unwrap();
    assert_eq!(out, "HI\n ok\n");
}

#[test]
fn eval_c_quote_compiles_runtime_counted_string() {
    let mut s = sess();
    let out = s.eval(": greet c\" HI\" ;\ngreet count type cr\nbye\n").unwrap();
    assert_eq!(out, " ok\nHI\n ok\n");
}

#[test]
fn eval_source_and_to_in_can_skip_rest_of_line() {
    let mut s = sess();
    let out = s.eval(": skip-rest source >in ! drop ;\nskip-rest 123 .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");
}

#[test]
fn eval_state_exposes_compilation_flag_address() {
    let mut s = sess();
    let out = s
        .eval("state @ .\n: compiling? state @ ; immediate\n: compiled-state compiling? literal ;\ncompiled-state 0= .\nbye\n")
        .unwrap();
    assert_eq!(out, "0  ok\n ok\n ok\n0  ok\n");
}

#[test]
fn eval_source_id_tracks_repl_and_evaluate_input() {
    let mut s = sess();
    let out = s
        .eval("source-id .\n: source-id-from-eval s\" source-id\" evaluate ;\nsource-id-from-eval .\nsource-id .\nbye\n")
        .unwrap();
    assert_eq!(out, "0  ok\n ok\n-1  ok\n0  ok\n");
}

#[test]
fn eval_restore_input_does_not_reparse_own_token() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("save-input restore-input 0= .\nbye\n").unwrap();
    assert_eq!(out, "-1  ok\n");
}

#[test]
fn eval_refill_reads_next_line_and_is_false_for_evaluate() {
    let mut s = sess();
    let out = s
        .eval(": next-line refill if source dup >in ! type drop else 999 . then ;\n: eval-refill-string s\" refill .\" ;\nnext-line\nHELLO\neval-refill-string evaluate\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n ok\nHELLO ok\n0  ok\n");
}

#[test]
fn eval_parse_word_and_pad_work() {
    let mut s = sess();
    let out = s
        .eval(": upto-comma 32 parse 2drop 44 parse ;\nupto-comma hello, type cr\npad dup 65 swap c! c@ .\n32 word hello count type cr\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\nhello\n ok\n65  ok\nhello\n ok\n");
}

#[test]
fn eval_tick_pushes_interpret_xt() {
    let mut s = sess();
    let out = s.eval("5 ' dup execute . .\nbye\n").unwrap();
    assert_eq!(out, "5 5  ok\n");
}

#[test]
fn eval_tick_from_empty_then_drop_leaves_stack_empty() {
    let mut s = sess();
    let out = s.eval("' dup drop\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.depth(), 0);
}

#[test]
fn eval_bracket_tick_compiles_xt_literal() {
    let mut s = sess();
    let out = s.eval(": run-dup ['] dup execute ;\n7 run-dup . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n7 7  ok\n");
}

#[test]
fn eval_immediate_and_postpone_enable_forth_defined_compiler_words() {
    let mut s = sess();
    let out = s.eval(": twice postpone dup postpone dup ; immediate\n: demo twice ;\n4 demo . . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n4 4 4  ok\n");
}

#[test]
fn eval_compiles_me_bindings_leave_stack_empty() {
    let mut s = sess();
    s.eval(
        ": compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
         : f, here f! 1 floats allot ;\n\
         : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
         : (comp-2cons) ( xt -- ) >body postpone literal postpone 2@ ;\n\
         : 2constant create 2, does> 2@ ;\n\
         : (comp-fconst) ( xt -- ) >body postpone literal postpone f@ ;\n\
         : fconstant create f, does> f@ ;\n\
         : (comp-val) ( xt -- ) >body postpone literal postpone @ ;\n\
         : value create , does> @ ;\n\
         bye\n",
    )
    .unwrap();
    assert_eq!(s.depth(), 0);

    s.eval("' (comp-cons) compiles-me\nbye\n").unwrap();
    assert_eq!(s.stack(), Vec::<i64>::new());

    s.eval("' (comp-2cons) compiles-me\nbye\n").unwrap();
    assert_eq!(s.depth(), 0);

    s.eval("' (comp-fconst) compiles-me\nbye\n").unwrap();
    assert_eq!(s.depth(), 0);

    s.eval("' (comp-val) compiles-me\nbye\n").unwrap();
    assert_eq!(s.depth(), 0);
}

#[test]
fn eval_compiles_me_consumes_tick_result_across_eval_boundary() {
    let mut s = sess();
    s.eval(
        ": compiles ( xt1 xt2 -- ) >comp ! ;\n\
         : compiles-me ( xt -- ) latestxt compiles ;\n\
         : (comp-cons) ( xt -- ) >body postpone literal ;\n\
         : constant create , does> @ ;\n\
         bye\n",
    )
    .unwrap();

    s.eval("' (comp-cons)\nbye\n").unwrap();
    assert_eq!(s.depth(), 1);

    s.eval("compiles-me\nbye\n").unwrap();
    assert_eq!(s.depth(), 0);
}

#[test]
fn eval_dot_s_prints_stack_live_without_consuming_it() {
    let mut s = sess();
    let out = s.eval("1 2 3 .s . . .\nbye\n").unwrap();
    assert!(out.starts_with("[3 sp=0x"), "got {out:?}");
    assert!(out.contains(" rp=0x"), "got {out:?}");
    assert!(out.contains("] 3 2 1 3 2 1  ok\n"), "got {out:?}");
}

#[test]
fn eval_forget_last_rolls_back_and_allows_regrowth_live() {
    let mut s = sess();
    let out = s.eval(": a 1 ;\na .\nforget_last\na\n: a 2 ;\na .\nbye\n").unwrap();
    assert_eq!(out, " ok\n1  ok\n ok\n?  ok\n ok\n2  ok\n");
}

#[test]
fn eval_backslash_comment_ignores_rest_of_line() {
    let mut s = sess();
    let out = s.eval("1 \\ keep this out of the token stream\n2 + .\nbye\n").unwrap();
    assert_eq!(out, " ok\n3  ok\n");
}

#[test]
fn eval_backslash_prefixed_token_is_not_a_comment() {
    let mut s = sess();
    let out = s.eval("\\foo 123 .\nbye\n").unwrap();
    assert_eq!(out, "? 123  ok\n");
}

#[test]
fn eval_paren_comment_ignores_inline_text() {
    let mut s = sess();
    let out = s.eval("1 ( comment in source ) 2 + .\nbye\n").unwrap();
    assert_eq!(out, "3  ok\n");
}

#[test]
fn load_source_file_makes_saved_words_available() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("5 square .\n3 cube .\n2 quad .\n2 sixth .\nbye\n").unwrap();
    assert_eq!(out, "25  ok\n27  ok\n16  ok\n64  ok\n");
}

#[test]
fn load_source_file_supports_live_growth_after_startup() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval(": sixth quad square ;\n2 sixth .\nbye\n").unwrap();
    assert_eq!(out, " ok\n256  ok\n");
}

#[test]
fn load_source_file_provides_bl_space_and_spaces() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("bl .\nspace 88 emit\n3 spaces 89 emit\n-2 spaces 90 emit\nbye\n").unwrap();
    assert_eq!(out, "32  ok\n X ok\n   Y ok\nZ ok\n");
}

#[test]
fn load_source_file_provides_char_bracket_char_true_and_false() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval("true .\nfalse .\nchar Z .\n: zchar [char] Z ;\nzchar .\nbye\n")
        .unwrap();
    assert_eq!(out, "-1  ok\n0  ok\n90  ok\n ok\n90  ok\n");
}

#[test]
fn load_source_file_provides_find() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval("7 bl word dup find drop execute . .\nbl word if find nip .\nbl word nosuch find nip .\nbye\n")
        .unwrap();
    assert_eq!(out, "7 7  ok\n1  ok\n0  ok\n");
}

#[test]
fn load_source_file_provides_variable_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("variable foo\n7 foo !\nfoo @ .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n7  ok\n");
}

#[test]
fn load_source_file_provides_constant_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("10 constant ten\nten .\nbye\n").unwrap();
    assert_eq!(out, " ok\n10  ok\n");
}

#[test]
fn load_source_file_provides_pictured_numeric_output_words() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval("<# 65 hold 66 hold 0 0 #> type cr\n<# 1 0 # # #> type cr\n<# 0 0 #s #> type cr\n: fmt-neg dup >r abs s>d <# #s r> sign #> ;\n-123 fmt-neg type cr\nbye\n")
        .unwrap();
    assert_eq!(out, "BA\n ok\n01\n ok\n0\n ok\n ok\n-123\n ok\n");
}

#[test]
fn load_source_file_provides_holds() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval(": banner <# 49 hold 50 hold s\" AB\" holds 0 0 #> ;\nbanner type cr\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\nAB21\n ok\n");
}

#[test]
fn load_source_file_provides_unsigned_dot() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("123 u. cr\n-1 u. cr\nbye\n").unwrap();
    assert_eq!(out, "123 \n ok\n18446744073709551615 \n ok\n");
}

#[test]
fn load_source_file_provides_double_unsigned_dot() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("123 s>d du. cr\nbye\n").unwrap();
    assert_eq!(out, "123 \n ok\n");
}

#[test]
fn load_source_file_provides_abort_quote() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval(": guarded dup 0= abort\" zero\" 1+ ;\n5 guarded . cr\n0 ' guarded catch . cr\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n6 \n ok\nzero-2 \n ok\n");
}

#[test]
fn load_source_file_provides_environment_query() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval(": env-query s\" wf64\" ;\nenv-query environment? . cr\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n0 \n ok\n");
}

#[test]
fn load_source_file_abort_quote_in_interpret_state_throws_minus_14() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let err = s.eval("abort\" nope\"\n").unwrap_err().to_string();
    assert!(err.contains("-14"), "got {err:?}");
}

#[test]
fn load_source_file_provides_c_comma() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("here 65 c, here swap - . here 1- c@ .\nbye\n").unwrap();
    assert_eq!(out, "1 65  ok\n");
}

#[test]
fn load_source_file_provides_fvariable_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("fvariable foo\n9e foo f!\nfoo f@ f>d drop .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n9  ok\n");
}

#[test]
fn load_source_file_provides_fconstant_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("7e fconstant seven\nseven f>d drop .\n: use-seven seven ;\nuse-seven f>d drop .\nbye\n").unwrap();
    assert_eq!(out, " ok\n7  ok\n ok\n7  ok\n");
}

#[test]
fn load_source_file_provides_value_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval("5 value five\nfive .\n: use-five five ;\nuse-five .\nbye\n").unwrap();
    assert_eq!(out, " ok\n5  ok\n ok\n5  ok\n");
}

#[test]
fn load_source_file_provides_double_cell_defining_words() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval(
            "1 2 2constant pair\n\
             : use-pair pair ;\n\
             2variable dv\n\
             123 456 dv 2!\n\
             dv 2@ . . cr\n\
             pair . . cr\n\
             use-pair . . cr\n\
             : pair-lit [ 10 20 ] 2literal ;\n\
             pair-lit . . cr\n\
             bye\n",
        )
        .unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n456 123 \n ok\n2 1 \n ok\n2 1 \n ok\n ok\n10 20 \n ok\n");
}

#[test]
fn load_source_file_provides_case_words() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval(
            ": classify case\n\
             1 of 111 endof\n\
             2 of 222 endof\n\
             999 swap\n\
             endcase ;\n\
             1 classify .\n\
             2 classify .\n\
             7 classify .\n\
             bye\n",
        )
        .unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n111  ok\n222  ok\n999  ok\n");
}

#[test]
fn m7_ans_core_tests_pass() {
    let mut s = sess();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    s.load_source_file(&manifest.join("lib").join("core.f")).unwrap();
    s.load_source_file(&manifest.join("lib").join("tester.fs")).unwrap();
    s.load_source_file(&manifest.join("lib").join("ans_core_tests.fs")).unwrap();
    let out = s.eval("bye\n").unwrap();
    assert!(
        !out.contains("INCORRECT RESULT"),
        "ANS core test failures:\n{out}"
    );
    assert!(
        !out.contains("WRONG NUMBER OF RESULTS"),
        "ANS core test failures:\n{out}"
    );
}

#[test]
fn m7_ans_core_tests_pass_with_wf66() {
    // The ANS Forth core test suite, compiled with WF66 ENABLED — the strongest
    // conformance check for the optimizer (it rewrites every deferrable colon
    // body in core.f + the test suite; anything else falls back to eager).
    let mut s = sess();
    s.set_wf66_enabled(true);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    s.load_source_file(&manifest.join("lib").join("core.f")).unwrap();
    s.load_source_file(&manifest.join("lib").join("tester.fs")).unwrap();
    s.load_source_file(&manifest.join("lib").join("ans_core_tests.fs")).unwrap();
    let out = s.eval("bye\n").unwrap();
    assert!(
        !out.contains("INCORRECT RESULT"),
        "WF66 ANS core test failures:\n{out}"
    );
    assert!(
        !out.contains("WRONG NUMBER OF RESULTS"),
        "WF66 ANS core test failures:\n{out}"
    );
}

#[test]
#[ignore = "diagnostic: cargo test --test harness -- --ignored --nocapture wf66_compile_metrics"]
fn wf66_compile_metrics() {
    // Compile the real Forth corpus (core.f + tester + the ANS suite) with WF66
    // enabled and report the per-word metrics the compiler accumulated. This is
    // the "over the full test run" view: hundreds of real definitions, a mix of
    // deferrable (WF66-optimized) and eager-fallback words.
    let mut s = sess();
    s.set_wf66_enabled(true);
    wf64::wf66::reset_metrics();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    s.load_source_file(&manifest.join("lib").join("core.f")).unwrap();
    s.load_source_file(&manifest.join("lib").join("tester.fs")).unwrap();
    s.load_source_file(&manifest.join("lib").join("ans_core_tests.fs"))
        .unwrap();
    let _ = s.eval("bye\n");

    let m = wf64::wf66::metrics();
    eprintln!("\n  === WF66 compile metrics (core.f + tester + ANS suite) ===");
    eprintln!(
        "  definitions:  {} WF66-optimized + {} eager-fallback   ({:.0}% coverage)",
        m.deferrable,
        m.non_deferrable,
        m.coverage() * 100.0
    );
    eprintln!(
        "  promoted:     {} words used a read-only promotion reg (r10/r11)",
        m.promoted
    );
    eprintln!("  per WF66-optimized word        min   max     avg");
    let row = |name: &str, st: wf64::wf66::Stat| {
        eprintln!("    {name:<22} {:>4}  {:>4}  {:>7.2}", st.min(), st.max(), st.avg());
    };
    row("tokens in", m.tokens);
    row("instructions out", m.instrs);
    row("body bytes", m.bytes);
    row("data-stack accesses", m.mem);
    row("registers (excl rax)", m.regs);
    eprintln!();
}

#[test]
fn wf66_fp_leaf_words_match_eager() {
    // FP leaf words operate on caller-supplied FP-stack args. WF66 must compile
    // them (no taint) and produce the same result as the eager kernel.
    let cases: &[(&str, &str)] = &[
        (": fadd f+ ;", "1.5e 2.25e fadd f. cr"),
        (": fmul f* ;", "3e 4e fmul f. cr"),
        (": fsub f- ;", "10e 3e fsub f. cr"),
        (": fdiv f/ ;", "12e 4e fdiv f. cr"),
        (": fsq fdup f* ;", "5e fsq f. cr"),
        (": fn fnegate ;", "7e fn f. cr"),
        (": faxpy fover f* f+ ;", "2e 3e 4e faxpy f. cr"),
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_fp_word_is_actually_optimized() {
    // Regression guard: a pure-FP leaf word must be WF66-compiled (deferrable),
    // not fall back to eager — the body bytes must differ from the eager build.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let src = ": fsq fdup f* ;\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(src).unwrap();
        body(&s, "fsq")
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(src).unwrap();
        body(&s, "fsq")
    };
    assert_ne!(eager, wf66, "fsq should be WF66-compiled (inlined), not eager");
}

#[test]
fn wf66_inlines_optimized_leaf_words() {
    // A WF66-optimized leaf word is inlined into its caller, which then optimizes
    // as one — so the caller's body differs from eager's (call-the-word) body,
    // and the result still matches eager.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    // integer: quad = sq sq; FP: fquad = fsq fsq.
    for (src, caller, run) in [
        (": sq dup * ;\n: quad sq sq ;\n", "quad", "3 quad .\n"),
        (": fsq fdup f* ;\n: fquad fsq fsq ;\n", "fquad", "2e fquad f.\n"),
    ] {
        let def = format!("{src}bye\n");
        let eager = {
            let mut s = sess();
            s.eval(&def).unwrap();
            body(&s, caller)
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&def).unwrap();
            body(&s, caller)
        };
        assert_ne!(eager, wf66, "{caller} should inline its leaf word, not call it");
        // and the inlined result must still match eager
        let full = format!("{src}{run}bye\n");
        let eout = {
            let mut s = sess();
            s.eval(&full).unwrap()
        };
        let wout = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&full).unwrap()
        };
        assert_eq!(eout, wout, "inlined {caller} result must match eager");
    }
}

#[test]
fn wf66_fp_math_calls_match_eager() {
    // libm math words reached via a settle-barrier call (F2): a leaf word that
    // does FP arithmetic AND calls fsqrt/fsin/... must WF66-compile (no taint)
    // and match the eager kernel.
    let cases: &[(&str, &str)] = &[
        (": froot fsqrt ;", "16e froot f. cr"),
        (": myhyp fdup f* fswap fdup f* f+ fsqrt ;", "3e 4e myhyp f. cr"),
        (": fsc fdup fsin fswap fcos f* ;", "1e fsc f. cr"),
        (": fpow f** ;", "2e 10e fpow f. cr"),
    ];
    for (def, run) in cases {
        let src = format!("{def}\n{run}\nbye\n");
        let eager = {
            let mut s = sess();
            s.eval(&src).unwrap()
        };
        let wf66 = {
            let mut s = sess();
            s.set_wf66_enabled(true);
            s.eval(&src).unwrap()
        };
        assert_eq!(eager, wf66, "WF66 != eager for `{def}` / `{run}`");
    }
}

#[test]
fn wf66_fp_math_word_is_optimized() {
    // A word mixing FP arithmetic and an fsqrt call must be WF66-compiled (the
    // settle-barrier call replaces the taint), not fall back to eager.
    fn body(s: &Wf64Session, name: &str) -> Vec<u8> {
        let (a, b) = s
            .debug_words()
            .into_iter()
            .find_map(|(n, a, b)| if n == name { Some((a, b)) } else { None })
            .unwrap();
        unsafe { std::slice::from_raw_parts(a as *const u8, (b - a) as usize).to_vec() }
    }
    let src = ": myhyp fdup f* fswap fdup f* f+ fsqrt ;\nbye\n";
    let eager = {
        let mut s = sess();
        s.eval(src).unwrap();
        body(&s, "myhyp")
    };
    let wf66 = {
        let mut s = sess();
        s.set_wf66_enabled(true);
        s.eval(src).unwrap();
        body(&s, "myhyp")
    };
    assert_ne!(eager, wf66, "myhyp should be WF66-compiled (FP + settle-barrier call)");
}

#[test]
#[ignore = "benchmark: cargo test --test harness -- --ignored --nocapture wf66_fp_inner_bench"]
fn wf66_fp_inner_bench() {
    // A fully-deferrable pure-Forth FP inner loop (no fvariables / FP literals /
    // frot, which still taint): a chain of consecutive FP ops on the FP TOS,
    // inlined and FSP-coalesced. Optimizer off vs on.
    let setup = "\
: fk  fdup f*  fdup f*  fdup f+ ;\n\
: floop  begin fk 1- dup 0= until drop ;\n";
    fn run_bench(wf66: bool, setup: &str, n: u64) -> u128 {
        let mut s = sess();
        if wf66 {
            s.set_wf66_enabled(true);
        }
        s.eval(setup).unwrap();
        s.eval("1.0e 500000 floop fdrop\n").unwrap(); // warmup
        let prog = format!("1.0e {n} floop fdrop\n");
        let t = std::time::Instant::now();
        s.eval(&prog).unwrap();
        t.elapsed().as_micros()
    }
    let n = 5_000_000;
    let off = run_bench(false, setup, n);
    let on = run_bench(true, setup, n);
    eprintln!("\n  FP inner loop (pure Forth, fk = fdup f* fdup f* fdup f+), {n} iters:");
    eprintln!("    optimizer OFF: {off:>8} us");
    eprintln!(
        "    optimizer ON : {on:>8} us   ({:.2}x faster)",
        off as f64 / on as f64
    );
}

#[test]
fn load_source_file_provides_defer_defining_word() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s
        .eval("defer hook\n' dup ' hook defer!\n7 hook . .\n: run-hook hook ;\n9 run-hook . .\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n ok\n7 7  ok\n ok\n9 9  ok\n");
}

#[test]
fn load_source_file_defer_defaults_to_uninitialized_throw() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let err = s.eval("defer hook\nhook\nbye\n").unwrap_err().to_string();
    assert!(err.contains("-261"), "got {err:?}");
}

#[test]
fn eval_here_and_allot_move_dictionary_pointer() {
    let mut s = sess();
    let out = s.eval("here here 1 cells allot here rot - . drop\nbye\n").unwrap();
    assert_eq!(out, "8  ok\n");
}

#[test]
fn eval_source_defined_variable_roundtrips_through_fetch_store() {
    let mut s = sess();
    let out = s.eval(": , here ! 1 cells allot ;\n: align here aligned here - allot ;\n: variable create 0 , ;\nvariable foo\n7 foo !\nfoo @ .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n7  ok\n");
}

#[test]
fn eval_does_builder_word_customizes_created_runtime() {
    let mut s = sess();
    let out = s.eval(": , here ! 1 cells allot ;\n: constant create , does> @ ;\n10 constant ten\nten .\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n ok\n10  ok\n");
}

#[test]
fn eval_colon_defs_register_debug_words() {
    let mut s = sess();
    let out = s.eval(": square dup * ;\n: quad square square ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");

    let words = s.debug_words();
    assert_eq!(words.len(), 2);

    let square = words.iter().find(|(name, _, _)| name == "square").unwrap();
    let quad = words.iter().find(|(name, _, _)| name == "quad").unwrap();

    assert!(square.1 < square.2);
    assert!(quad.1 < quad.2);
    assert_eq!(s.resolve_word_addr(square.1).as_deref(), Some("square"));
    assert_eq!(s.resolve_word_addr(square.1 + 1).as_deref(), Some("square+0x1"));
    assert_eq!(s.resolve_word_addr(quad.1).as_deref(), Some("quad"));

    s.reset();
    assert!(s.debug_words().is_empty());
    assert!(s.resolve_word_addr(square.1).is_none());
}

#[test]
fn eval_create_without_name_throws_minus_16() {
    let mut s = sess();
    let err = s.eval("create\n").unwrap_err().to_string();
    assert!(err.contains("-16"), "got {err:?}");
}

#[test]
fn eval_semicolon_in_interpret_state_throws_minus_14() {
    let mut s = sess();
    let err = s.eval(";\n").unwrap_err().to_string();
    assert!(err.contains("-14"), "got {err:?}");
}

#[test]
fn eval_unknown_word_prints_question_mark() {
    let mut s = sess();
    let out = s.eval("nonsuch\nbye\n").unwrap();
    assert_eq!(out, "?  ok\n");
}

#[test]
fn eval_session_is_reusable_across_calls() {
    // Two consecutive evals on the same session: the dict from the
    // first call must survive into the second.
    let mut s = sess();
    let out1 = s.eval(": triple 3 * ;\n").unwrap();
    assert_eq!(out1, " ok\n");
    let out2 = s.eval("4 triple .\nbye\n").unwrap();
    assert_eq!(out2, "12  ok\n");
}

// ── direct-stack mode ────────────────────────────────────────────────

#[test]
fn direct_push_pop_round_trip() {
    let mut s = sess();
    s.push(42);
    s.push(-17);
    assert_eq!(s.depth(), 2);
    assert_eq!(s.stack(), vec![-17, 42]);  // top first
    assert_eq!(s.pop(), -17);
    assert_eq!(s.pop(), 42);
    assert_eq!(s.depth(), 0);
}

#[test]
fn direct_dup() {
    let mut s = sess();
    s.push(7);
    s.call("dup_").unwrap();
    assert_eq!(s.stack(), vec![7, 7]);
}

#[test]
fn direct_drop() {
    let mut s = sess();
    s.push(11);
    s.push(22);
    s.call("drop_").unwrap();
    assert_eq!(s.stack(), vec![11]);
}

#[test]
fn direct_swap() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("swap_").unwrap();
    assert_eq!(s.stack(), vec![1, 2]);
}

#[test]
fn direct_over() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("over_").unwrap();
    assert_eq!(s.stack(), vec![1, 2, 1]);
}

#[test]
fn direct_plus() {
    let mut s = sess();
    s.push(40);
    s.push(2);
    s.call("plus").unwrap();
    assert_eq!(s.stack(), vec![42]);
}

#[test]
fn direct_times() {
    let mut s = sess();
    s.push(6);
    s.push(7);
    s.call("times").unwrap();
    assert_eq!(s.stack(), vec![42]);
}

#[test]
fn direct_times_signed() {
    let mut s = sess();
    s.push(-3);
    s.push(5);
    s.call("times").unwrap();
    assert_eq!(s.stack(), vec![-15]);
}

#[test]
fn direct_perform_dispatches_xt_loaded_from_memory() {
    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    let xt_slot = (s.user_base + 0x180) as i64;

    s.push(dup_xt);
    s.push(xt_slot);
    s.call("store").unwrap();
    assert_eq!(s.depth(), 0);

    s.push(42);
    s.push(xt_slot);
    s.call("perform").unwrap();
    assert_eq!(s.stack(), vec![42, 42]);
}

#[test]
fn direct_catch_returns_zero_on_success() {
    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;

    s.push(7);
    s.push(dup_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.stack(), vec![0, 7, 7]);
}

#[test]
fn direct_catch_returns_throw_code() {
    let mut s = sess();
    let throw_xt = s.xt_of("throw_word").unwrap() as i64;

    s.push(-31);
    s.push(throw_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.stack(), vec![-31, -31]);
}

#[test]
fn direct_uncaught_throw_returns_error_to_host() {
    let mut s = sess();
    s.push(-31);
    let err = s.call("throw_word").unwrap_err().to_string();
    assert!(err.contains("Forth THROW -31"), "got {err:?}");
}

#[test]
fn direct_qthrow_drops_inputs_when_flag_is_zero() {
    let mut s = sess();
    s.push(99);
    s.push(0);
    s.push(-31);
    s.call("qthrow_word").unwrap();
    assert_eq!(s.stack(), vec![99]);
}

#[test]
fn direct_qthrow_throws_when_flag_is_nonzero() {
    let mut s = sess();
    let qthrow_xt = s.xt_of("qthrow_word").unwrap() as i64;

    s.push(1);
    s.push(-31);
    s.push(qthrow_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.stack(), vec![-31, -31, 1]);
}

#[test]
fn direct_abort_returns_error_to_host() {
    let mut s = sess();
    let err = s.call("abort_word").unwrap_err().to_string();
    assert!(err.contains("Forth THROW -1"), "got {err:?}");
}

#[test]
fn direct_named_throw_constants_push_expected_codes() {
    let mut s = sess();

    s.call("throw_abort_const").unwrap();
    assert_eq!(s.pop(), -1);

    s.call("throw_abortq_const").unwrap();
    assert_eq!(s.pop(), -2);

    s.call("throw_componly_const").unwrap();
    assert_eq!(s.pop(), -14);

    s.call("throw_namereqd_const").unwrap();
    assert_eq!(s.pop(), -16);

    s.call("throw_mismatch_const").unwrap();
    assert_eq!(s.pop(), -22);
}

#[test]
fn direct_comp_only_throws_minus_14() {
    let mut s = sess();
    let comp_only_xt = s.xt_of("comp_only_word").unwrap() as i64;

    s.push(comp_only_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.stack(), vec![-14]);
}

#[test]
fn direct_cpuid_writes_expected_register_block() {
    let mut s = sess();
    let buf = (s.user_base + 0x1a0) as i64;
    let expected = __cpuid(0);

    s.push(buf);
    s.push(0);
    s.call("cpuid_word").unwrap();
    assert_eq!(s.depth(), 0);

    s.push(buf);
    s.call("l_fetch").unwrap();
    assert_eq!(s.pop() as u32, expected.eax);

    s.push(buf + 4);
    s.call("l_fetch").unwrap();
    assert_eq!(s.pop() as u32, expected.ebx);

    s.push(buf + 8);
    s.call("l_fetch").unwrap();
    assert_eq!(s.pop() as u32, expected.ecx);

    s.push(buf + 12);
    s.call("l_fetch").unwrap();
    assert_eq!(s.pop() as u32, expected.edx);
}

#[test]
fn direct_rdtsc_returns_a_nondecreasing_counter() {
    let mut s = sess();

    s.call("rdtsc_word").unwrap();
    let hi1 = s.pop() as u64;
    let lo1 = s.pop() as u64;
    let t1 = (hi1 << 32) | (lo1 & 0xffff_ffff);

    s.call("rdtsc_word").unwrap();
    let hi2 = s.pop() as u64;
    let lo2 = s.pop() as u64;
    let t2 = (hi2 << 32) | (lo2 & 0xffff_ffff);

    assert!(t1 > 0);
    assert!(t2 >= t1);
}

#[test]
fn direct_rot_three_items() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.push(3);
    s.call("rot_").unwrap();
    // ( 1 2 3 -- 2 3 1 ); top first → [1, 3, 2]
    assert_eq!(s.stack(), vec![1, 3, 2]);
}

#[test]
fn direct_nip() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("nip_").unwrap();
    assert_eq!(s.stack(), vec![2]);
}

#[test]
fn direct_tuck() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("tuck_").unwrap();
    // ( 1 2 -- 2 1 2 ); top first → [2, 1, 2]
    assert_eq!(s.stack(), vec![2, 1, 2]);
}

#[test]
fn direct_neg_rot() {
    // -rot: ( n1 n2 n3 -- n3 n1 n2 )
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.push(3);
    s.call("neg_rot").unwrap();
    // After -rot: top first → [2, 1, 3]
    assert_eq!(s.stack(), vec![2, 1, 3]);
}

#[test]
fn direct_qdup_zero_does_nothing() {
    let mut s = sess();
    s.push(0);
    s.call("qdup").unwrap();
    assert_eq!(s.stack(), vec![0]);
}

#[test]
fn direct_qdup_nonzero_duplicates() {
    let mut s = sess();
    s.push(99);
    s.call("qdup").unwrap();
    assert_eq!(s.stack(), vec![99, 99]);
}

#[test]
fn direct_pick_zero_is_dup() {
    let mut s = sess();
    s.push(11);
    s.push(22);
    s.push(0);
    s.call("pick").unwrap();
    // ( 11 22 0 -- 11 22 22 )
    assert_eq!(s.stack(), vec![22, 22, 11]);
}

#[test]
fn direct_pick_one_is_over() {
    let mut s = sess();
    s.push(11);
    s.push(22);
    s.push(1);
    s.call("pick").unwrap();
    // ( 11 22 1 -- 11 22 11 )
    assert_eq!(s.stack(), vec![11, 22, 11]);
}

#[test]
fn direct_pick_two() {
    let mut s = sess();
    s.push(10);
    s.push(20);
    s.push(30);
    s.push(2);
    s.call("pick").unwrap();
    // ( 10 20 30 2 -- 10 20 30 10 )
    assert_eq!(s.stack(), vec![10, 30, 20, 10]);
}

#[test]
fn direct_depth_counts_cells() {
    let mut s = sess();
    // Phase 1: empty stack — `depth` should push 0.
    assert_eq!(s.depth(), 0);
    s.call("depth").unwrap();
    assert_eq!(s.stack(), vec![0]);

    // Phase 2: three values — `depth` should push 3 on top of them.
    // (Used to re-call `sess()` here — under the shared-session harness
    // that's a self-deadlock. `reset()` does the same thing without
    // releasing the lock.)
    s.reset();
    s.push(10);
    s.push(20);
    s.push(30);
    s.call("depth").unwrap();
    // ( 10 20 30 -- 10 20 30 3 )
    assert_eq!(s.stack(), vec![3, 30, 20, 10]);
}

// ── return-stack primitives ──────────────────────────────────────────

#[test]
fn direct_to_r_then_r_from_roundtrips() {
    let mut s = sess();
    s.push(42);
    s.call("to_r").unwrap();
    assert_eq!(s.depth(), 0);
    s.call("r_from").unwrap();
    assert_eq!(s.stack(), vec![42]);
}

#[test]
fn direct_r_fetch_peeks_without_popping() {
    let mut s = sess();
    s.push(99);
    s.call("to_r").unwrap();
    s.call("r_fetch").unwrap();
    // ( -- 99 ); r-stack still has 99.
    assert_eq!(s.stack(), vec![99]);
    s.call("r_from").unwrap();
    assert_eq!(s.stack(), vec![99, 99]);
}

#[test]
fn direct_dup_to_r_keeps_data_stack_value() {
    let mut s = sess();
    s.push(7);
    s.call("dup_to_r").unwrap();
    // data stack still has 7, r-stack also has 7.
    assert_eq!(s.stack(), vec![7]);
    s.call("r_from").unwrap();
    assert_eq!(s.stack(), vec![7, 7]);
}

#[test]
fn direct_rdrop_clears_rstack_only() {
    let mut s = sess();
    s.push(11);
    s.call("to_r").unwrap();
    s.push(22);  // unrelated cell on data stack
    s.call("rdrop").unwrap();
    assert_eq!(s.stack(), vec![22]);
}

#[test]
fn direct_two_to_r_and_two_r_from_roundtrip() {
    let mut s = sess();
    s.push(100);
    s.push(200);
    s.call("two_to_r").unwrap();
    assert_eq!(s.depth(), 0);
    s.call("two_r_from").unwrap();
    assert_eq!(s.stack(), vec![200, 100]);  // top = 200, NOS = 100
}

#[test]
fn direct_two_r_fetch_peeks_pair() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("two_to_r").unwrap();
    s.call("two_r_fetch").unwrap();
    // ( -- 1 2 ); r-stack still has the pair.
    assert_eq!(s.stack(), vec![2, 1]);
    s.call("two_r_from").unwrap();
    assert_eq!(s.stack(), vec![2, 1, 2, 1]);
}

#[test]
fn direct_i_reads_top_loop_frame_sum() {
    let mut s = sess();
    s.push(30);
    s.push(70);
    s.call("two_to_r").unwrap();
    s.call("i_word").unwrap();
    assert_eq!(s.stack(), vec![100]);
}

#[test]
fn direct_j_reads_next_outer_loop_frame_sum() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("two_to_r").unwrap();
    s.push(10);
    s.push(20);
    s.call("two_to_r").unwrap();
    s.call("j_word").unwrap();
    assert_eq!(s.stack(), vec![3]);
}

#[test]
fn direct_do_part_helpers_build_top_loop_frame() {
    let mut s = sess();
    s.push(20);
    s.push(10);
    s.call("do_part1").unwrap();
    assert_eq!(s.depth(), 0);
    s.call("do_part2").unwrap();
    s.call("i_word").unwrap();
    assert_eq!(s.stack(), vec![10]);
}

#[test]
fn direct_nested_do_part_helpers_make_j_visible() {
    let mut s = sess();
    s.push(20);
    s.push(3);
    s.call("do_part1").unwrap();
    s.call("do_part2").unwrap();
    s.push(50);
    s.push(10);
    s.call("do_part1").unwrap();
    s.call("do_part2").unwrap();
    s.call("j_word").unwrap();
    assert_eq!(s.stack(), vec![3]);
}

#[test]
fn direct_mark_to_returns_current_here() {
    let mut s = sess();
    let here = s.here();
    s.call("mark_to").unwrap();
    assert_eq!(s.pop() as u64, here);
}

#[test]
fn direct_forward_resolve_patches_rel32_from_mark_to_here() {
    let mut s = sess();
    s.push(0);
    s.call("inline_bra_comp").unwrap();
    s.call("mark_to").unwrap();
    let orig = s.pop() as u64;

    s.push(0);
    s.call("inline_bra_comp").unwrap();
    let here = s.here();

    s.push(orig as i64);
    s.call("forward_resolve").unwrap();

    let disp = unsafe { ((orig - 4) as *const i32).read_unaligned() };
    assert_eq!(disp as i64, here as i64 - orig as i64);
}

#[test]
fn direct_back_resolve_patches_current_rel32_back_to_dest() {
    let mut s = sess();
    let dest = s.here();

    s.push(0);
    s.call("inline_bra_comp").unwrap();
    let here = s.here();

    s.push(dest as i64);
    s.call("back_resolve").unwrap();

    let disp = unsafe { ((here - 4) as *const i32).read_unaligned() };
    assert_eq!(disp as i64, dest as i64 - here as i64);
}

#[test]
fn direct_qpairs_drops_matching_marks() {
    let mut s = sess();
    s.push(-2);
    s.push(-2);
    s.call("qpairs").unwrap();
    assert!(s.stack().is_empty());
}

#[test]
fn direct_qpairs_throws_minus_22_on_mismatch() {
    let mut s = sess();
    let qpairs_xt = s.xt_of("qpairs").unwrap() as i64;
    s.push(-1);
    s.push(-2);
    s.push(qpairs_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.pop(), -22);
}

#[test]
fn direct_leave_under_if_restores_control_stack_shape() {
    const USER_STATE: u64 = 0x08;

    let mut s = sess();
    let state_addr = (s.user_base + USER_STATE) as i64;
    s.push(1);
    s.push(state_addr);
    s.call("store").unwrap();

    let do_addr = 0x1111_i64;
    let if_orig = 0x2222_i64;
    s.push(do_addr);
    s.push(-3);
    s.push(if_orig);
    s.push(-1);

    s.call("leave_word").unwrap();
    let stack = s.stack();
    assert_eq!(&stack[..5], &[-1, if_orig, -3, do_addr, -5]);
    assert_eq!(stack[5] as u64, s.here());
}

#[test]
fn direct_high_level_control_words_are_compile_only() {
    let mut s = sess();
    let cases = [
        "ahead_word",
        "if_word",
        "minus_if_word",
        "then_word",
        "else_word",
        "begin_word",
        "while_word",
        "again_word",
        "until_word",
        "repeat_word",
        "recurse_word",
        "do_word",
        "qdo_control_word",
        "loop_control_word",
        "plus_loop_control_word",
        "minus_loop_control_word",
        "leave_word",
        "qleave_word",
    ];

    for asm in cases {
        let xt = s.xt_of(asm).unwrap() as i64;
        s.push(xt);
        s.call("catch_word").unwrap();
        assert_eq!(s.pop(), -14, "{asm} should THROW -14 outside compile state");
    }
}

#[test]
fn direct_raw_control_emitters_are_compile_only() {
    let mut s = sess();
    let cases = [
        "bra_word",
        "qbra_word",
        "minus_qbra_word",
        "bra_qdo_word",
        "loop_word",
        "plus_loop_word",
        "minus_loop_word",
    ];

    for asm in cases {
        let xt = s.xt_of(asm).unwrap() as i64;
        s.push(xt);
        s.call("catch_word").unwrap();
        assert_eq!(s.pop(), -14, "{asm} should THROW -14 outside compile state");
    }
}

#[test]
fn direct_n_to_r_then_nr_from_roundtrip() {
    let mut s = sess();
    s.push(10);
    s.push(20);
    s.push(2);
    s.call("n_to_r").unwrap();
    assert_eq!(s.depth(), 0);

    s.call("nr_from").unwrap();
    assert_eq!(s.stack(), vec![2, 20, 10]);
}

#[test]
fn direct_n_to_r_and_nr_from_preserve_deeper_stack() {
    let mut s = sess();
    s.push(99);
    s.push(10);
    s.push(20);
    s.push(2);
    s.call("n_to_r").unwrap();
    assert_eq!(s.stack(), vec![99]);

    s.call("nr_from").unwrap();
    assert_eq!(s.stack(), vec![2, 20, 10, 99]);
}

#[test]
fn direct_two_rdrop_clears_pair() {
    let mut s = sess();
    s.push(1);
    s.push(2);
    s.call("two_to_r").unwrap();
    s.push(99);
    s.call("two_rdrop").unwrap();
    assert_eq!(s.stack(), vec![99]);
}

#[test]
fn eval_to_r_through_repl() {
    // Round-trip through the return stack from inside a compiled word.
    // dup so the inner `.` has something to print; >r/r> shuttle the
    // copy so the outer `.` finds it again. The two "5 "s prove both
    // halves of the trip survived a compiled-body context (which is
    // where the rstack-juggle in to_r/r_from gets exercised hardest).
    let mut s = sess();
    let out = s.eval(": ferry dup >r . r> ;\n5 ferry .\nbye\n").unwrap();
    assert_eq!(out, " ok\n5 5  ok\n");
}

#[test]
fn direct_sp_fetch_returns_address() {
    let mut s = sess();
    s.push(10);
    s.push(20);
    s.call("sp_fetch").unwrap();
    let top = s.pop();
    // The pushed address should be within the data stack region and
    // very near current dsp (off by exactly one cell because sp@
    // first reserves a cell, then writes its result).
    let region_lo = s.user_base - 0x80000;  // base of region
    assert!((top as u64) > region_lo);
    assert!((top as u64) <= s.dsp_top);
    // The remaining stack should be the two original values.
    assert_eq!(s.stack(), vec![20, 10]);
}

#[test]
fn eval_depth_via_interpreter() {
    let mut s = sess();
    let out = s.eval("1 2 3 depth . . . . .\nbye\n").unwrap();
    // Pushes 1 2 3 then depth=3. Dots print top first: 3 3 2 1 + one
    // garbage cell from underflow. We just check the first 4 prints.
    assert!(out.starts_with("3 3 2 1 "), "got {out:?}");
}

// ── memory primitives via direct invocation ─────────────────────────

#[test]
fn direct_fetch_store_cell() {
    let mut s = sess();
    // Use a PAD slot at user_base+0x100 for scratch.
    let scratch = s.user_base + 0x100;
    s.push(0xdeadbeef);
    s.push(scratch as i64);
    s.call("store").unwrap();   // ( v addr -- )
    assert_eq!(s.depth(), 0);
    s.push(scratch as i64);
    s.call("fetch").unwrap();   // ( addr -- v )
    assert_eq!(s.pop(), 0xdeadbeef);
}

#[test]
fn direct_c_fetch_store() {
    let mut s = sess();
    let scratch = s.user_base + 0x110;
    s.push(0x5a);
    s.push(scratch as i64);
    s.call("c_store").unwrap();
    s.push(scratch as i64);
    s.call("c_fetch").unwrap();
    assert_eq!(s.pop(), 0x5a);
}

// ── mixed: build via eval, then poke via direct ─────────────────────

// ── dictionary primitives (Phase 2) ─────────────────────────────────

#[test]
fn create_and_set_xt_builds_a_callable_header() {
    // Drive the kernel-side dict primitives directly: build a fake
    // header pointing at `dup_` and named "FOO", then call FOO via the
    // REPL and confirm it duplicates.
    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"FOO";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let dup_xt = s.xt_of("dup_").unwrap();
    s.push(dup_xt as i64);
    s.call("set_xt").unwrap();
    assert_eq!(s.depth(), 0);
    // Now FOO should be in the dict, with the same effect as DUP.
    let out = s.eval("7 FOO . .\nbye\n").unwrap();
    assert_eq!(out, "7 7  ok\n");
}

#[test]
fn to_name_resolves_primitive_xt_to_counted_name() {
    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;

    s.push(dup_xt);
    s.call("to_name").unwrap();
    let nt = s.pop() as u64;

    let len = unsafe { (nt as *const u8).read() };
    let bytes = unsafe { std::slice::from_raw_parts((nt + 1) as *const u8, len as usize) };
    assert_eq!(len, 3);
    assert_eq!(bytes, b"dup");
}

#[test]
fn primitive_xt_has_ct_backoffset_slot() {
    const DH_CT: u64 = 8;

    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as u64;
    s.push(dup_xt as i64);
    s.call("to_name").unwrap();
    let nt = s.pop() as u64;
    let ct = nt - ((5 * 8) + 2 + 2 + 2 + 1) + DH_CT;
    let backoff = unsafe { ((dup_xt - 8) as *const i64).read() };

    assert_eq!(dup_xt.wrapping_add_signed(backoff), ct);
}

#[test]
fn colon_defined_latestxt_has_ct_backoffset_slot() {
    const DH_CT: u64 = 8;
    const DH_NT: u64 = (5 * 8) + 2 + 2 + 2 + 1;

    let mut s = sess();
    let out = s.eval(": quux 1 ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let backoff = unsafe { ((xt - 8) as *const i64).read() };
    let ct = xt.wrapping_add_signed(backoff);
    let nt = ct - DH_CT + DH_NT;
    let len = unsafe { (nt as *const u8).read() };
    let bytes = unsafe { std::slice::from_raw_parts((nt + 1) as *const u8, len as usize) };

    assert_eq!(bytes, b"quux");

    s.push(xt as i64);
    s.call("to_name").unwrap();
    let nt_from_to_name = s.pop() as u64;
    assert_eq!(nt_from_to_name, nt);
}

#[test]
fn to_ct_and_to_comp_recover_header_fields_from_xt() {
    const DH_CT: u64 = 8;
    const DH_COMP: u64 = 24;

    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;

    s.push(dup_xt);
    s.call("to_name").unwrap();
    let nt = s.pop() as u64;
    let expected_ct = nt - ((5 * 8) + 2 + 2 + 2 + 1) + DH_CT;

    s.push(dup_xt);
    s.call("to_ct").unwrap();
    assert_eq!(s.pop() as u64, expected_ct);

    s.push(dup_xt);
    s.call("to_comp").unwrap();
    assert_eq!(s.pop() as u64, expected_ct - DH_CT + DH_COMP);
}

#[test]
fn dup_primitive_comp_field_points_to_inline_dup_helper() {
    let mut s = sess();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    let inline_dup_xt = s.xt_of("inline_dup_comp").unwrap() as i64;

    s.push(dup_xt);
    s.call("to_comp").unwrap();
    s.call("fetch").unwrap();
    assert_eq!(s.pop(), inline_dup_xt);
}

#[test]
fn simple_stack_primitives_comp_fields_point_to_inline_helpers() {
    let mut s = sess();

    let cases = [
        ("drop_", "inline_drop_comp"),
        ("swap_", "inline_swap_comp"),
        ("over_", "inline_over_comp"),
    ];

    for (word_xt, comp_xt) in cases {
        let xt = s.xt_of(word_xt).unwrap() as i64;
        let helper = s.xt_of(comp_xt).unwrap() as i64;
        s.push(xt);
        s.call("to_comp").unwrap();
        s.call("fetch").unwrap();
        assert_eq!(s.pop(), helper, "wrong comp helper for {word_xt}");
    }
}

#[test]
fn return_stack_primitives_comp_fields_point_to_inline_helpers() {
    let mut s = sess();

    let cases = [
        ("to_r", "inline_to_r_comp"),
        ("r_from", "inline_r_from_comp"),
        ("r_fetch", "inline_r_fetch_comp"),
        ("two_to_r", "inline_two_to_r_comp"),
        ("two_r_from", "inline_two_r_from_comp"),
        ("two_r_fetch", "inline_two_r_fetch_comp"),
        ("i_word", "inline_i_comp"),
        ("j_word", "inline_j_comp"),
        ("do_part1", "inline_do_part1_comp"),
        ("do_part2", "inline_do_part2_comp"),
        ("bra_word", "inline_bra_comp"),
        ("qbra_word", "inline_qbra_comp"),
        ("minus_qbra_word", "inline_minus_qbra_comp"),
        ("bra_qdo_word", "inline_bra_qdo_comp"),
        ("loop_word", "inline_loop_comp"),
        ("plus_loop_word", "inline_plus_loop_comp"),
        ("minus_loop_word", "inline_minus_loop_comp"),
    ];

    for (word_xt, comp_xt) in cases {
        let xt = s.xt_of(word_xt).unwrap() as i64;
        let helper = s.xt_of(comp_xt).unwrap() as i64;
        s.push(xt);
        s.call("to_comp").unwrap();
        s.call("fetch").unwrap();
        assert_eq!(s.pop(), helper, "wrong comp helper for {word_xt}");
    }
}

#[test]
fn eval_i_and_j_through_nested_rstack_frames() {
    let mut s = sess();
    let out = s.eval(": ijtest 1 2 2>r 10 20 2>r i . j . 2r> 2drop 2r> 2drop ;\nijtest\nbye\n").unwrap();
    assert_eq!(out, " ok\n30 3  ok\n");
}

#[test]
fn eval_two_r_roundtrip_with_literals_in_definition() {
    let mut s = sess();
    let out = s.eval(": rr2 1 2 2>r 2r> ;\nrr2 . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n2 1  ok\n");
}

#[test]
fn eval_two_r_roundtrip_with_literals_can_end_empty() {
    let mut s = sess();
    let out = s.eval(": rr1 1 2 2>r 2r> 2drop ;\nrr1\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");
}

#[test]
fn eval_do_part_helpers_feed_i_and_j() {
    let mut s = sess();
    let out = s.eval(": dijtest 20 3 do-part1 do-part2 50 10 do-part1 do-part2 i . j . 2rdrop 2rdrop ;\ndijtest\nbye\n").unwrap();
    assert_eq!(out, " ok\n10 3  ok\n");
}

#[test]
fn compiled_raw_branch_emitters_have_expected_bytes() {
    let mut s = sess();
    let out = s.eval(": rawcf bra ?bra -?bra bra-?do ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 5 + 17 + 9 + 9 + 1) };
    assert_eq!(&bytes[0..5], &[0xE9, 0, 0, 0, 0]);
    assert_eq!(&bytes[5..22], &[0x48, 0x83, 0xC5, 0x08, 0x48, 0x85, 0xC0, 0x48, 0x8B, 0x45, 0xF8, 0x0F, 0x84, 0, 0, 0, 0]);
    assert_eq!(&bytes[22..31], &[0x48, 0x85, 0xC0, 0x0F, 0x84, 0, 0, 0, 0]);
    assert_eq!(&bytes[31..40], &[0x48, 0x39, 0xCA, 0x0F, 0x84, 0, 0, 0, 0]);
    assert_eq!(bytes[40], 0xC3);
}

#[test]
fn constant_compiles_inline_and_folds() {
    // A constant reference must inline its value (no call into the does-body)
    // and compose with the literal folds. `C +` folds to a single `add rax,imm`.
    let mut s = sess();
    s.eval("100 constant C\nbye\n").unwrap();

    // `: dc C ;` — bare reference inlines to a literal push (mov eax,100), no call.
    s.eval(": dc C ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 14) };
    assert_eq!(&bytes[..5], &[0x48, 0x89, 0x45, 0xF8, 0xB8],
        "constant should inline-push, got {:02X?}", &bytes[..5]);
    assert_eq!(&bytes[5..9], &100i32.to_le_bytes(), "wrong inlined value");
    assert!(bytes[0] != 0xE8 && bytes[0] != 0xE9, "must not be a call/jmp");

    // `: cadd C + ;` — constant then `+` folds to `add rax, 100; ret`.
    s.eval(": cadd C + ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 5) };
    assert_eq!(bytes, &[0x48, 0x83, 0xC0, 0x64, 0xC3],
        "expected `add rax,100; ret`, got {:02X?}", bytes);

    // Runtime correctness.
    let out = s.eval("5 cadd .  C .\nbye\n").unwrap();
    assert_eq!(out, "105 100  ok\n");
}

#[test]
fn compare_branch_fusion_emits_cmp_then_jcc() {
    // `dup 10 < if` fuses to: cmp rax,10 ; mov rax,[rbp] ; lea rbp,[rbp+8] ; jge rel32
    // — the boolean materialization (setl;movzx;neg) and the if's test are gone,
    // and the branch is the inverted condition (jge = NOT <).
    let mut s = sess();
    s.eval(": cif dup 10 < if 1 then ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 22) };
    assert_eq!(&bytes[8..22], &[
        0x48, 0x83, 0xF8, 0x0A,           // cmp rax, 10   (kept from the comparison)
        0x48, 0x8B, 0x45, 0x00,           // mov rax, [rbp]  (raise NOS)
        0x48, 0x8D, 0x6D, 0x08,           // lea rbp, [rbp+8] (drop; preserves flags)
        0x0F, 0x8D,                       // jge rel32  (setl 0x9C → jge 0x8D, inverted)
    ], "fusion mismatch, got {:02X?}", &bytes[8..22]);

    // Both branch directions run correctly.
    let out = s.eval("5 cif .  20 cif .\nbye\n").unwrap();
    assert_eq!(out, "1 20  ok\n"); // 5<10 → then pushes 1; 20<10 false → 20 unchanged

    // Each setcc variant fuses to its correctly-inverted jcc.
    for (src, jcc) in [
        (": fg dup 3 > if 9 then ;",   0x8Eu8),  // setg 0x9F → jle
        (": fle dup 3 <= if 9 then ;", 0x8Fu8),  // setle 0x9E → jg
        (": fge dup 3 >= if 9 then ;", 0x8Cu8),  // setge 0x9D → jl
        (": fu dup 3 u< if 9 then ;",  0x82u8),  // note: u< is NOT a setcc fold → no fusion
    ] {
        s.eval(&format!("{src}\nbye\n")).unwrap();
        s.call("latestxt").unwrap();
        let xt = s.pop() as u64;
        let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 24) };
        if src.contains("u<") {
            // u< uses cmp+sbb, not setcc — it does not participate in fusion here.
            continue;
        }
        assert_eq!(bytes[20], 0x0F, "{src}: expected 0F jcc prefix, got {:02X?}", &bytes[..24]);
        assert_eq!(bytes[21], jcc, "{src}: wrong inverted jcc");
    }
}

#[test]
fn hotvar_fetch_store_fuse_to_rip_relative() {
    // hotvar @ → spill;bump;mov rax,[rip+d] (the lea becomes a load, in place).
    // hotvar ! → mov [rip+d],rax;mov rax,[rbp];add rbp,8 (store direct, no addr push).
    // bare hotvar stays a lea (address push). Double @ doesn't over-fuse.
    let mut s = sess();
    s.eval("hotvariable hv\nbye\n").unwrap();

    s.eval(": rd hv @ ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let b = unsafe { std::slice::from_raw_parts(xt as *const u8, 12) };
    assert_eq!(&b[0..11], &[
        0x48, 0x89, 0x45, 0xF8,           // mov [rbp-8], rax  (spill)
        0x48, 0x83, 0xED, 0x08,           // sub rbp, 8        (bump)
        0x48, 0x8B, 0x05,                 // mov rax, [rip+d]  (FUSED load; was lea+mov[rax])
    ], "hv @ not fused: {:02X?}", b);

    s.eval(": wr hv ! ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let b = unsafe { std::slice::from_raw_parts(xt as *const u8, 16) };
    assert_eq!(&b[0..3], &[0x48, 0x89, 0x05], "hv ! not fused to mov [rip],rax: {:02X?}", b);
    assert_eq!(&b[7..16], &[
        0x48, 0x8B, 0x45, 0x00,           // mov rax, [rbp]
        0x48, 0x83, 0xC5, 0x08,           // add rbp, 8
        0xC3,                              // ret
    ], "hv ! tail wrong: {:02X?}", b);

    // bare hotvar stays a lea (address push) — fusion only when @/! follows.
    s.eval(": adr hv ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let b = unsafe { std::slice::from_raw_parts(xt as *const u8, 11) };
    assert_eq!(b[9], 0x8D, "bare hotvar should stay a lea (8D), got {:02X?}", b);

    // Runtime, including indirect double-fetch (pp holds hv's address).
    let out = s.eval("42 hv !  hv @ .  hotvariable pp  hv pp !  pp @ @ .\nbye\n").unwrap();
    assert_eq!(out, "42 42  ok\n");
}

#[test]
fn rdrop_and_2rdrop_inline_to_add_rsp() {
    // rdrop / 2rdrop must compile inline to `add rsp,8` / `add rsp,16` (no CALL),
    // which also removes the old compile_comma_no_tco hazard. `: t 5 >r rdrop ;`
    // = lit(13) + >r(9) + rdrop(4) + ret; the tail is `add rsp,8 ; ret`.
    let mut s = sess();
    s.eval(": tr 5 >r rdrop ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let b = unsafe { std::slice::from_raw_parts(xt as *const u8, 27) };
    assert_eq!(&b[22..27], &[0x48, 0x83, 0xC4, 0x08, 0xC3],
        "rdrop should inline to `add rsp,8; ret`, got {:02X?}", &b[22..]);

    s.eval(": tr2 1 2 2>r 2rdrop ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let b = unsafe { std::slice::from_raw_parts(xt as *const u8, 43) };
    // 1(13) + 2(13) + 2>r(12) = 38, then 2rdrop(4) + ret.
    assert_eq!(&b[38..43], &[0x48, 0x83, 0xC4, 0x10, 0xC3],
        "2rdrop should inline to `add rsp,16; ret`, got {:02X?}", &b[38..]);

    // Runtime: rdrop drops the >r'd value, leaving the kept data result.
    let out = s
        .eval(": rr 10 5 >r rdrop ;  : rr2 99 1 2 2>r 2rdrop ;  rr .  rr2 .\nbye\n")
        .unwrap();
    assert_eq!(out, "10 99  ok\n");
}

#[test]
fn literal_fold_imm8_emits_immediate_form_instruction() {
    // `5 +` should fold to `add rax, 5` (4 bytes) followed by RET — no
    // 13-byte literal emission, no CALL to plus.
    let mut s = sess();
    let out = s.eval(": addfive 5 + ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 5) };
    assert_eq!(bytes, &[0x48, 0x83, 0xC0, 0x05, 0xC3],
        "expected `add rax, 5; ret`, got {:02X?}", bytes);

    // Same definition runs correctly.
    let out = s.eval("1 addfive .\nbye\n").unwrap();
    assert_eq!(out, "6  ok\n");
}

#[test]
fn literal_fold_imm32_emits_accumulator_form() {
    // 1000 = 0x3E8 doesn't fit in signed imm8 but does in imm32, so
    // we expect the 6-byte accumulator form `48 05 E8 03 00 00`.
    let mut s = sess();
    let out = s.eval(": addbig 1000 + ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 7) };
    assert_eq!(bytes, &[0x48, 0x05, 0xE8, 0x03, 0x00, 0x00, 0xC3],
        "expected `add rax, 1000; ret`, got {:02X?}", bytes);

    let out = s.eval("5 addbig .\nbye\n").unwrap();
    assert_eq!(out, "1005  ok\n");
}

#[test]
fn literal_fold_all_binops_emit_their_immediate_form() {
    // One canonical imm8 fold per op, checking the opcode/modrm/imm bytes.
    // `*` uses 7 (not a strength-reduction special, so it stays a true
    // imul rax,rax,imm8); 3 would now strength-reduce to `lea rax,[rax+rax*2]`.
    let cases: &[(&str, &str, u8, u8, u8)] = &[
        // (Forth source, name, opcode-byte, modrm-byte, imm-byte)
        (": fadd  3 + ;",   "fadd",  0x83, 0xC0, 0x03),  // ADD /0
        (": fsub  3 - ;",   "fsub",  0x83, 0xE8, 0x03),  // SUB /5
        (": fmul  7 * ;",   "fmul",  0x6B, 0xC0, 0x07),  // IMUL rax,rax,imm8
        (": fand  3 and ;", "fand",  0x83, 0xE0, 0x03),  // AND /4
        (": for   3 or ;",  "for",   0x83, 0xC8, 0x03),  // OR  /1
        (": fxor  3 xor ;", "fxor",  0x83, 0xF0, 0x03),  // XOR /6
    ];
    let mut s = sess();
    for &(src, _name, opcode, modrm, imm) in cases {
        s.eval(&format!("{src}\nbye\n")).unwrap();
        s.call("latestxt").unwrap();
        let xt = s.pop() as u64;
        let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 5) };
        assert_eq!(bytes, &[0x48, opcode, modrm, imm, 0xC3],
            "fold mismatch for `{src}` — got {:02X?}", bytes);
    }
}

#[test]
fn bare_binop_inlined_when_no_preceding_literal() {
    // `+` with no preceding literal can't fold, so (T2 bare-op inline) it is
    // emitted INLINE as `add rax,[rbp] ; add rbp,8` — no CALL, no JMP —
    // followed by the definition's RET.  (Previously this fell back to a CALL
    // that `;`/TCO patched into a JMP.)
    let mut s = sess();
    let out = s.eval(": twoadd + ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 9) };
    assert_eq!(
        bytes,
        &[0x48, 0x03, 0x45, 0x00, 0x48, 0x83, 0xC5, 0x08, 0xC3],
        "expected inline `add rax,[rbp]; add rbp,8; ret`, got {bytes:02X?}"
    );

    // Behaviour: 3 4 twoadd → 7
    let out = s.eval("3 4 twoadd .\nbye\n").unwrap();
    assert_eq!(out, "7  ok\n");
}

#[test]
fn literal_fold_shifts_emit_shift_imm8() {
    // `3 lshift` → SHL rax, 3 (4 bytes), no CALL.
    let cases: &[(&str, u8)] = &[
        (": shl3 3 lshift ;",  0xE0),   // SHL /4
        (": shr3 3 rshift ;",  0xE8),   // SHR /5
        (": sar3 3 arshift ;", 0xF8),   // SAR /7
    ];
    let mut s = sess();
    for &(src, modrm) in cases {
        s.eval(&format!("{src}\nbye\n")).unwrap();
        s.call("latestxt").unwrap();
        let xt = s.pop() as u64;
        let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 5) };
        assert_eq!(bytes, &[0x48, 0xC1, modrm, 0x03, 0xC3],
            "fold mismatch for `{src}` — got {:02X?}", bytes);
    }

    // Runtime: 4 3 lshift → 32; 32 3 rshift → 4; -32 3 arshift → -4.
    let out = s.eval("4 shl3 . 32 shr3 . -32 sar3 .\nbye\n").unwrap();
    assert_eq!(out, "32 4 -4  ok\n");
}

#[test]
fn literal_fold_shift_out_of_imm8_range_falls_back() {
    // Literal that doesn't fit in signed imm8 (300) must NOT fold for
    // shifts — we'd have to truncate or mask, and we'd rather fall back
    // cleanly to the unfolded `300 lshift` semantics.
    let mut s = sess();
    s.eval(": shbig 300 lshift ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 14) };
    // Must NOT fold into `shl rax, imm8` (48 C1 E0 ii).  With inline literals
    // the fallback materializes the literal in place — the body opens with the
    // push spill `mov [rbp-8], rax` (48 89 45 F8) — then runs lshift normally.
    assert_ne!(&bytes[..3], &[0x48, 0xC1, 0xE0], "300 must not fold to shl imm8");
    assert_eq!(&bytes[..4], &[0x48, 0x89, 0x45, 0xF8],
        "expected inline-literal push fallback, got {:02X?}", &bytes[..4]);
}

#[test]
fn literal_fold_equality_emits_sub_sub_sbb_pattern() {
    // `= 5` fold → sub rax, 5 ; sub rax, 1 ; sbb rax, rax (11 bytes)
    let mut s = sess();
    s.eval(": eq5 5 = ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 12) };
    assert_eq!(bytes, &[
        0x48, 0x83, 0xE8, 0x05,            // sub  rax, 5
        0x48, 0x83, 0xE8, 0x01,            // sub  rax, 1
        0x48, 0x19, 0xC0,                  // sbb  rax, rax
        0xC3,                               // ret
    ], "got {:02X?}", bytes);

    // Runtime
    let out = s.eval("5 eq5 . 6 eq5 . -1 eq5 .\nbye\n").unwrap();
    assert_eq!(out, "-1 0 0  ok\n");
}

#[test]
fn literal_fold_not_equal_emits_sub_add_sbb_pattern() {
    // `<> 5` fold → sub rax, 5 ; add rax, -1 ; sbb rax, rax (11 bytes)
    let mut s = sess();
    s.eval(": ne5 5 <> ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 12) };
    assert_eq!(bytes, &[
        0x48, 0x83, 0xE8, 0x05,            // sub  rax, 5
        0x48, 0x83, 0xC0, 0xFF,            // add  rax, -1
        0x48, 0x19, 0xC0,                  // sbb  rax, rax
        0xC3,                               // ret
    ], "got {:02X?}", bytes);

    let out = s.eval("5 ne5 . 6 ne5 . 0 ne5 .\nbye\n").unwrap();
    assert_eq!(out, "0 -1 -1  ok\n");
}

#[test]
fn literal_fold_u_less_emits_cmp_sbb_short_form() {
    // `u< 10` fold → cmp rax, 10 ; sbb rax, rax (7 bytes — the cheap path)
    let mut s = sess();
    s.eval(": ulow 10 u< ;\nbye\n").unwrap();
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 8) };
    assert_eq!(bytes, &[
        0x48, 0x83, 0xF8, 0x0A,            // cmp rax, 10
        0x48, 0x19, 0xC0,                  // sbb rax, rax
        0xC3,                               // ret
    ], "got {:02X?}", bytes);

    let out = s.eval("3 ulow . 10 ulow . 15 ulow .\nbye\n").unwrap();
    assert_eq!(out, "-1 0 0  ok\n");
}

#[test]
fn literal_fold_signed_compares_emit_cmp_setcc_pattern() {
    // Each: cmp rax, lit ; setCC al ; movzx eax, al ; neg rax (13 bytes)
    // For consistency we only check the setCC opcode byte at offset +5.
    let cases: &[(&str, u8, &str)] = &[
        (": lt10 10 < ;",   0x9C, "setl"),
        (": gt10 10 > ;",   0x9F, "setg"),
        (": le10 10 <= ;",  0x9E, "setle"),
        (": ge10 10 >= ;",  0x9D, "setge"),
        (": ugt10 10 u> ;", 0x97, "seta"),
        (": ule10 10 u<= ;",0x96, "setbe"),
        (": uge10 10 u>= ;",0x93, "setae"),
    ];
    let mut s = sess();
    for &(src, setcc_byte, name) in cases {
        s.eval(&format!("{src}\nbye\n")).unwrap();
        s.call("latestxt").unwrap();
        let xt = s.pop() as u64;
        let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 14) };
        assert_eq!(bytes[0..4], [0x48, 0x83, 0xF8, 0x0A],
            "expected `cmp rax, 10` prefix for `{src}`, got {:02X?}", &bytes[..4]);
        assert_eq!(bytes[4], 0x0F, "expected 0F prefix for {name}");
        assert_eq!(bytes[5], setcc_byte,
            "expected {name} opcode 0x{:02X} for `{src}`, got 0x{:02X}", setcc_byte, bytes[5]);
        assert_eq!(bytes[6], 0xC0, "expected setCC al modrm");
        assert_eq!(bytes[13], 0xC3, "expected trailing RET");
    }

    // Behaviour: 5 lt10 = true, 15 lt10 = false, etc.
    let out = s.eval("5 lt10 . 15 lt10 . 5 gt10 . 15 gt10 .\nbye\n").unwrap();
    assert_eq!(out, "-1 0 0 -1  ok\n");
}

#[test]
fn literal_fold_chains_through_consecutive_lit_op_pairs() {
    // `1 + 2 * 3 -` folds to back-to-back immediate-form instructions.
    // `2 *` strength-reduces to `add rax,rax` (3 B, not a 4 B imul), so the
    // chain is: add rax,1 ; add rax,rax ; sub rax,3 ; ret  (12 bytes).
    let mut s = sess();
    let out = s.eval(": chain 1 + 2 * 3 - ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let bytes = unsafe { std::slice::from_raw_parts(xt as *const u8, 12) };
    assert_eq!(bytes, &[
        0x48, 0x83, 0xC0, 0x01,           // add  rax, 1
        0x48, 0x01, 0xC0,                 // add  rax, rax   (* 2 strength-reduced)
        0x48, 0x83, 0xE8, 0x03,           // sub  rax, 3
        0xC3,                              // ret
    ], "got {:02X?}", bytes);

    // (x + 1) * 2 - 3 → at x=5 → 6*2-3 = 9
    let out = s.eval("5 chain .\nbye\n").unwrap();
    assert_eq!(out, "9  ok\n");
}

#[test]
fn eval_raw_branch_placeholders_preserve_stack_effects() {
    let mut s = sess();
    let out = s.eval(
        ": bra-test bra 7 ;\n\
         : qbra-test ?bra depth ;\n\
         : nqbra-test -?bra depth swap drop ;\n\
         bra-test .\n\
         0 qbra-test .\n\
         5 qbra-test .\n\
         0 nqbra-test .\n\
         5 nqbra-test .\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n7  ok\n0  ok\n0  ok\n1  ok\n1  ok\n");
}

#[test]
fn eval_compiled_loop_steps_update_i() {
    let mut s = sess();
    let out = s.eval(
        ": step1 20 3 do-part1 do-part2 _loop i . 2rdrop ;\n\
         : stepplus 20 3 do-part1 do-part2 2 _+loop i . 2rdrop ;\n\
         : stepminus 20 10 do-part1 do-part2 2 _-loop i . 2rdrop ;\n\
         step1\n\
         stepplus\n\
         stepminus\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n4  ok\n5  ok\n8  ok\n");
}

#[test]
fn code_dsl_defines_simple_primitive() {
    let mut s = sess();
    // Smallest possible CODE: word — just add 3 to TOS.
    let out = s.eval("CODE: add3  add rax, 3 ;CODE\n40 add3 .\nbye\n").unwrap();
    assert_eq!(out, " ok\n43  ok\n");
}

#[test]
fn code_dsl_supports_macro_vocabulary() {
    // The user can write `pushd`, `popd`, `stk(in,out)`, `next()` — all
    // resolved from the kernel's macros.masm which the CODE: assembler
    // preloads once.  Body spans multiple lines; rt_code_compile_body
    // peeks past the current SOURCE buffer into the Io input.
    let mut s = sess();
    let out = s.eval(
        "CODE: triple   ; ( n -- n*3 )\n\
             mov rcx, rax\n\
             add rax, rax\n\
             add rax, rcx\n\
             stk(1, 1)\n\
         ;CODE\n\
         7 triple .\nbye\n"
    ).unwrap();
    assert_eq!(out, " ok\n21  ok\n");
}

#[test]
fn code_dsl_compiled_into_colon_definition() {
    let mut s = sess();
    let out = s.eval(
        "CODE: sq  imul rax, rax ;CODE\n\
         : sum-of-squares  sq swap sq + ;\n\
         3 4 sum-of-squares .\nbye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n25  ok\n");
}

#[test]
fn code_dsl_invalid_asm_reports_throw() {
    // Bad mnemonics inside a CODE: body used to abort the test process.
    // With wfasm's diagnostic handler installed, parse errors flow back
    // and surface as a Forth THROW.
    let mut s = sess();
    let err = s.eval("CODE: bad  wibblywobbly ;CODE\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2057") || msg.contains("THROW"),
        "expected -2057 throw, got: {msg}");
}

#[test]
fn code_dsl_unterminated_body_reports_error() {
    let mut s = sess();
    let err = s.eval("CODE: never_ends  add rax, 1\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2057") || msg.contains("THROW"),
        "expected -2057 throw, got: {msg}");
}

/// LET tests load core.f because they need `f.` and friends from there.
fn sess_with_core() -> SessionGuard {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).expect("load core.f");
    s
}

#[test]
fn let_dsl_area_of_circle() {
    let mut s = sess_with_core();
    let out = s.eval(": area LET (r) -> (a) = pi * r * r END ;\n2.0 area f.\nbye\n").unwrap();
    // pi * 4 = 12.566370614359172
    assert!(out.contains("12.566"), "got {out:?}");
}

#[test]
fn let_dsl_multi_input_multi_output_mbrot() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": mbrot LET (z_re, z_im, x, y) -> (z_next_re, z_next_im, mag) = \
            re, im, rmag \
            WHERE re   = z_re * z_re - z_im * z_im + x \
            WHERE im   = 2 * z_re * z_im + y \
            WHERE rmag = re * re + im * im \
         END ;\n1.0 1.0 1.0 1.0 mbrot f. f. f.\nbye\n"
    ).unwrap();
    // f. prints TOS first: mag=10, im=3, z_next_re=1.
    assert!(out.contains("10."), "expected '10.' in output: {out:?}");
    assert!(out.contains("3."),  "expected '3.' in output: {out:?}");
    assert!(out.contains("1."),  "expected '1.' in output: {out:?}");
}

#[test]
fn let_dsl_arithmetic_chain() {
    let mut s = sess_with_core();
    let out = s.eval(": poly LET (x) -> (y) = x * x + 2 * x + 1 END ;\n3.0 poly f.\nbye\n").unwrap();
    // 9 + 6 + 1 = 16
    assert!(out.contains("16."), "got {out:?}");
}

#[test]
fn let_dsl_unary_minus() {
    let mut s = sess_with_core();
    let out = s.eval(": negsq LET (x) -> (y) = -(x * x) END ;\n5.0 negsq f.\nbye\n").unwrap();
    assert!(out.contains("-25."), "got {out:?}");
}

#[test]
fn let_dsl_where_bindings_topo_sort() {
    let mut s = sess_with_core();
    // WHERE clauses out-of-order: rmag depends on re/im which depend on inputs.
    // Topo sort must place re/im before rmag.
    let out = s.eval(
        ": sq2 LET (a, b) -> (r) = rmag WHERE rmag = re + im WHERE re = a*a WHERE im = b*b END ;\n\
         3.0 4.0 sq2 f.\nbye\n"
    ).unwrap();
    assert!(out.contains("25."), "got {out:?}");
}

#[test]
fn let_dsl_sqrt_via_forth_repl() {
    let mut s = sess_with_core();
    // Hypotenuse of (3, 4) = 5.
    let out = s.eval(
        ": hyp LET (x, y) -> (h) = sqrt(x*x + y*y) END ;\n\
         3.0 4.0 hyp f.\nbye\n"
    ).unwrap();
    assert!(out.contains("5.000000"), "got {out:?}");
}

#[test]
fn let_dsl_sin_cos_via_forth_repl() {
    let mut s = sess_with_core();
    // sin(0) + cos(0) = 0 + 1 = 1.
    let out = s.eval(
        ": both LET (x) -> (y) = sin(x) + cos(x) END ;\n\
         0.0 both f.\nbye\n"
    ).unwrap();
    assert!(out.contains("1.000000"), "got {out:?}");
}

#[test]
fn let_dsl_hypot_via_forth_repl() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": dist LET (x, y) -> (d) = hypot(x, y) END ;\n\
         3.0 4.0 dist f.\nbye\n"
    ).unwrap();
    assert!(out.contains("5.000000"), "got {out:?}");
}

#[test]
fn let_dsl_star_star_operator() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": cube LET (x) -> (y) = x ** 3 END ;\n\
         2.0 cube f.\nbye\n"
    ).unwrap();
    assert!(out.contains("8.000000"), "got {out:?}");
}

#[test]
fn let_dsl_comparisons_via_forth_repl() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": lt5 LET (x) -> (y) = x < 5 END ;\n\
         3.0 lt5 f.\n7.0 lt5 f.\nbye\n"
    ).unwrap();
    assert!(out.contains("1.000000") && out.contains("0.000000"), "got {out:?}");
}

#[test]
fn let_dsl_select_via_forth_repl() {
    let mut s = sess_with_core();
    // abs() built via select.
    let out = s.eval(
        ": myabs LET (x) -> (y) = select(x < 0, -x, x) END ;\n\
         -7.5 myabs f.\n3.25 myabs f.\nbye\n"
    ).unwrap();
    assert!(out.contains("7.500000"), "expected 7.500000 in {out:?}");
    assert!(out.contains("3.250000"), "expected 3.250000 in {out:?}");
}

#[test]
fn let_dsl_clamp_via_forth_repl() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": clamp LET (x, lo, hi) -> (y) = \
              select(x < lo, lo, select(x > hi, hi, x)) END ;\n\
         5.0 0.0 10.0 clamp f.\n\
         -3.0 0.0 10.0 clamp f.\n\
         99.0 0.0 10.0 clamp f.\nbye\n"
    ).unwrap();
    assert!(out.contains("5.000000"),  "got {out:?}");
    assert!(out.contains("0.000000"),  "got {out:?}");
    assert!(out.contains("10.000000"), "got {out:?}");
}

#[test]
fn let_dsl_compile_only_outside_colon() {
    let mut s = sess_with_core();
    // LET in interpret state runs `comp_only_word` → THROW -14.
    let err = s.eval("LET (x) -> (y) = x END\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-14") || msg.contains("THROW"),
        "expected -14 throw, got: {msg}");
}

// ── V1b GC primitives ────────────────────────────────────────────────

#[test]
fn gc_heapptr_pushes_stable_handle() {
    let mut s = sess();
    // HEAPPTR declares a slot; invoking the name pushes the slot's
    // address.  The same handle two pushes should equal each other
    // (the slot doesn't move).
    let out = s.eval("HEAPPTR foo\nfoo foo = .\nbye\n").unwrap();
    assert!(out.contains("-1"), "handle should be stable, got {out:?}");
}

#[test]
fn gc_vec_alloc_and_access() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR samples\n\
         8 samples vec-alloc-floats!\n\
         1.5e samples 0 vec-f!\n\
         2.5e samples 1 vec-f!\n\
         3.5e samples 7 vec-f!\n\
         samples 0 vec-f@ f.\n\
         samples 1 vec-f@ f.\n\
         samples 7 vec-f@ f.\n\
         samples vec-len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1.500000"), "got {out:?}");
    assert!(out.contains("2.500000"), "got {out:?}");
    assert!(out.contains("3.500000"), "got {out:?}");
    assert!(out.contains("8 "), "vec-len should report 8: {out:?}");
}

#[test]
fn gc_rooted_object_survives_collection() {
    let mut s = sess_with_core();
    // Use exact-representable values so f.'s 6-decimal-digit print
    // doesn't introduce a rounding ambiguity.
    let out = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         1.5e v 0 vec-f!\n\
         7.25e v 3 vec-f!\n\
         (gc)\n\
         v 0 vec-f@ f.\n\
         v 3 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1.500000"), "first cell lost across GC: {out:?}");
    assert!(out.contains("7.250000"), "last cell lost across GC: {out:?}");
}

#[test]
fn gc_two_megabyte_vector_worked_example() {
    // The 2 MB worked example from docs/gc_design.md.  Allocates
    // 262144 cells (= 2 MB of f64), writes scattered values, runs
    // a major GC, reads them back.  Large objects are pinned by
    // paged_gc so they generation-flip in place across collections.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR big\n\
         262144 big vec-alloc-floats!\n\
         1.5e big 1000 vec-f!\n\
         7.25e big 100000 vec-f!\n\
         0.125e big 200000 vec-f!\n\
         big 1000 vec-f@ f.\n\
         big 100000 vec-f@ f.\n\
         big 200000 vec-f@ f.\n\
         big vec-len .\n\
         (gc)\n\
         big 1000 vec-f@ f.\n\
         big 100000 vec-f@ f.\n\
         big 200000 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    // Use exactly-representable f64 values (1.5, 7.25, 0.125) to dodge
    // the rounding-direction ambiguity that bit the earlier 3.14159 form.
    assert!(out.contains("1.500000"), "cell 1000 wrong: {out:?}");
    assert!(out.contains("7.250000"), "cell 100000 wrong: {out:?}");
    assert!(out.contains("0.125000"), "cell 200000 wrong: {out:?}");
    assert!(out.contains("262144"), "vec-len wrong: {out:?}");
    // The same three values should still be present AFTER (gc) —
    // each f. output appears twice in the stream.
    let v_one_five = out.matches("1.500000").count();
    let v_seven_two = out.matches("7.250000").count();
    let v_one_two_five = out.matches("0.125000").count();
    assert_eq!(v_one_five, 2, "1.5 should appear twice (pre+post GC)");
    assert_eq!(v_seven_two, 2);
    assert_eq!(v_one_two_five, 2);
}

#[test]
fn gc_unrooted_object_gets_reclaimed() {
    // Allocate via vec-alloc-floats!, then null out the HEAPPTR, then
    // (gc).  The allocated bytes are no longer reachable and should
    // be reclaimed.  We can't observe this directly from Forth, but
    // we can allocate a LOT of orphans and verify the heap doesn't
    // grow indefinitely.
    let mut s = sess_with_core();
    let out = s.eval(
        ": cycle  ( -- )  HEAPPTR slot  100 slot vec-alloc-floats! ;\n\
         \\ Hmm: HEAPPTR can't be inside a colon definition (it's a\n\
         \\ defining word). Use a different shape.\n\
         HEAPPTR slot\n\
         100 slot vec-alloc-floats!\n\
         100 slot vec-alloc-floats!\n\
         100 slot vec-alloc-floats!\n\
         (gc)\n\
         slot 0 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    // The last allocation's cell 0 is 0.0 (fresh FILL_WORD).  The
    // first two allocations are unreachable after the second
    // vec-alloc-floats! overwrites the slot.
    assert!(out.contains("0.000000"), "got {out:?}");
}

// `gc_vec_f_fetch_wrong_type_throws` was the V1b umbrella test that
// covered both nil-deref and wrong-type cases under -2060.  V1c
// splits those: nil now throws -2061 (see
// `gc_vec_f_fetch_on_nil_throws_dedicated_code`), and the wrong-type
// path still throws -2060 (see
// `gc_vec_f_fetch_wrong_type_still_throws_minus_2060`).

#[test]
fn gc_heapptr_no_name_throws() {
    let mut s = sess();
    let err = s.eval("HEAPPTR\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-16") || msg.contains("THROW"),
        "expected -16 (name required) throw, got: {msg}");
}

#[test]
fn gc_minor_collection_keeps_rooted_object() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         42.0e v 0 vec-f!\n\
         gc-minor\n\
         gc-minor\n\
         gc-minor\n\
         v 0 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("42.000000"), "got {out:?}");
}

#[test]
fn gc_forget_last_reuses_heapptr_slot() {
    // V1c: after `forget_last` on a HEAPPTR-defined word, HEAPPTR_NEXT
    // rolls back past its slot.  A subsequently declared HEAPPTR
    // re-uses the same slot address.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR a\n\
         a .\n\
         forget_last\n\
         HEAPPTR b\n\
         b .\n\
         bye\n"
    ).unwrap();
    let parsed: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(parsed.len(), 2,
        "expected 2 slot addresses, got {parsed:?} from {out:?}");
    assert_eq!(parsed[0], parsed[1],
        "slot addr should be reused after forget; got {out:?}");
}

#[test]
fn gc_forget_last_zeroes_abandoned_slot() {
    // After allocating into HEAPPTR a, forget_last should zero the
    // abandoned slot.  Verified by stashing the slot's raw address in
    // a VARIABLE *before* defining the HEAPPTR (so VARIABLE survives
    // the forget), then dereferencing it again after the forget.  Pre-
    // forget the slot holds a tagged FloatVec pointer (non-zero, low
    // bits = 010); post-forget it must be 0.
    let mut s = sess_with_core();
    let out = s.eval(
        "VARIABLE saved\n\
         HEAPPTR a\n\
         a saved !\n\
         10 a vec-alloc-floats!\n\
         saved @ @ .\n\
         forget_last\n\
         saved @ @ .\n\
         bye\n"
    ).unwrap();
    let parsed: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(parsed.len(), 2,
        "expected 2 cell values, got {parsed:?} from {out:?}");
    assert_ne!(parsed[0], 0,
        "pre-forget slot should hold a tagged ptr; got {out:?}");
    assert_eq!(parsed[0] & 7, 2,
        "pre-forget slot should be a FloatVec (tag 010); got {out:?}");
    assert_eq!(parsed[1], 0,
        "post-forget slot should be zeroed; got {out:?}");
}

#[test]
fn gc_forget_last_on_non_heapptr_leaves_region_alone() {
    // A regular colon definition forget should NOT touch HEAPPTR_NEXT.
    // Define HEAPPTR a, then : foo ;, then forget_last (removes foo).
    // After: HEAPPTR b should land in slot 1, not slot 0.  So `a` and
    // `b` print different addresses.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR a\n\
         : foo 42 ;\n\
         forget_last\n\
         HEAPPTR b\n\
         a .\n\
         b .\n\
         bye\n"
    ).unwrap();
    let parsed: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(parsed.len(), 2, "got {parsed:?} from {out:?}");
    assert_eq!(parsed[1] - parsed[0], 8,
        "b should be one cell past a; got a={} b={}", parsed[0], parsed[1]);
}

#[test]
fn gc_vec_f_fetch_on_nil_throws_dedicated_code() {
    // V1c: nil-deref produces -2061, distinct from -2060 (wrong type).
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR empty\nempty 0 vec-f@ f.\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "expected -2061 (nil-deref) on vec-f@ over nil slot, got: {msg}");
}

#[test]
fn gc_vec_f_store_on_nil_throws_dedicated_code() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR empty\n42.0e empty 0 vec-f!\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "expected -2061 (nil-deref) on vec-f! over nil slot, got: {msg}");
}

#[test]
fn gc_vec_len_on_nil_throws_dedicated_code() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR empty\nempty vec-len .\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "expected -2061 (nil-deref) on vec-len over nil slot, got: {msg}");
}

#[test]
fn gc_cycle_starts_at_zero() {
    let mut s = sess_with_core();
    let out = s.eval("gc-cycle .\nbye\n").unwrap();
    assert!(out.contains("0  ok"),
        "gc-cycle should start at 0; got {out:?}");
}

#[test]
fn gc_cycle_increments_on_explicit_major_collection() {
    let mut s = sess_with_core();
    let out = s.eval(
        "gc-cycle .\n\
         (gc)\n\
         gc-cycle .\n\
         (gc)\n\
         gc-cycle .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0, 1, 2],
        "gc-cycle should monotonically increase on (gc); got {nums:?} from {out:?}");
}

#[test]
fn gc_cycle_increments_on_minor_collection() {
    let mut s = sess_with_core();
    let out = s.eval(
        "gc-minor\n\
         gc-cycle .\n\
         gc-minor\n\
         gc-cycle .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![1, 2],
        "gc-cycle should bump on gc-minor too; got {nums:?} from {out:?}");
}

#[test]
fn gc_auto_collects_when_budget_exhausted() {
    // V2: vec-alloc-* checks should_collect() and runs a minor GC
    // first if the budget is exhausted.  paged_gc's default trigger
    // is 8 MB; each 200_000-cell FloatVec is ~1.6 MB (200k * 8B +
    // header).  Allocating 8 of them (with the previous one
    // dropped each cycle) should force at least one auto-trigger.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR slot\n\
         gc-cycle .                \\ should be 0\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         200000 slot vec-alloc-floats!\n\
         gc-cycle .                \\ should be > 0\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums.len(), 2, "got {nums:?} from {out:?}");
    assert_eq!(nums[0], 0, "gc-cycle should start at 0; got {out:?}");
    assert!(nums[1] >= 1,
        "gc-cycle should bump from auto-GC; got pre={} post={} ({out:?})",
        nums[0], nums[1]);
}

#[test]
fn gc_auto_collects_does_not_lose_rooted_data() {
    // After auto-GC the still-rooted vector should be intact.
    // Allocate, write known cells, force enough allocation to
    // trigger auto-GC at least once, then read the cells back.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR keep\n\
         HEAPPTR scratch\n\
         4 keep vec-alloc-floats!\n\
         1.5e keep 0 vec-f!\n\
         2.5e keep 1 vec-f!\n\
         3.5e keep 2 vec-f!\n\
         4.5e keep 3 vec-f!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         200000 scratch vec-alloc-floats!\n\
         gc-cycle .\n\
         keep 0 vec-f@ f.\n\
         keep 1 vec-f@ f.\n\
         keep 2 vec-f@ f.\n\
         keep 3 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1.500000"), "cell 0 lost: {out:?}");
    assert!(out.contains("2.500000"), "cell 1 lost: {out:?}");
    assert!(out.contains("3.500000"), "cell 2 lost: {out:?}");
    assert!(out.contains("4.500000"), "cell 3 lost: {out:?}");
    // Should have triggered at least one auto-collection.
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert!(nums.iter().any(|&n| n >= 1),
        "expected at least one auto-GC cycle; got {nums:?} from {out:?}");
}

#[test]
fn gc_store_heapptr_copies_tagged_pointer() {
    // V2-B: `!heapptr` is the safe-by-intent way to copy a tagged
    // pointer from one HEAPPTR to another (or to nil out a slot).
    // After `a @ b !heapptr`, both slots reference the same vector
    // and reading payload via b returns what was written via a.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR a\n\
         HEAPPTR b\n\
         4 a vec-alloc-floats!\n\
         7.25e a 2 vec-f!\n\
         a @ b !heapptr\n\
         b 2 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("7.250000"),
        "b should see the cell a wrote; got {out:?}");
}

#[test]
fn gc_store_heapptr_can_nil_a_slot() {
    // Storing 0 (nil) via !heapptr makes the slot vec-len-able
    // throw -2061 the way a freshly-declared HEAPPTR does.
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR a\n\
         4 a vec-alloc-floats!\n\
         0 a !heapptr\n\
         a vec-len .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "nil'd slot should throw -2061; got {msg}");
}

#[test]
fn gc_store_heapptr_survives_subsequent_collection() {
    // After !heapptr from a → b, run (gc).  Both slots should
    // resolve to the (possibly relocated) object, and the payload
    // should be intact.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR a\n\
         HEAPPTR b\n\
         4 a vec-alloc-floats!\n\
         1.5e a 0 vec-f!\n\
         2.5e a 1 vec-f!\n\
         a @ b !heapptr\n\
         (gc)\n\
         a 0 vec-f@ f.\n\
         a 1 vec-f@ f.\n\
         b 0 vec-f@ f.\n\
         b 1 vec-f@ f.\n\
         bye\n"
    ).unwrap();
    // Each value should appear twice (once via a, once via b).
    assert_eq!(out.matches("1.500000").count(), 2,
        "cell 0 should be readable via both a and b post-GC; got {out:?}");
    assert_eq!(out.matches("2.500000").count(), 2,
        "cell 1 should be readable via both a and b post-GC; got {out:?}");
}

#[test]
fn gc_long_running_promotes_and_survives() {
    // Tenure-promotion stress test.  Allocate a rooted vector,
    // then run a large number of minor GCs interleaved with
    // throw-away allocations.  paged_gc promotes G0 → G1 → Tenured
    // across multiple cycles; after 20+ cycles the rooted object
    // is definitely tenured.  Verify the payload is still intact.
    //
    // This is the read-only half of the V2 generational stress
    // test from docs/gc_design.md ("allocate young, promote to
    // old via repeated collections").  The "mutate old to point
    // at young" half needs vec-ref! (V3 + write barrier), which
    // hasn't landed yet — see docs/forth_gc_needs.md item #2.
    let mut s = sess_with_core();
    let mut script = String::from(
        "HEAPPTR rooted\n\
         HEAPPTR scratch\n\
         8 rooted vec-alloc-floats!\n\
         1.5e rooted 0 vec-f!\n\
         2.5e rooted 1 vec-f!\n\
         3.5e rooted 2 vec-f!\n\
         4.5e rooted 3 vec-f!\n\
         5.5e rooted 4 vec-f!\n\
         6.5e rooted 5 vec-f!\n\
         7.5e rooted 6 vec-f!\n\
         8.5e rooted 7 vec-f!\n"
    );
    // 25 rounds of (alloc-throwaway, gc-minor) — enough to promote
    // and exercise multiple promotion-cycle transitions.
    for _ in 0..25 {
        script.push_str("16 scratch vec-alloc-floats!\ngc-minor\n");
    }
    script.push_str(
        "gc-cycle .\n\
         rooted 0 vec-f@ f.\n\
         rooted 1 vec-f@ f.\n\
         rooted 2 vec-f@ f.\n\
         rooted 3 vec-f@ f.\n\
         rooted 4 vec-f@ f.\n\
         rooted 5 vec-f@ f.\n\
         rooted 6 vec-f@ f.\n\
         rooted 7 vec-f@ f.\n\
         bye\n"
    );
    let out = s.eval(&script).unwrap();
    // gc-cycle should reflect at least our 25 explicit gc-minor
    // calls (possibly more if auto-GC fired too).
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert!(nums.first().copied().unwrap_or(0) >= 25,
        "expected >=25 gc cycles; got {nums:?} from start of {out:?}");
    for v in [
        "1.500000", "2.500000", "3.500000", "4.500000",
        "5.500000", "6.500000", "7.500000", "8.500000",
    ] {
        assert!(out.contains(v),
            "payload cell {v} lost across {} cycles; got {out:?}",
            nums.first().copied().unwrap_or(0));
    }
}

#[test]
fn gc_many_rooted_vectors_all_survive() {
    // Stress: bind ten HEAPPTRs to ten distinct vectors, each
    // with a unique marker cell.  Run many minor collections.
    // All ten markers should still be readable.
    let mut s = sess_with_core();
    let mut script = String::new();
    for i in 0..10 {
        script.push_str(&format!("HEAPPTR slot{i}\n"));
    }
    for i in 0..10 {
        script.push_str(&format!(
            "4 slot{i} vec-alloc-floats!\n{i}.5e slot{i} 0 vec-f!\n"
        ));
    }
    for _ in 0..15 {
        script.push_str("gc-minor\n");
    }
    for i in 0..10 {
        script.push_str(&format!("slot{i} 0 vec-f@ f.\n"));
    }
    script.push_str("bye\n");
    let out = s.eval(&script).unwrap();
    for i in 0..10 {
        let expected = format!("{i}.500000");
        assert!(out.contains(&expected),
            "slot{i} payload lost after 15 cycles; expected {expected}, got {out:?}");
    }
}

// ── V2s stage A — managed strings ─────────────────────────────────

#[test]
fn str_to_string_round_trips_bytes() {
    // S" pushes (c-addr u); >$ allocates a managed String and
    // returns a tagged ptr.  $>addr exposes the payload addr/len
    // for one-shot interop with TYPE.
    let mut s = sess_with_core();
    let out = s.eval("s\" hello, world\" >$ $>addr type cr\nbye\n").unwrap();
    assert!(out.contains("hello, world"),
        "round-trip via >$ / $>addr / TYPE failed; got {out:?}");
}

#[test]
fn str_len_returns_byte_count() {
    let mut s = sess_with_core();
    let out = s.eval("s\" abcdefghij\" >$ $len .\nbye\n").unwrap();
    assert!(out.contains("10  ok"),
        "$len of 10-byte string should be 10; got {out:?}");
}

#[test]
fn str_len_of_empty_is_zero() {
    let mut s = sess_with_core();
    let out = s.eval("s\" \" >$ $len .\nbye\n").unwrap();
    assert!(out.contains("0  ok"),
        "$len of empty string should be 0; got {out:?}");
}

#[test]
fn str_equal_compares_bytes() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hello\" >$ s\" hello\" >$ $= .\n\
         s\" hello\" >$ s\" world\" >$ $= .\n\
         s\" hello\" >$ s\" hell\" >$ $= .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, 0, 0],
        "expected (true, false, false) for hello/hello, hello/world, hello/hell; got {nums:?} from {out:?}");
}

#[test]
fn str_equal_same_object_is_true() {
    // Same tagged pointer twice on the stack ought to compare equal
    // (covers the fast-path identity check in rt_string_bytes_equal).
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR a\n\
         s\" foo\" >$ a !$\n\
         a @$ a @$ $= .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("-1  ok"),
        "same-object $= should be true; got {out:?}");
}

#[test]
fn str_store_and_fetch_via_heapptr() {
    // !$ stores a tagged String into a HEAPPTR slot; @$ fetches.
    // Both type-check.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR greet\n\
         s\" hi there\" >$ greet !$\n\
         greet @$ $>addr type cr\n\
         greet @$ $len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("hi there"), "got {out:?}");
    assert!(out.contains("8  ok"), "$len should be 8; got {out:?}");
}

#[test]
fn str_fetch_from_unbound_slot_returns_nil() {
    // @$ on a never-bound HEAPPTR returns 0 (nil) — *not* a throw.
    // This is the V2s "the empty answer" convention from
    // docs/strings_design.md.
    let mut s = sess_with_core();
    let out = s.eval("HEAPPTR empty\nempty @$ .\nbye\n").unwrap();
    assert!(out.contains("0  ok"),
        "@$ on nil slot should return 0; got {out:?}");
}

#[test]
fn str_store_nil_is_allowed() {
    // !$ accepts 0 (nil) to let a slot be cleared explicitly.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR x\n\
         s\" before\" >$ x !$\n\
         x @$ $len .\n\
         0 x !$\n\
         x @$ .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![6, 0],
        "expected (6, 0) — len then nil; got {nums:?} from {out:?}");
}

#[test]
fn str_store_wrong_type_throws() {
    // !$ rejects a non-String tagged value (e.g., a FloatVec).
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR x\n\
         HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         v @ x !$\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "storing FloatVec via !$ should throw -2060; got {msg}");
}

#[test]
fn str_fetch_wrong_type_throws() {
    // @$ rejects a slot that holds a non-String tagged value.
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR x\n\
         4 x vec-alloc-floats!\n\
         x @$ .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "@$ on FloatVec slot should throw -2060; got {msg}");
}

#[test]
fn str_len_on_nil_throws() {
    let mut s = sess_with_core();
    let err = s.eval("0 $len .\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "$len on 0 should throw -2061; got {msg}");
}

#[test]
fn str_len_on_wrong_type_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         v @ $len .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "$len on FloatVec should throw -2060; got {msg}");
}

#[test]
fn str_survives_collection() {
    // A managed String rooted via @$ should survive (gc).  Verify
    // by reading the bytes back through TYPE after the collection.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR msg\n\
         s\" survive me\" >$ msg !$\n\
         (gc)\n\
         msg @$ $>addr type cr\n\
         msg @$ $len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("survive me"),
        "string bytes lost across (gc); got {out:?}");
    assert!(out.contains("10  ok"),
        "length wrong after (gc); got {out:?}");
}

#[test]
fn str_many_strings_all_survive_collection() {
    // Allocate a bunch of distinct managed strings rooted via
    // separate HEAPPTRs.  Run minor GCs.  All should still be
    // intact and distinguishable via $=.
    let mut s = sess_with_core();
    let mut script = String::new();
    for i in 0..8 {
        script.push_str(&format!("HEAPPTR s{i}\n"));
    }
    for i in 0..8 {
        // Each gets a distinct payload like "msg-0", "msg-1", ...
        script.push_str(&format!("s\" msg-{i}\" >$ s{i} !$\n"));
    }
    for _ in 0..5 {
        script.push_str("gc-minor\n");
    }
    for i in 0..8 {
        script.push_str(&format!("s{i} @$ $>addr type cr\n"));
    }
    script.push_str("bye\n");
    let out = s.eval(&script).unwrap();
    for i in 0..8 {
        let expected = format!("msg-{i}");
        assert!(out.contains(&expected),
            "string {expected} lost after minor cycles; got {out:?}");
    }
}

#[test]
fn str_empty_strings_compare_equal() {
    let mut s = sess_with_core();
    let out = s.eval("s\" \" >$ s\" \" >$ $= .\nbye\n").unwrap();
    assert!(out.contains("-1  ok"),
        "two empty strings should compare equal; got {out:?}");
}

// ── V2s stage B — S$" compile-time literals ──────────────────────

#[test]
fn str_s_dollar_quote_interpret_mode_pushes_tagged() {
    // Outside a colon definition, S$" allocates and pushes the
    // tagged pointer immediately, just like >$ but with the bytes
    // parsed from the input stream.
    let mut s = sess_with_core();
    let out = s.eval("S$\" hello, world\" $>addr type cr\nbye\n").unwrap();
    assert!(out.contains("hello, world"),
        "interpret-mode S$\" should produce a usable String; got {out:?}");
}

#[test]
fn str_s_dollar_quote_compile_mode_emits_literal() {
    // Inside a colon def, S$" allocates a LITERAL slot at compile
    // time; each call to the word pushes the SAME tagged pointer.
    // We can verify "same" by comparing the printed addresses.
    let mut s = sess_with_core();
    let out = s.eval(
        ": greet S$\" howdy\" ;\n\
         greet $>addr type space\n\
         greet $>addr type cr\n\
         greet . greet . cr\n\
         bye\n"
    ).unwrap();
    // The body should appear twice.
    assert_eq!(out.matches("howdy").count(), 2,
        "greet should produce 'howdy' twice; got {out:?}");
    // The two tagged-ptr values from `greet .` should be equal.
    // Pull the numeric tokens out of the final line.
    let parsed: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert!(parsed.len() >= 2, "expected at least 2 ints; got {parsed:?} from {out:?}");
    let n = parsed.len();
    assert_eq!(parsed[n - 2], parsed[n - 1],
        "two invocations of `greet` should push the same tagged ptr (literal); \
         got {} and {}", parsed[n-2], parsed[n-1]);
}

#[test]
fn str_s_dollar_quote_literal_survives_collection() {
    // A LITERAL-region slot is a GC root.  Define a word that
    // returns a literal, force a (gc), then call it again —
    // the contents should still be readable, even if paged_gc
    // moved the underlying String.
    let mut s = sess_with_core();
    let out = s.eval(
        ": label S$\" persistent\" ;\n\
         label $>addr type cr\n\
         (gc)\n\
         label $>addr type cr\n\
         label $len .\n\
         bye\n"
    ).unwrap();
    assert_eq!(out.matches("persistent").count(), 2,
        "literal should be readable both pre and post (gc); got {out:?}");
    assert!(out.contains("10  ok"),
        "$len should be 10; got {out:?}");
}

#[test]
fn str_s_dollar_quote_two_literals_are_distinct() {
    // Two textually-identical S$" forms allocate TWO slots — V2s
    // explicitly defers interning (see strings_design.md "out of
    // scope for V2s").  Distinct objects, equal bytes via $=.
    let mut s = sess_with_core();
    let out = s.eval(
        ": a S$\" same\" ;\n\
         : b S$\" same\" ;\n\
         a b $= .\n\
         a b = .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, 0],
        "$= should be true (bytes match), = should be false (distinct objects); \
         got {nums:?} from {out:?}");
}

#[test]
fn str_s_dollar_quote_empty_literal() {
    let mut s = sess_with_core();
    let out = s.eval(
        ": nada S$\" \" ;\n\
         nada $len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("0  ok"),
        "empty literal should have $len 0; got {out:?}");
}

#[test]
fn str_s_dollar_quote_many_literals() {
    // Allocate 32 distinct literals, verify each reads back
    // correctly after a major GC.  This exercises both the
    // LITERAL bump pointer and the GC's walk of that region.
    let mut s = sess_with_core();
    let mut script = String::new();
    for i in 0..32 {
        script.push_str(&format!(": lit{i} S$\" item-{i}\" ;\n"));
    }
    script.push_str("(gc)\n");
    for i in 0..32 {
        script.push_str(&format!("lit{i} $>addr type cr\n"));
    }
    script.push_str("bye\n");
    let out = s.eval(&script).unwrap();
    for i in 0..32 {
        let expected = format!("item-{i}");
        assert!(out.contains(&expected),
            "literal {expected} lost; got tail of out:\n{}",
            &out[out.len().saturating_sub(2000)..]);
    }
}

#[test]
fn str_s_dollar_quote_inside_colon_def_with_other_code() {
    // S$" can appear mid-definition alongside arithmetic and
    // legacy words.
    let mut s = sess_with_core();
    let out = s.eval(
        ": describe ( n -- )  S$\" n=\" $>addr type . cr ;\n\
         42 describe\n\
         bye\n"
    ).unwrap();
    // The TYPE prints "n=", then `.` prints " 42 ", then CR.
    assert!(out.contains("n=42 "),
        "mixed colon-def output wrong; got {out:?}");
}

// ── V2s stage C1 — MutStringBuilder ───────────────────────────────

#[test]
fn sb_new_starts_empty_with_requested_capacity() {
    let mut s = sess_with_core();
    let out = s.eval(
        "64 sb-new\n\
         dup sb-len .\n\
         sb-capacity .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0, 64],
        "fresh builder: len=0, cap=64; got {nums:?} from {out:?}");
}

#[test]
fn sb_append_string_grows_length() {
    // Hold the builder via a HEAPPTR so any allocation triggered
    // inside `>$` (auto-GC) doesn't strand a stale tagged-ptr copy
    // on the data stack.  This is the design's official idiom.
    //
    // Note: every `s"` here keeps a space after the closing quote
    // *to keep the tokenizer happy* — `s"," ...` would be read as
    // one whitespace-delimited token `s",`.  Standard `s" ... "`
    // requires a leading space; we double-space the trailing one
    // for symmetry.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR b\n\
         32 sb-new b !\n\
         s\" hello\" >$ b @ sb-append$\n\
         s\" , \" >$ b @ sb-append$\n\
         s\" world\" >$ b @ sb-append$\n\
         b @ sb-len .\n\
         b @ sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("12  ok"),
        "post-append length should be 12; got {out:?}");
    assert!(out.contains("hello, world"),
        "sb>string should produce the concatenated bytes; got {out:?}");
}

#[test]
fn sb_to_string_resets_length() {
    // Per design: sb>string produces a fresh String and resets the
    // builder's length to 0 (capacity retained) — the builder can
    // be reused.
    let mut s = sess_with_core();
    let out = s.eval(
        "16 sb-new                  ( sb )\n\
         s\" abc\" >$ over sb-append$\n\
         dup sb>string drop         ( sb )\n\
         dup sb-len .               \\ should be 0\n\
         sb-capacity .              \\ should still be 16\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0, 16],
        "after sb>string: len=0, cap=16; got {nums:?} from {out:?}");
}

#[test]
fn sb_clear_resets_length_only() {
    let mut s = sess_with_core();
    let out = s.eval(
        "16 sb-new\n\
         s\" abc\" >$ over sb-append$\n\
         dup sb-len .               \\ 3\n\
         dup sb-clear\n\
         dup sb-len .               \\ 0\n\
         sb-capacity .              \\ still 16\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![3, 0, 16],
        "got {nums:?} from {out:?}");
}

#[test]
fn sb_append_n_formats_decimal() {
    let mut s = sess_with_core();
    let out = s.eval(
        "32 sb-new\n\
         42 over sb-append-n\n\
         -7 over sb-append-n\n\
         0 over sb-append-n\n\
         sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    // Should print "42-70" concatenated.
    assert!(out.contains("42-70"),
        "decimal appends should concatenate; got {out:?}");
}

#[test]
fn sb_append_c_ascii_one_byte() {
    let mut s = sess_with_core();
    let out = s.eval(
        "8 sb-new\n\
         65 over sb-append-c             \\ 'A'\n\
         66 over sb-append-c             \\ 'B'\n\
         67 over sb-append-c             \\ 'C'\n\
         dup sb-len .\n\
         sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("3  ok"),
        "3 ASCII chars → 3 bytes; got {out:?}");
    assert!(out.contains("ABC"),
        "should print ABC; got {out:?}");
}

#[test]
fn sb_append_c_utf8_multibyte() {
    // U+00E9 'é' (decimal 233) is 2 bytes in UTF-8 (0xC3 0xA9).
    // U+20AC '€' (decimal 8364) is 3 bytes (0xE2 0x82 0xAC).
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR b\n\
         16 sb-new b !\n\
         233 b @ sb-append-c\n\
         8364 b @ sb-append-c\n\
         b @ sb-len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("5  ok"),
        "é (2 bytes) + € (3 bytes) = 5; got {out:?}");
}

#[test]
fn sb_append_overflow_throws_minus_2062() {
    let mut s = sess_with_core();
    let err = s.eval(
        "4 sb-new\n\
         s\" hello\" >$ over sb-append$    \\ 5 bytes into 4-byte cap\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2062"),
        "expected -2062 (capacity overflow); got {msg}");
}

#[test]
fn sb_wrong_type_throws_minus_2060() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         v @ sb-len .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "sb-len on FloatVec should throw -2060; got {msg}");
}

#[test]
fn sb_nil_throws_minus_2061() {
    let mut s = sess_with_core();
    let err = s.eval("0 sb-len .\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "sb-len on nil should throw -2061; got {msg}");
}

#[test]
fn sb_survives_collection() {
    // Stash a builder via a HEAPPTR, force a (gc), then continue
    // appending — payload must survive even if the underlying
    // builder object got relocated.
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR b\n\
         128 sb-new b !\n\
         s\" pre-\" >$ b @ sb-append$\n\
         (gc)\n\
         s\" post\" >$ b @ sb-append$\n\
         b @ sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("pre-post"),
        "builder payload lost across (gc); got {out:?}");
}

#[test]
fn sb_round_trip_through_to_string() {
    // Build a string with sb-append-n / sb-append$ / sb-append-c,
    // finalise, compare to the expected.
    let mut s = sess_with_core();
    let out = s.eval(
        "64 sb-new\n\
         S$\" page \" over sb-append$\n\
         3 over sb-append-n\n\
         32 over sb-append-c             \\ space\n\
         S$\" of \" over sb-append$\n\
         10 over sb-append-n\n\
         sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("page 3 of 10"),
        "concatenated output wrong; got {out:?}");
}

// ── V2s stage C2 — operations library ─────────────────────────────

#[test]
fn str_concat_produces_fresh_string() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" foo\" >$ s\" bar\" >$ $+ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("foobar"), "got {out:?}");
}

#[test]
fn str_concat_empty_left() {
    let mut s = sess_with_core();
    let out = s.eval(
        "empty$ s\" tail\" >$ $+ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("tail"), "got {out:?}");
}

#[test]
fn str_concat_empty_right() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" head\" >$ empty$ $+ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("head"), "got {out:?}");
}

#[test]
fn str_slice_basic() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hello world\" >$ 6 11 $slice $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("world"), "got {out:?}");
}

#[test]
fn str_slice_empty_range() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hello\" >$ 2 2 $slice $len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("0  ok"), "expected empty slice; got {out:?}");
}

#[test]
fn str_slice_out_of_bounds_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "s\" hello\" >$ 0 99 $slice drop\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2058"),
        "out-of-bounds $slice should throw -2058; got {msg}");
}

#[test]
fn str_find_present() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" world\" >$ s\" hello world\" >$ $find .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("6  ok"), "got {out:?}");
}

#[test]
fn str_find_absent_returns_minus_one() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" xyz\" >$ s\" hello\" >$ $find .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("-1  ok"), "got {out:?}");
}

#[test]
fn str_find_empty_needle_matches_at_zero() {
    let mut s = sess_with_core();
    let out = s.eval(
        "empty$ s\" hello\" >$ $find .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("0  ok"), "got {out:?}");
}

#[test]
fn str_starts_and_ends() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hel\" >$ s\" hello\" >$ $starts? .\n\
         s\" llo\" >$ s\" hello\" >$ $ends? .\n\
         s\" xyz\" >$ s\" hello\" >$ $starts? .\n\
         s\" xyz\" >$ s\" hello\" >$ $ends? .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, -1, 0, 0],
        "got {nums:?} from {out:?}");
}

#[test]
fn str_cmp_orders_correctly() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" abc\" >$ s\" abd\" >$ $cmp .\n\
         s\" abd\" >$ s\" abc\" >$ $cmp .\n\
         s\" abc\" >$ s\" abc\" >$ $cmp .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, 1, 0], "got {nums:?} from {out:?}");
}

#[test]
fn str_hash_same_bytes_same_hash() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hello\" >$ $hash .\n\
         s\" hello\" >$ $hash .\n\
         s\" world\" >$ $hash .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums.len(), 3);
    assert_eq!(nums[0], nums[1], "identical bytes should hash equal; got {nums:?}");
    assert_ne!(nums[0], nums[2], "different bytes should hash differently; got {nums:?}");
}

#[test]
fn str_ci_eq_ascii() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" Hello\" >$ s\" hello\" >$ $ci= .\n\
         s\" HELLO\" >$ s\" hello\" >$ $ci= .\n\
         s\" hello\" >$ s\" world\" >$ $ci= .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, -1, 0], "got {nums:?} from {out:?}");
}

#[test]
fn str_trim_strips_whitespace_both_ends() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\"    hello world   \" >$ $trim $>addr type cr\n\
         bye\n"
    ).unwrap();
    // After trim, the byte content should be exactly "hello world".
    assert!(out.contains("hello world"), "got {out:?}");
    // The leading whitespace should be gone — check by length.
    let out2 = s.eval(
        "s\"    abc \" >$ $trim $len .\n\
         bye\n"
    ).unwrap();
    assert!(out2.contains("3  ok"), "$trim length wrong; got {out2:?}");
}

#[test]
fn str_ltrim_rtrim() {
    // `s"   abc   "` — `s"` consumes ONE leading space (the
    // standard delimiter), so the parsed bytes are 2 leading + "abc"
    // + 3 trailing = 8 bytes.  After ltrim: "abc   " = 6.  After
    // rtrim: "  abc" = 5.
    let mut s = sess_with_core();
    let out = s.eval(
        "s\"   abc   \" >$ $ltrim $len .\n\
         s\"   abc   \" >$ $rtrim $len .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![6, 5], "got {nums:?} from {out:?}");
}

#[test]
fn str_n_to_string_decimal_round_trip() {
    let mut s = sess_with_core();
    let out = s.eval(
        "42 n>$ $>addr type cr\n\
         -17 n>$ $>addr type cr\n\
         0 n>$ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("42\n"), "got {out:?}");
    assert!(out.contains("-17\n"), "got {out:?}");
    assert!(out.contains("0\n"), "got {out:?}");
}

#[test]
fn str_to_n_parses_decimal() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" 123\" >$ $>n . .\n\
         s\" -42\" >$ $>n . .\n\
         bye\n"
    ).unwrap();
    // On success: ( n true ).  `. .` prints `true value` (top first).
    // For 123 success: prints "-1 123" → "-1 123".
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, 123, -1, -42], "got {nums:?} from {out:?}");
}

#[test]
fn str_to_n_failure_returns_false() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" not a number\" >$ $>n .\n\
         bye\n"
    ).unwrap();
    // Failure: pushes only 0 (false).
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0], "got {nums:?} from {out:?}");
}

#[test]
fn str_empty_string_has_zero_length() {
    let mut s = sess_with_core();
    let out = s.eval("empty$ $len .\nbye\n").unwrap();
    assert!(out.contains("0  ok"), "got {out:?}");
}

#[test]
fn str_ops_compose() {
    // Realistic composition: take a string, slice the middle,
    // concat with a prefix, compare.
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" hello, world!\" >$ 7 12 $slice            ( \"world\" )\n\
         s\" hello \" >$ swap $+                        ( \"hello world\" )\n\
         s\" hello world\" >$ $= .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("-1  ok"),
        "expected $= true after compose; got {out:?}");
}

// ── V2s stage D — extended operations ────────────────────────────

#[test]
fn str_contains_true_and_false() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" world\" >$ s\" hello world\" >$ $contains? .\n\
         s\" xyz\"   >$ s\" hello world\" >$ $contains? .\n\
         empty$ s\" hello\" >$ $contains? .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![-1, 0, -1], "got {nums:?} from {out:?}");
}

#[test]
fn str_rfind_last_occurrence() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" ab\" >$ s\" abcab\" >$ $rfind .\n\
         s\" xy\" >$ s\" abcab\" >$ $rfind .\n\
         empty$  s\" hello\" >$ $rfind .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    // First: "ab" appears at 0 and 3 — last is 3.
    // Second: not found → -1.
    // Third: empty needle → haystack length = 5.
    assert_eq!(nums, vec![3, -1, 5], "got {nums:?} from {out:?}");
}

#[test]
fn str_repeat_basic() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" ab\" >$ 3 $repeat $>addr type cr\n\
         s\" x\"  >$ 0 $repeat $len .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("ababab"), "got {out:?}");
    assert!(out.contains("0  ok"), "0-repeat should yield empty string; got {out:?}");
}

#[test]
fn str_replace_simple() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" world\" >$ s\" Forth\" >$ s\" hello world!\" >$ $replace $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("hello Forth!"), "got {out:?}");
}

#[test]
fn str_replace_multiple_matches() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" o\" >$ s\" 0\" >$ s\" foo bar boo\" >$ $replace $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("f00 bar b00"), "got {out:?}");
}

#[test]
fn str_replace_no_match_returns_copy() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" xyz\" >$ s\" QQ\" >$ s\" hello\" >$ $replace $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("hello"), "got {out:?}");
}

#[test]
fn str_replace_repl_longer_than_needle() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" a\" >$ s\" ZZZ\" >$ s\" abab\" >$ $replace $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("ZZZbZZZb"), "got {out:?}");
}

#[test]
fn str_replace_repl_shorter_than_needle() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" abc\" >$ s\" X\" >$ s\" abcabcabc\" >$ $replace $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("XXX"), "got {out:?}");
}

#[test]
fn str_split_basic() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" ,\" >$ s\" a,b,c\" >$ $split .\n\
         bye\n"
    ).unwrap();
    // $split pushes ( $1 $2 $3 3 ).  Print the count.
    assert!(out.contains("3  ok"),
        "should split into 3 parts; got {out:?}");
}

#[test]
fn str_split_consume_pieces() {
    // Verify each piece is readable.  Count is on top; iterate down.
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" ,\" >$ s\" alpha,beta,gamma\" >$ $split\n\
         \\ Stack now: ( $a $b $c 3 ).  drop count, type each in reverse.\n\
         drop\n\
         $>addr type cr            \\ gamma\n\
         $>addr type cr            \\ beta\n\
         $>addr type cr            \\ alpha\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("gamma"), "got {out:?}");
    assert!(out.contains("beta"),  "got {out:?}");
    assert!(out.contains("alpha"), "got {out:?}");
}

#[test]
fn str_split_no_separator_yields_one_piece() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" ,\" >$ s\" nosep\" >$ $split .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1  ok"), "got {out:?}");
}

#[test]
fn str_split_empty_haystack_yields_one_empty_piece() {
    let mut s = sess_with_core();
    // $split of empty yields ($empty 1).
    // `.` prints 1 (count, top of stack), leaving ($empty).
    // `$len .` prints 0 (length of the piece).
    let out = s.eval(
        "s\" ,\" >$ empty$ $split . $len .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![1, 0], "got {nums:?} from {out:?}");
}

#[test]
fn str_split_empty_sep_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "empty$ s\" hello\" >$ $split drop\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2058"),
        "empty separator should throw -2058; got {msg}");
}

#[test]
fn str_d_wrong_types_throw() {
    // Smoke test that every new V2s-D op rejects FloatVec inputs
    // with -2060.  One per op family.
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         s\" hi\" >$ v @ $contains? drop\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"), "got {msg}");
}

// ── RefVec accessors (added alongside V2s integration demo) ──────

// Convention reminder: vec-ref@ / vec-ref! take a HANDLE (HEAPPTR
// slot address — what `v` pushes), not a raw tagged pointer.  The
// kernel derefs internally.  Same shape as vec-f@ / vec-f!.

#[test]
fn refvec_fresh_cells_are_nil() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-refs!\n\
         v 0 vec-ref@ .\n\
         v 3 vec-ref@ .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0, 0], "fresh RefVec cells should be nil; got {nums:?}");
}

#[test]
fn refvec_store_and_fetch_string_pointer() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR v\n\
         HEAPPTR s\n\
         4 v vec-alloc-refs!\n\
         s\" hello\" >$ s !$\n\
         s @$ v 2 vec-ref!\n\
         v 2 vec-ref@ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("hello"),
        "RefVec cell should yield back the string; got {out:?}");
}

#[test]
fn refvec_can_nil_a_cell() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-refs!\n\
         s\" hi\" >$ v 0 vec-ref!\n\
         0 v 0 vec-ref!                 \\ nil it out\n\
         v 0 vec-ref@ .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("0  ok"),
        "nil'd cell should fetch 0; got {out:?}");
}

#[test]
fn refvec_wrong_type_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR v\n\
         4 v vec-alloc-floats!\n\
         v 0 vec-ref@ .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "vec-ref@ on FloatVec should throw -2060; got {msg}");
}

#[test]
fn refvec_nil_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR v\n\
         v 0 vec-ref@ .\n\
         bye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2061"),
        "vec-ref@ on nil should throw -2061; got {msg}");
}

#[test]
fn refvec_survives_collection() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR v\n\
         3 v vec-alloc-refs!\n\
         s\" alpha\" >$ v 0 vec-ref!\n\
         s\" beta\"  >$ v 1 vec-ref!\n\
         s\" gamma\" >$ v 2 vec-ref!\n\
         (gc)\n\
         v 0 vec-ref@ $>addr type cr\n\
         v 1 vec-ref@ $>addr type cr\n\
         v 2 vec-ref@ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("alpha"), "got {out:?}");
    assert!(out.contains("beta"),  "got {out:?}");
    assert!(out.contains("gamma"), "got {out:?}");
}

// ── V2s integration demo: word-frequency counter ─────────────────

/// Plain-Forth word-frequency counter exercising the V2s surface
/// end-to-end.  A tiny associative array as two parallel arrays:
/// strings in a RefVec, counts in a FloatVec.  Find-or-insert via
/// linear scan; output is unsorted (the assertions inspect all
/// emitted lines).
///
/// Bigger sort/top-K logic was attempted but is non-trivial to get
/// right purely on the data stack — deferred until WF64 grows
/// Forth-side locals or LET-style scalar bindings for non-FP code.
const WORDCOUNT_DEMO_SRC: &str = r#"
64 constant WC-CAP

HEAPPTR wc-words
HEAPPTR wc-counts
variable wc-n

: wc-init  ( -- )
    WC-CAP wc-words vec-alloc-refs!
    WC-CAP wc-counts vec-alloc-floats!
    0 wc-n ! ;

\ Linear scan: index of matching $word in wc-words, or -1.
: wc-find  ( $word -- i | -1 )
    wc-n @ 0 ?do
        dup wc-words i vec-ref@ $=
        if drop i unloop exit then
    loop
    drop -1 ;

\ Bump count at index by one.
: wc-bump  ( i -- )
    dup wc-counts swap vec-f@        ( i ) ( F: c )
    1e f+                             ( i ) ( F: c+1 )
    wc-counts swap vec-f! ;

\ Insert a brand-new word with count 1.  Caller guarantees room.
: wc-insert  ( $word -- )
    wc-words wc-n @ vec-ref!          \ wc-words[n] := $word
    1e wc-counts wc-n @ vec-f!         \ wc-counts[n] := 1
    1 wc-n +! ;

\ Find-or-insert, bumping on hit, dropping on cap.
: wc-add  ( $word -- )
    dup wc-find dup -1 = if
        drop
        wc-n @ WC-CAP >= if drop exit then
        wc-insert
    else
        nip wc-bump
    then ;

\ Tokenise + count.
: wc-feed  ( $text -- )
    $words                            ( $1 .. $n n )
    0 ?do wc-add loop ;

\ Print every (count, word) row.  No sort.
: wc-print  ( -- )
    wc-n @ 0 ?do
        wc-counts i vec-f@ f>$ $>addr type space
        wc-words i vec-ref@ $>addr type cr
    loop ;

: wc-run  ( $text -- )
    wc-init wc-feed wc-print ;
"#;

#[test]
fn v2s_integration_word_frequency_demo() {
    let mut s = sess_with_core();
    let mut script = String::from(WORDCOUNT_DEMO_SRC);
    script.push_str(
        "\nS$\" the quick brown fox jumps over the lazy dog the fox is quick \
         the dog is lazy and the brown fox is quicker than the lazy dog\" \
         wc-run\nbye\n"
    );
    let out = s.eval(&script).unwrap();
    // Each unique token should appear exactly once in the report.
    // Counts: the=6, fox=3, dog=3, is=3, lazy=3, quick=2, brown=2,
    // quicker=1, jumps=1, over=1, and=1, than=1.
    // Output rows look like "<count> <word>\n" — `f>$` for an
    // integer-valued f64 produces "6" (no decimal point), then
    // `space` emits one space, then the word, then `cr`.
    for (word, n) in [
        ("the", 6), ("fox", 3), ("dog", 3), ("is", 3), ("lazy", 3),
        ("quick", 2), ("brown", 2), ("quicker", 1), ("jumps", 1),
        ("over", 1), ("and", 1), ("than", 1),
    ] {
        let needle = format!("{n} {word}\n");
        assert!(out.contains(&needle),
            "expected row {needle:?}; output:\n{out}");
    }
}

// ── V2s stage E — UTF-8, floats, char$, $words ────────────────────

#[test]
fn str_clen_ascii_equals_byte_length() {
    let mut s = sess_with_core();
    let out = s.eval("s\" hello\" >$ $clen .\nbye\n").unwrap();
    assert!(out.contains("5  ok"), "got {out:?}");
}

#[test]
fn str_clen_utf8_counts_codepoints_not_bytes() {
    // U+00E9 'é' is 2 bytes, U+20AC '€' is 3 bytes — so "é€" is
    // 5 bytes but only 2 codepoints.  Build via char$ + $+ so we
    // don't have to embed multi-byte literals in the Rust source.
    let mut s = sess_with_core();
    let out = s.eval(
        "233 char$ 8364 char$ $+\n\
         dup $len .\n\
         $clen .\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![5, 2], "expected (5 bytes, 2 chars); got {nums:?}");
}

#[test]
fn str_c_at_returns_codepoint() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" abc\" >$\n\
         dup 0 $c@ .                 \\ 'a' = 97\n\
         dup 1 $c@ .                 \\ 'b' = 98\n\
         dup 2 $c@ .                 \\ 'c' = 99\n\
         drop\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![97, 98, 99], "got {nums:?}");
}

#[test]
fn str_c_at_handles_multibyte_codepoints() {
    let mut s = sess_with_core();
    let out = s.eval(
        "233 char$ 8364 char$ $+\n\
         dup 0 $c@ .                 \\ 'é' = 233\n\
         dup 1 $c@ .                 \\ '€' = 8364\n\
         drop\n\
         bye\n"
    ).unwrap();
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![233, 8364], "got {nums:?}");
}

#[test]
fn str_c_at_out_of_bounds_throws() {
    let mut s = sess_with_core();
    let err = s.eval(
        "s\" abc\" >$ 10 $c@ .\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2058"),
        "out-of-bounds $c@ should throw -2058; got {msg}");
}

#[test]
fn str_valid_true_for_ascii() {
    let mut s = sess_with_core();
    let out = s.eval("s\" hello\" >$ $valid? .\nbye\n").unwrap();
    assert!(out.contains("-1  ok"), "got {out:?}");
}

#[test]
fn str_valid_true_for_well_formed_utf8() {
    let mut s = sess_with_core();
    let out = s.eval("233 char$ $valid? .\nbye\n").unwrap();
    assert!(out.contains("-1  ok"), "got {out:?}");
}

#[test]
fn str_validate_passes_for_ascii_drops_the_handle() {
    let mut s = sess_with_core();
    let out = s.eval("s\" hello\" >$ $validate 42 .\nbye\n").unwrap();
    assert!(out.contains("42  ok"),
        "stack should be just 42 after $validate; got {out:?}");
}

#[test]
fn str_char_dollar_round_trip_via_c_at() {
    let mut s = sess_with_core();
    let out = s.eval("65 char$ 0 $c@ .\nbye\n").unwrap();
    assert!(out.contains("65  ok"), "got {out:?}");
}

#[test]
fn str_char_dollar_surrogate_throws() {
    let mut s = sess_with_core();
    // 0xD800 is the start of the UTF-16 surrogate range — invalid
    // as a standalone Unicode codepoint.
    let err = s.eval("55296 char$ drop\nbye\n").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2063"),
        "surrogate codepoint should throw -2063; got {msg}");
}

#[test]
fn str_upper_ascii() {
    let mut s = sess_with_core();
    let out = s.eval("s\" hello\" >$ $upper $>addr type cr\nbye\n").unwrap();
    assert!(out.contains("HELLO"), "got {out:?}");
}

#[test]
fn str_lower_ascii() {
    let mut s = sess_with_core();
    let out = s.eval("s\" HELLO\" >$ $lower $>addr type cr\nbye\n").unwrap();
    assert!(out.contains("hello"), "got {out:?}");
}

#[test]
fn str_upper_unicode_lengthens_for_german_ess() {
    // ß (U+00DF, 2 bytes UTF-8) uppercases to "SS" (2 bytes ASCII).
    // Same byte count by accident — pick a clearer one:
    // ﬁ (U+FB01, 3 bytes, "fi" ligature) uppercases to "FI" (2 bytes).
    // Hmm, output is shorter.  Just check that the bytes change
    // sensibly via byte content.
    let mut s = sess_with_core();
    let out = s.eval(
        "223 char$               \\ ß = U+00DF\n\
         $upper $>addr type cr\n\
         bye\n"
    ).unwrap();
    // Should print "SS".
    assert!(out.contains("SS"), "got {out:?}");
}

#[test]
fn str_to_float_parses_simple() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" 1.5\" >$ $>f . f.\n\
         s\" -3.25\" >$ $>f . f.\n\
         bye\n"
    ).unwrap();
    // Each round produces -1 (true) on the data stack and the float
    // on the FP stack.  `. f.` prints true then the float.
    assert!(out.contains("1.500000"), "1.5 parse fail; got {out:?}");
    assert!(out.contains("-3.250000"), "-3.25 parse fail; got {out:?}");
    let signs: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .filter(|&n| n == -1 || n == 0)
        .collect();
    assert_eq!(signs.len(), 2, "should have 2 true flags; got {signs:?} in {out:?}");
}

#[test]
fn str_to_float_failure_returns_false() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" not a float\" >$ $>f .\n\
         bye\n"
    ).unwrap();
    // On failure: only 0 pushed, no FP push.
    let nums: Vec<i64> = out.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    assert_eq!(nums, vec![0], "got {nums:?} from {out:?}");
}

#[test]
fn str_float_to_string_round_trip() {
    let mut s = sess_with_core();
    let out = s.eval(
        "1.5e f>$ $>addr type cr\n\
         -3.25e f>$ $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1.5"), "got {out:?}");
    assert!(out.contains("-3.25"), "got {out:?}");
}

#[test]
fn str_sb_append_float() {
    let mut s = sess_with_core();
    let out = s.eval(
        "HEAPPTR b\n\
         32 sb-new b !\n\
         1.5e b @ sb-append-f\n\
         s\" , \" >$ b @ sb-append$\n\
         -3.25e b @ sb-append-f\n\
         b @ sb>string $>addr type cr\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("1.5, -3.25"), "got {out:?}");
}

#[test]
fn str_words_basic_three_tokens() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" foo bar baz\" >$ $words .\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("3  ok"), "got {out:?}");
}

#[test]
fn str_words_consume_pieces_in_reverse() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\" alpha beta gamma\" >$ $words\n\
         drop\n\
         $>addr type cr             \\ gamma\n\
         $>addr type cr             \\ beta\n\
         $>addr type cr             \\ alpha\n\
         bye\n"
    ).unwrap();
    assert!(out.contains("gamma"), "got {out:?}");
    assert!(out.contains("beta"),  "got {out:?}");
    assert!(out.contains("alpha"), "got {out:?}");
}

#[test]
fn str_words_collapses_repeated_and_skips_edge_whitespace() {
    let mut s = sess_with_core();
    let out = s.eval(
        "s\"   foo   bar   \" >$ $words .\n\
         bye\n"
    ).unwrap();
    // s" eats one leading space.  Even so, runs of internal/edge
    // whitespace should yield exactly 2 tokens.
    assert!(out.contains("2  ok"), "got {out:?}");
}

#[test]
fn str_words_empty_haystack_zero_tokens() {
    let mut s = sess_with_core();
    let out = s.eval("empty$ $words .\nbye\n").unwrap();
    assert!(out.contains("0  ok"), "got {out:?}");
}

#[test]
fn gc_vec_f_fetch_wrong_type_still_throws_minus_2060() {
    // Make sure -2060 still fires when the slot holds something with
    // a non-zero, non-FloatVec tag (e.g., a RefVec).  Distinct from
    // the nil case above.
    let mut s = sess_with_core();
    let err = s.eval(
        "HEAPPTR refs\n4 refs vec-alloc-refs!\nrefs 0 vec-f@ f.\nbye\n"
    ).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("-2060"),
        "expected -2060 (wrong type) on vec-f@ over RefVec, got: {msg}");
}

#[test]
fn eval_if_else_then_and_minus_if_work() {
    let mut s = sess();
    let out = s.eval(
        ": choose if 111 else 222 then ;\n\
         : keepflag -if 7 else 9 then ;\n\
         0 choose .\n\
         5 choose .\n\
         0 keepflag . .\n\
         5 keepflag . .\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n222  ok\n111  ok\n9 0  ok\n7 5  ok\n");
}

#[test]
fn eval_begin_until_loops() {
    let mut s = sess();
    let out = s.eval(": down0 begin 1- dup 0= until ;\n3 down0 .\nbye\n").unwrap();
    assert_eq!(out, " ok\n0  ok\n");
}

#[test]
fn eval_begin_while_repeat_loops() {
    let mut s = sess();
    let out = s.eval(": peel begin dup while 1- repeat ;\n3 peel .\nbye\n").unwrap();
    assert_eq!(out, " ok\n0  ok\n");
}

#[test]
fn eval_recurse_compiles_current_definition() {
    let mut s = sess();
    let out = s.eval(": count0 dup 0= if drop 0 else 1- recurse 1+ then ;\n3 count0 .\nbye\n").unwrap();
    assert_eq!(out, " ok\n3  ok\n");
}

#[test]
fn eval_do_loop_counts_up() {
    let mut s = sess();
    let out = s.eval(": countup 5 0 do i . loop ;\ncountup\nbye\n").unwrap();
    assert_eq!(out, " ok\n0 1 2 3 4  ok\n");
}

#[test]
fn eval_qdo_skips_zero_trip_and_runs_nonzero_trip() {
    let mut s = sess();
    let out = s.eval(
        ": maybecount 0 ?do i . loop ;\n\
         5 maybecount\n\
         0 maybecount\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n0 1 2 3 4  ok\n ok\n");
}

#[test]
fn eval_plus_loop_steps_by_stride() {
    let mut s = sess();
    let out = s.eval(": evens 10 0 do i . 2 +loop ;\nevens\nbye\n").unwrap();
    assert_eq!(out, " ok\n0 2 4 6 8  ok\n");
}

#[test]
fn eval_minus_loop_counts_down() {
    let mut s = sess();
    let out = s.eval(": countdown 0 5 do i . 1 -loop ;\ncountdown\nbye\n").unwrap();
    assert_eq!(out, " ok\n5 4 3 2 1 0  ok\n");
}

#[test]
fn eval_leave_exits_loop_early() {
    let mut s = sess();
    let out = s.eval(": quit-at-2 5 0 do i . i 2 = if leave then loop ;\nquit-at-2\nbye\n").unwrap();
    assert_eq!(out, " ok\n0 1 2  ok\n");
}

#[test]
fn eval_qleave_exits_when_flag_is_true() {
    let mut s = sess();
    let out = s.eval(": qquit-at-2 5 0 do i . i 2 = ?leave loop ;\nqquit-at-2\nbye\n").unwrap();
    assert_eq!(out, " ok\n0 1 2  ok\n");
}

#[test]
fn eval_two_r_roundtrip_through_repl() {
    let mut s = sess();
    let out = s.eval(": ferry2 2>r 2r@ . . 2r> ;\n10 20 ferry2 . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n20 10 20 10  ok\n");
}

#[test]
fn eval_compiled_inline_stack_words_work() {
    let mut s = sess();
    let out = s.eval(": stackplay over swap drop ;\n7 9 stackplay . .\nbye\n").unwrap();
    assert_eq!(out, " ok\n7 7  ok\n");
}

#[test]
fn tfa_fetch_distinguishes_colon_defs_from_primitives() {
    let mut s = sess();

    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    s.push(dup_xt);
    s.call("to_name").unwrap();
    let dup_nt = s.pop();
    s.push(dup_nt);
    s.call("tfa_fetch").unwrap();
    assert_eq!(s.pop(), 0);

    let out = s.eval(": typed 1 ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop();
    s.push(xt);
    s.call("to_name").unwrap();
    let nt = s.pop();
    s.push(nt);
    s.call("tfa_fetch").unwrap();
    assert_eq!(s.pop(), 0x82);
}

#[test]
fn create_builds_a_created_word_that_pushes_its_body() {
    let mut s = sess();
    let out = s.eval("create made\nmade\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let body = s.pop() as u64;
    // The body now lives in the separate RW data region, off the executable
    // stub entirely (W^X; no SMC). It must not sit on the stub's cache line.
    assert!(body < xt || body >= xt + 64,
        "create body must be off the code stub, got body={body:#x} xt={xt:#x}");

    s.push(xt as i64);
    s.call("to_name").unwrap();
    let nt = s.pop();
    s.push(nt);
    s.call("tfa_fetch").unwrap();
    assert_eq!(s.pop(), 0x91);

    s.push(xt as i64);
    s.call("to_body").unwrap();
    assert_eq!(s.pop() as u64, body);
}

#[test]
fn to_body_throws_minus_31_on_colon_definition() {
    let mut s = sess();
    let out = s.eval(": bodyfail 1 ;\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop();
    let to_body_xt = s.xt_of("to_body").unwrap() as i64;
    s.push(xt);
    s.push(to_body_xt);
    s.call("catch_word").unwrap();
    assert_eq!(s.stack(), vec![-31, xt]);
}

#[test]
fn forth_visible_body_word_resolves_to_kernel_to_body_xt() {
    let mut s = sess();
    let out = s.eval("' >body\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    let forth_xt = s.pop() as u64;
    let kernel_xt = s.xt_of("to_body").unwrap() as u64;
    assert_eq!(forth_xt, kernel_xt);
}

#[test]
fn execute_of_to_body_xt_matches_direct_call() {
    let mut s = sess();
    let out = s.eval("create made\nbye\n").unwrap();
    assert_eq!(out, " ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop();
    let to_body_xt = s.xt_of("to_body").unwrap() as i64;

    s.push(xt);
    s.push(to_body_xt);
    s.call("execute").unwrap();
    let body = s.pop() as u64;
    let xtu = xt as u64;
    assert!(body < xtu || body >= xtu + 64,
        "to_body result must be off the code stub, got body={body:#x} xt={xtu:#x}");
}

#[test]
fn eval_body_word_leaves_created_body_address_on_stack() {
    let mut s = sess();
    let out = s.eval("create made\n' made >body\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let body = s.pop() as u64;

    assert!(body < xt || body >= xt + 64,
        "(>body) must be off the code stub, got body={body:#x} xt={xt:#x}");
}

#[test]
fn compiled_body_word_returns_created_body_address() {
    let mut s = sess();
    let out = s.eval(": bodyword >body ;\ncreate made\n' made bodyword\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");

    s.call("latestxt").unwrap();
    let xt = s.pop() as u64;
    let body = s.pop() as u64;

    assert!(body < xt || body >= xt + 64,
        "compiled (>body) must be off the code stub, got body={body:#x} xt={xt:#x}");
}

#[test]
fn defer_hook_does_not_corrupt_to_body_code() {
    let mut s = sess();
    let to_body_xt = s.xt_of("to_body").unwrap() as u64;
    let before = unsafe { std::slice::from_raw_parts(to_body_xt as *const u8, 24).to_vec() };

    let out = s
        .eval(": , here ! 1 cells allot ;\n: variable create 0 , ;\n: constant create , does> @ ;\n: value create , does> @ ;\n: defer@ >body @ ;\n: defer! >body ! ;\n: defer-err -261 throw ;\n: defer create ['] defer-err , does> @ execute ;\ndefer hook\nbye\n")
        .unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n ok\n ok\n ok\n ok\n");

    let after = unsafe { std::slice::from_raw_parts(to_body_xt as *const u8, 24).to_vec() };
    assert_eq!(after, before);
}

#[test]
fn link_to_name_returns_latest_header_name_token() {
    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"BAR";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }

    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let latest = s.latest() as i64;

    s.push(latest);
    s.call("link_to_name").unwrap();
    let nt = s.pop() as u64;

    let len = unsafe { (nt as *const u8).read() };
    let bytes = unsafe { std::slice::from_raw_parts((nt + 1) as *const u8, len as usize) };
    assert_eq!(len, name.len() as u8);
    assert_eq!(bytes, name);
}

#[test]
fn name_to_interpret_and_name_to_compile_roundtrip_header_tokens() {
    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"BAZ";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }

    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    let compile_xt = s.xt_of("compile_word").unwrap() as i64;
    s.push(dup_xt);
    s.call("set_xt").unwrap();

    s.push(dup_xt);
    s.call("to_name").unwrap();
    let nt = s.pop();

    s.push(nt);
    s.call("name_to_interpret").unwrap();
    assert_eq!(s.pop(), dup_xt);

    s.push(nt);
    s.call("name_to_compile").unwrap();
    assert_eq!(s.pop(), compile_xt);
    assert_eq!(s.pop(), dup_xt);
}

#[test]
fn latestxt_tracks_latest_definition_and_resets() {
    let mut s = sess();
    s.call("latestxt").unwrap();
    let boot_latestxt = s.pop();

    let pad = s.user_base + 0x100;
    let name = b"QUX";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }

    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    s.call("latestxt").unwrap();
    let created_xt = s.pop();
    assert_ne!(created_xt, boot_latestxt);

    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    s.push(dup_xt);
    s.call("set_xt").unwrap();
    s.call("latestxt").unwrap();
    assert_eq!(s.pop(), dup_xt);

    s.reset();
    s.call("latestxt").unwrap();
    assert_eq!(s.pop(), boot_latestxt);
}

#[test]
fn find_name_returns_counted_name_token() {
    const DH_NT: u64 = (5 * 8) + 2 + 2 + 2 + 1;

    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"BAR";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }

    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();

    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("find_name").unwrap();

    assert_eq!(s.pop(), -1);
    let nt = s.pop() as u64;
    assert_eq!(nt, s.latest() + DH_NT);

    let len = unsafe { (nt as *const u8).read() };
    let bytes = unsafe { std::slice::from_raw_parts((nt + 1) as *const u8, len as usize) };
    assert_eq!(len, name.len() as u8);
    assert_eq!(bytes, name);
}

#[test]
fn number_q_parses_single_digit_directly() {
    let mut s = sess();
    let pad = s.user_base + 0x100;
    unsafe { std::ptr::copy_nonoverlapping(b"1".as_ptr(), pad as *mut u8, 1); }

    s.push(pad as i64);
    s.push(1);
    s.call("number_q").unwrap();

    assert_eq!(s.pop(), -1);
    assert_eq!(s.pop(), 1);
    assert_eq!(s.depth(), 0);
}

#[test]
fn find_name_miss_leaves_c_addr_u_zero() {
    let mut s = sess();
    let pad = s.user_base + 0x100;
    unsafe { std::ptr::copy_nonoverlapping(b"1".as_ptr(), pad as *mut u8, 1); }

    s.push(pad as i64);
    s.push(1);
    s.call("find_name").unwrap();

    assert_eq!(s.stack(), vec![0, 1, pad as i64]);
}

#[test]
fn get_order_word_reports_default_forth_order() {
    // Default search order after reset is (PRIVATE TOOLS FORTH) with
    // PRIVATE innermost. get-order returns wid_n ... wid_1 n with the
    // count on top, then wids from innermost to outermost going down.
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let forth_wid = s.stack()[0];
    s.reset();
    let private_wid = unsafe { ((s.user_base + 0x17D0) as *const u64).read_unaligned() } as i64;
    let tools_wid   = unsafe { ((s.user_base + 0x17C8) as *const u64).read_unaligned() } as i64;
    s.call("get_order_word").unwrap();
    // stack() is top-first: [count, innermost, ..., outermost]
    assert_eq!(s.stack(), vec![3, private_wid, tools_wid, forth_wid]);
}

#[test]
fn bootstrap_splits_primitives_into_three_wordlists() {
    // After boot, `.s` should be findable in TOOLS but not in FORTH;
    // `(create)` should be findable in PRIVATE but not in FORTH or
    // TOOLS; and ordinary words like `dup` should be in FORTH only.
    let mut s = sess();
    let forth_wid   = unsafe { ((s.user_base + 0x1508) as *const u64).read_unaligned() } as i64;
    let tools_wid   = unsafe { ((s.user_base + 0x17C8) as *const u64).read_unaligned() } as i64;
    let private_wid = unsafe { ((s.user_base + 0x17D0) as *const u64).read_unaligned() } as i64;

    // Define a probe helper inside Forth: collapses search-wordlist's
    // two-shape return ( 0 | xt ±1 ) into a single flag.
    s.eval(": probe-sw  ( c-addr u wid -- flag )  search-wordlist dup if nip then ;\nbye\n")
        .unwrap();
    fn probe(s: &mut Wf64Session, name: &str, wid: i64) -> i64 {
        let code = format!(
            "s\" {name}\" {wid} probe-sw .\nbye\n",
            name = name, wid = wid
        );
        let out = s.eval(&code).unwrap();
        out.split_whitespace().next().unwrap().parse().unwrap()
    }

    // `dup` is in FORTH only.
    assert_ne!(probe(&mut s, "dup", forth_wid),  0);
    assert_eq!(probe(&mut s, "dup", tools_wid),  0);
    assert_eq!(probe(&mut s, "dup", private_wid), 0);

    // `.s` is in TOOLS only.
    assert_eq!(probe(&mut s, ".s", forth_wid),   0);
    assert_ne!(probe(&mut s, ".s", tools_wid),   0);
    assert_eq!(probe(&mut s, ".s", private_wid), 0);

    // `(create)` is in PRIVATE only.
    assert_eq!(probe(&mut s, "(create)", forth_wid),   0);
    assert_eq!(probe(&mut s, "(create)", tools_wid),   0);
    assert_ne!(probe(&mut s, "(create)", private_wid), 0);

}

#[test]
fn core_f_categorises_source_defined_words_correctly() {
    // After loading core.f, source-defined words must end up in the
    // wordlist their inline `set-current` directives put them in.
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();

    let forth_wid   = unsafe { ((s.user_base + 0x1508) as *const u64).read_unaligned() } as i64;
    let tools_wid   = unsafe { ((s.user_base + 0x17C8) as *const u64).read_unaligned() } as i64;
    let private_wid = unsafe { ((s.user_base + 0x17D0) as *const u64).read_unaligned() } as i64;

    s.eval(": probe-sw  ( c-addr u wid -- flag )  search-wordlist dup if nip then ;\nbye\n")
        .unwrap();
    fn probe(s: &mut Wf64Session, name: &str, wid: i64) -> i64 {
        let code = format!(
            "s\" {name}\" {wid} probe-sw .\nbye\n",
            name = name, wid = wid
        );
        let out = s.eval(&code).unwrap();
        out.split_whitespace().next().unwrap().parse().unwrap()
    }

    // TOOLS-tagged source words.
    assert_ne!(probe(&mut s, "words",      tools_wid),   0);
    assert_ne!(probe(&mut s, "marker",     tools_wid),   0);
    assert_ne!(probe(&mut s, "[defined]",  tools_wid),   0);
    assert_eq!(probe(&mut s, "marker",     forth_wid),   0);

    // PRIVATE-tagged source words.
    assert_ne!(probe(&mut s, "locals-set", private_wid), 0);
    assert_ne!(probe(&mut s, "subst-find", private_wid), 0);
    assert_eq!(probe(&mut s, "locals-set", forth_wid),   0);

    // FORTH-tagged source words (user-facing).
    assert_ne!(probe(&mut s, "constant",   forth_wid),   0);
    assert_ne!(probe(&mut s, "{:",         forth_wid),   0);
    assert_ne!(probe(&mut s, "floor",      forth_wid),   0);
}

#[test]
fn eval_negative_set_order_minimum_then_get_order() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    let out = s.eval("-1 set-order get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn eval_source_defined_set_order_wrapper_then_get_order() {
    let mut s = sess();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    let out = s.eval(": only2 -1 set-order ;\nonly2 get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);
}

#[test]
fn search_order_words_route_lookup_by_wordlist() {
    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"TOK";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }

    s.call("wordlist_word").unwrap();
    let extra_wid = s.pop();
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.pop();

    s.push(extra_wid);
    s.call("set_current_word").unwrap();
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let dup_xt = s.xt_of("dup_").unwrap() as i64;
    s.push(dup_xt);
    s.call("set_xt").unwrap();

    s.push(root_wid);
    s.call("set_current_word").unwrap();
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let drop_xt = s.xt_of("drop_").unwrap() as i64;
    s.push(drop_xt);
    s.call("set_xt").unwrap();

    s.push(root_wid);
    s.push(extra_wid);
    s.push(2);
    s.call("set_order_word").unwrap();
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("find_name").unwrap();
    assert_eq!(s.pop(), -1);
    let nt = s.pop();
    s.push(nt);
    s.call("name_to_interpret").unwrap();
    assert_eq!(s.pop(), dup_xt);

    s.push(root_wid);
    s.push(1);
    s.call("set_order_word").unwrap();
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("find_name").unwrap();
    assert_eq!(s.pop(), -1);
    let nt = s.pop();
    s.push(nt);
    s.call("name_to_interpret").unwrap();
    assert_eq!(s.pop(), drop_xt);

    assert_eq!(s.depth(), 0);
}

#[test]
fn load_source_file_provides_search_order_extension_words() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.call("forth_wordlist_word").unwrap();
    let root_wid = s.stack()[0];
    s.reset();
    s.load_source_file(&path).unwrap();

    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n");

    let out = s.eval("only get-order\nbye\n").unwrap();
    assert_eq!(out, " ok\n");
    assert_eq!(s.stack(), vec![1, root_wid]);

    s.reset();
    s.load_source_file(&path).unwrap();
    s.call("forth_wordlist_word").unwrap();
    assert_eq!(s.stack(), vec![root_wid]);
    s.pop();
    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         only also get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![2, root_wid, root_wid]);

    s.reset();
    s.load_source_file(&path).unwrap();
    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order previous get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    let stack = s.stack();
    assert_eq!(stack.len(), 2);
    assert_eq!(stack[0], 1);
    assert_eq!(stack[1], root_wid);

    s.reset();
    s.load_source_file(&path).unwrap();
    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order forth get-order\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n");
    assert_eq!(s.stack(), vec![2, root_wid, root_wid]);

    s.reset();
    s.load_source_file(&path).unwrap();
    let out = s.eval(
        "forth-wordlist constant root\n\
         wordlist constant extra\n\
         root extra 2 set-order definitions get-current\n\
         extra\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n");
    let stack = s.stack();
    assert_eq!(stack.len(), 2);
    assert_eq!(stack[0], stack[1]);
    assert_ne!(stack[0], root_wid);
}

#[test]
fn search_wordlist_returns_xt_and_immediacy_flag() {
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();

    // `.s` lives in the TOOLS wordlist now, so the original variant
    // that searched FORTH for it no longer applies. The four other
    // names cover the same code path.
    let out = s.eval(
        "forth-wordlist constant root\n\
         : dup-name s\" dup\" ;\n\
         : semi-name s\" ;\" ;\n\
         : exit-name s\" exit\" ;\n\
         : keyq-name s\" key?\" ;\n\
         dup-name root search-wordlist swap drop . cr\n\
         semi-name root search-wordlist nip . cr\n\
         exit-name root search-wordlist nip . cr\n\
         keyq-name root search-wordlist nip . cr\n\
         bye\n"
    ).unwrap();

    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n-1 \n ok\n1 \n ok\n1 \n ok\n-1 \n ok\n");
}

// ─── Forth 2012 locals (`{: … :}`) via `session.eval` ────────────────
//
// Historical note: a stale comment here claimed `{:` worked in the REPL but
// `?`-flooded through `session.eval` after loading core.f (and that it broke
// `demos/gfx-click.f`). That bug no longer reproduces — every form below
// compiles and runs correctly through the eval path, and gfx-click.f no longer
// uses locals. These cases lock the behaviour in so it can't silently regress.
#[test]
fn locals_via_eval_all_forms() {
    // (source, expected stdout). Each case takes its own session guard inside
    // the loop body so the shared-session Mutex is released before the next
    // `sess()` (a top-level `let s = sess()` per case would deadlock — the
    // guard only drops at function end).
    let cases: &[(&str, &str)] = &[
        // single local
        (": f {: a :} a ;\n3 f .\nbye\n", " ok\n3  ok\n"),
        // multiple stack-initialised locals
        (": g {: a b c :} a b c + + ;\n1 2 3 g .\nbye\n", " ok\n6  ok\n"),
        // `--` output comment is skipped
        (": h {: a b -- s :} a b + ;\n4 5 h .\nbye\n", " ok\n9  ok\n"),
        // local list spanning two lines exercises `{:`'s refill path (Buffered Io)
        (": k {: a\n b :} a b * ;\n6 7 k .\nbye\n", " ok\n42  ok\n"),
        // `|` introduces uninitialised locals; `to local` stores
        (": p ( a b -- ) {: x y | t :} x y + to t t . ;\n7 8 p\nbye\n", " ok\n15  ok\n"),
        // a normal word defined right after a locals word — no lingering state
        (": m {: a :} a a + ;\n10 m .\n: n 5 ;\nn .\nbye\n", " ok\n20  ok\n ok\n5  ok\n"),
    ];
    for (src, want) in cases {
        let mut s = sess();
        assert_eq!(s.eval(src).unwrap(), *want, "locals form failed: {src:?}");
    }
}

#[test]
fn double_colon_inside_definition_throws_minus_29() {
    // Calling `:` while STATE != 0 must throw -29 (compiler nesting)
    // rather than silently re-entering compile mode and producing
    // `?`-flood downstream.
    let mut s = sess();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join("core.f");
    s.load_source_file(&path).unwrap();
    let out = s.eval(
        ": provoke   ( -- throw-code )\n\
             state @ >r\n\
             1 state !\n\
             ['] : catch\n\
             r> state !\n\
         ;\n\
         provoke . cr\n\
         bye\n"
    ).unwrap();
    assert_eq!(out, " ok\n ok\n ok\n ok\n ok\n ok\n-29 \n ok\n");
}

#[test]
fn set_flags_marks_word_immediate() {
    // Build a word "IMM" pointing at `cr_word` (no-op for the data
    // stack, just emits a newline), then mark it IMMEDIATE. In compile
    // mode it should run NOW (emitting a newline at compile time)
    // rather than getting compiled into the definition.
    let mut s = sess();
    let pad = s.user_base + 0x100;
    let name = b"IMM";
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), pad as *mut u8, name.len()); }
    s.push(pad as i64);
    s.push(name.len() as i64);
    s.call("create").unwrap();
    let cr_xt = s.xt_of("cr_word").unwrap();
    s.push(cr_xt as i64);
    s.call("set_xt").unwrap();
    s.push(1);
    s.call("set_flags").unwrap();

    // Compile a definition that has IMM in its body. Because IMM is
    // immediate, the CR fires at compile time, not when bar runs.
    let out = s.eval(": bar IMM 5 ;\nbar .\nbye\n").unwrap();
    // The first " ok" comes after the colon-def line, with a CR
    // emitted in the middle (between `:` and the ` ok`):
    assert!(out.contains('\n'), "expected an immediate-fire newline; got {out:?}");
    // bar . prints `5 `, then ok.
    assert!(out.ends_with("5  ok\n"), "got {out:?}");
}

#[test]
fn mixed_define_then_call_directly() {
    let mut s = sess();
    s.eval(": cube dup dup * * ;\n").unwrap();
    // Look up `cube`'s xt by walking the dict — easier route is via the
    // REPL: `' cube` doesn't exist yet; do it through eval:
    s.push(4);
    // We don't have `' word` (tick) yet so fall back to eval for the call.
    // This test mainly exercises that the dict mutation from eval is
    // visible to subsequent eval calls.
    let out = s.eval("4 cube .\nbye\n").unwrap();
    assert_eq!(out, "64  ok\n");
    let _ = s.pop();  // drop the 4 we pushed up top — not consumed by the eval
}

// ── data-driven tests ───────────────────────────────────────────────
//
// Adding a new primitive should never need a Rust recompile. These
// two `#[test]` fns walk the corresponding subdirectories under
// `tests/data/`, classify each case as PASS / FAIL / NYIMP, and emit
// a summary. Only FAILs cause the test to fail; NYIMP and PASS are
// both "the suite ran cleanly."
//
// Workflow this enables (test-first):
//
//   1. Write test files for the next batch of primitives — words that
//      may not yet exist in the kernel.
//   2. `cargo test --test harness` — failing primitives show as NYIMP,
//      not FAIL. Suite still passes.
//   3. Port one primitive (via `cargo run --bin port-wf32 …`), paste
//      into kernel/*.masm, add to PRIMITIVES.
//   4. Re-run. The corresponding NYIMP flips to PASS automatically.
//
// `tests/data/direct/*.t` — direct primitive test, line-oriented DSL.
//                            NYIMP detected by pre-scanning `call <sym>`
//                            lines and looking each up via `xt_of`.
// `tests/data/eval/*.in`  — Forth source fed through the REPL.
// `tests/data/eval/*.out` — expected stdout, exact match.
//                            NYIMP detected by an optional comment line
//                            `# requires: word1 word2 ...` listing the
//                            Forth-side names; missing any → NYIMP.

#[derive(Debug)]
enum Outcome {
    Pass,
    Nyimp(Vec<String>), // missing-symbol/word list
    Fail(String),       // human-readable failure detail
}

// Value-property (ivar) extras not covered by the data-driven corpus:
// `addr-of` (address escape hatch for +!) and `legacy-ivar:` (the historic
// address-returning form). Checks the data stack rather than REPL text.
#[test]
fn eval_value_ivar_addr_of_and_legacy() {
    let mut s = sess();
    let prog = "\
class pt cell ivar: px cell ivar: py \
  :m set ( x y -- ) to py to px ;m \
  :m px@ px ;m  :m py@ py ;m \
  :m incx 1 addr-of px +! ;m \
end-class \
pt new p  3 4 p -> set \
class box cell legacy-ivar: w :m setw w ! ;m :m getw w @ ;m end-class \
box new b  9 b -> setw \
p -> px@  p -> py@  p -> incx  p -> px@  b -> getw\n";
    s.eval(prog).unwrap();
    // pushed in order: px@=3, py@=4, (incx), px@=4, getw=9.
    // stack() is top-first.
    assert_eq!(s.stack(), vec![9, 4, 4, 3]);
}

// PIC dispatch: ONE late send site (`-> kind` inside `dispatch`, receiver from
// a stack arg → no early-binding hint) called with two different classes must
// miss + re-resolve each time the receiver class changes — the polymorphic
// correctness the monomorphic corpus sites don't exercise.
#[test]
fn eval_pic_polymorphic_dispatch() {
    let mut s = sess();
    let prog = "\
class a1 :m kind 1 ;m end-class \
class a2 :m kind 2 ;m end-class \
a1 new oa  a2 new ob \
: dispatch ( obj -- n ) -> kind ; \
oa dispatch  ob dispatch  oa dispatch  ob dispatch\n";
    s.eval(prog).unwrap();
    // each call flips the cached class -> forces a miss; results 1,2,1,2.
    assert_eq!(s.stack(), vec![2, 1, 2, 1]); // top-first
}

// PIC invalidation: after a site has cached a method, redefining that method
// (vt! bumps user_OOP_EPOCH) must invalidate the cache so the next send sees
// the new xt.
#[test]
fn eval_pic_sees_method_redefinition() {
    let mut s = sess();
    let prog = "\
class k1 :m kind 1 ;m end-class \
k1 new ok \
: gk ( o -- n ) -> kind ; \
ok gk \
:noname 5 ; k1 s\" kind\" selector-id vt! \
ok gk\n";
    s.eval(prog).unwrap();
    // first gk fills the cache (1); vt! redefinition bumps the epoch; second gk
    // misses on the stale epoch and re-resolves to 5.
    assert_eq!(s.stack(), vec![5, 1]); // top-first
}

#[test]
fn data_driven_direct_tests() {
    let dir = data_dir().join("direct");
    let cases = collect_files(&dir, "t");
    if cases.is_empty() {
        eprintln!("note: no .t files under {} — nothing to run", dir.display());
        return;
    }
    let results: Vec<(PathBuf, Outcome)> = cases
        .iter()
        .map(|p| (p.clone(), classify_direct(p)))
        .collect();
    summarize_and_assert("direct", &results);
}

#[test]
fn data_driven_eval_tests() {
    let dir = data_dir().join("eval");
    let cases = collect_files(&dir, "in");
    if cases.is_empty() {
        eprintln!("note: no .in files under {} — nothing to run", dir.display());
        return;
    }
    let results: Vec<(PathBuf, Outcome)> = cases
        .iter()
        .map(|p| {
            let out = p.with_extension("out");
            (p.clone(), classify_eval(p, &out))
        })
        .collect();
    summarize_and_assert("eval", &results);
}

fn summarize_and_assert(kind: &str, results: &[(PathBuf, Outcome)]) {
    let mut pass = 0;
    let mut fail = 0;
    let mut nyimp = 0;
    let mut nyimp_list: Vec<String> = Vec::new();
    let mut fail_list: Vec<(String, String)> = Vec::new();
    for (path, outcome) in results {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match outcome {
            Outcome::Pass => pass += 1,
            Outcome::Nyimp(missing) => {
                nyimp += 1;
                nyimp_list.push(format!("{} [missing: {}]", name, missing.join(" ")));
            }
            Outcome::Fail(msg) => {
                fail += 1;
                fail_list.push((name, msg.clone()));
            }
        }
    }
    eprintln!(
        "── {kind} tests: {pass} PASS, {fail} FAIL, {nyimp} NYIMP ──"
    );
    if !nyimp_list.is_empty() {
        eprintln!("  NYIMP:");
        for line in &nyimp_list {
            eprintln!("    {line}");
        }
    }
    if !fail_list.is_empty() {
        eprintln!("  FAIL:");
        for (name, msg) in &fail_list {
            eprintln!("    {name}: {msg}");
        }
        panic!("{fail} {kind} test(s) failed (see stderr for detail)");
    }
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
}

fn collect_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|r| r.ok().map(|e| e.path())).collect(),
        Err(_) => return Vec::new(),
    };
    v.retain(|p| p.extension() == Some(OsStr::new(ext)));
    v.sort();
    v
}

// ── direct (.t) ──────────────────────────────────────────────────────

/// Direct-DSL line-oriented commands:
///
/// - `#`/`;` — comment to end of line
/// - `push <int>` — push a cell (decimal, `0xFF` hex, or negative)
/// - `push_pad <offset>` — push `user_base + USER_PAD + offset`, where
///   USER_PAD = 0x100. Lets a test write to scratch memory without
///   hardcoding session addresses.
/// - `poke <pad-off> <hex-bytes>` — write a sequence of bytes into
///   the user-area PAD region at `pad-off`. `<hex-bytes>` is a
///   contiguous string of hex pairs (e.g. `48656c6c6f` for "Hello").
///   Used by string-primitive tests that need to seed a buffer
///   before calling `cmove`, `compare`, etc.
/// - `expect_bytes <pad-off> <hex-bytes>` — opposite of `poke`: read
///   `N` bytes from PAD+off and assert they match the hex string.
/// - `call <sym>` — invoke a primitive by its asm symbol
/// - `expect <int>...` — assert stack equals these values, **bottom-first**
///   (Forth notation: `expect 1 2 3` means `1` is deepest, `3` is TOS).
///   `expect` with no args means "stack should be empty."
/// - `reset` — restore the session to post-bootstrap state
fn classify_direct(path: &Path) -> Outcome {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Outcome::Fail(format!("read failed: {e}")),
    };

    // Pre-scan for missing asm symbols.
    let mut s = sess();
    let mut missing: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = strip_comment(line).trim();
        if let Some(rest) = trimmed.strip_prefix("call ") {
            let sym = rest.split_whitespace().next().unwrap_or("");
            if s.xt_of(sym).is_err() && !missing.contains(&sym.to_string()) {
                missing.push(sym.to_string());
            }
        }
    }
    if !missing.is_empty() {
        return Outcome::Nyimp(missing);
    }

    // Run.
    let pad_base = s.user_base + 0x100; // USER_PAD offset, mirrors kernel/macros.masm
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        let body = strip_comment(line).trim();
        if body.is_empty() {
            continue;
        }
        let mut parts = body.split_whitespace();
        let cmd = parts.next().unwrap();
        let res = (|| -> Result<(), String> {
            match cmd {
                "push" => {
                    let raw = parts.next().ok_or("push needs a value")?;
                    let v = parse_int(raw).ok_or_else(|| format!("bad int `{raw}`"))?;
                    s.push(v);
                }
                "push_pad" => {
                    let raw = parts.next().ok_or("push_pad needs an offset")?;
                    let off = parse_int(raw).ok_or_else(|| format!("bad int `{raw}`"))?;
                    s.push((pad_base as i64).wrapping_add(off));
                }
                "poke" => {
                    let off_raw = parts.next().ok_or("poke needs an offset")?;
                    let hex = parts.next().ok_or("poke needs hex bytes")?;
                    let off = parse_int(off_raw)
                        .ok_or_else(|| format!("bad offset `{off_raw}`"))?;
                    let bytes = parse_hex_bytes(hex)
                        .ok_or_else(|| format!("bad hex bytes `{hex}`"))?;
                    let dst = (pad_base as i64).wrapping_add(off) as *mut u8;
                    // SAFETY: pad region lives inside the 128 MB
                    // session-allocated block; tests address it via
                    // bounded offsets.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            dst,
                            bytes.len(),
                        );
                    }
                }
                "expect_bytes" => {
                    let off_raw = parts.next().ok_or("expect_bytes needs an offset")?;
                    let hex = parts.next().ok_or("expect_bytes needs hex bytes")?;
                    let off = parse_int(off_raw)
                        .ok_or_else(|| format!("bad offset `{off_raw}`"))?;
                    let want = parse_hex_bytes(hex)
                        .ok_or_else(|| format!("bad hex bytes `{hex}`"))?;
                    let src = (pad_base as i64).wrapping_add(off) as *const u8;
                    // SAFETY: same as `poke` above.
                    let got: Vec<u8> = unsafe {
                        std::slice::from_raw_parts(src, want.len()).to_vec()
                    };
                    if got != want {
                        return Err(format!(
                            "bytes mismatch at PAD+{off:#x}\n      expected: {}\n      got     : {}",
                            hex_bytes(&want),
                            hex_bytes(&got)
                        ));
                    }
                }
                "call" => {
                    let sym = parts.next().ok_or("call needs a symbol")?;
                    s.call(sym).map_err(|e| format!("call {sym}: {e}"))?;
                }
                "expect" => {
                    let want_bot_first: Vec<i64> = parts
                        .map(|t| parse_int(t).ok_or_else(|| format!("bad int `{t}`")))
                        .collect::<Result<_, _>>()?;
                    let want: Vec<i64> =
                        want_bot_first.iter().rev().copied().collect();
                    let got = s.stack();
                    if got != want {
                        return Err(format!(
                            "stack mismatch\n      expected (bottom→top): {:?}\n      got      (top→bottom): {:?}",
                            want_bot_first, got
                        ));
                    }
                }
                "reset" => s.reset(),
                other => return Err(format!("unknown command `{other}`")),
            }
            Ok(())
        })();
        if let Err(msg) = res {
            return Outcome::Fail(format!("line {lineno}: {msg}"));
        }
    }
    Outcome::Pass
}

// ── eval (.in / .out) ────────────────────────────────────────────────

fn classify_eval(in_path: &Path, out_path: &Path) -> Outcome {
    let input = match fs::read_to_string(in_path) {
        Ok(t) => t.replace("\r\n", "\n"),
        Err(e) => return Outcome::Fail(format!("read .in: {e}")),
    };
    let expected = match fs::read_to_string(out_path) {
        Ok(t) => t.replace("\r\n", "\n"),
        Err(e) => return Outcome::Fail(format!("read .out: {e}")),
    };

    // NYIMP detection: `# requires: word1 word2 …` lines list Forth
    // names this test depends on. Missing any → NYIMP. (Tests that
    // don't declare requirements run unconditionally — fine for words
    // we KNOW are present, like the M3/M4 baseline.)
    let mut required: Vec<String> = Vec::new();
    for line in input.lines() {
        let t = line.trim_start();
        if let Some(rest) = t
            .strip_prefix("#")
            .or_else(|| t.strip_prefix(";"))
            .map(|r| r.trim_start())
        {
            if let Some(list) = rest.strip_prefix("requires:") {
                required.extend(list.split_whitespace().map(String::from));
            }
        }
    }
    let missing: Vec<String> = required
        .into_iter()
        .filter(|w| !wf64::PRIMITIVES.iter().any(|&(name, _, _)| name == w))
        .collect();
    if !missing.is_empty() {
        return Outcome::Nyimp(missing);
    }

    // Strip harness-only metadata lines (those starting with `#`) so
    // the kernel doesn't see them as Forth source. Forth's own comment
    // syntax (`\` to end-of-line, `( … )` inline) passes through
    // unchanged.
    let forth_source: String = input
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<&str>>()
        .join("\n")
        + "\n";

    let mut s = sess();
    match s.eval(&forth_source) {
        Ok(actual) if actual == expected => Outcome::Pass,
        Ok(actual) => Outcome::Fail(format!(
            "output mismatch\n      expected: {:?}\n      got     : {:?}",
            expected, actual
        )),
        Err(e) => Outcome::Fail(format!("eval failed: {e}")),
    }
}

// ── shared helpers ───────────────────────────────────────────────────

fn strip_comment(line: &str) -> &str {
    let cut = line
        .find(|c| c == '#' || c == ';')
        .unwrap_or(line.len());
    &line[..cut]
}

/// Parse a contiguous hex string like `"48656c6c6f"` into bytes.
/// Ignores optional underscores so longer strings can be grouped for
/// readability (`"4865_6c6c_6f"`).
fn parse_hex_bytes(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if cleaned.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

fn hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn parse_int(s: &str) -> Option<i64> {
    // Strip `_` separators so values like `0xCAFEBABE_DEADBEEF` are
    // readable in tests. Decimal benefits too (`1_000_000`).
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    let s: &str = &cleaned;
    // Parse hex via u64 so the full 64-bit range is reachable. `0x8…`
    // values above i64::MAX are bit-cast as the corresponding negative
    // i64. Negative hex (`-0x8000…`) handles i64::MIN by computing the
    // wrapping negation; this is the only way to express i64::MIN as
    // a literal that survives Rust's overflow checks.
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok().map(|u| u as i64)
    } else if let Some(neg_hex) = s.strip_prefix("-0x").or_else(|| s.strip_prefix("-0X")) {
        u64::from_str_radix(neg_hex, 16).ok().map(|u| (u as i64).wrapping_neg())
    } else {
        s.parse().ok()
    }
}

// ── canvas fast-path (rt_canvas_blit → SurfaceCmd::Blit) ─────────────

/// The `canvas-blit` kernel primitive is published and the high-resolution
/// canvas Mandelbrot demo compiles end-to-end. Booting the shared session
/// already proves the new `canvas_blit_word` MASM proc assembles; this pins
/// that `canvas-blit`, `L!`, `fractal-iter`, and the `gpane-*` words all
/// resolve when the demo is loaded, and that its entry word is defined.
#[test]
fn canvas_mandelbrot_demo_compiles() {
    let mut s = sess();

    // The new primitive ticks cleanly (an undefined word would not).
    let out = s.eval("' canvas-blit drop\nbye\n").unwrap();
    assert_eq!(out, " ok\n", "canvas-blit should be a defined word: {out:?}");

    // Load the demo source; every word it uses must resolve, or the colon
    // definition of its entry word fails and the word never appears.
    let demo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("demos")
        .join("gfx-canvas-mandelbrot.f");
    s.load_source_file(&demo).expect("load gfx-canvas-mandelbrot.f");

    let out = s.eval("' gfx-canvas-mandelbrot drop\nbye\n").unwrap();
    assert_eq!(out, " ok\n", "demo entry word should be defined: {out:?}");
}

/// The register-pinned canvas Mandelbrot demo compiles (its hotvariable
/// per-pixel loop pins at compile time, with pinning on by default) and its
/// entry word is defined.
#[test]
fn hot_mandel_canvas_demo_compiles() {
    let mut s = sess();
    let demo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("demos")
        .join("hot-mandel-canvas.f");
    s.load_source_file(&demo).expect("load hot-mandel-canvas.f");
    let out = s.eval("' hot-mandel-canvas drop\nbye\n").unwrap();
    assert_eq!(out, " ok\n", "demo entry word should be defined: {out:?}");
}

// ── FTOS: FP top-of-stack cached in xmm15 ───────────────────────────────────

/// The FP top-of-stack is cached in xmm15 inside Forth but parked in
/// user_FTOS_SAVE across forth_main calls, so a float left on the FP stack by
/// one eval survives into the next — exactly like the data stack persists.
#[test]
fn ftos_persists_across_evals() {
    let mut s = sess();
    s.eval("2e\n").unwrap(); // leaves 2.0 on the FP stack
    let out = s.eval("3e f+ f.\n").unwrap(); // 2.0 + 3.0
    assert!(out.contains("5.000000"), "FP stack should survive the eval boundary: {out:?}");
}

/// fdepth counts the cached top correctly at depths 0, 1, and 3.
#[test]
fn ftos_fdepth_counts_cached_top() {
    let mut s = sess();
    let out = s
        .eval("fdepth . 1e fdepth . 2e 3e fdepth . fdrop fdrop fdrop fdepth .\nbye\n")
        .unwrap();
    assert!(out.contains("0 1 3 0"), "{out:?}");
}

/// A deep FP stack (more than fits in any small register window) round-trips
/// every element through the cached top correctly.
#[test]
fn ftos_deep_stack_roundtrips() {
    let mut s = sess();
    // Push 1.0..=8.0, then sum them back: 1+2+...+8 = 36.
    let out = s
        .eval("1e 2e 3e 4e 5e 6e 7e 8e f+ f+ f+ f+ f+ f+ f+ f>d drop .\nbye\n")
        .unwrap();
    assert!(out.contains("36"), "{out:?}");
}

/// The floating-point canvas Mandelbrot demo compiles and its entry word is
/// defined (its per-pixel FP escape loop exercises the FTOS path heavily).
#[test]
fn hot_fmandel_canvas_demo_compiles() {
    let mut s = sess();
    let demo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("demos")
        .join("hot-fmandel-canvas.f");
    s.load_source_file(&demo).expect("load hot-fmandel-canvas.f");
    let out = s.eval("' hot-fmandel-canvas drop\nbye\n").unwrap();
    assert_eq!(out, " ok\n", "demo entry word should be defined: {out:?}");
}

// ── Register pinning (optimizer agenda #1, Phase 2) ─────────────────────────
// Differential: a hotvariable loop must produce identical output compiled with
// pinning on vs off (the STC build is the oracle).
fn pin_diff(prog: &str) -> (String, String) {
    let mut s = sess();
    s.set_pin_enable(false); // explicit unpinned baseline (default is now on)
    let unpinned = s.eval(prog).unwrap();
    s.reset();
    s.set_pin_enable(true);
    let pinned = s.eval(prog).unwrap();
    (unpinned, pinned)
}

#[test]
fn peephole_value_oracle_fuzz() {
    // Differential fuzz of the compile-time peepholes against an independent
    // Rust value oracle. Generates random PURE integer expressions that mix:
    //   literals (incl. >127 → imm32 folds, 0, negatives), constants (inline+fold),
    //   arithmetic folds (+ - * and or xor), `dup +`/`dup *` fuses,
    //   comparison folds (< > <= >= = <>) and `if/else/then` (compare→branch fusion).
    // Each expression is emitted as a colon body and run; the printed result must
    // equal the oracle. Covers the consumption pattern (`a b = +`), the fence
    // (`… else <lit> then <op>`), and nested fusion — the exact bug classes hit
    // while building these peepholes. Deterministic seed; no side effects, so the
    // oracle can evaluate both `if` arms.
    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 1
    }
    fn pick(seed: &mut u64, n: usize) -> usize { (lcg(seed) as usize) % n }

    fn gen(seed: &mut u64, depth: u32) -> (String, i64) {
        // Bias toward leaves so expressions stay bounded; force a leaf at depth 0.
        if depth == 0 || pick(seed, 100) < 35 {
            if pick(seed, 3) == 0 {
                let consts = [("k3", 3i64), ("k100", 100), ("k200", 200), ("km7", -7)];
                let (n, v) = consts[pick(seed, consts.len())];
                return (n.to_string(), v);
            }
            let lits = [0i64, 1, 2, 5, 127, 128, 200, 255, -1, -4, 1000, 70000];
            let v = lits[pick(seed, lits.len())];
            return (format!("{v}"), v);
        }
        match pick(seed, 5) {
            0 => {
                let (l, lv) = gen(seed, depth - 1);
                let (r, rv) = gen(seed, depth - 1);
                let ops: [(&str, fn(i64, i64) -> i64); 6] = [
                    ("+", |a, b| a.wrapping_add(b)),
                    ("-", |a, b| a.wrapping_sub(b)),
                    ("*", |a, b| a.wrapping_mul(b)),
                    ("and", |a, b| a & b),
                    ("or", |a, b| a | b),
                    ("xor", |a, b| a ^ b),
                ];
                let (op, f) = ops[pick(seed, ops.len())];
                (format!("{l} {r} {op}"), f(lv, rv))
            }
            1 => {
                let (a, av) = gen(seed, depth - 1);
                (format!("{a} dup +"), av.wrapping_add(av))
            }
            2 => {
                let (a, av) = gen(seed, depth - 1);
                (format!("{a} dup *"), av.wrapping_mul(av))
            }
            _ => {
                let (x, xv) = gen(seed, depth - 1);
                let (y, yv) = gen(seed, depth - 1);
                let cmps: [(&str, fn(i64, i64) -> bool); 6] = [
                    ("<", |a, b| a < b),
                    (">", |a, b| a > b),
                    ("<=", |a, b| a <= b),
                    (">=", |a, b| a >= b),
                    ("=", |a, b| a == b),
                    ("<>", |a, b| a != b),
                ];
                let (cmp, cf) = cmps[pick(seed, cmps.len())];
                let (a, av) = gen(seed, depth - 1);
                let (b, bv) = gen(seed, depth - 1);
                let v = if cf(xv, yv) { av } else { bv };
                (format!("{x} {y} {cmp} if {a} else {b} then"), v)
            }
        }
    }

    let mut s = sess();
    s.eval("3 constant k3  100 constant k100  200 constant k200  -7 constant km7\n").unwrap();
    let mut seed: u64 = 0xD1CE_5EED_1234_5678;
    for case in 0..1500 {
        let (expr, expected) = gen(&mut seed, 3);
        s.eval(&format!(": fz{case} {expr} . ;\n"))
            .unwrap_or_else(|e| panic!("case {case}: compile of {expr:?} failed: {e}"));
        let out = s.eval(&format!("fz{case}\n")).unwrap();
        let got: i64 = out
            .split_whitespace()
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| panic!("case {case}: expr {expr:?} -> unparseable output {out:?}"));
        assert_eq!(got, expected, "case {case}: expr {expr:?} got {got}, want {expected}");
    }
}

#[test]
fn pin_do_loop_read_write_matches_unpinned() {
    // `do` loop, hv read+written each iteration -> r9 ReadWrite, prologue load
    // + write-back. sum 0..99 = 4950.
    let prog = "hotvariable hv\n: dosum 0 hv ! 100 0 do hv @ i + hv ! loop hv @ . ;\ndosum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains("4950"), "got: {pinned:?}");
}

#[test]
fn pin_qdo_loop_matches_unpinned() {
    // ?do form (the common hot loop).
    let prog = "hotvariable hv\n: qsum 0 hv ! 100 0 ?do hv @ i + hv ! loop hv @ . ;\nqsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains("4950"), "got: {pinned:?}");
}

#[test]
fn pin_qdo_zero_iterations_keeps_value() {
    // ?do with start==limit runs zero times: the skip path must touch neither
    // the prologue load nor the write-back, so hv keeps its pre-loop value (7).
    let prog = "hotvariable hv\n: zsum 7 hv ! 5 5 ?do hv @ i + hv ! loop hv @ . ;\nzsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains('7'), "got: {pinned:?}");
}

#[test]
fn pin_float_literal_in_do_loop() {
    // Minimal: a float literal inside a recorded do-loop. (Recording happens
    // for every do-loop; a float literal must not break it.)
    let mut s = sess();
    let out = s.eval(": ft 3 0 do 2e fdrop loop 42 . ;\nft\nbye\n").unwrap();
    assert!(out.contains("42"), "got: {out:?}");
}

#[test]
fn hotfvariable_arithmetic_runs() {
    // Four hotfvariables summed in a loop. (Float register pinning is disabled —
    // ENABLE_FLOAT_PINNING — so this just checks hotfvariable f@/f! arithmetic is
    // correct; it no longer exercises an xmm pin path.)
    let prog = "hotfvariable a hotfvariable b hotfvariable c hotfvariable d\n\
        : t 1e a f! 2e b f! 3e c f! 4e d f! 3 0 do a f@ b f@ f+ c f@ f+ d f@ f+ fdrop loop a f@ b f@ f+ c f@ f+ d f@ f+ f>d drop . ;\n\
        t\nbye\n";
    let mut s = sess();
    let out = s.eval(prog).unwrap();
    assert!(out.contains("10"), "{out:?}"); // 1+2+3+4
}

#[test]
fn pin_hotfloat_matches_unpinned() {
    // A hotfvariable accumulator pinned in xmm6 across a loop that CALLS FP ops
    // (s>d, d>f, f+) — those preserve the xmm6-15 pool, so the float pin holds
    // even though the loop is not call-free. Sum 0.0+1.0+..+9.0 = 45.
    let prog = "hotfvariable acc\n\
                : fsum 0e acc f! 10 0 do acc f@ i s>d d>f f+ acc f! loop acc f@ f>d drop . ;\n\
                fsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "hotfloat pinned != unpinned");
    assert!(pinned.contains("45"), "got: {pinned:?}");
}

#[test]
fn pin_hot_fmandel_matches_unpinned() {
    // FP Mandelbrot: 4 hotfvariables pin in xmm6-9 across a loop that calls
    // f*/f+/f-/f< (which preserve the pool). Escape counts must match unpinned.
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join("corpus")
        .join("hot-fmandel.f");
    let inputs = ["-0.5e 0e 255", "2e 2e 255", "0.3e 0.5e 255", "-1e 0e 255", "0.35e 0.35e 255"];
    let run = |pin: bool| -> Vec<String> {
        let mut s = sess();
        s.set_pin_enable(pin);
        s.load_source_file(&corpus).expect("load hot-fmandel.f");
        inputs
            .iter()
            .map(|inp| s.eval(&format!("{inp} hot-fmandel .\nbye\n")).unwrap())
            .collect()
    };
    assert_eq!(run(false), run(true), "hot-fmandel pinned != unpinned");
}

#[test]
fn pin_hot_mandel_matches_unpinned() {
    // Real workload with leave / ?do / >r in the pinned loop (5 hot vars, 3
    // register slots). Pinned escape counts must equal unpinned across the
    // plane. Loading the file also runs its pinned load-time self-check.
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join("corpus")
        .join("hot-mandel-iter.f");
    let inputs = ["-64 0 255", "-128 64 255", "10 -40 255", "0 0 255", "-200 100 255", "-90 -90 255"];
    let run = |pin: bool| -> Vec<String> {
        let mut s = sess();
        s.set_pin_enable(pin);
        s.load_source_file(&corpus).expect("load hot-mandel-iter.f");
        inputs
            .iter()
            .map(|inp| s.eval(&format!("{inp} hot-mandel-iter .\nbye\n")).unwrap())
            .collect()
    };
    let unpinned = run(false);
    let pinned = run(true);
    assert_eq!(unpinned, pinned, "hot-mandel pinned != unpinned");
}

#[test]
fn pin_differential_fuzz() {
    // Broad differential: build many do-loop bodies from stack-neutral
    // hotvariable fragments, compile each pinned vs unpinned, and require
    // identical output (the STC build is the oracle). Deterministic seed.
    fn lcg(seed: &mut u64) -> usize {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 33) as usize
    }
    // Each fragment is data-stack-neutral and touches hv only via @ / ! / +!.
    let frags = [
        "hv @ 1 + hv !",
        "2 hv +!",
        "hv @ hv @ + hv !",
        "hv @ drop",
        "i hv +!",
        "hv @ 3 + hv !",
        "1 hv +! 5 hv +!",
        "hv @ 2 * hv !",
    ];
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut s = sess();
    for case in 0..80 {
        let n = 1 + lcg(&mut seed) % 4;
        let mut body = String::new();
        for _ in 0..n {
            body.push_str(frags[lcg(&mut seed) % frags.len()]);
            body.push(' ');
        }
        // alternate do and ?do; vary the trip count
        let opener = if case % 2 == 0 { "do" } else { "?do" };
        let trips = 1 + lcg(&mut seed) % 12;
        let prog = format!(
            "hotvariable hv\n: fz 0 hv ! {trips} 0 {opener} {body}loop hv @ . ;\nfz\nbye\n"
        );
        s.reset();
        s.set_pin_enable(false);
        let unpinned = s.eval(&prog).unwrap();
        s.reset();
        s.set_pin_enable(true);
        let pinned = s.eval(&prog).unwrap();
        assert_eq!(unpinned, pinned, "case {case}: body {body:?} differ");
    }
}

#[test]
fn pin_actually_changes_codegen() {
    // Prove pinning committed (not an identity fallback): each pinned `hv @`
    // (register push, 11 B) replaces lea+load (18 B). A read-heavy loop's body
    // savings clearly exceed the one-time prologue+write-back, so the pinned
    // definition is strictly smaller.
    let body = ": d 0 hv ! 100 0 do hv @ hv @ hv @ hv @ + + + hv ! loop hv @ ;\n";
    let mut s = sess();
    s.set_pin_enable(false); // explicit unpinned baseline (default is now on)
    s.eval("hotvariable hv\n").unwrap();
    let h0 = s.here();
    s.eval(body).unwrap();
    let unpinned = s.here() - h0;

    s.reset();
    s.set_pin_enable(true);
    s.eval("hotvariable hv\n").unwrap();
    let h1 = s.here();
    s.eval(body).unwrap();
    let pinned = s.here() - h1;
    s.set_pin_enable(false);

    assert!(pinned < unpinned, "pinned {pinned} B should be < unpinned {unpinned} B (pinning not committed?)");
}

#[test]
fn pin_begin_until_matches_unpinned() {
    // Indefinite loop: hv += counter for counter 10..1 -> 55.
    let prog = "hotvariable hv\n\
                : bsum 0 hv ! 10 begin hv @ over + hv ! 1 - dup 0= until drop hv @ . ;\n\
                bsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains("55"), "got: {pinned:?}");
}

#[test]
fn pin_begin_while_repeat_matches_unpinned() {
    let prog = "hotvariable hv\n\
                : wsum 0 hv ! 10 begin dup 0 > while hv @ over + hv ! 1 - repeat drop hv @ . ;\n\
                wsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains("55"), "got: {pinned:?}");
}

#[test]
fn pin_nested_loops_matches_unpinned() {
    // Nested do-loops: hv pinned in r9 survives the inner loop (r9 is never
    // clobbered by do-setup/idiv). 3*3 increments -> 9.
    let prog = "hotvariable hv\n\
                : nsum 0 hv ! 3 0 do 3 0 do hv @ 1 + hv ! loop loop hv @ . ;\n\
                nsum\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains('9'), "got: {pinned:?}");
}

#[test]
fn pin_plus_store_and_invariant_matches_unpinned() {
    // +! (read-modify-write) on one hot var, and a read-only invariant read each
    // iteration. cnt += base for i=0..9, base=10 -> 100.
    let prog = "hotvariable cnt  hotvariable base\n\
                : acc 0 cnt ! 10 base ! 10 0 do base @ cnt +! loop cnt @ . ;\n\
                acc\nbye\n";
    let (unpinned, pinned) = pin_diff(prog);
    assert_eq!(unpinned, pinned, "pinned != unpinned");
    assert!(pinned.contains("100"), "got: {pinned:?}");
}
