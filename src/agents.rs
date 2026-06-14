//! WF66 cooperative agents — the green-thread substrate (step 1).
//!
//! Single-threaded Forth multiplexing: each agent is a Win32 **fiber** with its
//! own data/locals/FP stacks, switched cooperatively. Fibers own the native
//! return stack (and keep the TEB/guard pages/SEH correct); we manage the
//! Forth-specific state around the switch. See docs/design/wf66_agents.md.
//!
//! Thread model: fibers are thread-bound, so every agent call must happen on the
//! one thread that runs the scheduler (the IDE worker, or a test thread). The
//! agent table + operator fiber are therefore thread-local; the trampoline
//! address and user-area base are process-globals set once at boot.
//!
//! Step 1 scope: spawn / switch / done / init + a trampoline, enough to prove
//! round-robin in pure Forth. Mailboxes, the OO scheduler, per-agent catch
//! frames, and FSP/HANDLER swapping are layered on after this is solid.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// Address of the kernel `agent_trampoline` proc (a fiber start routine).
static TRAMPOLINE: AtomicU64 = AtomicU64::new(0);
/// The single user-area base (UP); one shared user area across all agents.
static USER_BASE: AtomicU64 = AtomicU64::new(0);

// User-area cells that are per-agent execution state but live in the one shared
// user area, so the switch swaps their VALUES (mirror kernel/macros.masm).
const USER_HANDLER: u64 = 0x80; // catch/throw handler chain head
const USER_FSP: u64 = 0x1218; // FP stack pointer

#[inline]
unsafe fn rd_cell(off: u64) -> u64 {
    *((USER_BASE.load(Ordering::SeqCst) + off) as *const u64)
}
#[inline]
unsafe fn wr_cell(off: u64, v: u64) {
    *((USER_BASE.load(Ordering::SeqCst) + off) as *mut u64) = v;
}

/// Set at boot once the kernel is assembled (any thread; read-only after).
pub fn set_globals(trampoline: u64, user_base: u64) {
    TRAMPOLINE.store(trampoline, Ordering::SeqCst);
    USER_BASE.store(user_base, Ordering::SeqCst);
}

struct Slot {
    /// Win32 fiber handle (the operator's converted-thread fiber for slot 0).
    fiber: *mut c_void,
    /// `[up, ds_top, ls_top, entry_xt]` — the fiber parameter the trampoline reads.
    /// Boxed so its address is stable; kept alive for the fiber's lifetime.
    _ctx: Box<[u64; 4]>,
    done: bool,
    /// Saved FP stack pointer (`user_FSP`) — per-agent, swapped at each switch so
    /// every agent has its own FP stack. Init to the FP region top.
    fsp: u64,
    /// Saved catch/throw handler chain head (`user_HANDLER`) — per-agent so an
    /// agent's `catch` is isolated. Init 0 (no handler).
    handler: u64,
    /// This agent's incoming message queue (the mailbox).
    mailbox: VecDeque<u64>,
    /// True while the agent is in the ready queue (dedup guard).
    ready: bool,
    /// Pane binding: the `child_id` this agent drives, or -1 if unbound. When
    /// bound (>= 0) the agent's output routes to its pane buffer (`out`) instead
    /// of the shared session I/O — see `route_output` and
    /// docs/design/wf66_pane_agent_gui.md (stage 1).
    child_id: i64,
    /// Per-agent output buffer (the pane transcript), drained by the host pump's
    /// flush step (`take_pane_output`). Only filled while `child_id >= 0`.
    out: Vec<u8>,
    /// Backing storage for the agent's Forth stacks (kept alive; grows downward
    /// from the *_top addresses baked into `_ctx`). Empty for the operator.
    _ds: Vec<u64>,
    _ls: Vec<u64>,
    _fp: Vec<u64>,
}

thread_local! {
    static OPERATOR: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    static AGENTS: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
    static CURRENT: Cell<usize> = const { Cell::new(0) };
    /// Round-robin ready queue of runnable agent ids (the scheduler's run set).
    static READY: RefCell<VecDeque<usize>> = const { RefCell::new(VecDeque::new()) };
}

