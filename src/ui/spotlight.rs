//! Spotlight: a centered search bar that finds and launches apps.

use std::collections::HashMap;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::{lerp, Spring};
use crate::render::gfx::Surface;
use crate::util::{lp_y, rect, rgba};
use crate::win::apps;
use crate::win::glass::Glass;
use crate::win::icons;

pub const ICON_CONSUMER: u8 = 2;
const MAX_ROWS: usize = 7;

pub struct Spotlight {
    glass: Glass,
    pub hwnd: HWND,
    surf: Option<Surface>,
    results: Vec<usize>,
    icons: HashMap<usize, Option<ID2D1Bitmap>>,
    query: String,
    selected: usize,
    open: Spring,
    /// Animated height of the result list in pixels.
    list_h: Spring,
    pub visible: bool,
    closing: bool,
    dirty: bool,
    win_h: i32,
    generation: u32,
}

fn tint(dark: bool) -> u32 {
    if dark { 0xA0_24_22_22 } else { 0xB0_F2_F2_F2 }
}

impl Spotlight {
    pub fn new(cx: &Ctx) -> Self {
        let glass = Glass::new(tint(cx.dark));
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, Some(glass.hwnd));
        Self {
            glass,
            hwnd,
            surf: None,
            results: Vec::new(),
            icons: HashMap::new(),
            query: String::new(),
            selected: 0,
            open: Spring::new(0.0, 420.0, 38.0),
            list_h: Spring::new(0.0, 480.0, 42.0),
            visible: false,
            closing: false,
            dirty: false,
            win_h: 0,
            generation: 0,
        }
    }

    /// Bar x, y, width, bar height, row height — all in screen pixels.
    fn geometry(cx: &Ctx) -> (i32, i32, i32, f32, f32) {
        let s = cx.s;
        let w = (660.0 * s).min(cx.sw as f32 - 40.0) as i32;
        ((cx.sw - w) / 2, (cx.sh as f32 * 0.22) as i32, w, 56.0 * s, 44.0 * s)
    }

    fn max_height(cx: &Ctx) -> i32 {
        let (_, _, _, bar, row) = Self::geometry(cx);
        (bar + row * MAX_ROWS as f32 + 10.0 * cx.s).ceil() as i32
    }

    pub fn toggle(&mut self, cx: &mut Ctx) {
        if self.visible && !self.closing {
            self.close(cx);
        } else {
            self.show(cx);
        }
    }

    fn show(&mut self, cx: &mut Ctx) {
        cx.request_apps();
        self.query.clear();
        self.results.clear();
        self.icons.clear();
        self.generation = self.generation.wrapping_add(1);
        self.selected = 0;
        self.list_h.snap(0.0);
        let (x, y, w, _, _) = Self::geometry(cx);
        let max_h = Self::max_height(cx);
        if self.surf.is_none() {
            self.surf = Surface::new(&cx.gfx, self.hwnd, w as u32, max_h as u32).ok();
        }
        self.glass.set_tint(tint(cx.dark));
        self.glass.set_opacity(0.0);
        self.win_h = 0;
        self.place(cx);
        self.glass.show(true);
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, w, max_h, SWP_NOACTIVATE | SWP_NOZORDER);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.visible = true;
        self.closing = false;
        if !cx.animate {
            self.open.snap(1.0);
        }
        self.open.set(1.0);
        self.dirty = true;
    }

    /// Size the glass to the bar plus the (animated) result list.
    fn place(&mut self, cx: &Ctx) {
        let (x, y, w, bar, _) = Self::geometry(cx);
        let extra = if self.list_h.v > 1.0 { self.list_h.v + 8.0 * cx.s } else { 0.0 };
        let h = (bar + extra).round() as i32;
        if h != self.win_h {
            self.win_h = h;
            self.glass.set_rect(x, y, w, h, (16.0 * cx.s) as i32);
        }
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
        self.visible = false;
        self.closing = false;
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.surf = None;
        self.icons.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// The shared app list was replaced; cached indices are stale.
    pub fn apps_changed(&mut self, cx: &Ctx) {
        self.icons.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.visible {
            self.search(cx);
        }
    }

    /// A background icon load finished.
    pub fn icon_ready(&mut self, generation: u32, index: usize, bmp: Option<ID2D1Bitmap>) {
        if generation == self.generation && self.visible {
            self.icons.insert(index, bmp);
            self.dirty = true;
        }
    }

    fn search(&mut self, cx: &Ctx) {
        let q = self.query.trim().to_lowercase();
        self.results.clear();
        if !q.is_empty() {
            let mut scored: Vec<(i32, usize)> =
                cx.apps.iter().enumerate().filter_map(|(i, a)| apps::score(&q, &a.lower).map(|s| (s, i))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.results = scored.into_iter().take(MAX_ROWS).map(|(_, i)| i).collect();
        }
        self.selected = 0;
        let (_, _, _, _, row) = Self::geometry(cx);
        self.list_h.set(self.results.len() as f32 * row);
        if !cx.animate {
            self.list_h.snap(self.results.len() as f32 * row);
        }
        self.dirty = true;
    }

    fn launch(&mut self, cx: &mut Ctx) {
        if let Some(&ai) = self.results.get(self.selected) {
            cx.cmds.push(Cmd::Launch(cx.apps[ai].launch.clone()));
            self.close(cx);
        }
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.hwnd {
            return None;
        }
        match msg {
            WM_KEYDOWN => {
                match wp.0 as u16 {
                    k if k == VK_ESCAPE.0 => self.close(cx),
                    k if k == VK_RETURN.0 => self.launch(cx),
                    k if k == VK_DOWN.0 && !self.results.is_empty() => {
                        self.selected = (self.selected + 1) % self.results.len();
                        self.dirty = true;
                    }
                    k if k == VK_UP.0 && !self.results.is_empty() => {
                        self.selected = (self.selected + self.results.len() - 1) % self.results.len();
                        self.dirty = true;
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CHAR => {
                match char::from_u32(wp.0 as u32) {
                    Some('\u{8}') => {
                        self.query.pop();
                        self.search(cx);
                    }
                    Some(c) if !c.is_control() => {
                        self.query.push(c);
                        self.search(cx);
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE | WM_LBUTTONUP => {
                let (_, _, _, bar, row) = Self::geometry(cx);
                let y = lp_y(lp) as f32 - bar;
                if y >= 0.0 {
                    let i = (y / row) as usize;
                    if i < self.results.len() {
                        if self.selected != i {
                            self.selected = i;
                            self.dirty = true;
                        }
                        if msg == WM_LBUTTONUP {
                            self.launch(cx);
                        }
                    }
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    self.close(cx);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.visible && (self.dirty || self.open.active() || self.list_h.active())
    }

    pub fn frame(&mut self, cx: &mut Ctx, dt: f32) {
        if !self.visible {
            return;
        }
        self.dirty = false;
        self.open.tick(dt);
        self.list_h.tick(dt);
        if self.closing && self.open.v < 0.02 {
            self.hide();
            return;
        }
        let t = self.open.v.clamp(0.0, 1.0);
        self.glass.set_opacity(t);
        self.place(cx);

        let s = cx.s;
        let (_, _, w, bar, row) = Self::geometry(cx);
        let w = w as f32;
        let icon_px = (30.0 * s).ceil() as i32;
        for &ai in &self.results {
            if !self.icons.contains_key(&ai) {
                self.icons.insert(ai, None);
                cx.icon_loader.request(icons::icon_id(ICON_CONSUMER, self.generation, ai), &cx.apps[ai].launch, icon_px);
            }
        }
        let Some(surf) = &self.surf else { return };
        let dark = cx.dark;
        let ink = move |a: f32| if dark { rgba(1.0, 1.0, 1.0, a * t) } else { rgba(0.0, 0.0, 0.0, a * t) };

        surf.begin(&cx.gfx);
        cx.gfx.set_scale(lerp(0.96, 1.0, t), w / 2.0, bar / 2.0);
        let total_h = self.win_h as f32;
        cx.gfx.stroke_rrect(rect(0.5, 0.5, w - 1.0, total_h - 1.0), 16.0 * s, 1.0, rgba(1.0, 1.0, 1.0, 0.2 * t));
        cx.gfx.glyph('\u{E721}', 20.0 * s, rect(10.0 * s, 0.0, 44.0 * s, bar), ink(0.6));
        let text_r = rect(56.0 * s, 0.0, w - 72.0 * s, bar);
        if self.query.is_empty() {
            cx.gfx.text("Spotlight Search", 22.0 * s, 300, text_r, -1, ink(0.35));
        } else {
            cx.gfx.text(&self.query, 22.0 * s, 300, text_r, -1, ink(0.95));
            let caret_x = text_r.left + cx.gfx.text_width(&self.query, 22.0 * s, 300) + 2.0 * s;
            cx.gfx.line(caret_x, bar * 0.28, caret_x, bar * 0.72, 1.5 * s, rgba(0.04, 0.52, 1.0, t));
        }

        // Results, revealed as the list height animates.
        let visible_h = self.list_h.v;
        if visible_h > 1.0 {
            cx.gfx.line(14.0 * s, bar, w - 14.0 * s, bar, 1.0, ink(0.12));
        }
        for (i, &ai) in self.results.iter().enumerate() {
            let top = bar + 4.0 * s + i as f32 * row;
            let reveal = ((visible_h - i as f32 * row) / row).clamp(0.0, 1.0);
            if reveal <= 0.0 {
                break;
            }
            let hot = i == self.selected;
            if hot {
                cx.gfx.fill_rrect(rect(8.0 * s, top, w - 16.0 * s, row), 8.0 * s, rgba(0.04, 0.52, 1.0, 0.9 * t * reveal));
            }
            let isz = 30.0 * s;
            let ir = rect(18.0 * s, top + (row - isz) / 2.0, isz, isz);
            if let Some(Some(bmp)) = self.icons.get(&ai) {
                cx.gfx.bitmap(bmp, ir, t * reveal);
            }
            let color = if hot { rgba(1.0, 1.0, 1.0, t * reveal) } else { ink(0.92 * reveal) };
            cx.gfx.text(&cx.apps[ai].name, 15.0 * s, 400, rect(60.0 * s, top, w - 80.0 * s, row), -1, color);
        }
        cx.gfx.reset_transform();
        surf.end(&cx.gfx);
    }
}
