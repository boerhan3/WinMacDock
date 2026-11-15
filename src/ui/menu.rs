//! Custom popup menu (Dock, menu bar, Launchpad). It never takes focus; a
//! click outside it closes it.

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::Spring;
use crate::render::gfx::Surface;
use crate::util::{lp_y, rect, rgba, WM_MOUSELEAVE};
use crate::win::glass::Glass;
use crate::win::outside;

pub struct MenuItem {
    pub label: String,
    /// `None` with an empty label is a separator; `None` with a label is disabled text.
    pub cmd: Option<Cmd>,
    pub checked: bool,
}

impl MenuItem {
    pub fn new(label: &str, cmd: Cmd) -> Self {
        Self { label: label.into(), cmd: Some(cmd), checked: false }
    }
    pub fn check(label: &str, cmd: Cmd, checked: bool) -> Self {
        Self { label: label.into(), cmd: Some(cmd), checked }
    }
    pub fn sep() -> Self {
        Self { label: String::new(), cmd: None, checked: false }
    }
    pub fn disabled(label: &str) -> Self {
        Self { label: label.into(), cmd: None, checked: false }
    }
}

/// Where the menu attaches to the anchor point.
#[derive(Clone, Copy, PartialEq)]
pub enum Anchor {
    /// Menu hangs below the point, left edge at x.
    Below,
    /// Menu sits above the point, centered on x.
    Above,
    /// Top-left corner at the point (context menus).
    At,
}

pub struct Menu {
    glass: Glass,
    pub hwnd: HWND,
    surf: Option<Surface>,
    items: Vec<MenuItem>,
    hover: Option<usize>,
    open: Spring,
    pub visible: bool,
    closing: bool,
    tracking: bool,
    dirty: bool,
    size: (i32, i32),
    /// Indent the labels to leave room for check marks.
    check_col: bool,
    /// Identifies what opened the menu, so a second click on it can toggle.
    pub owner_tag: u32,
    last_closed: (u32, std::time::Instant),
    /// Window holding the Esc/arrow/Enter hotkeys while the menu is open.
    keys_on: Option<HWND>,
}

/// Hotkey ids used while a menu is open (handled by the main window).
pub const HOTKEY_MENU_FIRST: i32 = 10;
const MENU_KEYS: [u32; 4] = [0x1B, 0x26, 0x28, 0x0D]; // Esc, Up, Down, Enter

impl Menu {
    pub fn new(cx: &Ctx) -> Self {
        let glass = Glass::new(menu_tint(cx.dark));
        let hwnd = app::create_window(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            Some(glass.hwnd),
        );
        Self {
            glass,
            hwnd,
            surf: None,
            items: Vec::new(),
            hover: None,
            open: Spring::new(0.0, 700.0, 46.0),
            visible: false,
            closing: false,
            tracking: false,
            dirty: false,
            size: (0, 0),
            check_col: false,
            owner_tag: 0,
            last_closed: (0, std::time::Instant::now()),
            keys_on: None,
        }
    }

