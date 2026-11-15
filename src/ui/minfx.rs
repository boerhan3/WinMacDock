//! Minimize effect: the window is sucked into its Dock icon with a macOS
//! style "genie", while the real window minimizes without Windows' own
//! animation.
//!
//! Best path (our own minimize: window buttons, Dock clicks): the window's
//! picture is captured just before it minimizes and the genie is drawn with
//! Direct2D, row by row at sub-pixel positions, so the curved sides are
//! perfectly smooth.
//!
//! Fallback (the window minimized itself, e.g. Win+Down): the window is
//! already gone, so DWM thumbnail strips of it are moved instead. Those can
//! only sit on whole pixels, so the curve is a little stepped.

use std::time::Instant;

use windows::core::BOOL;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Ctx};
use crate::render::anim::lerp;
use crate::render::gfx::Surface;
use crate::util::rect;

const GENIE_DURATION: f32 = 0.7;
const SCALE_DURATION: f32 = 0.34;
/// DWM fallback: about one strip per this many pixels of window height.
const STRIP_PX: i32 = 8;
/// Each strip is a separate call into DWM; too many and a frame gets drawn
/// half-updated (a visible kink). ~90 keeps a frame's updates around 5 ms.
const MAX_STRIPS: i32 = 90;

/// Geometry of one genie run, in screen pixels.
#[derive(Clone, Copy, Default)]
struct Genie {
    from: RECT,
    /// Icon centre and size.
    to: (f32, f32),
    icon: f32,
}

impl Genie {
    /// Where the band of source rows v0..v1 (0 = top, 1 = bottom) is at
    /// time t: (left, top, right, bottom).
    fn band(&self, t: f32, v0: f32, v1: f32) -> (f32, f32, f32, f32) {
        let (fl, ft) = (self.from.left as f32, self.from.top as f32);
        let (fw, fh) = ((self.from.right - self.from.left) as f32, (self.from.bottom - self.from.top) as f32);
        let (ix, iy, isz) = (self.to.0 - self.icon / 2.0, self.to.1 - self.icon / 2.0, self.icon);
        let smooth = |x: f32| {
            let x = x.clamp(0.0, 1.0);
            x * x * (3.0 - 2.0 * x)
        };
        // Phase 1: the funnel forms. Phase 2 (overlapping): the content pours in.
        let bend = smooth(t / 0.35);
        let slide = ((t - 0.12) / 0.78).clamp(0.0, 1.0);
        // The sides stay upright down to about the window's middle, then make
        // a sharp S-turn into the icon (smootherstep: flat, steep, flat).
        let neck = (ft + fh * 0.5).min(iy - 1.0);
        let side = |y: f32| -> (f32, f32) {
            if y >= iy {
                return (lerp(fl, ix, bend), lerp(fl + fw, ix + isz, bend));
            }
            let u = ((y - neck) / (iy - neck).max(1.0)).clamp(0.0, 1.0);
            let k = u * u * u * (u * (u * 6.0 - 15.0) + 10.0) * bend;
            (lerp(fl, ix, k), lerp(fl + fw, ix + isz, k))
        };
        // Lower rows lead; the motion speeds up as it nears the Dock.
        let y_at = |v: f32| {
            let q = (slide * 1.35 - (1.0 - v) * 0.35).clamp(0.0, 1.0);
            let e = q * q * (3.0 - 2.0 * q) * 0.6 + q * q * 0.4;
            lerp(ft + v * fh, iy + v * isz, e)
        };
        let (top, bottom) = (y_at(v0), y_at(v1));
        let (left, right) = side((top + bottom) / 2.0);
        (left, top, right, bottom)
    }

    fn opacity(t: f32) -> f32 {
        1.0 - ((t - 0.9) / 0.1).clamp(0.0, 1.0)
    }
}

/// What is being animated.
enum Run {
    None,
    /// Captured picture drawn by us (smooth).
    Picture { bmp: ID2D1Bitmap, size: (f32, f32) },
    /// DWM thumbnail strips (fallback), and the source image size.
    Strips { thumbs: Vec<isize>, src: (i32, i32) },
    /// A single thumbnail shrinking (last resort).
    Scale { thumb: isize },
}

