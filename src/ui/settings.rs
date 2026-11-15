//! The Settings window (styled after macOS System Settings). Every change
//! is saved to config.toml and applied immediately.

use std::collections::HashMap;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::config::Section;
use crate::render::gfx::Surface;
use crate::ui::widgets::{Input, Ui, ACCENT};
use crate::ui::wallpaper::palette;
use crate::util::{from_wide, rect, rgba};
use crate::win::{autostart, icons};

pub const CLASS: PCWSTR = w!("WinMacDockSettings");
/// A file picked in a dialog; `lParam` is a `Box<(u32, String)>` (purpose, path).
pub const WM_APP_PICKED: u32 = WM_APP + 11;
const PICK_APP: u32 = 1;
const PICK_VIDEO: u32 = 2;

const W: f32 = 800.0;
const H: f32 = 540.0;
const SIDEBAR: f32 = 210.0;
const TITLE_H: f32 = 52.0;

const SECTIONS: [(char, &str); 6] = [
    ('\u{E713}', "General"),
    ('\u{E8A9}', "Dock"),
    ('\u{E700}', "Menu Bar"),
    ('\u{E721}', "Spotlight"),
    ('\u{E8A7}', "Window Buttons"),
    ('\u{E771}', "Wallpaper"),
];

const HOTKEYS: [(&str, &str); 4] = [("alt+space", "Alt+Space"), ("ctrl+space", "Ctrl+Space"), ("alt+shift+space", "Alt+Shift+Space"), ("ctrl+alt+space", "Ctrl+Alt+Space")];

/// What the Settings window needs to know about other parts of the app.
pub struct Extra {
    /// (exe, tooltip) of apps currently showing tray icons.
    pub tray_apps: Vec<(String, String)>,
    pub hotkey_ok: bool,
}

pub struct Settings {
    pub hwnd: HWND,
    surf: Option<Surface>,
    inp: Input,
    pub visible: bool,
    section: usize,
    dirty: bool,
    size: (i32, i32),
    scroll: f32,
    content_h: f32,
    icons: HashMap<String, Option<ID2D1Bitmap>>,
    names: HashMap<String, String>,
    /// A change made by dragging a slider, applied when the drag ends.
    pending: Option<Section>,
    active: bool,
}

/// Window procedure of the Settings window: removes the system frame
/// (keeping the shadow, rounded corners and snap) and makes the top strip
/// draggable, then hands everything else to the shared procedure.
pub unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => LRESULT(0),
        WM_NCHITTEST => {
            let s = GetDpiForWindow(hwnd) as f32 / 96.0;
            let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            let in_buttons = (pt.x as f32) < 80.0 * s && (pt.y as f32) < 40.0 * s;
            if (pt.y as f32) < TITLE_H * s && !in_buttons {
                LRESULT(HTCAPTION as isize)
            } else {
                LRESULT(HTCLIENT as isize)
            }
        }
        _ => app::wndproc(hwnd, msg, wp, lp),
    }
}

