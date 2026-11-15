//! Live wallpaper behind the desktop icons: a procedural GPU shader or a
//! looping video. Rendering pauses whenever the desktop is not visible.

use std::ffi::c_void;
use std::time::Instant;

use windows::core::{implement, s, w, Interface, Result, BSTR, HRESULT, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Ctx};
use crate::render::gfx::Gfx;
use crate::util;
use crate::win::desktop;

const SHADER_SRC: &str = include_str!("../../assets/shaders/wallpaper.hlsl");
/// Frame interval while animating, and re-check interval while paused.
pub const ACTIVE_MS: u32 = 33;
pub const PAUSED_MS: u32 = 1000;

#[repr(C)]
struct Params {
    time: f32,
    aspect: f32,
    scene: f32,
    pad: f32,
    colors: [[f32; 4]; 4],
}

/// Built-in animated wallpapers: config key, name, group.
pub const SCENES: &[(&str, &str, &str)] = &[
    ("aurora", "Aurora", "Abstract"),
    ("dunes", "Dunes", "Abstract"),
    ("tide", "Tide", "Abstract"),
    ("borealis", "Northern Lights", "Nature"),
    ("ocean", "Ocean Sunset", "Nature"),
    ("mountains", "Misty Mountains", "Nature"),
    ("nebula", "Nebula", "Space"),
    ("galaxy", "Galaxy", "Space"),
    ("planet", "Planet", "Space"),
];

/// Program branch in wallpaper.hlsl for a scene.
fn scene_id(name: &str) -> f32 {
    match name {
        "nebula" => 1.0,
        "galaxy" => 2.0,
        "planet" => 3.0,
        "borealis" => 4.0,
        "ocean" => 5.0,
        "mountains" => 6.0,
        _ => 0.0,
    }
}

pub fn palette(name: &str) -> [[f32; 4]; 4] {
    match name {
        "dunes" => [[0.33, 0.11, 0.09, 1.0], [0.90, 0.45, 0.16, 1.0], [0.98, 0.76, 0.42, 1.0], [0.55, 0.20, 0.36, 1.0]],
        "tide" => [[0.01, 0.09, 0.17, 1.0], [0.02, 0.38, 0.50, 1.0], [0.24, 0.74, 0.70, 1.0], [0.76, 0.95, 0.90, 1.0]],
        "nebula" => [[0.01, 0.01, 0.04, 1.0], [0.35, 0.08, 0.45, 1.0], [0.05, 0.25, 0.55, 1.0], [0.90, 0.35, 0.50, 1.0]],
        "galaxy" => [[0.005, 0.005, 0.02, 1.0], [0.45, 0.55, 1.0, 1.0], [1.0, 0.80, 0.55, 1.0], [1.0, 0.50, 0.70, 1.0]],
        "planet" => [[0.01, 0.01, 0.03, 1.0], [0.85, 0.60, 0.38, 1.0], [0.95, 0.87, 0.72, 1.0], [0.30, 0.55, 1.0, 1.0]],
        "borealis" => [[0.0, 0.01, 0.03, 1.0], [0.02, 0.07, 0.10, 1.0], [0.10, 1.0, 0.55, 1.0], [0.60, 0.25, 0.90, 1.0]],
        "ocean" => [[0.15, 0.15, 0.40, 1.0], [0.85, 0.40, 0.45, 1.0], [1.0, 0.60, 0.30, 1.0], [0.05, 0.07, 0.20, 1.0]],
        "mountains" => [[0.45, 0.55, 0.75, 1.0], [0.98, 0.80, 0.65, 1.0], [0.35, 0.40, 0.55, 1.0], [0.10, 0.12, 0.20, 1.0]],
        // "aurora"
        _ => [[0.05, 0.06, 0.25, 1.0], [0.10, 0.34, 0.86, 1.0], [0.86, 0.26, 0.56, 1.0], [1.00, 0.63, 0.30, 1.0]],
    }
}

