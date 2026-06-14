//! Headless offscreen Direct2D surface — render `SurfaceCmd`/`PaneBatch`
//! drawings to an in-memory BGRA bitmap and save them as PNG, with NO window.
//!
//! Why this exists:
//!   * **Useful on its own** — batch-render any gpane drawing (the same
//!     `SurfaceCmd` stream the GUI panes consume) to a file: docs art, CI
//!     image-diff baselines, thumbnails, "render this Forth picture to PNG".
//!   * **A test surface for the cooperative-agents work** — it stands up the
//!     full Direct2D draw pipeline off the GUI thread (WARP-capable), so the
//!     D2D draw path can be exercised from a fiber / worker thread headlessly.
//!
//! It is independent of the GUI `renderer::ctx()` (which is bound to a swap
//! chain on the GUI thread): this owns its own `D3dContext` + `D2dContext` and
//! two D2D bitmaps — a `TARGET` to draw into and a `CPU_READ` one to map for
//! readback — so it works in a plain `cargo run` / `#[test]`.
//!
//! Scope: the geometry primitives the demos use (clear / fill+stroke rect /
//! line / oval / circle / blit). Text, paths and the Phase-5 composition
//! commands are ignored with a note — add them here if a headless drawing needs
//! them. See `src/igui/executor.rs` for the GUI-thread equivalent.

#![cfg(windows)]

use std::collections::HashMap;

use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap1, ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_CPU_READ,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_ELLIPSE,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_MAP_OPTIONS_READ, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows_numerics::Vector2;

use super::batch::{PaneBatch, Rgba, SurfaceCmd};
use super::d2d::D2dContext;
use super::d3d::D3dContext;
use super::IGuiError;

/// An offscreen Direct2D render surface of fixed pixel size.
pub struct OffscreenSurface {
    width: u32,
    height: u32,
    /// Held alive: the D2D device sits on this D3D device. Not touched directly.
    _d3d: D3dContext,
    d2d: D2dContext,
    /// The render target bitmap (drawn into).
    target: ID2D1Bitmap1,
    /// A CPU-readable bitmap; `CopyFromBitmap(target)` then `Map` reads pixels.
    readback: ID2D1Bitmap1,
    /// Solid-color brush cache keyed on packed 8-bit RGBA.
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
}

impl OffscreenSurface {
    /// Create a `width`×`height` offscreen surface. Builds its own D3D11
    /// device (hardware, WARP fallback) and Direct2D device context.
    pub fn new(width: u32, height: u32) -> Result<Self, IGuiError> {
        let width = width.max(1);
        let height = height.max(1);

        let d3d = D3dContext::new()?;
        let d2d = D2dContext::new(&d3d)?;
        let size = D2D_SIZE_U { width, height };
        let fmt = D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        };

        // Render target bitmap.
        let target_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: fmt,
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let target = unsafe { d2d.context.CreateBitmap(size, None, 0, &target_props) }
            .map_err(|e| IGuiError::D2D(format!("CreateBitmap(target) failed: {e}")))?;

        // CPU-readable copy target.
        let readback_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: fmt,
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let readback = unsafe { d2d.context.CreateBitmap(size, None, 0, &readback_props) }
            .map_err(|e| IGuiError::D2D(format!("CreateBitmap(readback) failed: {e}")))?;

        unsafe { d2d.context.SetTarget(&target) };

