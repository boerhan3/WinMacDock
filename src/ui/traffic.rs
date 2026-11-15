//! Mac-style close / minimize / zoom buttons drawn in place of Windows' own
//! caption buttons. The native buttons are found by hit-testing the window
//! (works for classic title bars and for apps that draw their own, such as
//! browsers and Electron apps), then covered by a small owned window.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::PhysicalToLogicalPointForPerMonitorDPI;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::gfx::Surface;
use crate::util::{lp_x, lp_y, rect, rgba, WM_MOUSELEAVE};

/// Posted to the main window when the active window moved or resized.
pub const WM_APP_TRAFFIC: u32 = WM_APP + 8;

const MAX_OVERLAYS: usize = 16;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static MOVE_PENDING: AtomicBool = AtomicBool::new(false);

/// Buttons area relative to the window's visible frame, for one window size.
#[derive(Clone, Copy, PartialEq)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

struct Overlay {
    target: HWND,
    hwnd: HWND,
    surf: Option<Surface>,
    placed: (i32, i32, i32, i32),
    bg: (f32, f32, f32),
    active: bool,
    hover: bool,
    tracking: bool,
    dirty: bool,
}

pub struct Traffic {
    overlays: Vec<Overlay>,
    hook: HWINEVENTHOOK,
    hooked: HWND,
    /// Probed button areas per window and size (None: no caption buttons).
    areas: HashMap<(isize, i32, i32, bool), Option<Area>>,
}

fn frame_bounds(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).ok()?;
    }
    Some(r)
}

/// What the window says is at a screen point; None if it does not answer quickly.
fn hit_test(h: HWND, x: i32, y: i32) -> Option<u32> {
    unsafe {
        let mut pt = POINT { x, y };
        // DPI-virtualized windows expect their own coordinates.
        let _ = PhysicalToLogicalPointForPerMonitorDPI(Some(h), &mut pt);
        let lp = LPARAM(((pt.y as u16 as isize) << 16) | pt.x as u16 as isize);
        let mut hit = 0usize;
        let ok = SendMessageTimeoutW(h, WM_NCHITTEST, WPARAM(0), lp, SMTO_ABORTIFHUNG, 80, Some(&mut hit));
        (ok.0 != 0).then_some(hit as u32)
    }
}

fn is_button(hit: u32) -> bool {
    matches!(hit, HTMINBUTTON | HTMAXBUTTON | HTCLOSE | HTHELP)
}

/// Find the caption-button strip by walking left from the top-right corner.
/// `Err` means the window did not answer (try again later).
fn probe(h: HWND, s: f32) -> Result<Option<Area>, ()> {
    let r = frame_bounds(h).ok_or(())?;
    let step = (3.0 * s).max(2.0) as i32;
    let y = r.top + (10.0 * s) as i32;
    let mut left = None;
    let mut found_close = false;
    let mut misses = 0;
    let mut x = r.right - 2;
    while x > r.right - (300.0 * s) as i32 && x > r.left {
        let hit = hit_test(h, x, y).ok_or(())?;
        if is_button(hit) {
            found_close |= hit == HTCLOSE;
            left = Some(x);
            misses = 0;
        } else if left.is_some() {
            misses += 1;
            if misses * step > (6.0 * s) as i32 {
                break;
            }
        } else if x < r.right - (60.0 * s) as i32 {
            break; // nothing like a close button near the corner
        }
        x -= step;
    }
    let (Some(left), true) = (left, found_close) else { return Ok(None) };
    // Height: walk down until the buttons end.
    let probe_x = r.right - (20.0 * s) as i32;
    let mut bottom = y;
    while bottom < r.top + (64.0 * s) as i32 {
        let hit = hit_test(h, probe_x, bottom + step).ok_or(())?;
        if !is_button(hit) {
            break;
        }
        bottom += step;
    }
    Ok(Some(Area { x: left - r.left, y: 0, w: r.right - left, h: (bottom - r.top + step).max((24.0 * s) as i32) }))
}

