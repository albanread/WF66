//! `fiber-seh-repro` — headless keystone for the cooperative-agents work.
//!
//! This is the test the revert (0d21774) said was missing: a HEADLESS harness
//! that (a) converts a worker thread to a Win32 fiber via `(agent-init)`, (b)
//! installs the REAL recovery VEH (`igui::crash_handler`) on a registered worker
//! thread, and (c) drives agents — so the fiber <-> `forth_main`-RSP-reroute <->
//! SEH/VEH interaction runs with NO window. The existing headless tests
//! (`agents_round_robin`, `agents_fp_and_mailbox`) never install the recovery VEH
//! and never split scheduling across multiple `eval`s, so this path had zero
//! coverage — the gap that let the bug reach the GUI.
//!
//! Topology mirrors `wf64-ui`: main thread installs the VEH FIRST, then spawns a
//! worker thread that registers itself, boots a session, and ARMS the scheduler
//! (operator fiber + spawned agents in the READY queue) BEFORE any "events" are
//! delivered — i.e. 4th is ready to switch on events before they start arriving.
//!
//! Modes (argv[1]):
//!   split   (default) — the 7a0c772 IDE-pump shape: the operator drives the
//!                       scheduler from a SEPARATE `eval("run-slice")` each round
//!                       (a fresh `forth_main` re-entry, RSP rerouted), with a
//!                       console `eval` interleaved to mimic IDE keystrokes.
//!   gated             — the shipped gated shape: agents advanced by Rust
//!                       `agents::run_slice()` (operator never re-enters
//!                       `forth_main` to schedule); console events via `eval`.
//!   oneeval           — control: arm + drive entirely under ONE `forth_main`
//!                       (mirrors `agents_round_robin`) — proves baseline-good
//!                       even with the recovery VEH installed.
//!   seh               — control: an agent forces a real access violation
//!                       (`0 @`) so we exercise the recovery VEH firing ON AN
//!                       AGENT FIBER with RSP rerouted, and assert clean recovery.
//!
//!   argv[2] = number of event rounds (default 10; agents finish in 8).
//!
//! Run:  cargo run --bin fiber-seh-repro -- split
//!       cargo run --bin fiber-seh-repro -- gated
//!       cargo run --bin fiber-seh-repro -- oneeval
//!       cargo run --bin fiber-seh-repro -- seh

#[cfg(not(windows))]
fn main() {
    eprintln!("fiber-seh-repro is Windows-only (fibers + VEH).");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let mode = std::env::args().nth(1).unwrap_or_else(|| "split".to_string());
    let rounds: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    println!("== fiber-seh-repro: mode={mode} rounds={rounds} ==");

    // (1) Install the REAL recovery VEH on the MAIN thread BEFORE the worker
    //     exists — mirrors wf64-ui's main() (crash_handler::install precedes the
    //     worker spawn). VEHs fire most-recently-added-first, so the JASM dumper
    //     installed later (when the session boots on the worker) fires first and
    //     CONTINUE_SEARCHes; this recovery handler fires last and ExitThreads.
    wf64::igui::crash_handler::install();

    let mode_for_worker = mode.clone();
    let handle = std::thread::Builder::new()
        .name("wf64-worker".into())
        .spawn(move || worker(&mode_for_worker, rounds))
        .expect("spawn worker thread");

    // (2) Join, swallowing the std "threads should not terminate unexpectedly"
    //     panic that fires when our VEH redirects the worker to ExitThread —
    //     exactly as run_supervisor does (src/bin/wf64_ui.rs:88). dev/release
    //     profiles unwind (panic=abort is release-ship only), so this catches.
    let joined = catch_unwind(AssertUnwindSafe(|| {
        let _ = handle.join();
    }));

    // (3) Verdict — did the worker fault (and did the VEH recover it cleanly)?
    match wf64::igui::crash_handler::take_dump() {
        Some(d) => {
            println!();
            println!("VERDICT: SEH CAPTURED on the worker — recovery VEH fired and the");
            println!("         process is still alive (worker exited via ExitThread).");
            println!("  code        = 0x{:08x}", d.code);
            println!("  rip         = 0x{:016x}", d.rip);
            println!("  rsp         = 0x{:016x}", d.rsp);
            println!("  access_addr = 0x{:016x} (kind {})", d.access_addr, d.access_kind);
            println!("  worker_tid  = {}", d.thread_id);
            println!();
            println!("  -> The fiber/forth_main/SEH path faulted, but the headless recovery");
            println!("     machinery worked: this is the missing coverage, now green.");
        }
        None => {
            println!();
            if joined.is_ok() {
                println!("VERDICT: worker completed cleanly — NO SEH. This shape is SAFE");
                println!("         under the fiber + recovery-VEH topology, headless.");
            } else {
                println!("VERDICT: worker died WITHOUT an SEH dump (Rust panic/abort, or a");
                println!("         non-recoverable SEH code the VEH passed through).");
            }
        }
    }
}