        Ok(Self {
            width,
            height,
            _d3d: d3d,
            d2d,
            target,
            readback,
            brushes: HashMap::new(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Render one `PaneBatch` (BeginDraw … cmds … EndDraw). Text, path and
    /// Phase-5 composition commands are skipped (see module note).
    pub fn render(&mut self, batch: &PaneBatch) -> Result<(), IGuiError> {
        let ctx = self.d2d.context.clone();
        unsafe { ctx.BeginDraw() };
        for cmd in &batch.cmds {
            self.exec_cmd(cmd)?;
        }
        unsafe { ctx.EndDraw(None, None) }
            .map_err(|e| IGuiError::D2D(format!("EndDraw failed: {e}")))?;
        Ok(())
    }

    /// Clear the whole surface to a solid color (its own BeginDraw/EndDraw).
    pub fn clear(&mut self, color: Rgba) -> Result<(), IGuiError> {
        let ctx = self.d2d.context.clone();
        unsafe {
            ctx.BeginDraw();
            ctx.Clear(Some(&d2d_color(color)));
            ctx.EndDraw(None, None)
                .map_err(|e| IGuiError::D2D(format!("EndDraw(clear) failed: {e}")))?;
        }
        Ok(())
    }

    fn brush(&mut self, color: Rgba) -> Result<ID2D1SolidColorBrush, IGuiError> {
        let key = pack_rgba8(color);
        if let Some(b) = self.brushes.get(&key) {
            return Ok(b.clone());
        }
        let brush = unsafe { self.d2d.context.CreateSolidColorBrush(&d2d_color(color), None) }
            .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush failed: {e}")))?;
        self.brushes.insert(key, brush.clone());
        Ok(brush)
    }

    fn exec_cmd(&mut self, cmd: &SurfaceCmd) -> Result<(), IGuiError> {
        let ctx = self.d2d.context.clone();
        match cmd {
            SurfaceCmd::Clear { color } => unsafe { ctx.Clear(Some(&d2d_color(*color))) },
            SurfaceCmd::PresentHint => {} // no swap chain offscreen
            SurfaceCmd::FillRect { rect, corner_radius, color } => {
                let brush = self.brush(*color)?;
                let r = rect_f(rect);
                if *corner_radius <= 0.0 {
                    unsafe { ctx.FillRectangle(&r, &brush) };
                } else {
                    let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: *corner_radius, radiusY: *corner_radius };
                    unsafe { ctx.FillRoundedRectangle(&rr, &brush) };
                }
            }
            SurfaceCmd::StrokeRect { rect, corner_radius, half_thickness, color } => {
                let brush = self.brush(*color)?;
                let r = rect_f(rect);
                let w = (2.0 * half_thickness).max(0.0);
                if *corner_radius <= 0.0 {
                    unsafe { ctx.DrawRectangle(&r, &brush, w, None) };
                } else {
                    let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: *corner_radius, radiusY: *corner_radius };
                    unsafe { ctx.DrawRoundedRectangle(&rr, &brush, w, None) };
                }
            }
            SurfaceCmd::DrawLine { p0, p1, half_thickness, color } => {
                let brush = self.brush(*color)?;
                let w = (2.0 * half_thickness).max(0.0);
                unsafe {
                    ctx.DrawLine(
                        Vector2 { X: p0.x, Y: p0.y },
                        Vector2 { X: p1.x, Y: p1.y },
                        &brush, w, None,
                    )
                };
            }
            SurfaceCmd::FillOval { rect, color } => {
                let brush = self.brush(*color)?;
                unsafe { ctx.FillEllipse(&ellipse_from_rect(rect), &brush) };
            }
            SurfaceCmd::FillCircle { center, radius, color } => {
                let brush = self.brush(*color)?;
                let e = D2D1_ELLIPSE { point: Vector2 { X: center.x, Y: center.y }, radiusX: *radius, radiusY: *radius };
                unsafe { ctx.FillEllipse(&e, &brush) };
            }
            SurfaceCmd::StrokeOval { rect, half_thickness, color } => {
                let brush = self.brush(*color)?;
                let w = (2.0 * half_thickness).max(0.0);
                unsafe { ctx.DrawEllipse(&ellipse_from_rect(rect), &brush, w, None) };
            }
            SurfaceCmd::StrokeCircle { center, radius, half_thickness, color } => {
                let brush = self.brush(*color)?;
                let e = D2D1_ELLIPSE { point: Vector2 { X: center.x, Y: center.y }, radiusX: *radius, radiusY: *radius };
                let w = (2.0 * half_thickness).max(0.0);
                unsafe { ctx.DrawEllipse(&e, &brush, w, None) };
            }
            SurfaceCmd::Blit { x, y, w, h, pixels } => {
                // Upload the CPU BGRA framebuffer as a D2D bitmap, draw it once.
                let size = D2D_SIZE_U { width: *w, height: *h };
                let bprops = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                    colorContext: std::mem::ManuallyDrop::new(None),
                };
                let bitmap: ID2D1Bitmap1 = unsafe {
                    ctx.CreateBitmap(size, Some(pixels.as_ptr() as *const _), *w * 4, &bprops)
                }
                .map_err(|e| IGuiError::D2D(format!("CreateBitmap(blit) failed: {e}")))?;
                let dst = D2D_RECT_F { left: *x, top: *y, right: *x + *w as f32, bottom: *y + *h as f32 };
                unsafe { ctx.DrawBitmap(&bitmap, Some(&dst), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None) };
            }
            // Not implemented for headless rendering yet — drawn as no-ops so
            // a batch carrying them still renders its geometry.
            _ => {}
        }
        Ok(())
    }

    /// Copy the rendered image out as tightly-packed BGRA8 (`width*height*4`),
    /// top row first.
    pub fn read_bgra(&self) -> Result<Vec<u8>, IGuiError> {
        unsafe {
            self.readback
                .CopyFromBitmap(None, &self.target, None)
                .map_err(|e| IGuiError::D2D(format!("CopyFromBitmap failed: {e}")))?;
            let mapped = self
                .readback
                .Map(D2D1_MAP_OPTIONS_READ)
                .map_err(|e| IGuiError::D2D(format!("Map(readback) failed: {e}")))?;

            let row_bytes = (self.width * 4) as usize;
            let mut out = vec![0u8; row_bytes * self.height as usize];
            for y in 0..self.height as usize {
                let src = mapped.bits.add(y * mapped.pitch as usize);
                let dst = out.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(src, dst, row_bytes);
            }
            let _ = self.readback.Unmap();
            Ok(out)
        }
    }