pub struct MinFx {
    /// Host for DWM thumbnails.
    hwnd: HWND,
    /// Host for our own drawing (created on first use).
    canvas: Option<(HWND, Surface)>,
    run: Run,
    geo: Genie,
    /// What each strip was last set to; unchanged strips are not sent to DWM again.
    last: Vec<(RECT, u8)>,
    /// Windows whose coming minimize event is ours (already animated or meant to be quiet).
    skip: Vec<HWND>,
    start: Instant,
    /// Top-left of the host windows (the virtual screen, which may start below 0).
    origin: (i32, i32),
}

impl MinFx {
    pub fn new() -> Self {
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT, None);
        Self { hwnd, canvas: None, run: Run::None, geo: Genie::default(), last: Vec::new(), skip: Vec::new(), start: Instant::now(), origin: (0, 0) }
    }

    /// Minimize `target` ourselves (Dock click, window buttons). The window
    /// is still visible, so its picture is captured for the smooth effect.
    pub fn minimize(&mut self, cx: &Ctx, target: HWND, dest: Option<(i32, i32)>) {
        unsafe {
            if IsIconic(target).as_bool() {
                return;
            }
            if let (true, Some(dest)) = (cx.animate, dest) {
                self.finish();
                let started = match capture(cx, target) {
                    Some((bmp, frame)) => {
                        self.begin_picture(cx, bmp, frame, dest);
                        true
                    }
                    None => {
                        let mut from = RECT::default();
                        GetWindowRect(target, &mut from).is_ok() && self.begin_thumbs(cx, target, from, dest)
                    }
                };
                if started {
                    // The minimize event that follows is ours; don't animate twice.
                    self.skip.push(target);
                }
            }
            let _ = ShowWindow(target, SW_MINIMIZE);
        }
    }

    /// Minimize without an effect (the other windows when an app hides all of
    /// its windows), and don't let their minimize events cut the running effect.
    pub fn minimize_quietly(&mut self, target: HWND) {
        unsafe {
            if IsIconic(target).as_bool() {
                return;
            }
            self.skip.push(target);
            if self.skip.len() > 32 {
                self.skip.remove(0);
            }
            let _ = ShowWindow(target, SW_MINIMIZE);
        }
    }

    /// Some window was minimized by other means (Win+Down, its own button):
    /// play the effect from where it was.
    pub fn minimized(&mut self, cx: &Ctx, target: HWND, dest: Option<(i32, i32)>) {
        if let Some(i) = self.skip.iter().position(|&h| h == target) {
            self.skip.remove(i);
            return;
        }
        let (Some(dest), true) = (dest, cx.animate) else { return };
        let Some(from) = restored_rect(target) else { return };
        self.finish();
        // The window is gone already; a recent picture of it (taken in the
        // background while it was in front) still gives the smooth effect.
        if let Some((px, w, h, frame)) = cached(target, from) {
            if let Some(bmp) = cx.gfx.create_bitmap(w as u32, h as u32, &px) {
                self.begin_picture(cx, bmp, frame, dest);
                return;
            }
        }
        crate::util::log(&format!("minimize effect: no picture of \"{}\", using the stepped fallback", crate::win::tracker::title(target)));
        self.begin_thumbs(cx, target, from, dest);
    }

    fn setup(&mut self, cx: &Ctx, from: RECT, dest: (i32, i32)) {
        let v = crate::win::monitors::virtual_rect();
        self.origin = (v.left, v.top);
        self.geo = Genie { from, to: (dest.0 as f32, dest.1 as f32), icon: cx.cfg.dock.icon_size * cx.s };
        self.start = Instant::now();
    }

    fn begin_picture(&mut self, cx: &Ctx, bmp: ID2D1Bitmap, frame: RECT, dest: (i32, i32)) {
        self.setup(cx, frame, dest);
        let v = crate::win::monitors::virtual_rect();
        let (w, h) = ((v.right - v.left) as u32, (v.bottom - v.top) as u32);
        if self.canvas.is_none() {
            let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT, None);
            match Surface::new(&cx.gfx, hwnd, w, h) {
                Ok(s) => self.canvas = Some((hwnd, s)),
                Err(_) => unsafe {
                    let _ = DestroyWindow(hwnd);
                    return;
                },
            }
        }
        if let Some((hwnd, surf)) = &mut self.canvas {
            surf.resize(&cx.gfx, w, h);
            unsafe {
                let _ = SetWindowPos(*hwnd, Some(HWND_TOPMOST), v.left, v.top, w as i32, h as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
        let size = unsafe { bmp.GetSize() };
        self.run = Run::Picture { bmp, size: (size.width, size.height) };
        self.draw(cx, 0.0);
    }

    fn begin_thumbs(&mut self, cx: &Ctx, target: HWND, from: RECT, dest: (i32, i32)) -> bool {
        self.setup(cx, from, dest);
        unsafe {
            let v = crate::win::monitors::virtual_rect();
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), v.left, v.top, v.right - v.left, v.bottom - v.top, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let Ok(first) = DwmRegisterThumbnail(self.hwnd, target) else {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                return false;
            };
            let size = DwmQueryThumbnailSourceSize(first).unwrap_or_default();
            // Slicing needs the window's real image size; a minimized stub is tiny.
            if size.cx < 120 || size.cy < 120 {
                self.run = Run::Scale { thumb: first };
            } else {
                let mut thumbs = vec![first];
                let strips = ((from.bottom - from.top) / STRIP_PX).clamp(32, MAX_STRIPS);
                for _ in 1..strips {
                    if let Ok(t) = DwmRegisterThumbnail(self.hwnd, target) {
                        thumbs.push(t);
                    }
                }
                self.run = Run::Strips { thumbs, src: (size.cx, size.cy) };
            }
        }
        self.last.clear();
        self.draw(cx, 0.0);
        true
    }

    fn duration(&self) -> f32 {
        if matches!(self.run, Run::Scale { .. }) { SCALE_DURATION } else { GENIE_DURATION }
    }

    fn draw(&mut self, cx: &Ctx, t: f32) {
        let (ox, oy) = (self.origin.0 as f32, self.origin.1 as f32);
        let g = self.geo;
        match &self.run {
            Run::None => {}
            Run::Picture { bmp, size } => {
                let Some((_, surf)) = &self.canvas else { return };
                let (sw, sh) = *size;
                // Two source pixels per band: smooth curves, a few hundred draws.
                let n = ((sh / 2.0) as usize).clamp(60, 450);
                let a = Genie::opacity(t);
                surf.begin(&cx.gfx);
                for i in 0..n {
                    let (v0, v1) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
                    let (l, top, r, b) = g.band(t, v0, v1);
                    if r - l < 0.5 {
                        continue;
                    }
                    // Overlap the next band slightly so no hairline seams show.
                    let dest = rect(l - ox, top - oy, r - l, (b - top) + 0.8);
                    let src = rect(0.0, v0 * sh, sw, (v1 - v0) * sh);
                    cx.gfx.bitmap_part(bmp, dest, src, a);
                }
                surf.end(&cx.gfx);
            }
            Run::Strips { thumbs, src } => {
                let n = thumbs.len();
                if self.last.len() != n {
                    self.last = vec![(RECT::default(), 0); n];
                }
                let opacity = (255.0 * Genie::opacity(t)) as u8;
                for (i, &thumb) in thumbs.iter().enumerate() {
                    let (v0, v1) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
                    let (l, top, r, b) = g.band(t, v0, v1);
                    let dest = RECT {
                        left: (l - ox).round() as i32,
                        top: (top - oy).floor() as i32,
                        right: (r - ox).round().max((l - ox).round() + 1.0) as i32,
                        // Overlap by a pixel so no seams show between strips.
                        bottom: (b - oy).ceil() as i32 + 1,
                    };
                    if self.last[i] != (dest, opacity) {
                        self.last[i] = (dest, opacity);
                        let s = RECT { left: 0, top: (src.1 as f32 * v0) as i32, right: src.0, bottom: (src.1 as f32 * v1).ceil() as i32 };
                        update(thumb, dest, Some(s), opacity);
                    }
                }
            }
            Run::Scale { thumb } => {
                let e = if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 };
                let (fw, fh) = ((g.from.right - g.from.left) as f32, (g.from.bottom - g.from.top) as f32);
                let aspect = if fh > 0.0 { fw / fh } else { 1.0 };
                let (tw, th) = if aspect > 1.0 { (g.icon, g.icon / aspect) } else { (g.icon * aspect, g.icon) };
                let (w, h) = (lerp(fw, tw, e), lerp(fh, th, e));
                let cx0 = g.from.left as f32 + fw / 2.0;
                let cy0 = g.from.top as f32 + fh / 2.0;
                let (cxp, cyp) = (lerp(cx0, g.to.0, e) - ox, lerp(cy0, g.to.1, e) - oy);
                let dest = RECT { left: (cxp - w / 2.0) as i32, top: (cyp - h / 2.0) as i32, right: (cxp + w / 2.0) as i32, bottom: (cyp + h / 2.0) as i32 };
                let opacity = (255.0 * (1.0 - ((t - 0.75) / 0.25).clamp(0.0, 1.0))) as u8;
                update(*thumb, dest, None, opacity);
            }
        }
    }

    fn finish(&mut self) {
        match std::mem::replace(&mut self.run, Run::None) {
            Run::None => {}
            Run::Picture { .. } => {
                // The full-screen surface is only needed while animating.
                if let Some((hwnd, _surf)) = self.canvas.take() {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            Run::Strips { thumbs, .. } => unsafe {
                for t in thumbs {
                    let _ = DwmUnregisterThumbnail(t);
                }
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            },
            Run::Scale { thumb } => unsafe {
                let _ = DwmUnregisterThumbnail(thumb);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            },
        }
        self.last.clear();
    }

    pub fn needs_frame(&self) -> bool {
        !matches!(self.run, Run::None)
    }

    pub fn frame(&mut self, cx: &Ctx) {
        if matches!(self.run, Run::None) {
            return;
        }
        let t = self.start.elapsed().as_secs_f32() / self.duration();
        if t >= 1.0 {
            self.finish();
        } else {
            self.draw(cx, t);
        }
    }
}

fn update(thumb: isize, dest: RECT, src: Option<RECT>, opacity: u8) {
    let mut flags = DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY;
    if src.is_some() {
        flags |= DWM_TNP_RECTSOURCE;
    }
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: flags,
        rcDestination: dest,
        rcSource: src.unwrap_or_default(),
        opacity,
        fVisible: BOOL(1),
        fSourceClientAreaOnly: BOOL(0),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(thumb, &props);
    }
}