/// The worker thread: register with the VEH, boot, ARM the scheduler before any
/// events, then drive the selected pump shape.
#[cfg(windows)]
fn worker(mode: &str, rounds: usize) {
    // Register THIS thread so the recovery VEH intercepts only its SEH.
    wf64::igui::crash_handler::register_worker_thread();

    let kernel = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("kernel")
        .join("main.masm");
    let mut session = match wf64::Wf64Session::with_kernel(&kernel) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[worker] session boot failed: {e:#}");
            return;
        }
    };

    // ── ARM THE SCHEDULER BEFORE ANY EVENTS ──────────────────────────────────
    // "4th must be ready to switch on events before the IDE starts sending them":
    // convert the worker to the operator fiber and spawn the agents so they sit
    // in the READY queue NOW, before the event-driven drive loop below. Agents
    // bump a counter (no `emit`) so the gated path doesn't trip the known
    // CURRENT_IO==None panic — that's a separate layer (F), kept out of this one.
    let arm = "\
(agent-init) drop\n\
variable tickA  variable tickB\n\
: agA  8 0 do  1 tickA +!  pause  loop ;\n\
: agB  8 0 do  1 tickB +!  pause  loop ;\n\
' agA agent drop\n\
' agB agent drop\n";
    if let Err(e) = session.eval(arm) {
        eprintln!("[worker] arm failed: {e:#}");
        return;
    }
    let ready = wf64::agents::ready_count();
    println!("[worker] armed BEFORE events: {ready} agent(s) ready to switch.");
    if ready == 0 {
        eprintln!("[worker] WARNING: scheduler not primed (ready_count==0).");
    }

    // ── NOW DELIVER "EVENTS" AND DRIVE THE SELECTED SHAPE ────────────────────
    match mode {
        // The 7a0c772 IDE-pump shape: operator schedules from a SEPARATE eval
        // each round (fresh forth_main, RSP rerouted), console eval interleaved.
        "split" => {
            for r in 0..rounds {
                let before = wf64::agents::ready_count();
                report_eval(&mut session, &format!("split/run-slice #{r} (ready {before})"), "run-slice\n");
                report_eval(&mut session, &format!("split/console  #{r}"), "2 3 + .\n");
            }
            report_eval(&mut session, "split/ticks", "tickA @ .  tickB @ .\n");
        }

        // The shipped gated shape: agents advanced from RUST; console via eval.
        "gated" => {
            for r in 0..rounds {
                let before = wf64::agents::ready_count();
                wf64::agents::run_slice();
                let after = wf64::agents::ready_count();
                println!("[worker] gated/run_slice #{r}: ready {before} -> {after}");
                report_eval(&mut session, &format!("gated/console  #{r}"), "2 3 + .\n");
            }
            report_eval(&mut session, "gated/ticks", "tickA @ .  tickB @ .\n");
        }

        // Control: arm + drive under ONE forth_main (mirrors agents_round_robin).
        // `run-until-idle` is a lib/agents.f colon word — drive it at the
        // interpreter (the begin/while/repeat loop itself is compile-only).
        "oneeval" => {
            report_eval(
                &mut session,
                "oneeval/run-until-idle+ticks",
                "run-until-idle   tickA @ .  tickB @ .\n",
            );
        }

        // Control: an agent forces a real AV so the recovery VEH fires on an
        // agent fiber with RSP rerouted. The eval never returns (worker
        // ExitThreads mid-slice); main's join + take_dump report the capture.
        "seh" => {
            let boom = "\
: agBoom  0 @ drop ;\n\
' agBoom agent drop\n\
run-until-idle\n";
            report_eval(&mut session, "seh/induced-AV", boom);
        }

        other => {
            eprintln!("[worker] unknown mode {other:?}; use split|gated|oneeval|seh");
        }
    }

    println!("[worker] drive loop returned normally (no fault on this thread).");
    wf64::igui::crash_handler::unregister_worker_thread();
}

#[cfg(windows)]
fn report_eval(session: &mut wf64::Wf64Session, tag: &str, prog: &str) {
    match session.eval(prog) {
        Ok(out) => {
            let t = out.trim();
            if t.is_empty() {
                println!("[{tag}] ok");
            } else {
                println!("[{tag}] -> {t}");
            }
        }
        Err(e) => println!("[{tag}] ERR: {e}"),
    }
}