/// Title-bar colour next to the buttons, if that spot really shows this window.
fn sample(h: HWND, x: i32, y: i32) -> Option<(f32, f32, f32)> {
    unsafe {
        let top = GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT);
        if top != h {
            return None;
        }
        let dc = GetDC(None);
        let c = GetPixel(dc, x, y).0;
        ReleaseDC(None, dc);
        (c != 0xFFFF_FFFF).then(|| ((c & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, ((c >> 16) & 0xFF) as f32 / 255.0))
    }
}

impl Traffic {
    pub fn new(_cx: &Ctx) -> windows::core::Result<Self> {
        Ok(Self { overlays: Vec::new(), hook: HWINEVENTHOOK::default(), hooked: HWND::default(), areas: HashMap::new() })
    }

    fn area_for(&mut self, cx: &Ctx, h: HWND) -> Option<Area> {
        let r = frame_bounds(h)?;
        let zoomed = unsafe { IsZoomed(h).as_bool() };
        let key = (h.0 as isize, r.right - r.left, r.bottom - r.top, zoomed);
        if let Some(a) = self.areas.get(&key) {
            return *a;
        }
        let exe = cx.tracker.wins.iter().find(|w| w.hwnd == h).and_then(|w| w.key.exe.clone()).unwrap_or_default();
        let file = exe.rsplit('\\').next().unwrap_or("");
        if cx.cfg.traffic_lights.exclude.iter().any(|x| x.eq_ignore_ascii_case(file)) {
            self.areas.insert(key, None);
            return None;
        }
        match probe(h, cx.s) {
            Ok(a) => {
                self.areas.insert(key, a);
                a
            }
            Err(()) => None, // busy window: try again next time
        }
    }