fn params(dark: bool, name: &str, time: f32, aspect: f32) -> Params {
    let scene = scene_id(name);
    let mut colors = palette(name);
    // The bright abstract ones are toned down in dark mode; the scenes are dark already.
    if scene == 0.0 && dark {
        for c in colors.iter_mut() {
            for v in c.iter_mut().take(3) {
                *v *= 0.62;
            }
        }
    }
    Params { time, aspect, scene, pad: 0.0, colors }
}

struct ShaderMode {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    params: ID3D11Buffer,
    rtv: ID3D11RenderTargetView,
    name: String,
}

/// Draw one scene into `rtv` over `vp` (D3D state is cleared afterwards
/// for Direct2D, which shares the device).
unsafe fn draw_scene(gfx: &Gfx, vs: &ID3D11VertexShader, ps: &ID3D11PixelShader, cbuf: &ID3D11Buffer, rtv: &ID3D11RenderTargetView, views: &[(D3D11_VIEWPORT, Params)]) {
    let ctx = &gfx.d3dctx;
    ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx.VSSetShader(vs, None);
    ctx.PSSetShader(ps, None);
    ctx.PSSetConstantBuffers(0, Some(&[Some(cbuf.clone())]));
    for (vp, p) in views {
        ctx.UpdateSubresource(cbuf, 0, None, p as *const _ as *const c_void, 0, 0);
        ctx.RSSetViewports(Some(&[*vp]));
        ctx.Draw(3, 0);
    }
    ctx.ClearState();
}

/// Render one frame of a scene into a new `w`×`h` texture.
unsafe fn render_texture(gfx: &Gfx, dark: bool, name: &str, time: f32, w: u32, h: u32) -> Option<ID3D11Texture2D> {
    let (vs, ps, cbuf) = shader_objects(gfx).ok()?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let mut tex = None;
    gfx.d3d.CreateTexture2D(&desc, None, Some(&mut tex)).ok()?;
    let tex = tex?;
    let mut rtv = None;
    gfx.d3d.CreateRenderTargetView(&tex, None, Some(&mut rtv)).ok()?;
    let vp = D3D11_VIEWPORT { Width: w as f32, Height: h as f32, MaxDepth: 1.0, ..Default::default() };
    draw_scene(gfx, &vs, &ps, &cbuf, &rtv?, &[(vp, params(dark, name, time, w as f32 / h.max(1) as f32))]);
    Some(tex)
}

/// `--render-wallpapers <dir>`: save a PNG of every built-in scene (for
/// checking how they look without changing the desktop).
pub fn render_all_to(dir: &str) -> Result<()> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    let (w, h) = (2560u32, 1440u32);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let gfx = Gfx::new()?;
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        for &(key, _, _) in SCENES {
            for (time, tag) in [(40.0f32, "a"), (160.0, "b")] {
                let tex = render_texture(&gfx, true, key, time, w, h).ok_or_else(|| windows::core::Error::new(E_FAIL, "render failed"))?;
                save_png(&gfx, &wic, &tex, w, h, &format!("{dir}\\{key}_{tag}.png"))?;
            }
        }
    }
    Ok(())
}

/// `--render-frames <dir> <scene> <w> <h> <count> <t0> <dt>`: an animation
/// as numbered PNGs (for making videos of the wallpapers).
pub fn render_frames(dir: &str, scene: &str, w: u32, h: u32, count: u32, t0: f32, dt: f32) -> Result<()> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let gfx = Gfx::new()?;
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        for i in 0..count {
            let tex = render_texture(&gfx, true, scene, t0 + dt * i as f32, w, h).ok_or_else(|| windows::core::Error::new(E_FAIL, "render failed"))?;
            save_png(&gfx, &wic, &tex, w, h, &format!("{dir}\\{scene}_{i:04}.png"))?;
        }
    }
    Ok(())
}