impl Settings {
    pub fn new(cx: &Ctx) -> Self {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
                CLASS,
                w!("WinMacDock Settings"),
                WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_THICKFRAME,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(cx.hinst),
                None,
            )
            .expect("settings window")
        };
        unsafe {
            // Mica behind the whole window; we leave the sidebar transparent.
            let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
            let backdrop = DWMSBT_MAINWINDOW;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as *const _, 4);
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        }
        Self {
            hwnd,
            surf: None,
            inp: Input::default(),
            visible: false,
            section: 0,
            dirty: false,
            size: (0, 0),
            scroll: 0.0,
            content_h: 0.0,
            icons: HashMap::new(),
            names: HashMap::new(),
            pending: None,
            active: false,
        }
    }

    pub fn show(&mut self, cx: &mut Ctx) {
        let (w, h) = ((W * cx.s) as i32, (H * cx.s) as i32);
        unsafe {
            let dark = BOOL(cx.dark as i32);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
            if !self.visible {
                let _ = SetWindowPos(self.hwnd, None, (cx.sw - w) / 2, (cx.sh - h) / 2, w, h, SWP_NOZORDER | SWP_FRAMECHANGED);
            }
            let _ = ShowWindow(self.hwnd, if IsIconic(self.hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(self.hwnd);
        }
        if self.surf.is_none() {
            self.surf = Surface::new(&cx.gfx, self.hwnd, w as u32, h as u32).ok();
            self.size = (w, h);
        }
        self.visible = true;
        self.active = true;
        self.dirty = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        // Free the surface and icons while closed.
        self.surf = None;
        self.icons.clear();
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        unsafe {
            let dark = BOOL(cx.dark as i32);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        }
        self.dirty = true;
    }

    pub fn refresh(&mut self) {
        self.dirty = self.visible;
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.hwnd {
            return None;
        }
        if self.inp.on_msg(hwnd, msg, wp, lp) {
            self.dirty = true;
            return Some(LRESULT(0));
        }
        match msg {
            WM_MOUSEACTIVATE => Some(LRESULT(MA_ACTIVATE as isize)),
            WM_ACTIVATE => {
                self.active = (wp.0 & 0xFFFF) as u32 != WA_INACTIVE;
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.hide();
                Some(LRESULT(0))
            }
            WM_SIZE => {
                let (w, h) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                if w > 0 && h > 0 {
                    if let Some(surf) = &mut self.surf {
                        surf.resize(&cx.gfx, w as u32, h as u32);
                    }
                    self.size = (w, h);
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN if wp.0 as u16 == windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 => {
                self.hide();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// A file chosen in a dialog came back.
    pub fn picked(&mut self, cx: &mut Ctx, purpose: u32, path: String) {
        match purpose {
            PICK_APP => {
                if !cx.cfg.dock.pinned.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
                    cx.cfg.dock.pinned.push(path);
                    cx.cmds.push(Cmd::ConfigChanged(Section::Dock));
                }
            }
            PICK_VIDEO => {
                cx.cfg.wallpaper.video = path;
                cx.cfg.wallpaper.mode = "video".into();
                cx.cmds.push(Cmd::ConfigChanged(Section::Wallpaper));
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub fn needs_frame(&self) -> bool {
        self.visible && (self.dirty || self.inp.dragging())
    }

    pub fn frame(&mut self, cx: &mut Ctx, extra: &Extra) {
        if !self.visible {
            return;
        }
        self.dirty = false;
        let Some(surf) = self.surf.take() else { return };
        let s = cx.s;
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        surf.begin(&cx.gfx);
        {
            let mut ui = Ui { cx, inp: &mut self.inp, a: 1.0 };
            // Content pane (the sidebar stays see-through to the Mica backdrop).
            let content_bg = if ui.cx.dark { rgba(0.12, 0.12, 0.13, 1.0) } else { rgba(0.97, 0.97, 0.97, 1.0) };
            ui.cx.gfx.fill_rect(rect(SIDEBAR * s, 0.0, w - SIDEBAR * s, h), content_bg);
            let line = ui.ink(0.1);
            ui.cx.gfx.line(SIDEBAR * s, 0.0, SIDEBAR * s, h, 1.0, line);

            // Window buttons: close and minimize.
            let lights = [(1.0, 0.37, 0.34), (1.0, 0.74, 0.18), (0.55, 0.55, 0.57)];
            let hover_lights = ui.hover(rect(0.0, 0.0, 80.0 * s, 40.0 * s));
            for (i, (r, g, b)) in lights.iter().enumerate() {
                let x = 20.0 * s + i as f32 * 20.0 * s;
                let c = if self.active || hover_lights { rgba(*r, *g, *b, if i == 2 { 0.5 } else { 1.0 }) } else { ui.ink(0.2) };
                ui.cx.gfx.fill_circle(x, 20.0 * s, 6.0 * s, c);
                if i < 2 && ui.clicked(rect(x - 9.0 * s, 11.0 * s, 18.0 * s, 18.0 * s)) {
                    if i == 0 {
                        ui.cx.cmds.push(Cmd::CloseSettings);
                    } else {
                        unsafe {
                            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
                        }
                    }
                }
            }

            // Sidebar.
            for (i, (g, name)) in SECTIONS.iter().enumerate() {
                let r = rect(10.0 * s, TITLE_H * s + i as f32 * 32.0 * s, (SIDEBAR - 20.0) * s, 30.0 * s);
                let sel = i == self.section;
                if sel {
                    ui.cx.gfx.fill_rrect(r, 6.0 * s, ACCENT);
                } else if ui.hover(r) {
                    let c = ui.ink(0.07);
                    ui.cx.gfx.fill_rrect(r, 6.0 * s, c);
                }
                let fg = if sel { rgba(1.0, 1.0, 1.0, 1.0) } else { ui.ink(0.88) };
                ui.glyph(*g, 13.0, rect(r.left + 6.0 * s, r.top, 24.0 * s, 30.0 * s), fg);
                ui.text(name, 13.0, if sel { 600 } else { 400 }, rect(r.left + 36.0 * s, r.top, r.right - r.left - 40.0 * s, 30.0 * s), -1, fg);
                if !sel && ui.clicked(r) {
                    self.section = i;
                    self.scroll = 0.0;
                }
            }

            // Section title.
            let x0 = SIDEBAR * s + 28.0 * s;
            let cw = w - x0 - 28.0 * s;
            ui.label(SECTIONS[self.section].1, 20.0, 700, rect(x0, 14.0 * s, cw, 32.0 * s), -1);

            // Scrollable body.
            let body = rect(SIDEBAR * s + 1.0, TITLE_H * s + 4.0 * s, w - SIDEBAR * s - 1.0, h - TITLE_H * s - 4.0 * s);
            let wheel = ui.inp.wheel;
            if wheel != 0.0 && ui.hover(body) {
                self.scroll = (self.scroll - wheel * 60.0 * s).clamp(0.0, (self.content_h - (body.bottom - body.top)).max(0.0));
            }
            ui.cx.gfx.push_clip(body);
            let top = body.top + 8.0 * s - self.scroll;
            let mut f = Form { x: x0, w: cw, y: top };
            let mut pending = None;
            match self.section {
                0 => general(&mut ui, &mut f),
                1 => dock(&mut ui, &mut f, &mut self.icons, &mut self.names, &mut pending),
                2 => menubar(&mut ui, &mut f, extra),
                3 => spotlight(&mut ui, &mut f, extra),
                4 => traffic(&mut ui, &mut f),
                _ => wallpaper(&mut ui, &mut f),
            }
            ui.cx.gfx.pop_clip();
            self.content_h = f.y + self.scroll - body.top + 20.0 * s;
            if pending.is_some() {
                self.pending = pending;
            }
            if !ui.inp.dragging() {
                if let Some(sec) = self.pending.take() {
                    ui.cx.cmds.push(Cmd::ConfigChanged(sec));
                }
            }
            ui.inp.end_frame();
        }
        surf.end(&cx.gfx);
        self.surf = Some(surf);
    }
}

// ------------------------------------------------------------------ layout

/// Running layout cursor for a section's rows.
struct Form {
    x: f32,
    w: f32,
    y: f32,
}

impl Form {
    /// Start a group of `rows` rows of height `row_h`; returns the group's rect.
    fn group(&mut self, ui: &mut Ui, rows: usize, row_h: f32) -> windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
        let s = ui.s();
        let r = rect(self.x, self.y, self.w, rows as f32 * row_h * s);
        let bg = if ui.cx.dark { rgba(1.0, 1.0, 1.0, 0.05) } else { rgba(1.0, 1.0, 1.0, 1.0) };
        ui.cx.gfx.fill_rrect(r, 10.0 * s, bg);
        let edge = ui.ink(0.08);
        ui.cx.gfx.stroke_rrect(r, 10.0 * s, 1.0, edge);
        for i in 1..rows {
            let y = r.top + i as f32 * row_h * s;
            ui.separator(r.left + 14.0 * s, r.right - 14.0 * s, y);
        }
        self.y = r.bottom + 18.0 * s;
        r
    }

    fn caption(&mut self, ui: &mut Ui, text: &str) {
        let s = ui.s();
        ui.dim_label(text, 12.0, rect(self.x + 4.0 * s, self.y, self.w, 22.0 * s), -1);
        self.y += 24.0 * s;
    }

    fn note(&mut self, ui: &mut Ui, text: &str) {
        let s = ui.s();
        ui.dim_label(text, 11.5, rect(self.x + 4.0 * s, self.y - 12.0 * s, self.w - 8.0 * s, 20.0 * s), -1);
        self.y += 12.0 * s;
    }
}

/// Label on the left of row `i` in group `g`; returns the row's vertical centre.
fn row_label(ui: &mut Ui, g: &windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F, i: usize, row_h: f32, label: &str) -> f32 {
    let s = ui.s();
    let top = g.top + i as f32 * row_h * s;
    ui.label(label, 13.0, 400, rect(g.left + 14.0 * s, top, (g.right - g.left) * 0.55, row_h * s), -1);
    top + row_h * s / 2.0
}

fn switch_row(ui: &mut Ui, g: &windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F, i: usize, label: &str, value: &mut bool) -> bool {
    let s = ui.s();
    let cy = row_label(ui, g, i, 44.0, label);
    if ui.switch(g.right - 52.0 * s, cy - 11.0 * s, *value) {
        *value = !*value;
        return true;
    }
    false
}

// ---------------------------------------------------------------- sections

fn general(ui: &mut Ui, f: &mut Form) {
    let s = ui.s();
    let g = f.group(ui, 2, 44.0);
    let mut on = autostart::enabled();
    if switch_row(ui, &g, 0, "Start WinMacDock when you sign in", &mut on) {
        autostart::set(on);
    }
    let mut fast = autostart::fast_startup();
    if switch_row(ui, &g, 1, "Skip Windows' startup delay after sign-in", &mut fast) {
        autostart::set_fast_startup(fast);
    }
    f.note(ui, "Without the delay, all your startup apps (WinMacDock included) launch right away.");

    f.caption(ui, "Appearance");
    let g = f.group(ui, 2, 44.0);
    row_label(ui, &g, 0, 44.0, "Light or dark");
    ui.dim_label("Follows Windows", 12.5, rect(g.left, g.top, g.right - g.left - 14.0 * s, 44.0 * s), 1);
    row_label(ui, &g, 1, 44.0, "Animations");
    ui.dim_label("Follow Windows “Animation effects”", 12.5, rect(g.left, g.top + 44.0 * s, g.right - g.left - 14.0 * s, 44.0 * s), 1);

    f.caption(ui, "Advanced");
    let g = f.group(ui, 2, 44.0);
    row_label(ui, &g, 0, 44.0, "Configuration file");
    if ui.button(rect(g.right - 124.0 * s, g.top + 8.0 * s, 110.0 * s, 28.0 * s), "Open File", false) {
        ui.cx.cmds.push(Cmd::OpenConfig);
    }
    row_label(ui, &g, 1, 44.0, "Restore all settings to their defaults");
    if ui.button(rect(g.right - 124.0 * s, g.top + 52.0 * s, 110.0 * s, 28.0 * s), "Reset", false) {
        let keep = ui.cx.cfg.dock.pinned.clone();
        ui.cx.cfg = crate::config::Config::default();
        ui.cx.cfg.dock.pinned = keep;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::All));
    }

    let r = rect(f.x, f.y, 160.0 * s, 30.0 * s);
    if ui.button(r, "Quit WinMacDock", false) {
        ui.cx.cmds.push(Cmd::QuitSelf);
    }
    ui.dim_label(concat!("Version ", env!("CARGO_PKG_VERSION")), 12.0, rect(f.x + 176.0 * s, f.y, 200.0 * s, 30.0 * s), -1);
    f.y += 40.0 * s;
}

fn dock(ui: &mut Ui, f: &mut Form, icons_cache: &mut HashMap<String, Option<ID2D1Bitmap>>, names: &mut HashMap<String, String>, pending: &mut Option<Section>) {
    let s = ui.s();
    let g = f.group(ui, 3, 48.0);
    let cy = row_label(ui, &g, 0, 48.0, "Size");
    let sr = rect(g.right - 234.0 * s, cy - 10.0 * s, 220.0 * s, 20.0 * s);
    let size_t = (ui.cx.cfg.dock.icon_size - 32.0) / 48.0;
    if let Some(v) = ui.slider(10, sr, size_t, None) {
        ui.cx.cfg.dock.icon_size = (32.0 + v * 48.0).round();
        *pending = Some(Section::Dock);
    }
    let cy = row_label(ui, &g, 1, 48.0, "Magnification");
    let sr = rect(g.right - 234.0 * s, cy - 10.0 * s, 220.0 * s, 20.0 * s);
    let mag_t = (ui.cx.cfg.dock.magnification - 1.0) / 1.5;
    if let Some(v) = ui.slider(11, sr, mag_t, None) {
        ui.cx.cfg.dock.magnification = ((1.0 + v * 1.5) * 20.0).round() / 20.0;
        *pending = Some(Section::Dock);
    }
    let cy = row_label(ui, &g, 2, 48.0, "Automatically hide and show the Dock");
    let mut hide = ui.cx.cfg.dock.autohide;
    if ui.switch(g.right - 52.0 * s, cy - 11.0 * s, hide) {
        hide = !hide;
        ui.cx.cfg.dock.autohide = hide;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Dock));
    }

    f.caption(ui, "Apps in the Dock");
    let pinned = ui.cx.cfg.dock.pinned.clone();
    let rows = pinned.len().max(1);
    let g = f.group(ui, rows, 44.0);
    if pinned.is_empty() {
        ui.dim_label("No apps kept in the Dock", 12.5, rect(g.left + 14.0 * s, g.top, g.right - g.left, 44.0 * s), -1);
    }
    let mut change: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 remove)
    for (i, p) in pinned.iter().enumerate() {
        let top = g.top + i as f32 * 44.0 * s;
        let icon = icons_cache.entry(p.clone()).or_insert_with(|| icons::load(&ui.cx.gfx, p, (28.0 * s) as i32));
        if let Some(bmp) = icon {
            ui.cx.gfx.bitmap(bmp, rect(g.left + 14.0 * s, top + 8.0 * s, 28.0 * s, 28.0 * s), 1.0);
        }
        let name = names.entry(p.clone()).or_insert_with(|| icons::display_name(p)).clone();
        ui.label(&name, 13.0, 400, rect(g.left + 52.0 * s, top, g.right - g.left - 170.0 * s, 44.0 * s), -1);
        let buttons = [('\u{E70E}', -1), ('\u{E70D}', 1), ('\u{E711}', 0)];
        for (k, (glyph, d)) in buttons.iter().enumerate() {
            let br = rect(g.right - 108.0 * s + k as f32 * 32.0 * s, top + 8.0 * s, 28.0 * s, 28.0 * s);
            let enabled = !((*d == -1 && i == 0) || (*d == 1 && i + 1 == pinned.len()));
            if enabled && ui.hover(br) {
                let c = ui.ink(0.1);
                ui.cx.gfx.fill_rrect(br, 6.0 * s, c);
            }
            let c = ui.ink(if enabled { 0.75 } else { 0.2 });
            ui.glyph(*glyph, 11.0, br, c);
            if enabled && ui.clicked(br) {
                change = Some((i, *d));
            }
        }
    }
    if let Some((i, d)) = change {
        let list = &mut ui.cx.cfg.dock.pinned;
        match d {
            0 => {
                list.remove(i);
            }
            -1 => list.swap(i, i - 1),
            _ => list.swap(i, i + 1),
        }
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Dock));
    }
    let r = rect(f.x, f.y - 6.0 * s, 120.0 * s, 30.0 * s);
    if ui.button(r, "Add App…", false) {
        pick_file(ui.cx.main, PICK_APP);
    }
    ui.dim_label("Tip: right-click an app in Launchpad or the Dock to add it.", 12.0, rect(f.x + 134.0 * s, f.y - 6.0 * s, f.w - 134.0 * s, 30.0 * s), -1);
    f.y += 40.0 * s;
}

