//! `gfx-render` — headless Direct2D image renderer.
//!
//! Renders a drawing to a PNG with NO window, using `wf64::igui::offscreen`
//! (its own WARP-capable D3D11 + Direct2D pipeline). Useful for docs art, CI
//! image baselines, or batch-rendering Forth pictures; it also doubles as a
//! headless exercise of the D2D draw path (optionally from a Win32 fiber).
//!
//! Usage:
//!   cargo run --bin gfx-render -- [--scene mandel|shapes] [--size WxH]
//!                                  [--blk N] [--iter N] [--out FILE] [--fiber]
//!
//! Examples:
//!   cargo run --bin gfx-render                       # 480x360 Mandelbrot -> offscreen-mandel.png
//!   cargo run --bin gfx-render -- --scene shapes --out shapes.png
//!   cargo run --bin gfx-render -- --size 960x720 --blk 1 --out big.png
//!   cargo run --bin gfx-render -- --fiber            # render from a fiber (D2D-on-fiber probe)

#[cfg(not(windows))]
fn main() {
    eprintln!("gfx-render is Windows-only (Direct2D).");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    use wf64::igui::batch::PaneBatch;
    use wf64::igui::offscreen::OffscreenSurface;

    let mut scene = "mandel".to_string();
    let mut out: Option<String> = None;
    let mut width = 480u32;
    let mut height = 360u32;
    let mut blk = 2u32;
    let mut maxiter = 64u32;
    let mut fiber = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--scene" => { i += 1; scene = args.get(i).cloned().unwrap_or(scene); }
            "--out" => { i += 1; out = args.get(i).cloned(); }
            "--blk" => { i += 1; blk = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(blk).max(1); }
            "--iter" => { i += 1; maxiter = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(maxiter).max(1); }
            "--fiber" => fiber = true,
            "--size" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    if let Some((w, h)) = s.split_once(['x', 'X']) {
                        width = w.parse().unwrap_or(width).max(1);
                        height = h.parse().unwrap_or(height).max(1);
                    }
                }
            }
            other => eprintln!("[gfx-render] ignoring unknown arg {other:?}"),
        }
        i += 1;
    }

    let out = out.unwrap_or_else(|| format!("offscreen-{scene}.png"));

    let batch: PaneBatch = match scene.as_str() {
        "shapes" => build_shapes(width, height),
        "mandel" => build_mandel(width, height, blk, maxiter),
        other => {
            eprintln!("[gfx-render] unknown scene {other:?}; use mandel|shapes");
            std::process::exit(2);
        }
    };
    println!(
        "== gfx-render: scene={scene} size={width}x{height} cmds={} fiber={fiber} -> {out} ==",
        batch.cmds.len()
    );

    let mut surface = match OffscreenSurface::new(width, height) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[gfx-render] could not create offscreen surface: {e}");
            std::process::exit(1);
        }
    };

    let render_result = if fiber {
        render_on_fiber(&mut surface, &batch)
    } else {
        surface.render(&batch).map_err(|e| e.to_string())
    };
    if let Err(e) = render_result {
        eprintln!("[gfx-render] render failed: {e}");
        std::process::exit(1);
    }

    match surface.save_png(&out) {
        Ok(()) => {
            let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("[gfx-render] wrote {out} ({bytes} bytes, {width}x{height} RGBA PNG)");
        }
        Err(e) => {
            eprintln!("[gfx-render] save failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Build a Mandelbrot as a batch of block FillRects — the same shape the
/// `gfx-mandelbrot.f` / `lm-agent` demos emit through gpane-fill-rect.
#[cfg(windows)]
fn build_mandel(width: u32, height: u32, blk: u32, maxiter: u32) -> wf64::igui::batch::PaneBatch {
    use wf64::igui::batch::{PaneBatch, Rect, Rgba, SurfaceCmd};

    // 16-step escape-time palette (deep navy -> ice-white -> amber -> black),
    // mirroring demos/gfx-mandel-live.f `lm-colour`.
    const PAL: [u32; 16] = [
        0x0D1540, 0x102B80, 0x1558C8, 0x3A8CF5, 0x7DC8FF, 0xB8EEFF, 0xFFFFFF, 0xFFF4A8,
        0xFFCC57, 0xFFA000, 0xFF6800, 0xE83800, 0xAA1200, 0x650000, 0x280000, 0x080010,
    ];
    let rgb = |hex: u32| Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    };

    let cells_x = (width / blk).max(1);
    let cells_y = (height / blk).max(1);
    let dx = 3.5 / cells_x as f32;
    let dy = 2.5 / cells_y as f32;

    let mut cmds = Vec::with_capacity((cells_x * cells_y) as usize + 1);
    cmds.push(SurfaceCmd::Clear { color: rgb(0x000000) });

    for py in 0..cells_y {
        let c_im = py as f32 * dy - 1.25;
        for px in 0..cells_x {
            let c_re = px as f32 * dx - 2.5;
            let (mut zr, mut zi) = (0.0f32, 0.0f32);
            let mut n = 0u32;
            while n < maxiter {
                let (zr2, zi2) = (zr * zr, zi * zi);
                if zr2 + zi2 > 4.0 {
                    break;
                }
                zi = 2.0 * zr * zi + c_im;
                zr = zr2 - zi2 + c_re;
                n += 1;
            }
            let color = if n >= maxiter { rgb(0x000000) } else { rgb(PAL[(n & 15) as usize]) };
            let (x0, y0) = ((px * blk) as f32, (py * blk) as f32);
            cmds.push(SurfaceCmd::FillRect {
                rect: Rect { x0, y0, x1: x0 + blk as f32, y1: y0 + blk as f32 },
                corner_radius: 0.0,
                color,
            });
        }
    }
    PaneBatch { child_id: 0, sequence: 1, flags: 0, cmds }
}

/// A primitive showcase: every geometry op the offscreen executor supports.
#[cfg(windows)]
fn build_shapes(width: u32, height: u32) -> wf64::igui::batch::PaneBatch {
    use wf64::igui::batch::{PaneBatch, Point, Rect, Rgba, SurfaceCmd};
    let rgba = |r, g, b| Rgba { r, g, b, a: 1.0 };
    let (w, h) = (width as f32, height as f32);
    let cmds = vec![
        SurfaceCmd::Clear { color: rgba(0.06, 0.08, 0.12) },
        SurfaceCmd::FillRect {
            rect: Rect { x0: 0.08 * w, y0: 0.10 * h, x1: 0.42 * w, y1: 0.42 * h },
            corner_radius: 12.0,
            color: rgba(0.23, 0.55, 0.96),
        },
        SurfaceCmd::StrokeRect {
            rect: Rect { x0: 0.08 * w, y0: 0.10 * h, x1: 0.42 * w, y1: 0.42 * h },
            corner_radius: 12.0,
            half_thickness: 1.5,
            color: rgba(1.0, 1.0, 1.0),
        },
        SurfaceCmd::FillCircle { center: Point { x: 0.72 * w, y: 0.27 * h }, radius: 0.15 * h, color: rgba(1.0, 0.63, 0.0) },
        SurfaceCmd::StrokeCircle { center: Point { x: 0.72 * w, y: 0.27 * h }, radius: 0.15 * h, half_thickness: 2.0, color: rgba(1.0, 1.0, 1.0) },
        SurfaceCmd::FillOval { rect: Rect { x0: 0.10 * w, y0: 0.60 * h, x1: 0.45 * w, y1: 0.85 * h }, color: rgba(0.90, 0.22, 0.0) },
        SurfaceCmd::DrawLine { p0: Point { x: 0.55 * w, y: 0.62 * h }, p1: Point { x: 0.92 * w, y: 0.88 * h }, half_thickness: 2.0, color: rgba(0.72, 0.93, 1.0) },
        SurfaceCmd::DrawLine { p0: Point { x: 0.55 * w, y: 0.88 * h }, p1: Point { x: 0.92 * w, y: 0.62 * h }, half_thickness: 2.0, color: rgba(0.72, 0.93, 1.0) },
    ];
    PaneBatch { child_id: 0, sequence: 1, flags: 0, cmds }
}

/// Run `surface.render(batch)` from inside a Win32 fiber (the calling thread is
/// converted to a fiber, a child fiber does the D2D draw work, then switches
/// back). A headless probe of "does the D2D draw path run on a fiber stack?".
#[cfg(windows)]
fn render_on_fiber(
    surface: &mut wf64::igui::offscreen::OffscreenSurface,
    batch: &wf64::igui::batch::PaneBatch,
) -> Result<(), String> {
    use std::ffi::c_void;
    use wf64::igui::batch::PaneBatch;
    use wf64::igui::offscreen::OffscreenSurface;
    use windows::Win32::System::Threading::{
        ConvertFiberToThread, ConvertThreadToFiber, CreateFiber, SwitchToFiber,
    };

    struct Job {
        main_fiber: *mut c_void,
        surface: *mut OffscreenSurface,
        batch: *const PaneBatch,
        result: *mut Result<(), String>,
    }

    unsafe extern "system" fn run(param: *mut c_void) {
        let job = &*(param as *const Job);
        let res = (*job.surface).render(&*job.batch).map_err(|e| e.to_string());
        *job.result = res;
        // Cooperative hand-back; this fiber never "returns".
        SwitchToFiber(job.main_fiber);
        loop {
            core::hint::spin_loop();
        }
    }

    unsafe {
        let main_fiber = ConvertThreadToFiber(None);
        if main_fiber.is_null() {
            return Err("ConvertThreadToFiber failed (already a fiber?)".into());
        }
        let mut result: Result<(), String> = Err("fiber did not run".into());
        let job = Job {
            main_fiber,
            surface: surface as *mut _,
            batch: batch as *const _,
            result: &mut result as *mut _,
        };
        let worker = CreateFiber(1 << 20, Some(run), Some(&job as *const Job as *const c_void));
        if worker.is_null() {
            let _ = ConvertFiberToThread();
            return Err("CreateFiber failed".into());
        }
        println!("[gfx-render] switching to render fiber…");
        SwitchToFiber(worker);
        // Back on the main fiber: the worker is parked at its spin loop.
        let _ = ConvertFiberToThread();
        println!("[gfx-render] render fiber returned control cleanly.");
        result
    }
}