// Per-agent Forth stack sizes (u64 cells).
const DS_CELLS: usize = 8 * 1024; // 64 KB data stack
const LS_CELLS: usize = 8 * 1024; // 64 KB locals stack
const FP_CELLS: usize = 4 * 1024; // 32 KB FP stack
const FIBER_STACK: usize = 4 * 1024 * 1024; // native stack per fiber (deep Win32/D2D call chains)

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use windows::Win32::System::Threading::{
        ConvertThreadToFiber, CreateFiber, SwitchToFiber, LPFIBER_START_ROUTINE,
    };

    /// Convert the calling thread to a fiber. Returns null if it already is one
    /// (the caller reuses the stored operator handle in that case).
    pub fn convert_thread() -> *mut c_void {
        unsafe { ConvertThreadToFiber(None) }
    }

    pub fn create_fiber(stack: usize, start: u64, param: *const c_void) -> *mut c_void {
        let routine: LPFIBER_START_ROUTINE = unsafe { std::mem::transmute(start) };
        unsafe { CreateFiber(stack, routine, Some(param)) }
    }

    pub fn switch_to(fiber: *mut c_void) {
        unsafe { SwitchToFiber(fiber as *const c_void) }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::ffi::c_void;
    pub fn convert_thread() -> *mut c_void {
        std::ptr::null_mut()
    }
    pub fn create_fiber(_s: usize, _start: u64, _p: *const c_void) -> *mut c_void {
        std::ptr::null_mut()
    }
    pub fn switch_to(_f: *mut c_void) {}
}

/// `(agent-init)` — convert this thread to the operator fiber and reset the table.
/// Must run on the scheduler thread before any spawn/switch. Returns 0 (the
/// operator's aid).
#[no_mangle]
pub extern "C" fn rt_agent_init() -> u64 {
    // Reuse the operator fiber if this thread was already converted (a prior
    // init); ConvertThreadToFiber fails on an already-fiber thread.
    let existing = OPERATOR.with(|c| c.get());
    let op = if existing.is_null() {
        sys::convert_thread()
    } else {
        existing
    };
    OPERATOR.with(|c| c.set(op));
    AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        a.clear();
        a.push(Slot {
            fiber: op,
            _ctx: Box::new([0; 4]),
            done: false,
            fsp: 0, // operator's live FSP/HANDLER are saved on the first switch-away
            handler: 0,
            mailbox: VecDeque::new(),
            ready: false,
            child_id: -1, // operator is never a pane
            out: Vec::new(),
            _ds: Vec::new(),
            _ls: Vec::new(),
            _fp: Vec::new(),
        });
    });
    READY.with(|r| r.borrow_mut().clear());
    CURRENT.with(|c| c.set(0));
    0
}

/// `(spawn) ( entry-xt -- aid )` — create an agent fiber that runs `entry_xt`.
#[no_mangle]
pub extern "C" fn rt_agent_spawn(entry_xt: u64) -> u64 {
    let up = USER_BASE.load(Ordering::SeqCst);
    let tramp = TRAMPOLINE.load(Ordering::SeqCst);
    let mut ds = vec![0u64; DS_CELLS];
    let mut ls = vec![0u64; LS_CELLS];
    let mut fp = vec![0u64; FP_CELLS];
    let ds_top = ds.as_mut_ptr() as u64 + (DS_CELLS * 8) as u64;
    let ls_top = ls.as_mut_ptr() as u64 + (LS_CELLS * 8) as u64;
    let fp_top = fp.as_mut_ptr() as u64 + (FP_CELLS * 8) as u64;
    let ctx: Box<[u64; 4]> = Box::new([up, ds_top, ls_top, entry_xt]);
    let ctx_ptr = ctx.as_ptr() as *const c_void;
    let fiber = sys::create_fiber(FIBER_STACK, tramp, ctx_ptr);
    AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        let aid = a.len();
        a.push(Slot {
            fiber,
            _ctx: ctx,
            done: false,
            fsp: fp_top, // own FP stack (top; grows down)
            handler: 0,  // no catch handler installed yet
            mailbox: VecDeque::new(),
            ready: false,
            child_id: -1, // unbound until (set-pane); output uses the shared buffer
            out: Vec::new(),
            _ds: ds,
            _ls: ls,
            _fp: fp,
        });
        aid as u64
    })
}

