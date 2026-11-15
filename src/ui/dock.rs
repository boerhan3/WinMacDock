//! The Dock: pinned + running apps, cursor magnification, launch bounce,
//! running indicators and name labels.

use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::ABE_BOTTOM;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::Spring;
use crate::render::gfx::Surface;
use crate::ui::menu::{Anchor, Menu, MenuItem};
use crate::util::{WM_MOUSELEAVE, lp_x, lp_y, rect, rgba, shell_open};
use crate::win::glass::Glass;
use crate::win::{appbar, icons};
use crate::win::tracker::{self, AppKey};

pub const MENU_TAG_DOCK: u32 = 1;
const BOUNCE_PERIOD: f32 = 0.55;

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Launchpad,
    App,
    Sep,
    Trash,
}

struct Item {
    kind: Kind,
    key: AppKey,
    /// What to hand to the shell to start the app / load its icon.
    launch: String,
    name: String,
    icon: Option<ID2D1Bitmap>,
    pinned: bool,
    running: bool,
    removing: bool,
    /// 0 → 1 as the item grows into the Dock.
    appear: Spring,
    /// Start time and, once known, the time (seconds) the bounce ends.
    bounce: Option<(Instant, Option<f32>)>,
    /// Horizontal offset (px) easing to 0 after the item changed places.
    slide: Spring,
}

/// An icon being dragged to a new place (or out of the Dock).
#[derive(Clone, Copy)]
struct Drag {
    idx: usize,
    start: (f32, f32),
    pos: (f32, f32),
    /// Moved far enough to count as a drag rather than a click.
    active: bool,
}

struct Metrics {
    icon: f32,
    gap: f32,
    pad_x: f32,
    pad_bottom: f32,
    panel_h: f32,
    margin: f32,
    max_scale: f32,
    extra: f32,
}

#[derive(Default)]
struct Lay {
    /// Panel x, y, w, h in window pixels.
    panel: (f32, f32, f32, f32),
    /// Per item: left edge, width, scale.
    slots: Vec<(f32, f32, f32)>,
}

pub struct Dock {
    glass: Glass,
    pub hwnd: HWND,
    surf: Surface,
    items: Vec<Item>,
    mag: Spring,
    cursor: f32,
    hover: bool,
    tracking: bool,
    hover_idx: Option<usize>,
    label: Spring,
    label_text: String,
    label_pos: (f32, f32),
    lay: Lay,
    dirty: bool,
    /// The display the Dock is on; it moves to whichever display the
    /// pointer pushes against the bottom of, like macOS.
    mon: RECT,
    win_x: i32,
    win_y: i32,
    win_h: i32,
    /// Invisible strips along the bottom of the other displays.
    edges: Vec<(HWND, RECT)>,
    shown: bool,
    autohide: bool,
    /// 0 = fully shown, 1 = slid off the bottom edge.
    hide: Spring,
    /// When to slide away after the pointer left.
    hide_at: Option<Instant>,
    held: bool,
    drag: Option<Drag>,
}

fn metrics(cx: &Ctx) -> Metrics {
    let s = cx.s;
    let icon = cx.cfg.dock.icon_size.clamp(24.0, 96.0) * s;
    let max_scale = cx.cfg.dock.magnification.clamp(1.0, 2.5);
    Metrics {
        icon,
        gap: 4.0 * s,
        pad_x: 6.0 * s,
        pad_bottom: 10.0 * s,
        panel_h: icon + 16.0 * s,
        margin: 6.0 * s,
        max_scale,
        extra: icon * (max_scale - 1.0),
    }
}

fn dock_tint(dark: bool) -> u32 {
    if dark { 0x60_22_22_24 } else { 0x60_F2_F2_F2 }
}

