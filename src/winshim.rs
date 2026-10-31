//! The kernel's Win32 and C runtime imports, on Linux (the BOX's /init).
//!
//! `kernel/facility.masm` and `kernel/fmath.masm` import a few functions
//! from KERNEL32.dll and msvcrt.dll and call them the Win64 way. On Linux
//! the loader binds the same names to the functions here, which have the
//! same shapes (and the Win64 convention), so the kernel source is one for
//! both platforms. They go to the host's hooks (f66-host): `allocate` to the
//! RISC OS RMA, files to RISC OS's, `ms` to the host's wait (which yields to
//! the desktop and sees Escape). With no hooks the heap is Rust's and files
//! fail.

#![cfg(not(windows))]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use f66_host::hooks;

const INVALID_HANDLE: u64 = u64::MAX;

/// The function the kernel imports as `name`, if this module has it.
pub fn lookup(name: &str) -> Option<*mut c_void> {
    let f: *const () = match name {
        "Sleep" => Sleep as *const (),
        "GetLocalTime" => GetLocalTime as *const (),
        "QueryPerformanceCounter" => QueryPerformanceCounter as *const (),
        "QueryPerformanceFrequency" => QueryPerformanceFrequency as *const (),
        "GetProcessHeap" => GetProcessHeap as *const (),
        "HeapAlloc" => HeapAlloc as *const (),
        "HeapFree" => HeapFree as *const (),
        "HeapReAlloc" => HeapReAlloc as *const (),
        "CreateFileA" => CreateFileA as *const (),
        "CloseHandle" => CloseHandle as *const (),
        "ReadFile" => ReadFile as *const (),
        "WriteFile" => WriteFile as *const (),
        "DeleteFileA" => DeleteFileA as *const (),
        "SetFilePointerEx" => SetFilePointerEx as *const (),
        "GetFileSizeEx" => GetFileSizeEx as *const (),
        "FlushFileBuffers" => FlushFileBuffers as *const (),
        "MoveFileExA" => MoveFileExA as *const (),
        "sqrt" => sqrt as *const (),
        "sin" => sin as *const (),
        "cos" => cos as *const (),
        "tan" => tan as *const (),
        "exp" => exp as *const (),
        "log" => log as *const (),
        "log10" => log10 as *const (),
        "atan" => atan as *const (),
        "asin" => asin as *const (),
        "acos" => acos as *const (),
        "pow" => pow as *const (),
        "atan2" => atan2 as *const (),
        _ => return None,
    };
    Some(f as *mut c_void)
}

// ── time ────────────────────────────────────────────────────────────────

extern "win64" fn Sleep(ms: u32) {
    match hooks().and_then(|h| h.ms) {
        Some(wait) => wait(ms as u64),
        None => std::thread::sleep(std::time::Duration::from_millis(ms as u64)),
    }
}

/// SYSTEMTIME: year, month, day of week, day, hour, minute, second, ms (UTC).
extern "win64" fn GetLocalTime(st: *mut u16) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    let fields = [
        y,
        m,
        (days + 4).rem_euclid(7),
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        now.subsec_millis() as i64,
    ];
    for (i, v) in fields.iter().enumerate() {
        unsafe { st.add(i).write_unaligned(*v as u16) };
    }
}