/// The window's picture as a Direct2D bitmap, and its frame on screen.
fn capture(cx: &Ctx, hwnd: HWND) -> Option<(ID2D1Bitmap, RECT)> {
    let (px, w, h, frame) = capture_pixels(hwnd, 8.0 * cx.s)?;
    Some((cx.gfx.create_bitmap(w as u32, h as u32, &px)?, frame))
}

// ---- background snapshots of the window in front -----------------------------

struct Snap {
    hwnd: isize,
    frame: RECT,
    w: i32,
    h: i32,
    px: Vec<u8>,
    at: Instant,
}

/// Pictures kept at most (the one in front at full size, others at half).
const SNAP_KEEP: usize = 6;
/// Pictures of windows that are not in front are retaken after this long.
const SNAP_STALE_SECS: u64 = 30;

static SNAPS: std::sync::Mutex<Vec<Snap>> = std::sync::Mutex::new(Vec::new());
/// Windows waiting to be pictured: (hwnd, full size).
static WANT: std::sync::Mutex<Vec<(isize, bool)>> = std::sync::Mutex::new(Vec::new());
static SNAP_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Take pictures of the window in front (`fg`) and of other visible app
/// windows (`others`, topmost first) on a worker thread, never on the UI
/// thread, so that a window that minimizes by itself (Win+Down, its own
/// button) still gets the smooth effect, whichever window it is.
pub fn prefetch(fg: HWND, others: &[HWND], scale: f32) {
    use std::sync::atomic::Ordering;
    let own = std::process::id();
    let usable = |h: HWND| !h.is_invalid() && crate::win::tracker::window_pid(h) != own && unsafe { !IsIconic(h).as_bool() && IsWindowVisible(h).as_bool() };
    let fresh = |h: HWND| SNAPS.lock().map(|s| s.iter().any(|x| x.hwnd == h.0 as isize && x.at.elapsed().as_secs() < SNAP_STALE_SECS)).unwrap_or(false);
    let mut jobs = Vec::new();
    if usable(fg) {
        jobs.push((fg.0 as isize, true));
    }
    // At most one window in the back per call: each picture makes that app redraw.
    if let Some(&h) = others.iter().filter(|&&h| h != fg).take(SNAP_KEEP - 1).find(|&&h| usable(h) && !fresh(h)) {
        jobs.push((h.0 as isize, false));
    }
    if jobs.is_empty() {
        return;
    }
    if let Ok(mut w) = WANT.lock() {
        for j in jobs {
            if !w.iter().any(|x| x.0 == j.0) {
                w.push(j);
            }
        }
    }
    // A worker already running picks the new requests up.
    if SNAP_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || loop {
        unsafe {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
        let job = WANT.lock().ok().and_then(|mut w| if w.is_empty() { None } else { Some(w.remove(0)) });
        let Some((h, full)) = job else {
            SNAP_BUSY.store(false, Ordering::Release);
            // A request may have arrived just before we cleared the flag.
            let more = WANT.lock().map(|w| !w.is_empty()).unwrap_or(false);
            if more && !SNAP_BUSY.swap(true, Ordering::AcqRel) {
                continue;
            }
            break;
        };
        let hwnd = HWND(h as *mut _);
        if unsafe { IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() } {
            continue;
        }
        let Some((mut px, mut w, mut h2, frame)) = capture_pixels(hwnd, 8.0 * scale) else { continue };
        if !full && w > 800 {
            (px, w, h2) = half(&px, w, h2);
        }
        if let Ok(mut s) = SNAPS.lock() {
            s.retain(|x| x.hwnd != h && unsafe { IsWindow(Some(HWND(x.hwnd as *mut _))).as_bool() });
            s.insert(0, Snap { hwnd: h, frame, w, h: h2, px, at: Instant::now() });
            s.truncate(SNAP_KEEP);
        }
    });
}