/// Read a rendered texture back and write it as a PNG file.
unsafe fn save_png(gfx: &Gfx, wic: &windows::Win32::Graphics::Imaging::IWICImagingFactory, tex: &ID3D11Texture2D, w: u32, h: u32, path: &str) -> Result<()> {
    use windows::Win32::Graphics::Imaging::*;
    // Copy to a CPU-readable texture.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    gfx.d3d.CreateTexture2D(&desc, None, Some(&mut staging))?;
    let staging = staging.unwrap();
    gfx.d3dctx.CopyResource(&staging, tex);
    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
    gfx.d3dctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        let src = std::slice::from_raw_parts((map.pData as *const u8).add(y * map.RowPitch as usize), (w * 4) as usize);
        px[y * (w * 4) as usize..(y + 1) * (w * 4) as usize].copy_from_slice(src);
    }
    gfx.d3dctx.Unmap(&staging, 0);
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let stream = wic.CreateStream()?;
    stream.InitializeFromFilename(windows::core::PCWSTR(path.as_ptr()), GENERIC_WRITE.0)?;
    let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
    enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
    let mut frame = None;
    enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
    let frame = frame.unwrap();
    frame.Initialize(None)?;
    frame.SetSize(w, h)?;
    let mut fmt = GUID_WICPixelFormat32bppBGRA;
    frame.SetPixelFormat(&mut fmt)?;
    frame.WritePixels(h, w * 4, &px)?;
    frame.Commit()?;
    enc.Commit()?;
    Ok(())
}

/// A still picture of a scene as a Direct2D bitmap (Settings previews, the
/// blurred Launchpad background).
pub fn snapshot(cx: &Ctx, name: &str, time: f32, w: u32, h: u32) -> Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap> {
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::{D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1};
    unsafe {
        let tex = render_texture(&cx.gfx, cx.dark, name, time, w, h)?;
        let surface: IDXGISurface = tex.cast().ok()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            ..Default::default()
        };
        let bmp = cx.gfx.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)).ok()?;
        bmp.cast().ok()
    }
}

#[implement(IMFMediaEngineNotify)]
struct EngineNotify;

impl IMFMediaEngineNotify_Impl for EngineNotify_Impl {
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> Result<()> {
        Ok(())
    }
}

struct VideoMode {
    engine: IMFMediaEngine,
    _manager: IMFDXGIDeviceManager,
}

enum Mode {
    Shader(ShaderMode),
    Video(VideoMode),
}

pub struct Wallpaper {
    hwnd: HWND,
    swap: IDXGISwapChain1,
    mode: Mode,
    size: (u32, u32),
    /// Each display, in swap-chain pixels (the window spans all of them).
    views: Vec<RECT>,
    /// Each display in screen pixels, for the "is it covered?" check.
    mons: Vec<RECT>,
    /// Size of the whole virtual screen in pixels.
    full: (i32, i32),
    /// Seconds of animation shown so far (does not advance while paused).
    time: f32,
    last: Instant,
    paused: bool,
    pub locked: bool,
    ticks: u32,
}

impl Wallpaper {
    /// Returns `None` when the wallpaper is turned off or cannot start.
    pub fn new(cx: &Ctx) -> Option<Self> {
        let cfg = &cx.cfg.wallpaper;
        let video = match cfg.mode.as_str() {
            "shader" => false,
            "video" => true,
            _ => return None,
        };
        match Self::create(cx, video) {
            Ok(w) => Some(w),
            Err(e) => {
                util::log(&format!("live wallpaper unavailable: {e}"));
                None
            }
        }
    }

