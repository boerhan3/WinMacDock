//! Shared GPU device (D3D11 + Direct2D + DirectWrite) and per-window
//! DirectComposition surfaces with premultiplied alpha.

use std::collections::HashMap;

use windows::core::{w, Interface, Result};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_numerics::Matrix3x2;

use crate::util::rgba;

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub d3dctx: ID3D11DeviceContext,
    pub factory: IDXGIFactory2,
    pub dc: ID2D1DeviceContext,
    pub dwrite: IDWriteFactory,
    pub dcomp: IDCompositionDevice,
    brush: ID2D1SolidColorBrush,
    formats: HashMap<(u32, i32, bool), IDWriteTextFormat>,
}

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut d3d = None;
            let mut d3dctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // VIDEO_SUPPORT lets the video wallpaper decode straight into our textures.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                Some(&mut d3dctx),
            )?;
            let d3d: ID3D11Device = d3d.unwrap();
            let d3dctx: ID3D11DeviceContext = d3dctx.unwrap();
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;

            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi)?;
            let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetUnitMode(D2D1_UNIT_MODE_PIXELS);

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let brush = dc.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)?;

            Ok(Self { d3d, d3dctx, factory, dc, dwrite, dcomp, brush, formats: HashMap::new() })
        }
    }

    fn brush(&self, c: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c) };
        &self.brush
    }

    pub fn fill_rect(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe { self.dc.FillRectangle(&r, self.brush(c)) }
    }

    pub fn fill_rrect(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.dc.FillRoundedRectangle(&rr, self.brush(c)) }
    }

    pub fn stroke_rrect(&self, r: D2D_RECT_F, radius: f32, width: f32, c: D2D1_COLOR_F) {
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.dc.DrawRoundedRectangle(&rr, self.brush(c), width, None) }
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, radius: f32, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: windows_numerics::Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
        unsafe { self.dc.FillEllipse(&e, self.brush(c)) }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: D2D1_COLOR_F) {
        use windows_numerics::Vector2;
        unsafe {
            self.dc.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, self.brush(c), width, None)
        }
    }

    pub fn bitmap(&self, bmp: &ID2D1Bitmap, r: D2D_RECT_F, opacity: f32) {
        unsafe {
            self.dc.DrawBitmap(bmp, Some(&r), opacity, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
        }
    }

    /// Draw the `src` part of a bitmap into `dest` (sub-pixel positions, smooth edges).
    pub fn bitmap_part(&self, bmp: &ID2D1Bitmap, dest: D2D_RECT_F, src: D2D_RECT_F, opacity: f32) {
        unsafe {
            self.dc.DrawBitmap(bmp, Some(&dest), opacity, D2D1_INTERPOLATION_MODE_LINEAR, Some(&src), None);
        }
    }

    /// Rounded rectangle filled with a diagonal gradient through `colors`.
    pub fn gradient_rrect(&self, r: D2D_RECT_F, radius: f32, colors: &[D2D1_COLOR_F]) {
        use windows_numerics::Vector2;
        let n = colors.len().max(2) as f32 - 1.0;
        let stops: Vec<D2D1_GRADIENT_STOP> = colors.iter().enumerate().map(|(i, c)| D2D1_GRADIENT_STOP { position: i as f32 / n, color: *c }).collect();
        unsafe {
            let rt: &ID2D1RenderTarget = &self.dc;
            let Ok(coll) = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: r.left, Y: r.top },
                endPoint: Vector2 { X: r.right, Y: r.bottom },
            };
            let Ok(brush) = rt.CreateLinearGradientBrush(&props, None, &coll) else { return };
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    pub fn push_clip(&self, r: D2D_RECT_F) {
        unsafe { self.dc.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED) }
    }

    pub fn pop_clip(&self) {
        unsafe { self.dc.PopAxisAlignedClip() }
    }

    /// Scale everything drawn afterwards around a pivot point.
    pub fn set_scale(&self, scale: f32, px: f32, py: f32) {
        let m = Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: px - px * scale, M32: py - py * scale };
        unsafe { self.dc.SetTransform(&m) }
    }

    pub fn reset_transform(&self) {
        unsafe { self.dc.SetTransform(&Matrix3x2::identity()) }
    }

    /// Cached text format. `weight` is a DWRITE_FONT_WEIGHT value (400, 600, ...).
    pub fn format(&mut self, size: f32, weight: i32) -> IDWriteTextFormat {
        self.format_ex(size, weight, false)
    }

    ///  selects the Segoe Fluent Icons symbol font.
    fn format_ex(&mut self, size: f32, weight: i32, icons: bool) -> IDWriteTextFormat {
        let key = ((size * 10.0) as u32, weight, icons);
        if let Some(f) = self.formats.get(&key) {
            return f.clone();
        }
        let f = unsafe {
            self.dwrite
                .CreateTextFormat(
                    if icons { w!("Segoe Fluent Icons") } else { w!("Segoe UI Variable Text") },
                    None,
                    DWRITE_FONT_WEIGHT(weight),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )
                .expect("text format")
        };
        unsafe {
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            // Too-long text ends in an ellipsis instead of being cut mid-letter.
            if let Ok(sign) = self.dwrite.CreateEllipsisTrimmingSign(&f) {
                let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                let _ = f.SetTrimming(&trim, &sign);
            }
        }
        self.formats.insert(key, f.clone());
        f
    }

    /// Draw one symbol from the system icon font, centered in .
    pub fn glyph(&mut self, code: char, size: f32, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let f = self.format_ex(size, 400, true);
        let mut buf = [0u16; 2];
        let text = code.encode_utf16(&mut buf);
        unsafe {
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            self.dc.DrawText(text, &f, &r, self.brush(c), D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    /// Draw single-line text. `align`: -1 left, 0 center, 1 right. Vertically centered in `r`.
    pub fn text(&mut self, s: &str, size: f32, weight: i32, r: D2D_RECT_F, align: i32, c: D2D1_COLOR_F) {
        let f = self.format(size, weight);
        let text: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let _ = f.SetTextAlignment(match align {
                0 => DWRITE_TEXT_ALIGNMENT_CENTER,
                1 => DWRITE_TEXT_ALIGNMENT_TRAILING,
                _ => DWRITE_TEXT_ALIGNMENT_LEADING,
            });
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            self.dc.DrawText(
                &text,
                &f,
                &r,
                self.brush(c),
                D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Width of single-line text in pixels.
    pub fn text_width(&mut self, s: &str, size: f32, weight: i32) -> f32 {
        let f = self.format(size, weight);
        let text: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dwrite.CreateTextLayout(&text, &f, 10000.0, 100.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    /// Upload premultiplied BGRA pixels as a bitmap.
    pub fn create_bitmap(&self, w: u32, h: u32, bgra: &[u8]) -> Option<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        unsafe {
            let rt: &ID2D1RenderTarget = &self.dc;
            rt.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(bgra.as_ptr() as *const _), w * 4, &props)
                .ok()
        }
    }
}

/// A swap chain bound to a window through DirectComposition.
pub struct Surface {
    pub swap: IDXGISwapChain1,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    effect: Option<IDCompositionEffectGroup>,
    bmp: Option<ID2D1Bitmap1>,
    pub w: u32,
    pub h: u32,
}

impl Surface {
    pub fn new(gfx: &Gfx, hwnd: HWND, w: u32, h: u32) -> Result<Self> {
        let (w, h) = (w.max(1), h.max(1));
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap = gfx.factory.CreateSwapChainForComposition(&gfx.d3d, &desc, None)?;
            let target = gfx.dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = gfx.dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            gfx.dcomp.Commit()?;
            let mut s = Self { swap, _target: target, visual, effect: None, bmp: None, w, h };
            s.bind(gfx)?;
            Ok(s)
        }
    }

    fn bind(&mut self, gfx: &Gfx) -> Result<()> {
        unsafe {
            let back: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            self.bmp = Some(gfx.dc.CreateBitmapFromDxgiSurface(&back, Some(&props))?);
        }
        Ok(())
    }

    pub fn resize(&mut self, gfx: &Gfx, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if w == self.w && h == self.h {
            return;
        }
        unsafe {
            gfx.dc.SetTarget(None);
            self.bmp = None;
            if self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
                self.w = w;
                self.h = h;
            }
            let _ = self.bind(gfx);
        }
    }

    /// Scale (around a pivot) and fade the whole surface in the compositor,
    /// without redrawing it. Cheap enough to call every frame.
    pub fn set_transform(&mut self, gfx: &Gfx, scale: f32, px: f32, py: f32, opacity: f32) {
        let m = Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: px - px * scale, M32: py - py * scale };
        unsafe {
            let _ = self.visual.SetTransform2(&m);
            if self.effect.is_none() {
                if let Ok(e) = gfx.dcomp.CreateEffectGroup() {
                    let _ = self.visual.SetEffect(&e);
                    self.effect = Some(e);
                }
            }
            if let Some(e) = &self.effect {
                let _ = e.SetOpacity2(opacity.clamp(0.0, 1.0));
            }
            let _ = gfx.dcomp.Commit();
        }
    }

    /// Start drawing; the surface is cleared to fully transparent.
    pub fn begin(&self, gfx: &Gfx) {
        unsafe {
            if let Some(b) = &self.bmp {
                gfx.dc.SetTarget(b);
            }
            gfx.dc.BeginDraw();
            gfx.dc.SetTransform(&Matrix3x2::identity());
            gfx.dc.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
        }
    }

    pub fn end(&self, gfx: &Gfx) {
        unsafe {
            let _ = gfx.dc.EndDraw(None, None);
            gfx.dc.SetTarget(None);
            // Pacing is done once per frame by the app loop, not per surface.
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
    }
}
