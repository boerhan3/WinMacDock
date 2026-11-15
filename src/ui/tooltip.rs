//! Small frosted tooltip (tray icons in the menu bar).

use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Ctx};
use crate::render::gfx::Surface;
use crate::util::{rect, rgba};
use crate::win::glass::Glass;

pub struct Tooltip {
    glass: Glass,
    hwnd: HWND,
    surf: Option<Surface>,
    pub visible: bool,
}

fn tint(dark: bool) -> u32 {
    if dark { 0xC0_26_26_28 } else { 0xD0_F4_F4_F4 }
}

impl Tooltip {
    pub fn new(cx: &Ctx) -> Self {
        let glass = Glass::new(tint(cx.dark));
        // Click-through: a tooltip never gets in the way of the pointer.
        let hwnd = app::create_window(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            Some(glass.hwnd),
        );
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        }
        Self { glass, hwnd, surf: None, visible: false }
    }

    /// Show `text` (may span lines) centred under `x`, top at `y` (screen pixels).
    pub fn show(&mut self, cx: &mut Ctx, text: &str, x: i32, y: i32) {
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).take(6).collect();
        if lines.is_empty() {
            self.hide();
            return;
        }
        let s = cx.s;
        let (size, line_h, pad) = (12.5 * s, 18.0 * s, 8.0 * s);
        let tw = lines.iter().map(|l| cx.gfx.text_width(l, size, 400)).fold(0.0, f32::max).min(420.0 * s);
        let (w, h) = ((tw + pad * 2.0 + 2.0).ceil() as i32, (lines.len() as f32 * line_h + pad * 1.5).ceil() as i32);
        let m = crate::win::monitors::at(x, y);
        let x = (x - w / 2).clamp(m.left + 6, (m.right - w - 6).max(m.left + 6));

        self.glass.set_tint(tint(cx.dark));
        self.glass.set_rect(x, y, w, h, (7.0 * s) as i32);
        self.glass.raise();
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        }
        match &mut self.surf {
            Some(surf) => surf.resize(&cx.gfx, w as u32, h as u32),
            None => self.surf = Surface::new(&cx.gfx, self.hwnd, w as u32, h as u32).ok(),
        }
        if let Some(surf) = &self.surf {
            let fg = if cx.dark { rgba(1.0, 1.0, 1.0, 0.92) } else { rgba(0.0, 0.0, 0.0, 0.88) };
            surf.begin(&cx.gfx);
            cx.gfx.stroke_rrect(rect(0.5, 0.5, w as f32 - 1.0, h as f32 - 1.0), 7.0 * s, 1.0, rgba(0.5, 0.5, 0.5, 0.35));
            for (i, l) in lines.iter().enumerate() {
                cx.gfx.text(l, size, 400, rect(pad, pad * 0.75 + i as f32 * line_h, w as f32 - pad * 2.0, line_h), -1, fg);
            }
            surf.end(&cx.gfx);
        }
        self.glass.show(true);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}