/// Halve a BGRA picture (2Ã—2 average), to keep pictures of windows in the back small.
fn half(px: &[u8], w: i32, h: i32) -> (Vec<u8>, i32, i32) {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh as usize {
        for x in 0..nw as usize {
            for c in 0..4 {
                let at = |xx: usize, yy: usize| px[(yy * w as usize + xx) * 4 + c] as u32;
                let s = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                out[(y * nw as usize + x) * 4 + c] = ((s + 2) / 4) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// `--snap-test`: picture every visible app window once and log whether it
/// worked, how long it took and how its frame compares with where Windows
/// would say it was after minimizing. Changes nothing on screen.
pub fn snap_test() {
    let mut t = crate::win::tracker::Tracker::new();
    t.refresh();
    for w in &t.wins {
        let h = w.hwnd;
        if unsafe { IsIconic(h).as_bool() } {
            continue;
        }
        let title: String = crate::win::tracker::title(h).chars().take(40).collect();
        let at = Instant::now();
        let r = capture_pixels(h, 10.0);
        let ms = at.elapsed().as_secs_f32() * 1000.0;
        let line = match (r, restored_rect(h)) {
            (Some((_, w, hh, f)), Some(rr)) => format!(
                "snap-test {ms:6.1} ms  ok {w}x{hh}  frame {},{},{},{}  restored {},{},{},{}  [{title}]",
                f.left, f.top, f.right, f.bottom, rr.left, rr.top, rr.right, rr.bottom
            ),
            (Some(_), None) => format!("snap-test {ms:6.1} ms  ok, no placement  [{title}]"),
            (None, _) => format!("snap-test {ms:6.1} ms  FAILED (black or error)  [{title}]"),
        };
        crate::util::log(&line);
    }
}

/// A recent picture of `hwnd`, if it was taken where the window last was.
fn cached(hwnd: HWND, restored: RECT) -> Option<(Vec<u8>, i32, i32, RECT)> {
    let mut s = SNAPS.lock().ok()?;
    let Some(i) = s.iter().position(|x| x.hwnd == hwnd.0 as isize && x.at.elapsed().as_secs() < 60) else {
        crate::util::log(&format!("minimize effect: no recent picture (have {} pictures)", s.len()));
        return None;
    };
    let snap = s.remove(i);
    let f = snap.frame;
    crate::util::log(&format!(
        "minimize effect: picture {}s old, frame {},{},{},{} vs restored {},{},{},{}",
        snap.at.elapsed().as_secs(), f.left, f.top, f.right, f.bottom, restored.left, restored.top, restored.right, restored.bottom
    ));
    // The window rect includes invisible borders; the frame sits inside it.
    // The window rect includes invisible borders; the frame sits inside it.
    // If the window moved or resized since (or the work area changed), the
    // picture is still right; it is just stretched to where the window was.
    let near = |a: i32, b: i32| (a - b).abs() <= 16;
    let f = snap.frame;
    let same = near(f.left, restored.left) && near(f.top, restored.top) && near(f.right, restored.right) && near(f.bottom, restored.bottom);
    Some((snap.px, snap.w, snap.h, if same { f } else { restored }))
}

/// The window's picture (its visible frame, with rounded corners of
/// `radius`), its size, and that frame's screen rectangle. None if the app
/// cannot be captured (it draws black). Safe to call from any thread.
fn capture_pixels(hwnd: HWND, radius: f32) -> Option<(Vec<u8>, i32, i32, RECT)> {
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    unsafe {
        let mut wr = RECT::default();
        GetWindowRect(hwnd, &mut wr).ok()?;
        // The visible frame (the window rect also holds invisible resize borders).
        let mut fr = wr;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut fr as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32);
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let (fx, fy) = ((fr.left - wr.left).max(0), (fr.top - wr.top).max(0));
        let (fw, fh) = ((fr.right - fr.left).min(ww - fx), (fr.bottom - fr.top).min(wh - fy));
        if ww <= 0 || wh <= 0 || fw <= 0 || fh <= 0 || ww > 8192 || wh > 8192 {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ww,
                biHeight: -wh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let result = (|| {
            let dib = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(mem, dib.into());
            // 2 = PW_RENDERFULLCONTENT: also captures GPU-drawn apps (browsers, UWP).
            let ok = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(2)).as_bool();
            let mut px = Vec::new();
            if ok && !bits.is_null() {
                let all = std::slice::from_raw_parts(bits as *const u8, (ww * wh * 4) as usize);
                px.reserve((fw * fh * 4) as usize);
                for y in 0..fh {
                    let row = ((fy + y) * ww + fx) as usize * 4;
                    px.extend_from_slice(&all[row..row + fw as usize * 4]);
                }
            }
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            (!px.is_empty()).then_some(px)
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let mut px = result?;
        // Apps that cannot be captured come back black.
        if px.chunks_exact(4).step_by(97).all(|p| p[0] < 4 && p[1] < 4 && p[2] < 4) {
            return None;
        }
        // Opaque, with Windows 11's rounded corners cut out (anti-aliased).
        for y in 0..fh {
            for x in 0..fw {
                let i = ((y * fw + x) * 4) as usize;
                let dx = (radius - x as f32 - 0.5).max(x as f32 + 0.5 - (fw as f32 - radius));
                let dy = (radius - y as f32 - 0.5).max(y as f32 + 0.5 - (fh as f32 - radius));
                let a = if dx > 0.0 && dy > 0.0 { (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0) } else { 1.0 };
                if a >= 1.0 {
                    px[i + 3] = 255;
                } else {
                    for c in 0..3 {
                        px[i + c] = (px[i + c] as f32 * a) as u8;
                    }
                    px[i + 3] = (255.0 * a) as u8;
                }
            }
        }
        Some((px, fw, fh, RECT { left: wr.left + fx, top: wr.top + fy, right: wr.left + fx + fw, bottom: wr.top + fy + fh }))
    }
}

/// Where a (now minimized) window was on screen before it was minimized.
fn restored_rect(h: HWND) -> Option<RECT> {
    unsafe {
        let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        GetWindowPlacement(h, &mut wp).ok()?;
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        if wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0 {
            // Maximized: it filled the work area of its own display (not always the primary one).
            use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
                return Some(mi.rcWork);
            }
            return Some(work);
        }
        // The normal position is in work-area coordinates (of the primary display).
        let r = wp.rcNormalPosition;
        Some(RECT { left: r.left + work.left, top: r.top + work.top, right: r.right + work.left, bottom: r.bottom + work.top })
    }
}

fn state_file() -> std::path::PathBuf {
    crate::util::data_dir().join("animation.state")
}

/// Turn Windows' own minimize/restore animation off while we run, so our
/// effect is the only one. The original value is saved for `restore_system_animation`.
pub fn disable_system_animation() {
    unsafe {
        let mut info = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: 0 };
        let size = info.cbSize;
        let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
        let _ = SystemParametersInfoW(SPI_GETANIMATION, size, Some(&mut info as *mut _ as *mut _), none);
        if info.iMinAnimate == 0 {
            return;
        }
        let _ = std::fs::write(state_file(), "1");
        info.iMinAnimate = 0;
        // Not written to the user profile: a sign-out resets it in any case.
        let _ = SystemParametersInfoW(SPI_SETANIMATION, size, Some(&mut info as *mut _ as *mut _), none);
    }
}

pub fn restore_system_animation() {
    if std::fs::read_to_string(state_file()).ok().as_deref() != Some("1") {
        return;
    }
    unsafe {
        let mut info = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: 1 };
        let size = info.cbSize;
        let _ = SystemParametersInfoW(SPI_SETANIMATION, size, Some(&mut info as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    let _ = std::fs::remove_file(state_file());
}