    fn create(cx: &Ctx, video: bool) -> Result<Self> {
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, None);
        let fail = |msg: &str| {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            windows::core::Error::new(E_FAIL, msg)
        };
        // One window across every display; each display gets its own full picture.
        let virt = crate::win::monitors::virtual_rect();
        let full = (virt.right - virt.left, virt.bottom - virt.top);
        if !desktop::attach(hwnd, full.0, full.1) {
            return Err(fail("desktop wallpaper layer not found"));
        }
        let mons = crate::win::monitors::list();
        let scale = if video { 1.0 } else { render_scale(&mons, full) };
        let px = |v: i32| (v as f32 * scale).round() as i32;
        let (w, h) = (px(full.0).max(1) as u32, px(full.1).max(1) as u32);
        let views = mons
            .iter()
            .map(|m| RECT {
                left: px(m.left - virt.left),
                top: px(m.top - virt.top),
                right: px(m.right - virt.left),
                bottom: px(m.bottom - virt.top),
            })
            .collect();
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swap = unsafe { cx.gfx.factory.CreateSwapChainForHwnd(&cx.gfx.d3d, hwnd, &desc, None, None) }
            .map_err(|e| fail(&format!("swap chain: {e}")))?;
        let mode = if video {
            unsafe { start_video(cx, &cx.cfg.wallpaper.video) }.map(Mode::Video)
        } else {
            unsafe { build_shader(cx, &swap) }.map(Mode::Shader)
        }
        .map_err(|e| fail(&format!("{e}")))?;
        let mut wp = Self { hwnd, swap, mode, size: (w, h), views, mons, full, time: 0.0, last: Instant::now(), paused: false, locked: false, ticks: 0 };
        wp.render(cx);
        Ok(wp)
    }

    /// Seconds of animation shown so far.
    pub fn time(&self) -> f32 {
        self.time
    }

    /// Explorer restarted: the desktop windows are new, attach again.
    pub fn reattach(&self, _cx: &Ctx) {
        desktop::attach(self.hwnd, self.full.0, self.full.1);
    }

    pub fn shutdown(&self) {
        unsafe {
            if let Mode::Video(v) = &self.mode {
                let _ = v.engine.Shutdown();
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }

    /// True when nothing of the desktop can be seen or power should be saved.
    fn should_pause(&self, cx: &Ctx, on_battery: bool) -> bool {
        if self.locked || (on_battery && cx.cfg.wallpaper.pause_on_battery) {
            return true;
        }
        // The desktop itself is in front (Win+D / Show Desktop keeps windows
        // maximized but puts the desktop on top): it is visible, keep animating.
        unsafe {
            let fg = GetForegroundWindow();
            let mut class = [0u16; 16];
            let n = GetClassNameW(fg, &mut class) as usize;
            if matches!(String::from_utf16_lossy(&class[..n]).as_str(), "Progman" | "WorkerW") {
                return false;
            }
        }
        // Pause only when every display is covered by a maximized or fullscreen window.
        let mut covered = vec![false; self.mons.len()];
        for win in &cx.tracker.wins {
            unsafe {
                if IsIconic(win.hwnd).as_bool() {
                    continue;
                }
                let mut r = RECT::default();
                if GetWindowRect(win.hwnd, &mut r).is_err() {
                    continue;
                }
                let zoomed = IsZoomed(win.hwnd).as_bool();
                for (i, m) in self.mons.iter().enumerate() {
                    let fills = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
                    let on = zoomed && crate::win::monitors::of_window(win.hwnd) == *m;
                    covered[i] |= fills || on;
                }
            }
        }
        !covered.is_empty() && covered.iter().all(|&c| c)
    }

    /// Timer tick. Returns the interval (ms) until the next one.
    pub fn tick(&mut self, cx: &Ctx, on_battery: bool) -> u32 {
        self.ticks = self.ticks.wrapping_add(1);
        // Re-evaluate visibility about twice a second (every tick while paused).
        if self.paused || self.ticks % 15 == 0 {
            desktop::keep_order(self.hwnd);
            let pause = self.should_pause(cx, on_battery);
            if pause != self.paused {
                self.paused = pause;
                self.last = Instant::now();
                if let Mode::Video(v) = &self.mode {
                    unsafe {
                        let _ = if pause { v.engine.Pause() } else { v.engine.Play() };
                    }
                }
            }
        }
        if self.paused {
            return PAUSED_MS;
        }
        let now = Instant::now();
        let speed = cx.cfg.wallpaper.speed.clamp(0.25, 20.0);
        self.time += now.duration_since(self.last).as_secs_f32().min(0.25) * speed;
        self.last = now;
        self.render(cx);
        ACTIVE_MS
    }

    fn render(&mut self, cx: &Ctx) {
        let (w, h) = self.size;
        unsafe {
            match &self.mode {
                Mode::Shader(sh) => {
                    // The whole picture on every display, shaped to that display.
                    let views: Vec<(D3D11_VIEWPORT, Params)> = self
                        .views
                        .iter()
                        .map(|v| {
                            let (vw, vh) = ((v.right - v.left).max(1) as f32, (v.bottom - v.top).max(1) as f32);
                            let vp = D3D11_VIEWPORT { TopLeftX: v.left as f32, TopLeftY: v.top as f32, Width: vw, Height: vh, MaxDepth: 1.0, ..Default::default() };
                            (vp, params(cx.dark, &sh.name, self.time, vw / vh))
                        })
                        .collect();
                    draw_scene(&cx.gfx, &sh.vs, &sh.ps, &sh.params, &sh.rtv, &views);
                    let _ = (w, h);
                    let _ = self.swap.Present(0, DXGI_PRESENT(0));
                }
                Mode::Video(v) => {
                    if !v.engine.HasVideo().as_bool() || v.engine.OnVideoStreamTick().is_err() {
                        return;
                    }
                    let Ok(back) = self.swap.GetBuffer::<ID3D11Texture2D>(0) else { return };
                    let (mut vw, mut vh) = (0u32, 0u32);
                    let _ = v.engine.GetNativeVideoSize(Some(&mut vw), Some(&mut vh));
                    let mut ok = false;
                    for dst in &self.views {
                        // Crop to fill each display (like "Fill" for a still wallpaper).
                        let (dw, dh) = ((dst.right - dst.left).max(1) as f32, (dst.bottom - dst.top).max(1) as f32);
                        let mut src = MFVideoNormalizedRect { left: 0.0, top: 0.0, right: 1.0, bottom: 1.0 };
                        if vw > 0 && vh > 0 {
                            let (va, sa) = (vw as f32 / vh as f32, dw / dh);
                            if va > sa {
                                let keep = sa / va;
                                src.left = (1.0 - keep) / 2.0;
                                src.right = 1.0 - src.left;
                            } else {
                                let keep = va / sa;
                                src.top = (1.0 - keep) / 2.0;
                                src.bottom = 1.0 - src.top;
                            }
                        }
                        ok |= v.engine.TransferVideoFrame(&back, Some(&src), dst, None).is_ok();
                    }
                    let _ = (w, h);
                    if ok {
                        let _ = self.swap.Present(0, DXGI_PRESENT(0));
                    }
                }
            }
        }
    }
}

/// Shortest display edge the shader is rendered at (1440 = 2K / QHD).
const MIN_RENDER_H: f32 = 1440.0;
/// Most supersampling applied to small displays.
const MAX_SUPERSAMPLE: f32 = 2.0;
/// D3D11 texture size limit.
const MAX_TEX: f32 = 16384.0;

/// Swap-chain pixels per screen pixel for the shader wallpaper: always at
/// least native (never upscaled), and small displays are supersampled so
/// each one gets at least a 2K picture (4K displays render at full 4K).
fn render_scale(mons: &[RECT], full: (i32, i32)) -> f32 {
    let min_h = mons.iter().map(|m| (m.bottom - m.top).min(m.right - m.left)).filter(|&h| h > 0).min().unwrap_or(full.1.max(1));
    let want = (MIN_RENDER_H / min_h as f32).clamp(1.0, MAX_SUPERSAMPLE);
    let fit = (MAX_TEX / full.0.max(1) as f32).min(MAX_TEX / full.1.max(1) as f32);
    want.min(fit).max(fit.min(1.0))
}

type D3DCompileFn =unsafe extern "system" fn(
    src: *const c_void,
    len: usize,
    name: PCSTR,
    defines: *const c_void,
    include: *const c_void,
    entry: PCSTR,
    target: PCSTR,
    flags1: u32,
    flags2: u32,
    code: *mut Option<ID3DBlob>,
    errors: *mut Option<ID3DBlob>,
) -> HRESULT;

unsafe fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
}