fn menubar(ui: &mut Ui, f: &mut Form, extra: &Extra) {
    let s = ui.s();
    let g = f.group(ui, 4, 44.0);
    let mut v = ui.cx.cfg.menubar.enabled;
    if switch_row(ui, &g, 0, "Show the menu bar", &mut v) {
        ui.cx.cfg.menubar.enabled = v;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::MenuBar));
    }
    let mut v = ui.cx.cfg.menubar.clock_24h;
    if switch_row(ui, &g, 1, "Use a 24-hour clock", &mut v) {
        ui.cx.cfg.menubar.clock_24h = v;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::MenuBar));
    }
    let mut v = ui.cx.cfg.menubar.tray_icons;
    if switch_row(ui, &g, 2, "Show icons of other apps (system tray)", &mut v) {
        ui.cx.cfg.menubar.tray_icons = v;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::MenuBar));
    }
    let mut v = ui.cx.cfg.menubar.battery_percent;
    if switch_row(ui, &g, 3, "Show battery percentage", &mut v) {
        ui.cx.cfg.menubar.battery_percent = v;
        // Only a redraw is needed.
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::TrayFilter));
    }

    if ui.cx.cfg.menubar.tray_icons {
        f.caption(ui, "Show in the menu bar");
        let mut apps: Vec<(String, String)> = Vec::new();
        for (exe, tip) in &extra.tray_apps {
            if !exe.is_empty() && !apps.iter().any(|(e, _)| e == exe) {
                apps.push((exe.clone(), tip.clone()));
            }
        }
        if apps.is_empty() {
            let g = f.group(ui, 1, 44.0);
            ui.dim_label("No apps are showing tray icons right now.", 12.5, rect(g.left + 14.0 * s, g.top, g.right - g.left, 44.0 * s), -1);
            return;
        }
        let g = f.group(ui, apps.len(), 44.0);
        for (i, (exe, tip)) in apps.iter().enumerate() {
            let label = if tip.is_empty() { exe.clone() } else { format!("{} — {}", tip.lines().next().unwrap_or(""), exe) };
            let mut shown = !ui.cx.cfg.menubar.tray_hidden.iter().any(|x| x.eq_ignore_ascii_case(exe));
            if switch_row(ui, &g, i, &label, &mut shown) {
                let hidden = &mut ui.cx.cfg.menubar.tray_hidden;
                hidden.retain(|x| !x.eq_ignore_ascii_case(exe));
                if !shown {
                    hidden.push(exe.clone());
                }
                ui.cx.cmds.push(Cmd::ConfigChanged(Section::TrayFilter));
            }
        }
    }
}