/// `(switch-to) ( aid -- )` — cooperatively switch to agent `aid` (0 = operator).
/// No-op if that agent has finished. The Forth-register save/restore around the
/// switch (TOS) is done by the kernel `(switch-to)` primitive; fibers preserve
/// RBP/R15/XMM15 (non-volatile) automatically.
#[no_mangle]
pub extern "C" fn rt_agent_switch(target: u64) -> u64 {
    let t = target as usize;
    let fiber = AGENTS.with(|a| {
        let a = a.borrow();
        match a.get(t) {
            Some(s) if !s.done => s.fiber,
            _ => std::ptr::null_mut(),
        }
    });
    if fiber.is_null() {
        return 0; // unknown or finished agent: stay put
    }
    // Swap the per-agent user-area cells (FSP, HANDLER): save the outgoing agent's
    // live values, install the target's. RBP/R15/XMM15/RBX are non-volatile, so
    // the fiber switch preserves them; only these memory cells are shared.
    let cur = CURRENT.with(|c| c.get());
    let (fsp, handler) = unsafe { (rd_cell(USER_FSP), rd_cell(USER_HANDLER)) };
    let (tfsp, thandler) = AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        if let Some(s) = a.get_mut(cur) {
            s.fsp = fsp;
            s.handler = handler;
        }
        let s = &a[t];
        (s.fsp, s.handler)
    });
    unsafe {
        wr_cell(USER_FSP, tfsp);
        wr_cell(USER_HANDLER, thandler);
    }
    CURRENT.with(|c| c.set(t));
    sys::switch_to(fiber);
    0
}

// ── Scheduler ready queue + mailboxes ──────────────────────────────────────

/// `(ready-push) ( aid -- )` — mark an agent runnable (dedup; no-op if queued).
#[no_mangle]
pub extern "C" fn rt_sched_ready_push(aid: u64) -> u64 {
    let t = aid as usize;
    let push = AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        match a.get_mut(t) {
            Some(s) if !s.ready && !s.done => {
                s.ready = true;
                true
            }
            _ => false,
        }
    });
    if push {
        READY.with(|r| r.borrow_mut().push_back(t));
    }
    0
}

/// `(ready-pop) ( -- aid )` — next runnable agent, or -1 if none.
#[no_mangle]
pub extern "C" fn rt_sched_ready_pop() -> u64 {
    let id = READY.with(|r| r.borrow_mut().pop_front());
    match id {
        Some(t) => {
            AGENTS.with(|a| {
                if let Some(s) = a.borrow_mut().get_mut(t) {
                    s.ready = false;
                }
            });
            t as u64
        }
        None => u64::MAX,
    }
}

/// `(ready-count) ( -- n )` — number of runnable agents queued right now.
#[no_mangle]
pub extern "C" fn rt_sched_ready_len() -> u64 {
    READY.with(|r| r.borrow().len() as u64)
}

/// Number of runnable agents on this thread (for the IDE pump: slice when > 0,
/// otherwise block on the event mailbox as before).
pub fn ready_count() -> u64 {
    rt_sched_ready_len()
}

/// Run one cooperative slice from RUST (the gated IDE pump): switch to each
/// currently-runnable agent once. Unlike the Forth `run-slice`, the operator
/// stays on its native fiber stack (Rust code) and never enters `forth_main`'s
/// return-stack reroute — which is what collided with the IDE's fiber/TEB state
/// and crashed the worker. Only the agents run Forth, each on its own fiber.
pub fn run_slice() {
    let n = rt_sched_ready_len();
    for _ in 0..n {
        let aid = rt_sched_ready_pop();
        if aid == u64::MAX {
            break;
        }
        if rt_agent_is_done(aid) == 0 {
            rt_agent_switch(aid);
        }
    }
}

/// `(send) ( msg aid -- )` — enqueue a message to an agent's mailbox and make it
/// runnable. (Win64 args: rcx=msg, rdx=aid — see the kernel primitive.)
#[no_mangle]
pub extern "C" fn rt_mailbox_send(msg: u64, aid: u64) -> u64 {
    let t = aid as usize;
    AGENTS.with(|a| {
        if let Some(s) = a.borrow_mut().get_mut(t) {
            s.mailbox.push_back(msg);
        }
    });
    rt_sched_ready_push(aid);
    0
}

/// `(mailbox-len) ( -- n )` — number of pending messages for the running agent.
#[no_mangle]
pub extern "C" fn rt_mailbox_len() -> u64 {
    let cur = CURRENT.with(|c| c.get());
    AGENTS.with(|a| a.borrow().get(cur).map_or(0, |s| s.mailbox.len() as u64))
}