/// Compiled shader bytecode, made once per run (previews and the wallpaper share it).
static BYTECODE: std::sync::OnceLock<Option<(Vec<u8>, Vec<u8>)>> = std::sync::OnceLock::new();

fn bytecode() -> Result<&'static (Vec<u8>, Vec<u8>)> {
    BYTECODE
        .get_or_init(|| match unsafe { compile_shaders() } {
            Ok(c) => Some(c),
            Err(e) => {
                util::log(&format!("wallpaper shader: {e}"));
                None
            }
        })
        .as_ref()
        .ok_or_else(|| windows::core::Error::new(E_FAIL, "wallpaper shader did not compile"))
}

/// Vertex/pixel shader and constant buffer on our device.
unsafe fn shader_objects(gfx: &Gfx) -> Result<(ID3D11VertexShader, ID3D11PixelShader, ID3D11Buffer)> {
    let (vs_code, ps_code) = bytecode()?;
    let d3d = &gfx.d3d;
    let (mut vs, mut ps, mut params) = (None, None, None);
    d3d.CreateVertexShader(vs_code, None, Some(&mut vs))?;
    d3d.CreatePixelShader(ps_code, None, Some(&mut ps))?;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<Params>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    d3d.CreateBuffer(&desc, None, Some(&mut params))?;
    Ok((vs.unwrap(), ps.unwrap(), params.unwrap()))
}