    /// Encode the current image as PNG bytes (8-bit RGBA).
    pub fn to_png(&self) -> Result<Vec<u8>, IGuiError> {
        let bgra = self.read_bgra()?;
        let mut rgba = vec![0u8; bgra.len()];
        for i in (0..bgra.len()).step_by(4) {
            rgba[i] = bgra[i + 2]; // R <- B
            rgba[i + 1] = bgra[i + 1]; // G
            rgba[i + 2] = bgra[i]; // B <- R
            rgba[i + 3] = 255; // opaque (target is IGNORE-alpha)
        }
        Ok(png::encode_rgba8(self.width, self.height, &rgba))
    }

    /// Render-and-save convenience: encode to PNG and write `path`.
    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> Result<(), IGuiError> {
        let bytes = self.to_png()?;
        std::fs::write(path.as_ref(), bytes)
            .map_err(|e| IGuiError::Win32(format!("write {}: {e}", path.as_ref().display())))
    }
}

fn d2d_color(c: Rgba) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r, g: c.g, b: c.b, a: c.a }
}

fn rect_f(r: &super::batch::Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: r.x0, top: r.y0, right: r.x1, bottom: r.y1 }
}

fn ellipse_from_rect(rect: &super::batch::Rect) -> D2D1_ELLIPSE {
    let cx = 0.5 * (rect.x0 + rect.x1);
    let cy = 0.5 * (rect.y0 + rect.y1);
    let rx = 0.5 * (rect.x1 - rect.x0).abs();
    let ry = 0.5 * (rect.y1 - rect.y0).abs();
    D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rx, radiusY: ry }
}

fn pack_rgba8(c: Rgba) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    q(c.r) | (q(c.g) << 8) | (q(c.b) << 16) | (q(c.a) << 24)
}

// ── Dependency-free PNG encoder (8-bit RGBA, stored/uncompressed DEFLATE) ────
//
// Small and self-contained so the offscreen renderer needs no image crate. Not
// optimized for size (uses uncompressed zlib blocks) — fine for tooling output.
mod png {
    pub fn encode_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        // IHDR
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.push(8); // bit depth
        ihdr.push(6); // color type: RGBA
        ihdr.push(0); // compression
        ihdr.push(0); // filter
        ihdr.push(0); // interlace
        write_chunk(&mut out, b"IHDR", &ihdr);

        // Filtered raw scanlines: each row prefixed with filter byte 0 (None).
        let row_len = width as usize * 4;
        let mut raw = Vec::with_capacity((row_len + 1) * height as usize);
        for y in 0..height as usize {
            raw.push(0);
            raw.extend_from_slice(&rgba[y * row_len..(y + 1) * row_len]);
        }

        let idat = zlib_store(&raw);
        write_chunk(&mut out, b"IDAT", &idat);
        write_chunk(&mut out, b"IEND", &[]);
        out
    }

    fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(&crc.finish().to_be_bytes());
    }

    /// zlib stream wrapping DEFLATE stored (uncompressed) blocks.
    fn zlib_store(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x78); // CMF: deflate, 32K window
        out.push(0x01); // FLG: no dict, fastest; (0x78*256+0x01) % 31 == 0
        let mut i = 0;
        loop {
            let remaining = data.len() - i;
            let block = remaining.min(0xFFFF);
            let final_block = i + block >= data.len();
            out.push(if final_block { 1 } else { 0 }); // BFINAL, BTYPE=00 (stored)
            out.extend_from_slice(&(block as u16).to_le_bytes());
            out.extend_from_slice(&(!(block as u16)).to_le_bytes());
            out.extend_from_slice(&data[i..i + block]);
            i += block;
            if final_block {
                break;
            }
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    fn adler32(data: &[u8]) -> u32 {
        const MOD: u32 = 65521;
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % MOD;
            b = (b + a) % MOD;
        }
        (b << 16) | a
    }

    struct Crc {
        value: u32,
    }
    impl Crc {
        fn new() -> Self {
            Self { value: 0xFFFF_FFFF }
        }
        fn update(&mut self, data: &[u8]) {
            for &byte in data {
                let idx = ((self.value ^ byte as u32) & 0xFF) as usize;
                self.value = CRC_TABLE[idx] ^ (self.value >> 8);
            }
        }
        fn finish(self) -> u32 {
            self.value ^ 0xFFFF_FFFF
        }
    }

    static CRC_TABLE: [u32; 256] = build_crc_table();

    const fn build_crc_table() -> [u32; 256] {
        let mut table = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            table[n] = c;
            n += 1;
        }
        table
    }
}
