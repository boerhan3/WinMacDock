//! A tiny immediate-mode widget kit (switches, sliders, buttons, rows)
//! shared by the menu-bar panels and the Settings window. Input collected
//! from window messages is consumed by the widgets while drawing.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::Ctx;
use crate::util::{lp_x, lp_y, rect, rgba, WM_MOUSELEAVE};

pub const ACCENT: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.04, g: 0.52, b: 1.0, a: 1.0 };

#[derive(Default)]
pub struct Input {
    pub mouse: (f32, f32),
    pub inside: bool,
    pub down: bool,
    /// Where the button went down, until a widget claims it.
    press: Option<(f32, f32)>,
    /// Where the button was released, until a widget claims it.
    click: Option<(f32, f32)>,
    right_click: Option<(f32, f32)>,
    /// Widget being dragged.
    active: Option<u32>,
    tracking: bool,
    pub wheel: f32,
}

impl Input {
    /// Feed a window message. Returns true when the window should redraw.
    pub fn on_msg(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> bool {
        let pos = (lp_x(lp) as f32, lp_y(lp) as f32);
        match msg {
            WM_MOUSEMOVE => {
                if !self.tracking {
                    self.tracking = true;
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                self.mouse = pos;
                self.inside = true;
                true
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                self.inside = false;
                true
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                self.mouse = pos;
                self.down = true;
                self.press = Some(pos);
                unsafe {
                    SetCapture(hwnd);
                }
                true
            }
            WM_LBUTTONUP => {
                self.mouse = pos;
                self.down = false;
                // A drag ends where it ends; it is not also a click.
                if self.active.take().is_none() {
                    self.click = Some(pos);
                }
                self.press = None;
                unsafe {
                    let _ = ReleaseCapture();
                }
                true
            }
            WM_RBUTTONUP => {
                self.right_click = Some(pos);
                true
            }
            WM_MOUSEWHEEL => {
                self.wheel += (wp.0 >> 16) as i16 as f32 / 120.0;
                true
            }
            WM_CAPTURECHANGED => {
                self.down = false;
                self.active = None;
                true
            }
            _ => false,
        }
    }

    /// Drop unclaimed clicks at the end of a frame.
    pub fn end_frame(&mut self) {
        self.click = None;
        self.right_click = None;
        self.wheel = 0.0;
        if !self.down {
            self.press = None;
        }
    }

    pub fn dragging(&self) -> bool {
        self.active.is_some()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

fn inside(r: &D2D_RECT_F, p: (f32, f32)) -> bool {
    p.0 >= r.left && p.0 < r.right && p.1 >= r.top && p.1 < r.bottom
}

pub struct Ui<'a> {
    pub cx: &'a mut Ctx,
    pub inp: &'a mut Input,
    /// Overall opacity (fade-in/out).
    pub a: f32,
}

impl Ui<'_> {
    pub fn s(&self) -> f32 {
        self.cx.s
    }

    /// Foreground ink at the given strength.
    pub fn ink(&self, k: f32) -> D2D1_COLOR_F {
        if self.cx.dark { rgba(1.0, 1.0, 1.0, k * self.a) } else { rgba(0.0, 0.0, 0.0, k * self.a) }
    }

    pub fn fade(&self, mut c: D2D1_COLOR_F) -> D2D1_COLOR_F {
        c.a *= self.a;
        c
    }

    pub fn hover(&self, r: D2D_RECT_F) -> bool {
        self.inp.inside && !self.inp.dragging() && inside(&r, self.inp.mouse)
    }

    /// True (once) when the mouse was released inside `r`.
    pub fn clicked(&mut self, r: D2D_RECT_F) -> bool {
        match self.inp.click {
            Some(p) if inside(&r, p) => {
                self.inp.click = None;
                true
            }
            _ => false,
        }
    }

    pub fn text(&mut self, s: &str, size: f32, weight: i32, r: D2D_RECT_F, align: i32, c: D2D1_COLOR_F) {
        let c = self.fade(c);
        self.cx.gfx.text(s, size * self.cx.s, weight, r, align, c);
    }

    pub fn label(&mut self, s: &str, size: f32, weight: i32, r: D2D_RECT_F, align: i32) {
        let c = self.ink(0.92);
        self.cx.gfx.text(s, size * self.cx.s, weight, r, align, c);
    }

    pub fn dim_label(&mut self, s: &str, size: f32, r: D2D_RECT_F, align: i32) {
        let c = self.ink(0.5);
        self.cx.gfx.text(s, size * self.cx.s, 400, r, align, c);
    }

    pub fn glyph(&mut self, g: char, size: f32, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let c = self.fade(c);
        self.cx.gfx.glyph(g, size * self.cx.s, r, c);
    }

    pub fn panel(&mut self, r: D2D_RECT_F, radius: f32) {
        let s = self.cx.s;
        let (bg, edge) = if self.cx.dark { (0.10, 0.08) } else { (0.55, 0.10) };
        let fill = if self.cx.dark { rgba(1.0, 1.0, 1.0, bg * self.a) } else { rgba(1.0, 1.0, 1.0, bg * self.a) };
        self.cx.gfx.fill_rrect(r, radius * s, fill);
        let c = self.ink(edge);
        self.cx.gfx.stroke_rrect(r, radius * s, 1.0, c);
    }

    pub fn separator(&mut self, x0: f32, x1: f32, y: f32) {
        let c = self.ink(0.12);
        self.cx.gfx.line(x0, y, x1, y, 1.0, c);
    }

    /// Row with a hover highlight; returns true when clicked.
    pub fn row(&mut self, r: D2D_RECT_F) -> bool {
        if self.hover(r) {
            let c = self.ink(0.09);
            self.cx.gfx.fill_rrect(r, 6.0 * self.cx.s, c);
        }
        self.clicked(r)
    }

    /// macOS-style switch. Returns true when clicked (caller flips the value).
    pub fn switch(&mut self, x: f32, y: f32, on: bool) -> bool {
        let s = self.cx.s;
        let (w, h) = (38.0 * s, 22.0 * s);
        let r = rect(x, y, w, h);
        let track = if on { self.fade(ACCENT) } else { self.ink(0.18) };
        self.cx.gfx.fill_rrect(r, h / 2.0, track);
        let kx = if on { x + w - h / 2.0 } else { x + h / 2.0 };
        self.cx.gfx.fill_circle(kx, y + h / 2.0, h / 2.0 - 2.0 * s, rgba(1.0, 1.0, 1.0, self.a));
        self.clicked(rect(x - 4.0 * s, y - 4.0 * s, w + 8.0 * s, h + 8.0 * s))
    }

    /// Circle toggle used by Control Center tiles.
    pub fn circle_toggle(&mut self, cx0: f32, cy0: f32, radius: f32, glyph: char, on: bool) -> bool {
        let bg = if on { self.fade(ACCENT) } else { self.ink(0.14) };
        self.cx.gfx.fill_circle(cx0, cy0, radius, bg);
        let fg = if on { rgba(1.0, 1.0, 1.0, self.a) } else { self.ink(0.85) };
        self.cx.gfx.glyph(glyph, radius * 0.9, rect(cx0 - radius, cy0 - radius, radius * 2.0, radius * 2.0), fg);
        self.clicked(rect(cx0 - radius, cy0 - radius, radius * 2.0, radius * 2.0))
    }

    /// Horizontal slider with an optional leading glyph inside the track
    /// (macOS Control Center style). Returns the new value while dragged.
    pub fn slider(&mut self, id: u32, r: D2D_RECT_F, value: f32, glyph: Option<char>) -> Option<f32> {
        let s = self.cx.s;
        let h = r.bottom - r.top;
        let v = value.clamp(0.0, 1.0);
        let track = self.ink(0.14);
        self.cx.gfx.fill_rrect(r, h / 2.0, track);
        let fill_w = h + (r.right - r.left - h) * v;
        let fill = rgba(1.0, 1.0, 1.0, 0.95 * self.a);
        self.cx.gfx.fill_rrect(rect(r.left, r.top, fill_w, h), h / 2.0, fill);
        if let Some(g) = glyph {
            let c = rgba(0.35, 0.35, 0.38, self.a);
            self.cx.gfx.glyph(g, h * 0.52, rect(r.left + 2.0 * s, r.top, h, h), c);
        }
        if let Some(p) = self.inp.press {
            if self.inp.active.is_none() && inside(&r, p) {
                self.inp.active = Some(id);
                self.inp.press = None;
            }
        }
        if self.inp.active == Some(id) {
            let t = ((self.inp.mouse.0 - r.left - h / 2.0) / (r.right - r.left - h)).clamp(0.0, 1.0);
            return Some(t);
        }
        None
    }

    /// Push button. `primary` uses the accent colour.
    pub fn button(&mut self, r: D2D_RECT_F, label: &str, primary: bool) -> bool {
        let s = self.cx.s;
        let hot = self.hover(r);
        let bg = if primary {
            self.fade(ACCENT)
        } else {
            self.ink(if hot { 0.16 } else { 0.10 })
        };
        self.cx.gfx.fill_rrect(r, 6.0 * s, bg);
        let fg = if primary { rgba(1.0, 1.0, 1.0, self.a) } else { self.ink(0.9) };
        self.cx.gfx.text(label, 13.0 * s, 500, r, 0, fg);
        self.clicked(r)
    }

    /// Segmented control. Returns the newly picked index.
    pub fn segmented(&mut self, r: D2D_RECT_F, options: &[&str], selected: usize) -> Option<usize> {
        let s = self.cx.s;
        let c = self.ink(0.10);
        self.cx.gfx.fill_rrect(r, 7.0 * s, c);
        let w = (r.right - r.left) / options.len() as f32;
        let mut picked = None;
        for (i, opt) in options.iter().enumerate() {
            let cell = rect(r.left + i as f32 * w, r.top, w, r.bottom - r.top);
            if i == selected {
                let inner = rect(cell.left + 2.0 * s, cell.top + 2.0 * s, w - 4.0 * s, cell.bottom - cell.top - 4.0 * s);
                let bg = if self.cx.dark { rgba(0.42, 0.42, 0.45, self.a) } else { rgba(1.0, 1.0, 1.0, self.a) };
                self.cx.gfx.fill_rrect(inner, 5.5 * s, bg);
            }
            let fg = self.ink(if i == selected { 0.95 } else { 0.7 });
            self.cx.gfx.text(opt, 12.5 * s, if i == selected { 600 } else { 400 }, cell, 0, fg);
            if i != selected && self.clicked(cell) {
                picked = Some(i);
            }
        }
        picked
    }
}
