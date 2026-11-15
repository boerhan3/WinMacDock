//! Live window previews above a Dock icon: one DWM thumbnail per window of
//! the app, with its title and a close button. Hover an app icon to open.

use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::Spring;
use crate::render::gfx::Surface;
use crate::ui::widgets::{Input, Ui};
use crate::util::{rect, rgba};
use crate::win::glass::Glass;
use crate::win::tracker::{self, AppKey};

const HOVER_DELAY: Duration = Duration::from_millis(450);
const LEAVE_DELAY: Duration = Duration::from_millis(300);

struct Thumb {
    hwnd: HWND,
    title: String,
    id: isize,
    /// Item box in panel coordinates (title row + thumbnail).
    item: D2D_RECT_F,
    /// Where the thumbnail is drawn, panel coordinates.
    dest: D2D_RECT_F,
}

pub struct Preview {
    glass: Glass,
    pub hwnd: HWND,
    surf: Option<Surface>,
    inp: Input,
    key: Option<AppKey>,
    thumbs: Vec<Thumb>,
    open: Spring,
    pub visible: bool,
    closing: bool,
    dirty: bool,
    /// Panel size and the icon it hangs above (screen x centre, y of the Dock top).
    size: (f32, f32),
    anchor: (i32, i32),
    placed: (i32, i32, i32, i32),
    pending: Option<(AppKey, Instant)>,
    leave_at: Option<Instant>,
}

fn tint(dark: bool) -> u32 {
    if dark { 0xB0_24_24_26 } else { 0xC8_F0_F0_F0 }
}