/// `(mailbox-pop) ( -- msg )` — dequeue the running agent's next message, or -1
/// if the mailbox is empty (callers normally check `(mailbox-len)` first).
#[no_mangle]
pub extern "C" fn rt_mailbox_pop() -> u64 {
    let cur = CURRENT.with(|c| c.get());
    AGENTS.with(|a| {
        a.borrow_mut()
            .get_mut(cur)
            .and_then(|s| s.mailbox.pop_front())
            .unwrap_or(u64::MAX)
    })
}

/// Called by the trampoline when an agent's entry word returns: mark it done and
/// switch back to the operator. Never returns to the agent.
#[no_mangle]
pub extern "C" fn rt_agent_done() -> u64 {
    let cur = CURRENT.with(|c| c.get());
    // Mark done and restore the OPERATOR's per-agent cells (FSP/HANDLER) before
    // switching to it — unlike rt_agent_switch, this path doesn't go through the
    // swap, so without this the operator would inherit the dead agent's FP stack.
    let (ofsp, ohandler) = AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        if let Some(s) = a.get_mut(cur) {
            s.done = true;
        }
        let op = &a[0];
        (op.fsp, op.handler)
    });
    unsafe {
        wr_cell(USER_FSP, ofsp);
        wr_cell(USER_HANDLER, ohandler);
    }
    let op = OPERATOR.with(|c| c.get());
    CURRENT.with(|c| c.set(0));
    sys::switch_to(op);
    0
}

/// `(agent-done?) ( aid -- flag )` — -1 if the agent has finished (or is unknown).
#[no_mangle]
pub extern "C" fn rt_agent_is_done(aid: u64) -> u64 {
    AGENTS.with(|a| {
        let a = a.borrow();
        match a.get(aid as usize) {
            Some(s) if !s.done => 0,
            _ => u64::MAX, // -1: done or unknown
        }
    })
}

/// `(self) ( -- aid )` — the running agent's id.
#[no_mangle]
pub extern "C" fn rt_agent_self() -> u64 {
    CURRENT.with(|c| c.get()) as u64
}

// ── Per-agent pane binding + output routing (pane-agent GUI, stage 1) ────────
// See docs/design/wf66_pane_agent_gui.md §2/§7. A pane's agent binds itself to a
// child_id; thereafter its output routes to a per-agent buffer the host pump
// flushes to that pane, instead of the one shared session transcript.

/// `(set-pane) ( child_id -- )` — bind the RUNNING agent to a pane `child_id`.
/// A negative value unbinds (output falls back to the shared session I/O).
#[no_mangle]
pub extern "C" fn rt_agent_set_pane(child_id: u64) -> u64 {
    let cur = CURRENT.with(|c| c.get());
    AGENTS.with(|a| {
        if let Some(s) = a.borrow_mut().get_mut(cur) {
            s.child_id = child_id as i64;
        }
    });
    0
}

/// Route output bytes to the running agent's pane buffer when it is BOUND to a
/// pane (`child_id >= 0`). Returns true if consumed here; false means the caller
/// should use the normal session I/O — i.e. the operator (aid 0) or an unbound
/// agent (e.g. the headless round-robin fixtures). Cooperative single-threading
/// makes "the running agent" unambiguous at every write, so no lock is needed.
pub fn route_output(bytes: &[u8]) -> bool {
    let cur = CURRENT.with(|c| c.get());
    if cur == 0 {
        return false; // the operator never routes to a pane
    }
    AGENTS.with(|a| {
        let mut a = a.borrow_mut();
        match a.get_mut(cur) {
            Some(s) if s.child_id >= 0 => {
                s.out.extend_from_slice(bytes);
                true
            }
            _ => false,
        }
    })
}

/// Drain the accumulated output for the agent bound to `child_id` (the pump's
/// flush step; tests read it directly). `None` if no agent is bound to that pane;
/// an empty `Vec` if bound but nothing is buffered.
pub fn take_pane_output(child_id: i64) -> Option<Vec<u8>> {
    if child_id < 0 {
        return None;
    }
    AGENTS.with(|a| {
        a.borrow_mut()
            .iter_mut()
            .find(|s| s.child_id == child_id)
            .map(|s| std::mem::take(&mut s.out))
    })
}

/// The `child_id` agent `aid` is bound to, or -1 if unbound/unknown.
pub fn agent_pane(aid: u64) -> i64 {
    AGENTS.with(|a| a.borrow().get(aid as usize).map_or(-1, |s| s.child_id))
}