fn epoch() -> Instant {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

extern "win64" fn QueryPerformanceCounter(out: *mut i64) -> i32 {
    unsafe { out.write_unaligned(epoch().elapsed().as_nanos() as i64) };
    1
}

extern "win64" fn QueryPerformanceFrequency(out: *mut i64) -> i32 {
    unsafe { out.write_unaligned(1_000_000_000) };
    1
}

// ── the heap: allocate, free, resize ─────────────────────────────────────
//
// Without hooks a block is Rust's, with its size in the 16 bytes before it.

const HEAP_ZERO_MEMORY: u32 = 8;

extern "win64" fn GetProcessHeap() -> u64 {
    1
}

fn layout(n: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(n + 16, 16).unwrap()
}

extern "win64" fn HeapAlloc(_heap: u64, flags: u32, n: usize) -> *mut u8 {
    if let Some(alloc) = hooks().and_then(|h| h.mem_alloc) {
        let p = alloc(n);
        if !p.is_null() && flags & HEAP_ZERO_MEMORY != 0 {
            unsafe { std::ptr::write_bytes(p, 0, n) };
        }
        return p;
    }
    unsafe {
        let p = std::alloc::alloc_zeroed(layout(n));
        if p.is_null() {
            return p;
        }
        (p as *mut usize).write(n);
        p.add(16)
    }
}

extern "win64" fn HeapFree(_heap: u64, _flags: u32, p: *mut u8) -> i32 {
    if p.is_null() {
        return 1;
    }
    if let Some(free) = hooks().and_then(|h| h.mem_free) {
        free(p);
        return 1;
    }
    unsafe {
        let base = p.sub(16);
        std::alloc::dealloc(base, layout((base as *const usize).read()));
    }
    1
}

extern "win64" fn HeapReAlloc(_heap: u64, _flags: u32, p: *mut u8, n: usize) -> *mut u8 {
    if let Some(resize) = hooks().and_then(|h| h.mem_resize) {
        return resize(p, n);
    }
    unsafe {
        let base = p.sub(16);
        let q = std::alloc::realloc(base, layout((base as *const usize).read()), n + 16);
        if q.is_null() {
            return q;
        }
        (q as *mut usize).write(n);
        q.add(16)
    }
}

// ── files, by the host's names ───────────────────────────────────────────

fn cstr<'a>(p: *const u8) -> &'a [u8] {
    if p.is_null() {
        return &[];
    }
    unsafe { std::ffi::CStr::from_ptr(p as *const std::ffi::c_char).to_bytes() }
}

/// CreateFileA: CREATE_ALWAYS (2) creates; OPEN_EXISTING (3) opens to read,
/// write or update by the access mask.
extern "win64" fn CreateFileA(
    name: *const u8,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    _flags: u32,
    _template: u64,
) -> u64 {
    let Some(open) = hooks().and_then(|h| h.file_open) else {
        return INVALID_HANDLE;
    };
    let mode = if disposition == 2 {
        4
    } else {
        match access & 0xC000_0000 {
            0x8000_0000 => 1,
            0x4000_0000 => 2,
            _ => 3,
        }
    };
    let n = cstr(name);
    match open(n.as_ptr(), n.len(), mode) {
        0 => INVALID_HANDLE,
        h => h,
    }
}

extern "win64" fn CloseHandle(h: u64) -> i32 {
    match hooks().and_then(|k| k.file_close) {
        Some(close) => (close(h) == 0) as i32,
        None => 0,
    }
}

extern "win64" fn ReadFile(h: u64, buf: *mut u8, n: u32, got: *mut u32, _ov: *mut c_void) -> i32 {
    let Some(read) = hooks().and_then(|k| k.file_read) else {
        return 0;
    };
    let r = read(h, buf, n as usize);
    if !got.is_null() {
        unsafe { got.write_unaligned(r.max(0) as u32) };
    }
    (r >= 0) as i32
}

extern "win64" fn WriteFile(h: u64, buf: *const u8, n: u32, put: *mut u32, _ov: *mut c_void) -> i32 {
    let Some(write) = hooks().and_then(|k| k.file_write) else {
        return 0;
    };
    let r = write(h, buf, n as usize);
    if !put.is_null() {
        unsafe { put.write_unaligned(r.max(0) as u32) };
    }
    (r >= 0) as i32
}

extern "win64" fn DeleteFileA(name: *const u8) -> i32 {
    let n = cstr(name);
    match hooks().and_then(|k| k.file_delete) {
        Some(del) => (del(n.as_ptr(), n.len()) == 0) as i32,
        None => 0,
    }
}

/// SetFilePointerEx: method 0 from the start, 1 from here, 2 from the end.
extern "win64" fn SetFilePointerEx(h: u64, dist: i64, new: *mut i64, method: u32) -> i32 {
    let Some(k) = hooks() else { return 0 };
    let (Some(ptr), Some(ext)) = (k.file_ptr, k.file_extent) else {
        return 0;
    };
    let base = match method {
        0 => 0,
        1 => ptr(h, 0, 0),
        _ => ext(h),
    };
    if base < 0 || ptr(h, 1, base + dist) < 0 {
        return 0;
    }
    if !new.is_null() {
        unsafe { new.write_unaligned(ptr(h, 0, 0)) };
    }
    1
}

extern "win64" fn GetFileSizeEx(h: u64, out: *mut i64) -> i32 {
    match hooks().and_then(|k| k.file_extent) {
        Some(ext) => {
            let n = ext(h);
            if n < 0 {
                return 0;
            }
            unsafe { out.write_unaligned(n) };
            1
        }
        None => 0,
    }
}

extern "win64" fn FlushFileBuffers(_h: u64) -> i32 {
    1
}

extern "win64" fn MoveFileExA(a: *const u8, b: *const u8, _flags: u32) -> i32 {
    let (a, b) = (cstr(a), cstr(b));
    match hooks().and_then(|k| k.file_rename) {
        Some(ren) => (ren(a.as_ptr(), a.len(), b.as_ptr(), b.len()) == 0) as i32,
        None => 0,
    }
}

// ── msvcrt's maths ───────────────────────────────────────────────────────

extern "win64" fn sqrt(x: f64) -> f64 { x.sqrt() }
extern "win64" fn sin(x: f64) -> f64 { x.sin() }
extern "win64" fn cos(x: f64) -> f64 { x.cos() }
extern "win64" fn tan(x: f64) -> f64 { x.tan() }
extern "win64" fn exp(x: f64) -> f64 { x.exp() }
extern "win64" fn log(x: f64) -> f64 { x.ln() }
extern "win64" fn log10(x: f64) -> f64 { x.log10() }
extern "win64" fn atan(x: f64) -> f64 { x.atan() }
extern "win64" fn asin(x: f64) -> f64 { x.asin() }
extern "win64" fn acos(x: f64) -> f64 { x.acos() }
extern "win64" fn pow(x: f64, y: f64) -> f64 { x.powf(y) }
extern "win64" fn atan2(y: f64, x: f64) -> f64 { y.atan2(x) }