fn spotlight(ui: &mut Ui, f: &mut Form, extra: &Extra) {
    let s = ui.s();
    let g = f.group(ui, 2, 52.0);
    let cy = row_label(ui, &g, 0, 52.0, "Search apps with a keyboard shortcut");
    let mut on = ui.cx.cfg.spotlight.enabled;
    if ui.switch(g.right - 52.0 * s, cy - 11.0 * s, on) {
        on = !on;
        ui.cx.cfg.spotlight.enabled = on;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Spotlight));
    }
    let cy = row_label(ui, &g, 1, 52.0, "Shortcut");
    let current = HOTKEYS.iter().position(|(k, _)| k.eq_ignore_ascii_case(&ui.cx.cfg.spotlight.hotkey)).unwrap_or(0);
    let labels: Vec<&str> = HOTKEYS.iter().map(|(_, l)| *l).collect();
    let seg = rect(g.left + 110.0 * s, cy - 14.0 * s, g.right - g.left - 124.0 * s, 28.0 * s);
    if let Some(i) = ui.segmented(seg, &labels, current) {
        ui.cx.cfg.spotlight.hotkey = HOTKEYS[i].0.into();
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Spotlight));
    }
    if ui.cx.cfg.spotlight.enabled {
        let (msg, color) = if extra.hotkey_ok {
            ("The shortcut is active.".to_string(), rgba(0.2, 0.7, 0.35, 1.0))
        } else {
            ("This shortcut is already used by another app. Pick another one.".to_string(), rgba(1.0, 0.35, 0.3, 1.0))
        };
        ui.text(&msg, 12.0, 400, rect(f.x + 4.0 * s, f.y - 12.0 * s, f.w, 20.0 * s), -1, color);
        f.y += 16.0 * s;
    }
    if ui.button(rect(f.x, f.y, 140.0 * s, 30.0 * s), "Open Spotlight", false) {
        ui.cx.cmds.push(Cmd::Spotlight);
    }
    f.y += 40.0 * s;
}

