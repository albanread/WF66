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
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// Address of the kernel `agent_trampoline` proc (a fiber start routine).
static TRAMPOLINE: AtomicU64 = AtomicU64::new(0);
/// The single user-area base (UP); one shared user area across all agents.
static USER_BASE: AtomicU64 = AtomicU64::new(0);

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
}

// Per-agent Forth stack sizes (u64 cells).
const DS_CELLS: usize = 8 * 1024; // 64 KB data stack
const LS_CELLS: usize = 8 * 1024; // 64 KB locals stack
const FP_CELLS: usize = 4 * 1024; // 32 KB FP stack
const FIBER_STACK: usize = 512 * 1024; // native return stack per fiber

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
            _ds: Vec::new(),
            _ls: Vec::new(),
            _fp: Vec::new(),
        });
    });
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
    let _fp_top = fp.as_mut_ptr() as u64 + (FP_CELLS * 8) as u64;
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
    CURRENT.with(|c| c.set(t));
    sys::switch_to(fiber);
    0
}

/// Called by the trampoline when an agent's entry word returns: mark it done and
/// switch back to the operator. Never returns to the agent.
#[no_mangle]
pub extern "C" fn rt_agent_done() -> u64 {
    let cur = CURRENT.with(|c| c.get());
    AGENTS.with(|a| {
        if let Some(s) = a.borrow_mut().get_mut(cur) {
            s.done = true;
        }
    });
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