    /// Bring the overlays in line with the current windows; call after the
    /// tracker refreshes.
    pub fn update(&mut self, cx: &Ctx) {
        if !cx.cfg.traffic_lights.enabled {
            self.clear();
            return;
        }
        let live: Vec<HWND> = cx.tracker.wins.iter().map(|w| w.hwnd).filter(|&h| unsafe { !IsIconic(h).as_bool() }).take(MAX_OVERLAYS).collect();
        self.areas.retain(|k, _| live.iter().any(|h| h.0 as isize == k.0));
        // Drop overlays of windows that went away or were minimized.
        let mut i = 0;
        while i < self.overlays.len() {
            if live.contains(&self.overlays[i].target) {
                i += 1;
            } else {
                let ov = self.overlays.remove(i);
                unsafe {
                    let _ = DestroyWindow(ov.hwnd);
                }
            }
        }
        let fg = cx.tracker.fg;
        for h in live {
            let Some(area) = self.area_for(cx, h) else {
                if let Some(pos) = self.overlays.iter().position(|o| o.target == h) {
                    let ov = self.overlays.remove(pos);
                    unsafe {
                        let _ = DestroyWindow(ov.hwnd);
                    }
                }
                continue;
            };
            let idx = match self.overlays.iter().position(|o| o.target == h) {
                Some(i) => i,
                None => {
                    let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP, None);
                    unsafe {
                        // Owned windows always stay just above their owner.
                        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, h.0 as isize);
                    }
                    self.overlays.push(Overlay { target: h, hwnd, surf: None, placed: (0, 0, 0, 0), bg: (0.5, 0.5, 0.5), active: false, hover: false, tracking: false, dirty: true });
                    self.overlays.len() - 1
                }
            };
            let active = h == fg && unsafe { GetForegroundWindow() == h };
            let ov = &mut self.overlays[idx];
            if ov.active != active {
                ov.active = active;
                ov.dirty = true;
            }
            place(ov, area);
        }
        self.follow(fg);
    }

    /// Track live moves of the foreground window only (cheap).
    fn follow(&mut self, fg: HWND) {
        let want = if self.overlays.iter().any(|o| o.target == fg) { fg } else { HWND::default() };
        if want == self.hooked {
            return;
        }
        unsafe {
            if !self.hook.is_invalid() {
                let _ = UnhookWinEvent(self.hook);
                self.hook = HWINEVENTHOOK::default();
            }
            self.hooked = want;
            TARGET.store(want.0 as isize, Ordering::Relaxed);
            if !want.is_invalid() {
                let mut pid = 0u32;
                let tid = GetWindowThreadProcessId(want, Some(&mut pid));
                self.hook = SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, None, Some(location_changed), pid, tid, WINEVENT_OUTOFCONTEXT);
            }
        }
    }

    /// The followed window moved or resized.
    pub fn reposition(&mut self, cx: &Ctx) {
        MOVE_PENDING.store(false, Ordering::Relaxed);
        let h = self.hooked;
        if h.is_invalid() {
            return;
        }
        if unsafe { IsIconic(h).as_bool() || !IsWindow(Some(h)).as_bool() } {
            self.update(cx);
            return;
        }
        let area = self.area_for(cx, h);
        if let Some(ov) = self.overlays.iter_mut().find(|o| o.target == h) {
            match area {
                Some(a) => place(ov, a),
                None => unsafe {
                    let _ = ShowWindow(ov.hwnd, SW_HIDE);
                },
            }
        }
    }

    fn clear(&mut self) {
        for ov in self.overlays.drain(..) {
            unsafe {
                let _ = DestroyWindow(ov.hwnd);
            }
        }
        self.follow(HWND::default());
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let ov = self.overlays.iter_mut().find(|o| o.hwnd == hwnd)?;
        let s = cx.s;
        let (w, h) = (ov.placed.2 as f32, ov.placed.3 as f32);
        let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
        let lights = lights(w, h, s);
        let which = lights.iter().position(|&(cx0, cy0)| (x - cx0).powi(2) + (y - cy0).powi(2) <= (10.0 * s).powi(2));
        let target = ov.target;
        match msg {
            WM_MOUSEMOVE => {
                if !ov.tracking {
                    ov.tracking = true;
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                let over = x >= lights[0].0 - 12.0 * s && x <= lights[2].0 + 12.0 * s;
                if over != ov.hover {
                    ov.hover = over;
                    ov.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                ov.tracking = false;
                ov.hover = false;
                ov.dirty = true;
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN if which.is_none() => unsafe {
                // Elsewhere on the strip: behave like the title bar (drag to move).
                let _ = ReleaseCapture();
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = SetForegroundWindow(target);
                let lp = LPARAM(((pt.y as u16 as isize) << 16) | pt.x as u16 as isize);
                let _ = PostMessageW(Some(target), WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), lp);
                Some(LRESULT(0))
            },
            WM_LBUTTONDBLCLK if which.is_none() => unsafe {
                let sc = if IsZoomed(target).as_bool() { SC_RESTORE } else { SC_MAXIMIZE };
                let _ = PostMessageW(Some(target), WM_SYSCOMMAND, WPARAM(sc as usize), LPARAM(0));
                Some(LRESULT(0))
            },
            WM_LBUTTONUP => {
                unsafe {
                    match which {
                        Some(0) => {
                            let _ = PostMessageW(Some(target), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0));
                        }
                        Some(1) => cx.cmds.push(Cmd::Minimize(target)),
                        Some(_) => {
                            let sc = if IsZoomed(target).as_bool() { SC_RESTORE } else { SC_MAXIMIZE };
                            let _ = PostMessageW(Some(target), WM_SYSCOMMAND, WPARAM(sc as usize), LPARAM(0));
                        }
                        None => {}
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => Some(LRESULT(0)),
            _ => None,
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.overlays.iter().any(|o| o.dirty)
    }

    pub fn frame(&mut self, cx: &mut Ctx) {
        let s = cx.s;
        for ov in self.overlays.iter_mut().filter(|o| o.dirty) {
            ov.dirty = false;
            let (w, h) = (ov.placed.2.max(1) as u32, ov.placed.3.max(1) as u32);
            match &mut ov.surf {
                Some(surf) => surf.resize(&cx.gfx, w, h),
                None => ov.surf = Surface::new(&cx.gfx, ov.hwnd, w, h).ok(),
            }
            let Some(surf) = &ov.surf else { continue };
            let (wf, hf) = (w as f32, h as f32);
            surf.begin(&cx.gfx);
            let (r, g, b) = ov.bg;
            cx.gfx.fill_rect(rect(0.0, 0.0, wf, hf), rgba(r, g, b, 1.0));
            const COLORS: [(f32, f32, f32); 3] = [(1.0, 0.37, 0.34), (1.0, 0.74, 0.18), (0.16, 0.78, 0.25)];
            let dark_bg = r * 0.3 + g * 0.59 + b * 0.11 < 0.5;
            for (i, &(x, y)) in lights(wf, hf, s).iter().enumerate() {
                let rad = 6.5 * s;
                let c = if ov.active || ov.hover {
                    let (cr, cg, cb) = COLORS[i];
                    rgba(cr, cg, cb, 1.0)
                } else if dark_bg {
                    rgba(1.0, 1.0, 1.0, 0.22)
                } else {
                    rgba(0.0, 0.0, 0.0, 0.18)
                };
                cx.gfx.fill_circle(x, y, rad, c);
                if ov.hover {
                    let ink = rgba(0.0, 0.0, 0.0, 0.55);
                    let (d, lw) = (rad * 0.45, 1.3 * s);
                    match i {
                        0 => {
                            cx.gfx.line(x - d, y - d, x + d, y + d, lw, ink);
                            cx.gfx.line(x - d, y + d, x + d, y - d, lw, ink);
                        }
                        1 => cx.gfx.line(x - d * 1.2, y, x + d * 1.2, y, lw, ink),
                        _ => {
                            cx.gfx.line(x - d * 1.2, y, x + d * 1.2, y, lw, ink);
                            cx.gfx.line(x, y - d * 1.2, x, y + d * 1.2, lw, ink);
                        }
                    }
                }
            }
            surf.end(&cx.gfx);
        }
    }

    pub fn shutdown(&mut self) {
        self.clear();
    }
}

/// Centres of the three buttons (close, minimize, zoom), right-aligned in a
/// strip of `w`×`h`.
fn lights(w: f32, h: f32, s: f32) -> [(f32, f32); 3] {
    let y = (h / 2.0).min(16.0 * s);
    let x2 = w - 20.0 * s;
    [(x2 - 40.0 * s, y), (x2 - 20.0 * s, y), (x2, y)]
}

/// Move an overlay over its window's caption buttons and refresh its colour.
fn place(ov: &mut Overlay, a: Area) {
    let Some(r) = frame_bounds(ov.target) else { return };
    let placed = (r.left + a.x, r.top + a.y, a.w, a.h);
    if let Some(bg) = sample(ov.target, placed.0 - 3, placed.1 + 3) {
        if bg != ov.bg {
            ov.bg = bg;
            ov.dirty = true;
        }
    }
    unsafe {
        if placed != ov.placed {
            ov.placed = placed;
            ov.dirty = true;
            let _ = SetWindowPos(ov.hwnd, Some(HWND_TOP), placed.0, placed.1, placed.2, placed.3, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        } else if !IsWindowVisible(ov.hwnd).as_bool() {
            let _ = ShowWindow(ov.hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

unsafe extern "system" fn location_changed(_hook: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    // Only the window itself (not its caret or children), and one queued update at a time.
    if id_object == 0 && hwnd.0 as isize == TARGET.load(Ordering::Relaxed) && !MOVE_PENDING.swap(true, Ordering::Relaxed) {
        let _ = PostMessageW(Some(app::main_hwnd()), WM_APP_TRAFFIC, WPARAM(0), LPARAM(0));
    }
}