impl Preview {
    pub fn new(cx: &Ctx) -> Self {
        let glass = Glass::new(tint(cx.dark));
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, Some(glass.hwnd));
        Self {
            glass,
            hwnd,
            surf: None,
            inp: Input::default(),
            key: None,
            thumbs: Vec::new(),
            open: Spring::new(0.0, 520.0, 42.0),
            visible: false,
            closing: false,
            dirty: false,
            size: (0.0, 0.0),
            anchor: (0, 0),
            placed: (0, 0, 0, 0),
            pending: None,
            leave_at: None,
        }
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        self.glass.set_tint(tint(cx.dark));
        self.dirty = true;
    }

    /// Called every frame with the running app under the pointer in the Dock
    /// (and where its icon is). Opens, switches and closes the panel.
    pub fn track(&mut self, cx: &mut Ctx, hovered: Option<(AppKey, (i32, i32))>) {
        let now = Instant::now();
        let inside = self.visible && !self.closing && self.inp.inside;
        match hovered {
            Some((key, anchor)) => {
                self.leave_at = None;
                if self.visible && !self.closing {
                    if self.key.as_ref().is_some_and(|k| k.matches(&key)) {
                        if anchor != self.anchor {
                            self.anchor = anchor;
                            self.dirty = true;
                        }
                    } else {
                        // Sliding along the Dock switches previews without delay.
                        self.show(cx, key, anchor);
                    }
                } else {
                    match &self.pending {
                        Some((k, t)) if k.matches(&key) => {
                            if now.duration_since(*t) >= HOVER_DELAY {
                                self.pending = None;
                                self.show(cx, key, anchor);
                            }
                        }
                        _ => self.pending = Some((key, now)),
                    }
                }
            }
            None => {
                self.pending = None;
                if inside {
                    self.leave_at = None;
                } else if self.visible && !self.closing {
                    let at = *self.leave_at.get_or_insert(now + LEAVE_DELAY);
                    if now >= at {
                        self.close(cx);
                    }
                }
            }
        }
        // Windows opened or closed while showing: rebuild.
        if self.visible && !self.closing {
            if let Some(key) = self.key.clone() {
                let current: Vec<HWND> = cx.tracker.windows_of(&key).map(|w| w.hwnd).collect();
                let shown: Vec<HWND> = self.thumbs.iter().map(|t| t.hwnd).collect();
                if current.is_empty() {
                    self.close(cx);
                } else if current.len() != shown.len() || current.iter().any(|h| !shown.contains(h)) {
                    let anchor = self.anchor;
                    self.show(cx, key, anchor);
                }
            }
        }
    }

    /// Open right away (clicking an app that has several windows).
    pub fn show_now(&mut self, cx: &mut Ctx, key: AppKey, anchor: (i32, i32)) {
        self.pending = None;
        self.show(cx, key, anchor);
    }

    fn unregister(&mut self) {
        for t in self.thumbs.drain(..) {
            unsafe {
                let _ = DwmUnregisterThumbnail(t.id);
            }
        }
    }

    fn show(&mut self, cx: &mut Ctx, key: AppKey, anchor: (i32, i32)) {
        let s = cx.s;
        let wins: Vec<HWND> = cx.tracker.windows_of(&key).map(|w| w.hwnd).collect();
        if wins.is_empty() {
            return;
        }
        let was_open = self.visible && !self.closing;
        self.unregister();
        self.key = Some(key);
        self.anchor = anchor;

        // Size the items so the row fits on screen.
        let (pad, gap, title_h) = (10.0 * s, 10.0 * s, 26.0 * s);
        let n = wins.len() as f32;
        let mut item_w = 220.0 * s;
        let mon = crate::win::monitors::at(anchor.0, anchor.1);
        let max_w = (mon.right - mon.left) as f32 - 40.0 * s;
        if n * item_w + (n - 1.0) * gap + 2.0 * pad > max_w {
            item_w = (max_w - 2.0 * pad - (n - 1.0) * gap) / n;
        }
        let thumb_h = item_w * 0.6;
        let (w, h) = (n * item_w + (n - 1.0) * gap + 2.0 * pad, pad + title_h + thumb_h + pad);
        self.size = (w, h);

        for (i, hwnd) in wins.into_iter().enumerate() {
            let item = rect(pad + i as f32 * (item_w + gap), pad, item_w, title_h + thumb_h);
            let Ok(id) = (unsafe { DwmRegisterThumbnail(self.glass.hwnd, hwnd) }) else { continue };
            // Fit the source into the thumbnail area, keeping its shape.
            let src = unsafe { DwmQueryThumbnailSourceSize(id) }.unwrap_or_default();
            let (sw, sh) = (src.cx.max(1) as f32, src.cy.max(1) as f32);
            let k = ((item_w - 8.0 * s) / sw).min((thumb_h - 6.0 * s) / sh);
            let (tw, th) = (sw * k, sh * k);
            let dest = rect(item.left + (item_w - tw) / 2.0, item.top + title_h + (thumb_h - th) / 2.0, tw, th);
            self.thumbs.push(Thumb { hwnd, title: tracker::title(hwnd), id, item, dest });
        }

        if self.surf.is_none() {
            self.surf = Surface::new(&cx.gfx, self.hwnd, w.ceil() as u32, h.ceil() as u32).ok();
        } else if let Some(surf) = &mut self.surf {
            surf.resize(&cx.gfx, w.ceil() as u32, h.ceil() as u32);
        }
        self.glass.set_tint(tint(cx.dark));
        self.glass.raise();
        self.visible = true;
        self.closing = false;
        self.leave_at = None;
        if !was_open {
            self.inp.reset();
            self.open.snap(if cx.animate { 0.0 } else { 1.0 });
        }
        self.open.set(1.0);
        self.placed = (0, 0, 0, 0);
        self.dirty = true;
    }

    pub fn close(&mut self, cx: &Ctx) {
        if !self.visible || self.closing {
            return;
        }
        self.closing = true;
        self.open.set(0.0);
        if !cx.animate {
            self.open.snap(0.0);
        }
        self.dirty = true;
    }

    fn hide(&mut self) {
        self.unregister();
        self.visible = false;
        self.closing = false;
        self.key = None;
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.surf = None;
    }

    pub fn on_msg(&mut self, _cx: &mut Ctx, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.hwnd {
            return None;
        }
        if self.inp.on_msg(hwnd, msg, wp, lp) {
            self.dirty = true;
            return Some(LRESULT(0));
        }
        None
    }

    pub fn needs_frame(&self) -> bool {
        self.pending.is_some() || (self.visible && (self.dirty || self.open.active() || self.leave_at.is_some()))
    }

    pub fn frame(&mut self, cx: &mut Ctx, dt: f32) {
        if !self.visible {
            return;
        }
        let animating = self.open.tick(dt);
        if self.closing && self.open.v < 0.02 {
            self.hide();
            return;
        }
        if !(self.dirty || animating) {
            return;
        }
        self.dirty = false;
        let s = cx.s;
        let t = self.open.v.clamp(0.0, 1.0);
        let (w, h) = self.size;

        // Rise into place while fading in.
        let mon = crate::win::monitors::at(self.anchor.0, self.anchor.1);
        let (ml, mr) = (mon.left as f32, mon.right as f32);
        let x = (self.anchor.0 as f32 - w / 2.0).clamp(ml + 8.0, (mr - w - 8.0).max(ml + 8.0)).round() as i32;
        let y = (self.anchor.1 as f32 - 12.0 * s - h + (1.0 - t) * 10.0 * s).round() as i32;
        let placed = (x, y, w.ceil() as i32, h.ceil() as i32);
        if placed != self.placed {
            self.placed = placed;
            self.glass.set_rect(x, y, placed.2, placed.3, (14.0 * s) as i32);
            unsafe {
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, placed.2, placed.3, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            self.glass.show(true);
        }
        self.glass.set_opacity(t);

        // Thumbnails are composed by DWM onto the backdrop window.
        let hot = self.thumbs.iter().position(|th| self.inp.inside && {
            let (mx, my) = self.inp.mouse;
            mx >= th.item.left && mx < th.item.right && my >= th.item.top && my < th.item.bottom
        });
        for (i, th) in self.thumbs.iter().enumerate() {
            let lift = if hot == Some(i) { 1.0 } else { 0.0 };
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: (th.dest.left - lift) as i32,
                    top: (th.dest.top - lift) as i32,
                    right: (th.dest.right + lift) as i32,
                    bottom: (th.dest.bottom + lift) as i32,
                },
                opacity: (255.0 * t) as u8,
                fVisible: BOOL(1),
                fSourceClientAreaOnly: BOOL(0),
                ..Default::default()
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(th.id, &props);
            }
        }

        let Some(surf) = self.surf.take() else { return };
        surf.begin(&cx.gfx);
        let mut activate = None;
        let mut close_win = None;
        {
            let mut ui = Ui { cx, inp: &mut self.inp, a: t };
            let edge = ui.ink(0.14);
            ui.cx.gfx.stroke_rrect(rect(0.5, 0.5, w - 1.0, h - 1.0), 14.0 * s, 1.0, edge);
            for (i, th) in self.thumbs.iter().enumerate() {
                let is_hot = hot == Some(i);
                if is_hot {
                    let c = ui.ink(0.10);
                    ui.cx.gfx.fill_rrect(rect(th.item.left - 4.0 * s, th.item.top - 4.0 * s, th.item.right - th.item.left + 8.0 * s, th.item.bottom - th.item.top + 8.0 * s), 10.0 * s, c);
                }
                let title_r = rect(th.item.left + 4.0 * s, th.item.top, th.item.right - th.item.left - 32.0 * s, 22.0 * s);
                ui.label(&th.title, 12.5, if is_hot { 600 } else { 400 }, title_r, -1);
                let close_r = rect(th.item.right - 24.0 * s, th.item.top + 1.0 * s, 20.0 * s, 20.0 * s);
                if is_hot {
                    let over = ui.hover(close_r);
                    let c = if over { rgba(1.0, 0.37, 0.34, t) } else { ui.ink(0.15) };
                    ui.cx.gfx.fill_circle((close_r.left + close_r.right) / 2.0, (close_r.top + close_r.bottom) / 2.0, 9.0 * s, c);
                    let g = if over { rgba(0.25, 0.0, 0.0, t) } else { ui.ink(0.8) };
                    ui.glyph('\u{E711}', 8.0, close_r, g);
                    if ui.clicked(close_r) {
                        close_win = Some(th.hwnd);
                    }
                }
                if ui.clicked(th.item) {
                    activate = Some(th.hwnd);
                }
            }
            ui.inp.end_frame();
        }
        surf.end(&cx.gfx);
        self.surf = Some(surf);

        if let Some(h) = close_win {
            unsafe {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        } else if let Some(h) = activate {
            cx.cmds.push(Cmd::Activate(h));
            self.close(cx);
        }
    }
}