unsafe fn build_shader(cx: &Ctx, swap: &IDXGISwapChain1) -> Result<ShaderMode> {
    let (vs, ps, params) = shader_objects(&cx.gfx)?;
    let back: ID3D11Texture2D = swap.GetBuffer(0)?;
    let mut rtv = None;
    cx.gfx.d3d.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
    Ok(ShaderMode { vs, ps, params, rtv: rtv.unwrap(), name: cx.cfg.wallpaper.shader.clone() })
}

/// Compile the shaders. The compiler DLL is loaded only for this call and
/// unloaded afterwards so it does not sit in memory.
unsafe fn compile_shaders() -> Result<(Vec<u8>, Vec<u8>)> {
    let lib = LoadLibraryW(w!("d3dcompiler_47.dll"))?;
    let result = (|| -> Result<(Vec<u8>, Vec<u8>)> {
        let proc = GetProcAddress(lib, s!("D3DCompile")).ok_or_else(|| windows::core::Error::new(E_FAIL, "D3DCompile missing"))?;
        let compile: D3DCompileFn = std::mem::transmute(proc);
        let build = |entry: PCSTR, target: PCSTR| -> Result<Vec<u8>> {
            let (mut code, mut errors) = (None, None);
            let hr = compile(
                SHADER_SRC.as_ptr() as *const c_void,
                SHADER_SRC.len(),
                s!("wallpaper.hlsl"),
                std::ptr::null(),
                std::ptr::null(),
                entry,
                target,
                0,
                0,
                &mut code,
                &mut errors,
            );
            if let Some(e) = &errors {
                util::log(&format!("shader: {}", String::from_utf8_lossy(blob_bytes(e))));
            }
            hr.ok()?;
            // Copy out: the blob lives in the compiler DLL, which is unloaded below.
            code.map(|c| blob_bytes(&c).to_vec()).ok_or_else(|| windows::core::Error::new(E_FAIL, "no shader bytecode"))
        };
        Ok((build(s!("vs_main"), s!("vs_5_0"))?, build(s!("ps_main"), s!("ps_5_0"))?))
    })();
    let _ = FreeLibrary(lib);
    result
}

/// Start looping, muted, hardware-decoded playback of a video file.
unsafe fn start_video(cx: &Ctx, path: &str) -> Result<VideoMode> {
    if path.is_empty() || !std::path::Path::new(path).exists() {
        return Err(windows::core::Error::new(E_FAIL, "wallpaper.video does not point to a file"));
    }
    MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    // The decoder calls into our device from its own threads.
    let mt: ID3D11Multithread = cx.gfx.d3d.cast()?;
    let _ = mt.SetMultithreadProtected(true);
    let mut token = 0u32;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
    let manager: IMFDXGIDeviceManager = manager.unwrap();
    manager.ResetDevice(&cx.gfx.d3d, token)?;

    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 3)?;
    let attrs: IMFAttributes = attrs.unwrap();
    let notify: IMFMediaEngineNotify = EngineNotify.into();
    attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
    attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
    attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;

    let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
    let engine = factory.CreateInstance(0, &attrs)?;
    engine.SetSource(&BSTR::from(path))?;
    engine.SetLoop(true)?;
    engine.SetMuted(true)?;
    engine.Play()?;
    Ok(VideoMode { engine, _manager: manager })
}