    /// The menu never takes focus, so grab Esc/arrows/Enter as hotkeys
    /// while it is open (like a macOS menu, which owns the keyboard).
    fn grab_keys(&mut self, main: Option<HWND>) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS};
        unsafe {
            if let Some(h) = self.keys_on.take() {
                for i in 0..MENU_KEYS.len() as i32 {
                    let _ = UnregisterHotKey(Some(h), HOTKEY_MENU_FIRST + i);
                }
            }
            if let Some(h) = main {
                for (i, &vk) in MENU_KEYS.iter().enumerate() {
                    let _ = RegisterHotKey(Some(h), HOTKEY_MENU_FIRST + i as i32, HOT_KEY_MODIFIERS(0), vk);
                }
                self.keys_on = Some(h);
            }
        }
    }

    /// A menu hotkey fired; `id` is the hotkey id.
    pub fn on_key(&mut self, cx: &mut Ctx, id: i32) {
        if !self.visible || self.closing {
            return;
        }
        let selectable: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].cmd.is_some()).collect();
        match MENU_KEYS.get((id - HOTKEY_MENU_FIRST) as usize) {
            Some(0x1B) => self.close(cx),
            Some(&vk @ (0x26 | 0x28)) if !selectable.is_empty() => {
                let pos = self.hover.and_then(|h| selectable.iter().position(|&i| i == h));
                let n = selectable.len();
                let next = match (pos, vk) {
                    (None, 0x28) => 0,
                    (None, _) => n - 1,
                    (Some(p), 0x28) => (p + 1) % n,
                    (Some(p), _) => (p + n - 1) % n,
                };
                self.hover = Some(selectable[next]);
                self.dirty = true;
            }
            Some(0x0D) => {
                if let Some(cmd) = self.hover.and_then(|i| self.items[i].cmd.clone()) {
                    cx.cmds.push(cmd);
                }
                self.close(cx);
            }
            _ => {}
        }
    }

    fn row_h(cx: &Ctx, it: &MenuItem) -> f32 {
        if it.label.is_empty() { 9.0 * cx.s } else { 24.0 * cx.s }
    }

    fn text_x(&self, s: f32) -> f32 {
        if self.check_col { 26.0 * s } else { 15.0 * s }
    }

    /// Open at a screen point.
    pub fn show(&mut self, cx: &mut Ctx, items: Vec<MenuItem>, x: i32, y: i32, anchor: Anchor, tag: u32) {
        let s = cx.s;
        self.check_col = items.iter().any(|i| i.checked);
        let pad = 5.0 * s;
        let mut w = 120.0 * s;
        let mut h = pad * 2.0;
        for it in &items {
            w = w.max(cx.gfx.text_width(&it.label, 13.0 * s, 400) + self.text_x(s) + 19.0 * s);
            h += Self::row_h(cx, it);
        }
        // Long window titles are cut with an ellipsis instead of making a huge menu.
        let w = w.min(420.0 * s);
        let (w, h) = (w.ceil() as i32, h.ceil() as i32);
        let (mut x, mut y) = match anchor {
            Anchor::Below | Anchor::At => (x, y),
            Anchor::Above => (x - w / 2, y - h),
        };
        // Stay on the display the menu was opened on.
        (x, y) = crate::win::monitors::clamp_box(x, y, w, h, 4);

        self.items = items;
        self.hover = None;
        self.size = (w, h);
        self.owner_tag = tag;
        self.glass.set_tint(menu_tint(cx.dark));
        self.glass.set_rect(x, y, w, h, (9.0 * s) as i32);
        self.glass.raise();
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        }
        match &mut self.surf {
            Some(surf) => surf.resize(&cx.gfx, w as u32, h as u32),
            None => self.surf = Surface::new(&cx.gfx, self.hwnd, w as u32, h as u32).ok(),
        }
        outside::watch(outside::SLOT_MENU, RECT { left: x, top: y, right: x + w, bottom: y + h }, cx.hinst);
        if self.keys_on.is_none() {
            self.grab_keys(Some(cx.main));
        }

        self.visible = true;
        self.closing = false;
        self.open.snap(if cx.animate { 0.0 } else { 1.0 });
        self.open.set(1.0);
        self.dirty = true;
        self.glass.show(true);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    pub fn close(&mut self, cx: &Ctx) {
        if !self.visible || self.closing {
            return;
        }
        outside::unwatch(outside::SLOT_MENU);
        self.grab_keys(None);
        self.last_closed = (self.owner_tag, std::time::Instant::now());
        if cx.animate {
            self.closing = true;
            self.open.set(0.0);
        } else {
            self.hide();
        }
    }

    /// True if a menu with this tag was dismissed a moment ago (the same
    /// click that dismissed it should not reopen it).
    pub fn just_closed(&self, tag: u32) -> bool {
        self.last_closed.0 == tag && self.last_closed.1.elapsed().as_millis() < 250
    }

    fn hide(&mut self) {
        self.grab_keys(None);
        self.visible = false;
        self.closing = false;
        self.owner_tag = 0;
        self.items.clear();
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn item_at(&self, cx: &Ctx, y: f32) -> Option<usize> {
        let mut top = 5.0 * cx.s;
        for (i, it) in self.items.iter().enumerate() {
            let h = Self::row_h(cx, it);
            if y >= top && y < top + h {
                return it.cmd.is_some().then_some(i);
            }
            top += h;
        }
        None
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.hwnd {
            return None;
        }
        match msg {
            WM_MOUSEMOVE => {
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
                let hit = self.item_at(cx, lp_y(lp) as f32);
                if hit != self.hover {
                    self.hover = hit;
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.hover.take().is_some() {
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                if let Some(i) = self.item_at(cx, lp_y(lp) as f32) {
                    if let Some(cmd) = self.items[i].cmd.clone() {
                        cx.cmds.push(cmd);
                    }
                    self.close(cx);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.visible && (self.dirty || self.open.active())
    }

    pub fn frame(&mut self, cx: &mut Ctx, dt: f32) {
        if !self.visible {
            return;
        }
        self.open.tick(dt);
        if self.closing && self.open.v <= 0.02 {
            self.hide();
            return;
        }
        self.dirty = false;
        let Some(surf) = &self.surf else { return };
        let s = cx.s;
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let a = self.open.v.clamp(0.0, 1.0);
        let fg = if cx.dark { rgba(1.0, 1.0, 1.0, 0.92 * a) } else { rgba(0.0, 0.0, 0.0, 0.88 * a) };
        let dim = if cx.dark { rgba(1.0, 1.0, 1.0, 0.35 * a) } else { rgba(0.0, 0.0, 0.0, 0.35 * a) };
        let tx = self.text_x(s);

        surf.begin(&cx.gfx);
        cx.gfx.stroke_rrect(rect(0.5, 0.5, w - 1.0, h - 1.0), 9.0 * s, 1.0, rgba(1.0, 1.0, 1.0, 0.18 * a));
        let mut top = 5.0 * s;
        for (i, it) in self.items.iter().enumerate() {
            let rh = Self::row_h(cx, it);
            if it.label.is_empty() {
                cx.gfx.line(12.0 * s, top + rh / 2.0, w - 12.0 * s, top + rh / 2.0, 1.0, dim);
            } else {
                let hot = self.hover == Some(i);
                if hot {
                    cx.gfx.fill_rrect(rect(5.0 * s, top, w - 10.0 * s, rh), 5.0 * s, rgba(0.04, 0.52, 1.0, a));
                }
                let color = if hot {
                    rgba(1.0, 1.0, 1.0, a)
                } else if it.cmd.is_some() {
                    fg
                } else {
                    dim
                };
                if it.checked {
                    cx.gfx.glyph('\u{E73E}', 11.0 * s, rect(6.0 * s, top, 18.0 * s, rh), color);
                }
                cx.gfx.text(&it.label, 13.0 * s, 400, rect(tx, top, w - tx - 14.0 * s, rh), -1, color);
            }
            top += rh;
        }
        surf.end(&cx.gfx);
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        self.glass.set_tint(menu_tint(cx.dark));
        self.dirty = true;
    }
}

fn menu_tint(dark: bool) -> u32 {
    if dark { 0xB8_26_26_28 } else { 0xC8_F4_F4_F4 }
}