impl Dock {
    pub fn new(cx: &mut Ctx) -> windows::core::Result<Self> {
        let m = metrics(cx);
        let win_h = (m.margin + m.panel_h + m.extra + 44.0 * cx.s).ceil() as i32;
        let mon = crate::win::monitors::list()[0];
        let mw = mon.right - mon.left;
        let win_y = mon.bottom - win_h;
        let glass = Glass::new(dock_tint(cx.dark));
        let hwnd = app::create_window(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            Some(glass.hwnd),
        );
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(hwnd, None, mon.left, win_y, mw, win_h, SWP_NOACTIVATE | SWP_NOZORDER);
        }
        let surf = Surface::new(&cx.gfx, hwnd, mw as u32, win_h as u32)?;
        let mut dock = Self {
            glass,
            hwnd,
            surf,
            items: Vec::new(),
            mag: Spring::new(0.0, 420.0, 36.0),
            cursor: mw as f32 / 2.0,
            hover: false,
            tracking: false,
            hover_idx: None,
            label: Spring::new(0.0, 500.0, 44.0),
            label_text: String::new(),
            label_pos: (0.0, 0.0),
            lay: Lay::default(),
            dirty: true,
            mon,
            win_x: mon.left,
            win_y,
            win_h,
            edges: Vec::new(),
            shown: false,
            autohide: cx.cfg.dock.autohide,
            hide: Spring::new(if cx.cfg.dock.autohide { 1.0 } else { 0.0 }, 300.0, 34.0),
            hide_at: None,
            held: false,
            drag: None,
        };
        dock.rebuild(cx);
        dock.set_visible(true);
        dock.reserve(cx);
        dock.make_edges();
        Ok(dock)
    }

    /// The display the Dock is on.
    pub fn monitor(&self) -> RECT {
        self.mon
    }

    fn mw(&self) -> i32 {
        self.mon.right - self.mon.left
    }

    /// Keep maximized windows clear of a Dock that does not hide.
    fn reserve(&self, cx: &Ctx) {
        appbar::unregister(self.glass.hwnd);
        if !cx.cfg.dock.autohide {
            let m = self.mon;
            appbar::register(self.glass.hwnd, ABE_BOTTOM, RECT { left: m.left, top: m.bottom - Self::reserved_height(cx), right: m.right, bottom: m.bottom });
        }
    }

    /// Thin catchers along the bottom of every other display: pushing the
    /// pointer there brings the Dock over.
    fn make_edges(&mut self) {
        unsafe {
            for (h, _) in self.edges.drain(..) {
                let _ = DestroyWindow(h);
            }
            for m in crate::win::monitors::list() {
                if m == self.mon {
                    continue;
                }
                let h = app::create_window(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, None);
                // Alpha 1: invisible, yet it still receives the mouse.
                let _ = SetLayeredWindowAttributes(h, COLORREF(0), 1, LWA_ALPHA);
                let _ = SetWindowPos(h, Some(HWND_TOPMOST), m.left, m.bottom - 2, m.right - m.left, 2, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                self.edges.push((h, m));
            }
        }
    }

    /// Move the Dock to another display.
    fn move_to(&mut self, cx: &mut Ctx, mon: RECT) {
        if mon == self.mon {
            return;
        }
        self.mon = mon;
        let mw = self.mw();
        self.win_x = mon.left;
        self.win_y = mon.bottom - self.win_h;
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, self.win_x, self.win_y, mw, self.win_h, SWP_NOACTIVATE | SWP_NOZORDER);
        }
        self.surf.resize(&cx.gfx, mw as u32, self.win_h as u32);
        self.reserve(cx);
        self.make_edges();
        self.raise();
        self.lay = Lay::default();
        self.dirty = true;
    }

    /// Put the Dock above other topmost windows (e.g. Launchpad).
    pub fn raise(&self) {
        unsafe {
            for h in [self.glass.hwnd, self.hwnd] {
                let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    pub fn shutdown(&mut self) {
        appbar::unregister(self.glass.hwnd);
        unsafe {
            for (h, _) in self.edges.drain(..) {
                let _ = DestroyWindow(h);
            }
        }
    }

    fn icon_px(cx: &Ctx) -> i32 {
        let m = metrics(cx);
        (m.icon * m.max_scale).ceil() as i32
    }

    /// Rebuild the item list from the config (pinned apps), then add running apps.
    pub fn rebuild(&mut self, cx: &mut Ctx) {
        let px = Self::icon_px(cx);
        let mut items = vec![Item::fixed(Kind::Launchpad, "Launchpad", None)];
        for p in cx.cfg.dock.pinned.clone() {
            let lower = p.to_lowercase();
            let key = if let Some(id) = lower.strip_prefix("shell:appsfolder\\") {
                AppKey { aumid: Some(id.to_string()), exe: icons::link_target(&p) }
            } else if lower.ends_with(".lnk") {
                AppKey { aumid: None, exe: icons::link_target(&p) }
            } else {
                AppKey { aumid: None, exe: Some(lower) }
            };
            // Skip apps that are not installed on this machine.
            let Some(icon) = icons::load(&cx.gfx, &p, px) else { continue };
            items.push(Item {
                kind: Kind::App,
                key,
                name: icons::display_name(&p),
                launch: p,
                icon: Some(icon),
                pinned: true,
                running: false,
                removing: false,
                appear: Spring::new(1.0, 260.0, 26.0),
                bounce: None,
            slide: Spring::new(0.0, 420.0, 36.0),
            });
        }
        items.push(Item::fixed(Kind::Sep, "", None));
        items.push(Item::fixed(Kind::Trash, "Recycle Bin", icons::load(&cx.gfx, "shell:RecycleBinFolder", px)));
        self.items = items;
        self.autohide = cx.cfg.dock.autohide;
        self.hide.set(if self.autohide && !self.hover { 1.0 } else { 0.0 });
        self.sync(cx, false);
    }

    /// Reconcile items with the tracker's window list.
    pub fn sync(&mut self, cx: &mut Ctx, animate: bool) {
        let px = Self::icon_px(cx);
        for it in self.items.iter_mut().filter(|i| i.kind == Kind::App) {
            it.running = cx.tracker.is_running(&it.key);
            if it.running {
                it.removing = false;
                it.appear.set(1.0);
            } else if !it.pinned {
                it.removing = true;
                it.appear.set(0.0);
                if !(animate && cx.animate) {
                    it.appear.snap(0.0);
                }
            }
        }
        let new_keys: Vec<AppKey> = cx
            .tracker
            .wins
            .iter()
            .map(|w| w.key.clone())
            .filter(|k| !self.items.iter().any(|it| it.kind == Kind::App && it.key.matches(k)))
            .collect();
        for key in new_keys {
            if self.items.iter().any(|it| it.kind == Kind::App && it.key.matches(&key)) {
                continue; // several windows of the same new app
            }
            let mut launch = key.launch_target();
            if launch.is_empty() {
                continue;
            }
            let mut icon = icons::load(&cx.gfx, &launch, px);
            if icon.is_none() {
                if let Some(id) = &key.aumid {
                    launch = format!("shell:AppsFolder\\{id}");
                    icon = icons::load(&cx.gfx, &launch, px);
                }
            }
            let start = if animate && cx.animate { 0.0 } else { 1.0 };
            let mut appear = Spring::new(start, 260.0, 26.0);
            appear.set(1.0);
            let sep = self.items.iter().position(|i| i.kind == Kind::Sep).unwrap_or(self.items.len());
            self.items.insert(
                sep,
                Item {
                    kind: Kind::App,
                    name: icons::display_name(&launch),
                    key,
                    launch,
                    icon,
                    pinned: false,
                    running: true,
                    removing: false,
                    appear,
                    bounce: None,
            slide: Spring::new(0.0, 420.0, 36.0),
                },
            );
        }
        self.items.retain(|it| !(it.removing && !it.appear.active() && it.appear.v < 0.02));
        self.dirty = true;
    }

    pub fn set_pinned(&mut self, cx: &mut Ctx, launch: &str, pinned: bool) {
        if let Some(it) = self.items.iter_mut().find(|i| i.kind == Kind::App && i.launch == launch) {
            it.pinned = pinned;
        }
        cx.cfg.dock.pinned.retain(|p| p != launch);
        if pinned {
            cx.cfg.dock.pinned.push(launch.to_string());
        }
        cx.cfg.save();
        self.sync(cx, true);
    }

    pub fn set_visible(&mut self, show: bool) {
        if self.shown == show {
            return;
        }
        self.shown = show;
        self.glass.show(show);
        unsafe {
            let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        self.dirty = true;
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        self.glass.set_tint(dock_tint(cx.dark));
        self.dirty = true;
    }

    /// Screen position of an app's icon center (for minimize animations).
    /// Where the icon sits once the Dock is fully shown (an auto-hidden Dock
    /// slides up for the effect, see `peek`).
    pub fn icon_center(&self, cx: &Ctx, key: &AppKey) -> Option<(i32, i32)> {
        let i = self.items.iter().position(|it| it.kind == Kind::App && it.key.matches(key))?;
        let (left, w, _) = *self.lay.slots.get(i)?;
        let m = metrics(cx);
        let py = self.win_h as f32 - m.margin - m.panel_h;
        Some((self.win_x + (left + w / 2.0) as i32, self.win_y + (py + m.panel_h / 2.0) as i32))
    }

    /// Show an auto-hidden Dock for a moment (a window is minimizing into it).
    pub fn peek(&mut self) {
        if self.autohide && !self.hover && self.shown {
            self.hide.set(0.0);
            self.hide_at = Some(Instant::now() + std::time::Duration::from_millis(1300));
            self.dirty = true;
        }
    }

    /// Height in pixels the Dock occupies at the bottom of the screen.
    pub fn reserved_height(cx: &Ctx) -> i32 {
        let m = metrics(cx);
        (m.panel_h + m.margin * 2.0).ceil() as i32
    }

    fn layout(&self, cx: &Ctx) -> Lay {
        let m = metrics(cx);
        let w = self.mw() as f32;
        let slot_w: Vec<f32> = self
            .items
            .iter()
            .map(|it| {
                let a = it.appear.v.clamp(0.0, 1.15);
                if it.kind == Kind::Sep { 14.0 * cx.s * a } else { (m.icon + m.gap) * a }
            })
            .collect();
        let rest_total: f32 = slot_w.iter().sum();
        let rest_start = (w - rest_total) / 2.0;
        let amt = self.mag.v.max(0.0);
        let radius = 2.6 * (m.icon + m.gap);

        // Scale from the distance between the cursor and each icon's resting center.
        let mut slots = Vec::with_capacity(self.items.len());
        let mut acc = rest_start;
        for (it, &sw) in self.items.iter().zip(&slot_w) {
            let center = acc + sw / 2.0;
            let d = (self.cursor - center).abs();
            let fall = if d < radius { 0.5 * (1.0 + (std::f32::consts::PI * d / radius).cos()) } else { 0.0 };
            let scale = if it.kind == Kind::Sep { 1.0 } else { 1.0 + (m.max_scale - 1.0) * amt * fall };
            slots.push((0.0, sw * scale, scale));
            acc += sw;
        }

        // Anchor: the resting point under the cursor stays under the cursor.
        let u = self.cursor.clamp(rest_start, rest_start + rest_total);
        let (mut rest_acc, mut mag_acc) = (rest_start, 0.0);
        for (i, &sw) in slot_w.iter().enumerate() {
            if u >= rest_acc + sw {
                mag_acc += slots[i].1;
                rest_acc += sw;
            } else {
                if sw > 0.0 {
                    mag_acc += (u - rest_acc) / sw * slots[i].1;
                }
                break;
            }
        }
        let start = u - mag_acc;
        let mut x = start;
        for slot in slots.iter_mut() {
            slot.0 = x;
            x += slot.1;
        }
        let total = x - start;
        let panel_y = self.win_h as f32 - m.margin - m.panel_h + self.hide.v.max(0.0) * (m.panel_h + m.margin + 2.0);
        Lay { panel: (start - m.pad_x, panel_y, total + m.pad_x * 2.0, m.panel_h), slots }
    }

    fn set_click_through(&self, on: bool) {
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let t = WS_EX_TRANSPARENT.0 as isize;
            let new = if on { ex | t } else { ex & !t };
            if new != ex {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new);
            }
        }
    }

    fn enter(&mut self, x: f32) {
        self.hover = true;
        self.cursor = x;
        self.mag.set(1.0);
        self.hide.set(0.0);
        self.hide_at = None;
        self.set_click_through(false);
        self.dirty = true;
    }

    fn leave(&mut self) {
        if !self.hover {
            return;
        }
        self.hover = false;
        self.tracking = false;
        self.hover_idx = None;
        self.mag.set(0.0);
        self.label.set(0.0);
        if self.autohide {
            self.hide_at = Some(Instant::now() + std::time::Duration::from_millis(700));
        }
        self.set_click_through(true);
        self.dirty = true;
    }

    fn in_zone(&self, cx: &Ctx, x: f32, y: f32) -> bool {
        let m = metrics(cx);
        let (px, py, pw, _) = self.lay.panel;
        let edge = 6.0 * cx.s;
        // With auto-hide, the whole bottom edge of the screen keeps the Dock up.
        if self.autohide && y >= self.win_h as f32 - 3.0 * cx.s {
            return true;
        }
        x >= px - edge && x <= px + pw + edge && y >= py - m.extra * self.mag.v.clamp(0.0, 1.0) - edge
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, menu: &mut Menu, hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if let Some(&(_, mon)) = self.edges.iter().find(|e| e.0 == hwnd) {
            // Pointer pushed against the bottom of another display: come over.
            if msg == WM_MOUSEMOVE && self.drag.is_none() && !menu.visible {
                let mut pt = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut pt);
                }
                self.move_to(cx, mon);
                self.enter((pt.x - self.win_x) as f32);
            }
            return Some(LRESULT(0));
        }
        if hwnd == self.glass.hwnd {
            if msg == WM_MOUSEMOVE && !self.hover {
                let mut pt = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut pt);
                }
                self.enter((pt.x - self.win_x) as f32);
            }
            return None;
        }
        if hwnd != self.hwnd {
            return None;
        }
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                if let Some(i) = self.hover_idx.filter(|&i| self.items.get(i).is_some_and(|it| it.kind == Kind::App)) {
                    self.drag = Some(Drag { idx: i, start: (x, y), pos: (x, y), active: false });
                    unsafe {
                        SetCapture(self.hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE if self.drag.is_some() => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                self.drag_to(cx, x, y);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE if self.drag.is_some() => Some(LRESULT(0)),
            WM_CAPTURECHANGED => {
                if let Some(d) = self.drag.take() {
                    if d.active {
                        self.drop_item(cx, d);
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                if !self.in_zone(cx, x, y) {
                    self.leave();
                    return Some(LRESULT(0));
                }
                if !self.tracking {
                    self.tracking = true;
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                self.enter(x);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                // The pointer may have moved onto our own popup menu; keep the Dock up then.
                if !menu.visible {
                    self.leave();
                } else {
                    self.tracking = false;
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                match self.drag.take() {
                    Some(d) => {
                        unsafe {
                            let _ = ReleaseCapture();
                        }
                        if d.active {
                            self.drop_item(cx, d);
                        } else {
                            self.click(cx);
                        }
                    }
                    None => self.click(cx),
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                self.context_menu(cx, menu);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn click(&mut self, cx: &mut Ctx) {
        let Some(i) = self.hover_idx else { return };
        let Some(it) = self.items.get_mut(i) else { return };
        match it.kind {
            Kind::Launchpad => cx.cmds.push(Cmd::Launchpad),
            Kind::Trash => shell_open("shell:RecycleBinFolder"),
            Kind::Sep => {}
            Kind::App => {
                // The window list may be a moment old; a closed window can't be activated.
                cx.tracker.refresh();
                let wins: Vec<HWND> = cx.tracker.windows_of(&it.key).map(|w| w.hwnd).collect();
                if wins.is_empty() {
                    shell_open(&it.launch);
                    if cx.animate {
                        it.bounce = Some((Instant::now(), None));
                    }
                } else {
                    let live = unsafe { GetForegroundWindow() };
                    let fg = if tracker::window_pid(live) == std::process::id() { cx.tracker.fg } else { live };
                    let front_is_ours = wins.contains(&fg) && unsafe { !IsIconic(fg).as_bool() };
                    if !front_is_ours {
                        // Like macOS: the app comes forward with all of its windows.
                        tracker::bring_all_to_front(&wins);
                    } else if wins.len() == 1 {
                        cx.cmds.push(Cmd::Minimize(fg));
                    } else {
                        // Several windows: show their live previews.
                        cx.cmds.push(Cmd::DockPreview(i));
                    }
                }
                self.dirty = true;
            }
        }
    }

    /// App key of item `i` and where its preview should hang (icon centre x, Dock top y).
    pub fn preview_target(&self, i: usize) -> Option<(AppKey, (i32, i32))> {
        let it = self.items.get(i).filter(|it| it.kind == Kind::App && it.running)?;
        let (left, w, _) = *self.lay.slots.get(i)?;
        Some((it.key.clone(), (self.win_x + (left + w / 2.0) as i32, self.win_y + self.lay.panel.1 as i32)))
    }

    /// How far up an icon must be pulled to take it out of the Dock.
    fn remove_distance(cx: &Ctx) -> f32 {
        metrics(cx).panel_h * 1.6
    }

    fn drag_removes(&self, cx: &Ctx, d: &Drag) -> bool {
        self.items.get(d.idx).is_some_and(|it| it.pinned) && d.start.1 - d.pos.1 > Self::remove_distance(cx)
    }

    /// Pointer moved while an icon is held: follow it and make room.
    fn drag_to(&mut self, cx: &Ctx, x: f32, y: f32) {
        let Some(mut d) = self.drag else { return };
        d.pos = (x, y);
        if !d.active && ((x - d.start.0).abs() > 6.0 * cx.s || (y - d.start.1).abs() > 6.0 * cx.s) {
            d.active = true;
            self.label.set(0.0);
        }
        if d.active {
            self.cursor = x;
            self.mag.set(1.0);
            // Apps live between Launchpad (first) and the separator.
            let first = 1;
            let sep = self.items.iter().position(|it| it.kind == Kind::Sep).unwrap_or(self.items.len());
            let center = |lay: &Lay, j: usize| lay.slots.get(j).map(|&(l, w, _)| l + w / 2.0);
            loop {
                let i = d.idx;
                let Some(&(_, w, _)) = self.lay.slots.get(i) else { break };
                if i + 1 < sep && center(&self.lay, i + 1).is_some_and(|c| x > c) {
                    self.items.swap(i, i + 1);
                    self.lay.slots.swap(i, i + 1);
                    // The neighbour moved one slot left: start it where it was.
                    self.items[i].slide.v += w;
                    d.idx = i + 1;
                } else if i > first && center(&self.lay, i - 1).is_some_and(|c| x < c) {
                    self.items.swap(i, i - 1);
                    self.lay.slots.swap(i, i - 1);
                    self.items[i].slide.v -= w;
                    d.idx = i - 1;
                } else {
                    break;
                }
            }
        }
        self.drag = Some(d);
        self.dirty = true;
    }

    /// Icon let go: keep the new order, or take it out of the Dock.
    fn drop_item(&mut self, cx: &mut Ctx, d: Drag) {
        if self.drag_removes(cx, &d) {
            let launch = self.items[d.idx].launch.clone();
            cx.cmds.push(Cmd::Unpin(launch));
        } else if let Some(it) = self.items.get_mut(d.idx) {
            // Dragging a running app into place keeps it in the Dock, like macOS.
            it.pinned = true;
            cx.cfg.dock.pinned = self.items.iter().filter(|it| it.kind == Kind::App && it.pinned).map(|it| it.launch.clone()).collect();
            cx.cfg.save();
        }
        if !self.in_zone(cx, d.pos.0, d.pos.1) {
            self.leave();
        }
        self.dirty = true;
    }

    /// Keep the Dock shown (e.g. while its menu is open), then let auto-hide resume.
    pub fn set_held(&mut self, cx: &Ctx, on: bool) {
        if self.held == on {
            return;
        }
        self.held = on;
        if !on && self.hover {
            // The pointer may have left while the menu was up (that leave was
            // ignored); without this the Dock would stay magnified.
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut pt);
            }
            if !self.in_zone(cx, pt.x as f32, pt.y as f32) {
                self.leave();
            }
        }
        if on {
            self.hide_at = None;
            self.hide.set(0.0);
        } else if self.autohide && !self.hover {
            self.hide_at = Some(Instant::now() + std::time::Duration::from_millis(700));
        }
        self.dirty = true;
    }

    /// The running app under the pointer, if any.
    pub fn hovered_app(&self) -> Option<(AppKey, (i32, i32))> {
        if !self.hover || self.drag.is_some_and(|d| d.active) {
            return None;
        }
        self.preview_target(self.hover_idx?)
    }

    fn context_menu(&mut self, cx: &mut Ctx, menu: &mut Menu) {
        let mut items = Vec::new();
        let mut x = self.cursor;
        if let Some(i) = self.hover_idx {
            let it = &self.items[i];
            if let Some(&(left, w, _)) = self.lay.slots.get(i) {
                x = left + w / 2.0;
            }
            if it.kind == Kind::App {
                items.push(MenuItem::new(if it.running { "New Window" } else { "Open" }, Cmd::Launch(it.launch.clone())));
                items.push(MenuItem::sep());
                if it.pinned {
                    items.push(MenuItem::new("Remove from Dock", Cmd::Unpin(it.launch.clone())));
                } else {
                    items.push(MenuItem::new("Keep in Dock", Cmd::Pin(it.launch.clone())));
                }
                if it.running {
                    items.push(MenuItem::sep());
                    items.push(MenuItem::new("Quit", Cmd::CloseApp(it.key.clone())));
                }
            } else if it.kind == Kind::Trash {
                items.push(MenuItem::new("Open", Cmd::Launch("shell:RecycleBinFolder".into())));
                items.push(MenuItem::new("Empty Recycle Bin", Cmd::EmptyTrash));
            }
        }
        if items.is_empty() {
            items.push(MenuItem::check("Turn Hiding On", Cmd::ToggleDockHiding, self.autohide));
            items.push(MenuItem::new("Dock Settings…", Cmd::OpenSettings));
            items.push(MenuItem::sep());
            items.push(MenuItem::new("Quit WinMacDock", Cmd::QuitSelf));
        }
        let y = self.win_y + (self.lay.panel.1 - 10.0 * cx.s) as i32;
        menu.show(cx, items, self.win_x + x as i32, y, Anchor::Above, MENU_TAG_DOCK);
    }

    pub fn needs_frame(&self) -> bool {
        self.shown
            && (self.dirty
                || self.mag.active()
                || self.label.active()
                || self.hide.active()
                || self.hide_at.is_some()
                || self.drag.is_some_and(|d| d.active)
                || self.items.iter().any(|it| it.appear.active() || it.slide.active() || it.bounce.is_some()))
    }

    pub fn frame(&mut self, cx: &mut Ctx, dt: f32) {
        if !self.shown {
            return;
        }
        self.dirty = false;
        self.mag.tick(dt);
        self.label.tick(dt);
        if self.hide_at.is_some_and(|t| Instant::now() >= t) {
            self.hide_at = None;
            if !self.hover && !self.held {
                self.hide.set(1.0);
            }
        }
        self.hide.tick(dt);
        let mut gone = false;
        for it in self.items.iter_mut() {
            it.slide.tick(dt);
            it.appear.tick(dt);
            gone |= it.removing && !it.appear.active() && it.appear.v < 0.02;
        }
        if gone {
            self.items.retain(|it| !(it.removing && !it.appear.active() && it.appear.v < 0.02));
        }

        let m = metrics(cx);
        let s = cx.s;
        let lay = self.layout(cx);
        let (px, py, pw, ph) = lay.panel;

        // Which item is under the cursor (in the magnified layout).
        let hit = if self.hover {
            lay.slots
                .iter()
                .position(|&(l, w, _)| self.cursor >= l && self.cursor < l + w)
                .filter(|&i| self.items[i].kind != Kind::Sep)
        } else {
            None
        };
        // No name label while an icon is being dragged.
        let hit = if self.drag.is_some_and(|d| d.active) { None } else { hit };
        if hit != self.hover_idx {
            self.hover_idx = hit;
            match hit {
                Some(i) => {
                    self.label_text = self.items[i].name.clone();
                    self.label.set(1.0);
                }
                None => self.label.set(0.0),
            }
        }

        if self.autohide && self.hide.v > 0.985 && !self.hide.active() {
            // Hidden: leave an invisible 2px strip at the screen edge to catch the pointer.
            self.glass.set_opacity(0.0);
            // The whole bottom edge reveals the Dock: its width cannot be seen while hidden.
            self.glass.set_rect(self.mon.left, self.mon.bottom - 2, self.mw(), 2, 0);
        } else {
            self.glass.set_opacity(1.0);
            self.glass.set_rect(self.win_x + px.round() as i32, self.win_y + py.round() as i32, pw.round() as i32, ph.round() as i32, (20.0 * s) as i32);
        }

        self.surf.begin(&cx.gfx);
        let edge = if cx.dark { rgba(1.0, 1.0, 1.0, 0.16) } else { rgba(1.0, 1.0, 1.0, 0.55) };
        cx.gfx.stroke_rrect(rect(px.round() + 0.5, py.round() + 0.5, pw.round() - 1.0, ph.round() - 1.0), 20.0 * s, 1.0, edge);

        let bottom = py + ph - m.pad_bottom;
        let now = Instant::now();
        for (i, it) in self.items.iter_mut().enumerate() {
            let (left, w, scale) = lay.slots[i];
            let a = it.appear.v.clamp(0.0, 1.0);
            if it.kind == Kind::Sep {
                let c = if cx.dark { rgba(1.0, 1.0, 1.0, 0.22 * a) } else { rgba(0.0, 0.0, 0.0, 0.2 * a) };
                cx.gfx.line(left + w / 2.0, py + 9.0 * s, left + w / 2.0, py + ph - 9.0 * s, 1.0, c);
                continue;
            }

            // Launch bounce: parabolic hops until the app's window shows up.
            let mut lift = 0.0;
            if let Some((t0, end)) = it.bounce {
                let t = now.duration_since(t0).as_secs_f32();
                let end = end.or_else(|| {
                    (it.running || t > 8.0).then(|| ((t / BOUNCE_PERIOD).ceil().max(1.0)) * BOUNCE_PERIOD)
                });
                match end {
                    Some(e) if t >= e => it.bounce = None,
                    _ => {
                        it.bounce = Some((t0, end));
                        let p = (t / BOUNCE_PERIOD).fract();
                        lift = 4.0 * p * (1.0 - p) * m.icon * 0.5;
                    }
                }
            }

            let dragged = self.drag.is_some_and(|d| d.active && d.idx == i);
            let size = m.icon * scale * it.appear.v.clamp(0.0, 1.1);
            let left = left + it.slide.v;
            let x = left + (w - size) / 2.0;
            if dragged {
                // Its place stays open; the icon itself is drawn at the pointer below.
                continue;
            }
            let mut y = bottom - size - lift;
            // At rest, land on whole pixels so icons stay crisp (moving icons
            // keep sub-pixel positions for smooth motion).
            let (x, size) = if (scale - 1.0).abs() < 0.002 && !it.appear.active() && it.slide.v.abs() < 0.5 && lift == 0.0 {
                y = y.round();
                (x.round(), size.round())
            } else {
                (x, size)
            };
            let r = rect(x, y, size, size);
            match (&it.icon, it.kind) {
                (_, Kind::Launchpad) => draw_launchpad(cx, x, y, size, a),
                (Some(bmp), _) => cx.gfx.bitmap(bmp, r, a),
                (None, _) => cx.gfx.fill_rrect(r, size * 0.22, rgba(0.5, 0.5, 0.55, 0.8 * a)),
            }
            if it.running {
                let c = if cx.dark { rgba(1.0, 1.0, 1.0, 0.85 * a) } else { rgba(0.0, 0.0, 0.0, 0.6 * a) };
                cx.gfx.fill_circle(left + w / 2.0, py + ph - 5.0 * s, 2.0 * s, c);
            }
            if self.hover_idx == Some(i) {
                self.label_pos = (left + w / 2.0, y);
            }
        }

        // The icon being dragged follows the pointer.
        if let Some(d) = self.drag.filter(|d| d.active) {
            if let Some(it) = self.items.get(d.idx) {
                let size = m.icon * m.max_scale.max(1.2);
                let (x, y) = (d.pos.0 - size / 2.0, (d.pos.1 - size / 2.0).max(0.0));
                let removing = self.drag_removes(cx, &d);
                let a = if removing { 0.55 } else { 0.95 };
                match &it.icon {
                    Some(bmp) => cx.gfx.bitmap(bmp, rect(x, y, size, size), a),
                    None => cx.gfx.fill_rrect(rect(x, y, size, size), size * 0.22, rgba(0.5, 0.5, 0.55, 0.8 * a)),
                }
                if removing {
                    let r = rect(d.pos.0 - 40.0 * s, (y - 30.0 * s).max(0.0), 80.0 * s, 24.0 * s);
                    cx.gfx.fill_rrect(r, 7.0 * s, rgba(0.16, 0.16, 0.17, 0.92));
                    cx.gfx.text("Remove", 12.5 * s, 600, r, 0, rgba(1.0, 1.0, 1.0, 0.95));
                }
            }
        }

        // Name label above the hovered icon.
        let la = self.label.v.clamp(0.0, 1.0);
        if la > 0.01 && !self.label_text.is_empty() {
            let tw = cx.gfx.text_width(&self.label_text, 13.0 * s, 400);
            let (lw, lh) = (tw + 22.0 * s, 26.0 * s);
            let lx = (self.label_pos.0 - lw / 2.0).clamp(4.0, self.mw() as f32 - lw - 4.0);
            let ly = (self.label_pos.1 - lh - 8.0 * s - (1.0 - la) * 4.0 * s).max(0.0);
            let r = rect(lx, ly, lw, lh);
            let (bg, fg) = if cx.dark {
                (rgba(0.16, 0.16, 0.17, 0.92 * la), rgba(1.0, 1.0, 1.0, 0.95 * la))
            } else {
                (rgba(0.96, 0.96, 0.96, 0.94 * la), rgba(0.0, 0.0, 0.0, 0.9 * la))
            };
            cx.gfx.fill_rrect(r, 7.0 * s, bg);
            cx.gfx.stroke_rrect(r, 7.0 * s, 1.0, rgba(0.5, 0.5, 0.5, 0.35 * la));
            cx.gfx.text(&self.label_text, 13.0 * s, 400, r, 0, fg);
        }
        self.surf.end(&cx.gfx);
        self.lay = lay;
    }
}

impl Item {
    fn fixed(kind: Kind, name: &str, icon: Option<ID2D1Bitmap>) -> Self {
        Self {
            kind,
            key: AppKey::default(),
            launch: String::new(),
            name: name.into(),
            icon,
            pinned: true,
            running: false,
            removing: false,
            appear: Spring::new(1.0, 260.0, 26.0),
            bounce: None,
            slide: Spring::new(0.0, 420.0, 36.0),
        }
    }
}

/// Our own Launchpad glyph: a rounded tile with a 3×3 grid of colored dots.
fn draw_launchpad(cx: &Ctx, x: f32, y: f32, size: f32, a: f32) {
    let inset = size * 0.06;
    let tile = rect(x + inset, y + inset, size - inset * 2.0, size - inset * 2.0);
    cx.gfx.fill_rrect(tile, size * 0.22, rgba(0.20, 0.21, 0.25, a));
    cx.gfx.stroke_rrect(tile, size * 0.22, 1.0, rgba(1.0, 1.0, 1.0, 0.18 * a));
    const COLORS: [(f32, f32, f32); 9] = [
        (1.00, 0.33, 0.30),
        (1.00, 0.62, 0.10),
        (1.00, 0.84, 0.10),
        (0.25, 0.80, 0.40),
        (0.20, 0.75, 0.90),
        (0.10, 0.52, 1.00),
        (0.55, 0.40, 0.95),
        (0.95, 0.35, 0.65),
        (0.75, 0.76, 0.80),
    ];
    let cell = (size - inset * 2.0) / 4.0;
    for (i, (r, g, b)) in COLORS.iter().enumerate() {
        let cxp = x + inset + cell * (1.0 + (i % 3) as f32);
        let cyp = y + inset + cell * (1.0 + (i / 3) as f32);
        let d = cell * 0.36;
        cx.gfx.fill_rrect(rect(cxp - d, cyp - d, d * 2.0, d * 2.0), d * 0.45, rgba(*r, *g, *b, a));
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        self.shutdown();
    }
}