fn traffic(ui: &mut Ui, f: &mut Form) {
    let g = f.group(ui, 1, 44.0);
    let mut v = ui.cx.cfg.traffic_lights.enabled;
    if switch_row(ui, &g, 0, "Replace the window buttons with Mac-style buttons", &mut v) {
        ui.cx.cfg.traffic_lights.enabled = v;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Traffic));
    }
    f.note(ui, "Red closes, yellow minimizes into the Dock, green zooms. Apps that don't");
    f.note(ui, "report their buttons to Windows keep their own.");
}

thread_local! {
    /// Rendered wallpaper previews, made once per size.
    static PREVIEWS: std::cell::RefCell<std::collections::HashMap<(&'static str, u32, u32), Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A still frame of a built-in wallpaper (a moment that shows it off).
fn preview(cx: &crate::app::Ctx, key: &'static str, w: u32, h: u32) -> Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap> {
    PREVIEWS.with(|p| p.borrow_mut().entry((key, w, h)).or_insert_with(|| crate::ui::wallpaper::snapshot(cx, key, 40.0, w.max(1), h.max(1))).clone())
}

fn wallpaper(ui: &mut Ui, f: &mut Form) {
    let s = ui.s();
    let g = f.group(ui, 1, 52.0);
    let cy = row_label(ui, &g, 0, 52.0, "Live wallpaper");
    let mode = match ui.cx.cfg.wallpaper.mode.as_str() {
        "shader" => 1,
        "video" => 2,
        _ => 0,
    };
    if let Some(i) = ui.segmented(rect(g.right - 284.0 * s, cy - 14.0 * s, 270.0 * s, 28.0 * s), &["Off", "Animated", "Video"], mode) {
        if i == 2 && ui.cx.cfg.wallpaper.video.is_empty() {
            pick_file(ui.cx.main, PICK_VIDEO);
        } else {
            ui.cx.cfg.wallpaper.mode = ["off", "shader", "video"][i].into();
            ui.cx.cmds.push(Cmd::ConfigChanged(Section::Wallpaper));
        }
    }

    let g = f.group(ui, 1, 52.0);
    let cy = row_label(ui, &g, 0, 52.0, "Animation speed");
    const SPEEDS: [f32; 3] = [1.0, 5.0, 12.0];
    let cur = SPEEDS.iter().enumerate().min_by(|a, b| (a.1 - ui.cx.cfg.wallpaper.speed).abs().total_cmp(&(b.1 - ui.cx.cfg.wallpaper.speed).abs())).map_or(1, |p| p.0);
    if let Some(i) = ui.segmented(rect(g.right - 284.0 * s, cy - 14.0 * s, 270.0 * s, 28.0 * s), &["Calm", "Normal", "Lively"], cur) {
        ui.cx.cfg.wallpaper.speed = SPEEDS[i];
        // Takes effect on the next tick; no need to restart the wallpaper.
        ui.cx.cfg.save();
    }

    // Animated wallpapers, grouped (Abstract, Nature, Space), with real previews.
    let tw = (f.w - 2.0 * 14.0 * s) / 3.0;
    let th = tw * 0.6;
    let mut group = "";
    let mut col = 0;
    for &(key, label, grp) in crate::ui::wallpaper::SCENES {
        if grp != group {
            if !group.is_empty() {
                f.y += th + 40.0 * s;
            }
            group = grp;
            col = 0;
            f.caption(ui, grp);
        }
        let r = rect(f.x + col as f32 * (tw + 14.0 * s), f.y, tw, th);
        col += 1;
        match preview(ui.cx, key, tw.round() as u32, th.round() as u32) {
            Some(bmp) => {
                // Rounded corners: clip with a rounded mask by drawing the border over it.
                ui.cx.gfx.bitmap(&bmp, r, 1.0);
                ui.cx.gfx.stroke_rrect(r, 10.0 * s, 1.0, rgba(1.0, 1.0, 1.0, 0.12));
            }
            None => {
                let colors: Vec<_> = palette(key).iter().map(|c| rgba(c[0], c[1], c[2], 1.0)).collect();
                ui.cx.gfx.gradient_rrect(r, 10.0 * s, &[colors[0], colors[1], colors[2], colors[3]]);
            }
        }
        let selected = ui.cx.cfg.wallpaper.mode == "shader" && ui.cx.cfg.wallpaper.shader == key;
        if selected {
            ui.cx.gfx.stroke_rrect(r, 10.0 * s, 3.0 * s, ACCENT);
        }
        ui.label(label, 12.5, if selected { 600 } else { 400 }, rect(r.left, r.bottom + 4.0 * s, tw, 22.0 * s), 0);
        if ui.clicked(r) {
            ui.cx.cfg.wallpaper.shader = key.into();
            ui.cx.cfg.wallpaper.mode = "shader".into();
            ui.cx.cmds.push(Cmd::ConfigChanged(Section::Wallpaper));
        }
    }
    f.y += th + 40.0 * s;

    f.caption(ui, "Video wallpaper");
    let g = f.group(ui, 1, 52.0);
    let path = ui.cx.cfg.wallpaper.video.clone();
    let shown = if path.is_empty() { "No video chosen".to_string() } else { path.rsplit('\\').next().unwrap_or(&path).to_string() };
    ui.label(&shown, 13.0, 400, rect(g.left + 14.0 * s, g.top, g.right - g.left - 170.0 * s, 52.0 * s), -1);
    if ui.button(rect(g.right - 144.0 * s, g.top + 12.0 * s, 130.0 * s, 28.0 * s), "Choose Video…", false) {
        pick_file(ui.cx.main, PICK_VIDEO);
    }

    let g = f.group(ui, 1, 44.0);
    let mut v = ui.cx.cfg.wallpaper.pause_on_battery;
    if switch_row(ui, &g, 0, "Pause while on battery power", &mut v) {
        ui.cx.cfg.wallpaper.pause_on_battery = v;
        ui.cx.cmds.push(Cmd::ConfigChanged(Section::Wallpaper));
    }
    f.note(ui, "The wallpaper also pauses whenever a window covers the whole screen.");
}

// ----------------------------------------------------------------- dialogs

/// Show a file-open dialog on its own thread so the Dock keeps animating.
fn pick_file(main: HWND, purpose: u32) {
    let main = main.0 as isize;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = (|| -> windows::core::Result<String> {
            let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            let (title, filters): (PCWSTR, Vec<(PCWSTR, PCWSTR)>) = if purpose == PICK_APP {
                (w!("Choose an app to keep in the Dock"), vec![(w!("Apps and shortcuts"), w!("*.exe;*.lnk"))])
            } else {
                (w!("Choose a video for the wallpaper"), vec![(w!("Videos"), w!("*.mp4;*.mov;*.m4v;*.wmv;*.mkv"))])
            };
            let specs: Vec<COMDLG_FILTERSPEC> = filters.iter().map(|(n, p)| COMDLG_FILTERSPEC { pszName: *n, pszSpec: *p }).collect();
            dlg.SetFileTypes(&specs)?;
            dlg.SetTitle(title)?;
            // Shortcuts should come back as the .lnk itself, not their target.
            let opts = dlg.GetOptions()?;
            dlg.SetOptions(opts | FOS_NODEREFERENCELINKS | FOS_FILEMUSTEXIST)?;
            dlg.Show(None)?;
            let item = dlg.GetResult()?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = from_wide(p.as_wide());
            CoTaskMemFree(Some(p.0 as *const _));
            Ok(path)
        })();
        if let Ok(path) = result {
            let b = Box::into_raw(Box::new((purpose, path)));
            if PostMessageW(Some(HWND(main as *mut _)), WM_APP_PICKED, WPARAM(0), LPARAM(b as isize)).is_err() {
                drop(Box::from_raw(b));
            }
        }
        CoUninitialize();
    });
}
