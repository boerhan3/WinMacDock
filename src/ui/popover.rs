//! Menu-bar panels drawn by us instead of opening Windows' own flyouts:
//! Control Center, Wi-Fi, Bluetooth, Sound, Battery and the calendar.

use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_BACK, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::{lerp, Spring};
use crate::render::gfx::Surface;
use crate::ui::widgets::{Input, Ui, ACCENT};
use crate::util::{rect, rgba};
use crate::win::glass::Glass;
use crate::win::outside;
use crate::win::status::{self, Status};
use crate::win::sysctl::{AudioDevice, Job, MediaCmd, MediaInfo, RadioSel, SysEvent};
use crate::win::bluetooth::{BtDevice, BtOp};
use crate::win::wifi::{WifiInfo, WifiNet};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Control,
    Wifi,
    Bluetooth,
    Sound,
    Battery,
    Calendar,
}

/// Live system state the panels show; updated from background results.
#[derive(Default)]
pub struct SysState {
    pub brightness: Option<f32>,
    pub wifi_radio: Option<bool>,
    pub bt_radio: Option<bool>,
    pub media: Option<MediaInfo>,
    pub wifi: WifiInfo,
    pub devices: Vec<AudioDevice>,
    /// Network being joined, and the last join error.
    pub joining: Option<String>,
    pub wifi_error: String,
    /// Paired Bluetooth devices (`None` until the first list arrives).
    pub bt: Option<Vec<BtDevice>>,
    /// Device being connected / disconnected, and the last error.
    pub bt_busy: Option<(String, BtOp)>,
    pub bt_error: String,
}

impl SysState {
    /// Apply a background result. Returns true when the default audio device changed.
    pub fn apply(&mut self, ev: SysEvent) -> bool {
        match ev {
            SysEvent::Brightness(b) => self.brightness = b,
            SysEvent::Radios { wifi, bluetooth } => {
                self.wifi_radio = wifi;
                self.bt_radio = bluetooth;
            }
            SysEvent::Media(m) => self.media = m,
            SysEvent::Wifi(w) => {
                if self.joining.as_ref().is_some_and(|j| w.current().is_some_and(|c| &c.ssid == j)) {
                    self.joining = None;
                }
                self.wifi = w;
            }
            SysEvent::WifiJoin(err) => {
                if !err.is_empty() {
                    self.joining = None;
                }
                self.wifi_error = err;
            }
            SysEvent::Bluetooth(d) => {
                // Done as soon as the device reaches the state we asked for.
                if let Some((id, op)) = &self.bt_busy {
                    let want = match op {
                        BtOp::Connect => Some(true),
                        BtOp::Disconnect => Some(false),
                        BtOp::Reconnect => None,
                    };
                    if want.is_some() && d.iter().find(|x| &x.id == id).map(|x| x.connected) == want {
                        self.bt_busy = None;
                    }
                }
                self.bt = Some(d);
            }
            SysEvent::BtDone(err) => {
                if !err.is_empty() {
                    self.bt_busy = None;
                }
                self.bt_error = err;
            }
            SysEvent::BtSettled => self.bt_busy = None,
            SysEvent::AudioDevices(d) => self.devices = d,
            SysEvent::AudioDefaultChanged => return true,
        }
        false
    }
}

pub struct Popover {
    glass: Glass,
    pub hwnd: HWND,
    surf: Option<Surface>,
    pub kind: Option<Kind>,
    inp: Input,
    open: Spring,
    pub visible: bool,
    closing: bool,
    dirty: bool,
    size: (i32, i32),
    anchor_right: i32,
    top: i32,
    /// Password being typed for this SSID.
    password: Option<(WifiNet, String)>,
    month_offset: i32,
    last_closed: (Option<Kind>, Instant),
}

fn tint(dark: bool) -> u32 {
    if dark { 0xA8_24_24_26 } else { 0xC0_EE_EE_EE }
}

impl Popover {
    pub fn new(cx: &Ctx) -> Self {
        let glass = Glass::new(tint(cx.dark));
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, Some(glass.hwnd));
        Self {
            glass,
            hwnd,
            surf: None,
            kind: None,
            inp: Input::default(),
            open: Spring::new(0.0, 520.0, 40.0),
            visible: false,
            closing: false,
            dirty: false,
            size: (0, 0),
            anchor_right: 0,
            top: 0,
            password: None,
            month_offset: 0,
            last_closed: (None, Instant::now()),
        }
    }

    pub fn just_closed(&self, kind: Kind) -> bool {
        self.last_closed.0 == Some(kind) && self.last_closed.1.elapsed().as_millis() < 250
    }

    /// Open `kind` with its right edge at `right` and its top at `top` (screen pixels).
    pub fn show(&mut self, cx: &mut Ctx, kind: Kind, right: i32, top: i32) {
        match kind {
            Kind::Control => {
                for job in [Job::Brightness, Job::Radios, Job::Media, Job::Wifi { scan: false }] {
                    cx.sys.send(job);
                }
            }
            Kind::Wifi => {
                cx.sys.send(Job::Radios);
                cx.sys.send(Job::Wifi { scan: true });
            }
            Kind::Bluetooth => {
                cx.sys.send(Job::Radios);
                cx.sys.send(Job::Bluetooth);
            }
            Kind::Sound => cx.sys.send(Job::AudioDevices),
            Kind::Battery | Kind::Calendar => {}
        }
        self.kind = Some(kind);
        self.anchor_right = right;
        self.top = top;
        self.password = None;
        self.month_offset = 0;
        self.inp.reset();
        self.size = (0, 0);
        self.visible = true;
        self.closing = false;
        self.glass.set_tint(tint(cx.dark));
        self.open.snap(if cx.animate { 0.0 } else { 1.0 });
        self.open.set(1.0);
        self.dirty = true;
    }

    pub fn close(&mut self, cx: &Ctx) {
        if !self.visible || self.closing {
            return;
        }
        outside::unwatch(outside::SLOT_POPOVER);
        self.last_closed = (self.kind, Instant::now());
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
        self.kind = None;
        self.password = None;
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.surf = None;
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        self.glass.set_tint(tint(cx.dark));
        self.dirty = true;
    }

    pub fn refresh(&mut self) {
        if self.visible {
            self.dirty = true;
        }
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
            WM_KEYDOWN => {
                let k = wp.0 as u16;
                if k == VK_ESCAPE.0 {
                    if self.password.take().is_none() {
                        self.close(cx);
                    }
                } else if k == VK_RETURN.0 {
                    self.join_with_password(cx);
                } else if k == VK_BACK.0 {
                    if let Some((_, pw)) = &mut self.password {
                        pw.pop();
                    }
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_CHAR => {
                if let (Some((_, pw)), Some(c)) = (&mut self.password, char::from_u32(wp.0 as u32)) {
                    if !c.is_control() && pw.len() < 63 {
                        pw.push(c);
                        self.dirty = true;
                    }
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn join_with_password(&mut self, cx: &mut Ctx) {
        if let Some((net, pw)) = self.password.take() {
            if pw.len() >= 8 {
                cx.sys.send(Job::WifiConnect(net.clone(), Some(pw)));
            } else {
                self.password = Some((net, pw));
            }
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.visible && (self.dirty || self.open.active() || self.inp.dragging())
    }

    fn width(kind: Kind) -> f32 {
        match kind {
            Kind::Control => 320.0,
            Kind::Wifi | Kind::Bluetooth | Kind::Sound => 300.0,
            Kind::Battery => 260.0,
            Kind::Calendar => 280.0,
        }
    }

    pub fn frame(&mut self, cx: &mut Ctx, st: &mut Status, sys: &mut SysState, dt: f32) {
        if !self.visible {
            return;
        }
        self.dirty = false;
        self.open.tick(dt);
        if self.closing && self.open.v < 0.02 {
            self.hide();
            return;
        }
        let Some(kind) = self.kind else { return };
        let s = cx.s;
        let w = (Self::width(kind) * s).round() as i32;
        let h = (self.height(cx, kind, sys) * s).round() as i32;

        if (w, h) != self.size {
            self.size = (w, h);
            let m = crate::win::monitors::at(self.anchor_right - 1, self.top);
            let x = (self.anchor_right - w).clamp(m.left + 6, (m.right - w - 6).max(m.left + 6));
            self.glass.set_rect(x, self.top, w, h, (14.0 * s) as i32);
            self.glass.raise();
            unsafe {
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, self.top, w, h, SWP_NOACTIVATE);
            }
            match &mut self.surf {
                Some(surf) => surf.resize(&cx.gfx, w as u32, h as u32),
                None => self.surf = Surface::new(&cx.gfx, self.hwnd, w as u32, h as u32).ok(),
            }
            outside::watch(outside::SLOT_POPOVER, RECT { left: x, top: self.top, right: x + w, bottom: self.top + h }, cx.hinst);
            self.glass.show(true);
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        let t = self.open.v.clamp(0.0, 1.0);
        self.glass.set_opacity(t);
        let Some(surf) = self.surf.take() else { return };
        surf.begin(&cx.gfx);
        cx.gfx.set_scale(lerp(0.97, 1.0, t), w as f32, 0.0);
        let mut close = false;
        let mut switch_to = None;
        {
            let mut ui = Ui { cx, inp: &mut self.inp, a: t };
            let edge = ui.ink(0.12);
            ui.cx.gfx.stroke_rrect(rect(0.5, 0.5, w as f32 - 1.0, h as f32 - 1.0), 14.0 * s, 1.0, edge);
            match kind {
                Kind::Control => draw_control(&mut ui, st, sys, w as f32, &mut close, &mut switch_to),
                Kind::Wifi => draw_wifi(&mut ui, sys, w as f32, &mut self.password, &mut close),
                Kind::Bluetooth => draw_bluetooth(&mut ui, sys, w as f32, &mut close),
                Kind::Sound => draw_sound(&mut ui, st, sys, w as f32, &mut close),
                Kind::Battery => draw_battery(&mut ui, st, w as f32, &mut close),
                Kind::Calendar => draw_calendar(&mut ui, w as f32, &mut self.month_offset, &mut close),
            }
            ui.inp.end_frame();
        }
        cx.gfx.reset_transform();
        surf.end(&cx.gfx);
        self.surf = Some(surf);

        if self.password.is_some() {
            // Typing needs keyboard focus; a click already gave us the right to take it.
            unsafe {
                if GetForegroundWindow() != self.hwnd {
                    let _ = SetForegroundWindow(self.hwnd);
                }
            }
        }
        if let Some(k) = switch_to {
            let (right, top) = (self.anchor_right, self.top);
            self.show(cx, k, right, top);
        } else if close {
            self.close(cx);
        }
    }

    /// Content height in logical pixels.
    fn height(&self, _cx: &Ctx, kind: Kind, sys: &SysState) -> f32 {
        match kind {
            Kind::Control => {
                let mut h = 12.0 + 150.0 + 10.0 + 64.0 + 12.0;
                if sys.brightness.is_some() {
                    h += 74.0;
                }
                if sys.media.is_some() {
                    h += 74.0;
                }
                h
            }
            Kind::Wifi => {
                let mut h = 12.0 + 32.0 + 10.0;
                if sys.wifi_radio == Some(false) || !sys.wifi.adapter {
                    h += 32.0;
                } else {
                    let others = sys.wifi.networks.iter().filter(|n| !n.connected).take(8).count();
                    if sys.wifi.current().is_some() {
                        h += 34.0 + 10.0;
                    }
                    h += 24.0 + others as f32 * 32.0;
                    if self.password.is_some() {
                        h += 44.0;
                    }
                    if !sys.wifi_error.is_empty() || sys.joining.is_some() {
                        h += 24.0;
                    }
                }
                h + 10.0 + 30.0 + 8.0
            }
            Kind::Bluetooth => {
                let mut h = 12.0 + 32.0 + 10.0;
                match &sys.bt {
                    Some(d) if sys.bt_radio != Some(false) && !d.is_empty() => h += 24.0 + d.len().min(BT_MAX) as f32 * 40.0,
                    _ => h += 32.0,
                }
                if !sys.bt_error.is_empty() {
                    h += 24.0;
                }
                h + 10.0 + 30.0 + 8.0
            }
            Kind::Sound => 12.0 + 32.0 + 40.0 + 24.0 + sys.devices.len().max(1) as f32 * 30.0 + 10.0 + 30.0 + 8.0,
            Kind::Battery => 12.0 + 32.0 + 24.0 + 24.0 + 10.0 + 30.0 + 8.0,
            Kind::Calendar => 12.0 + 34.0 + 22.0 + 6.0 * 32.0 + 10.0 + 30.0 + 30.0 + 8.0,
        }
    }
}

// ------------------------------------------------------------------ helpers

fn footer(ui: &mut Ui, w: f32, y: f32, label: &str) -> bool {
    let s = ui.s();
    ui.separator(12.0 * s, w - 12.0 * s, y + 5.0 * s);
    let r = rect(6.0 * s, y + 10.0 * s, w - 12.0 * s, 28.0 * s);
    let hit = ui.row(r);
    ui.label(label, 13.0, 400, rect(r.left + 8.0 * s, r.top, r.right - r.left - 16.0 * s, r.bottom - r.top), -1);
    hit
}

fn header(ui: &mut Ui, w: f32, title: &str) {
    let s = ui.s();
    ui.label(title, 14.0, 700, rect(16.0 * s, 12.0 * s, w - 90.0 * s, 32.0 * s), -1);
}

fn volume_glyph(st: &Status) -> char {
    if st.muted {
        '\u{E74F}'
    } else if st.volume < 0.01 {
        '\u{E992}'
    } else if st.volume < 0.34 {
        '\u{E993}'
    } else if st.volume < 0.67 {
        '\u{E994}'
    } else {
        '\u{E995}'
    }
}

fn wifi_glyph(signal: u32) -> char {
    match signal {
        0..=30 => '\u{E872}',
        31..=55 => '\u{E873}',
        56..=80 => '\u{E874}',
        _ => '\u{E701}',
    }
}

// ----------------------------------------------------------- Control Center

fn draw_control(ui: &mut Ui, st: &mut Status, sys: &mut SysState, w: f32, close: &mut bool, switch_to: &mut Option<Kind>) {
    let s = ui.s();
    let pad = 12.0 * s;
    let gap = 10.0 * s;
    let col = (w - pad * 2.0 - gap) / 2.0;

    // Connectivity tile.
    let tile = rect(pad, pad, col, 150.0 * s);
    ui.panel(tile, 14.0);
    let current = sys.wifi.current().map(|n| n.ssid.clone());
    let rows: [(char, &str, Option<bool>, String); 3] = [
        ('\u{E701}', "Wi-Fi", sys.wifi_radio, match (sys.wifi_radio, &current) {
            (Some(false), _) => "Off".into(),
            (_, Some(ssid)) => ssid.clone(),
            (Some(true), None) => "Not Connected".into(),
            (None, _) => "Not available".into(),
        }),
        ('\u{E702}', "Bluetooth", sys.bt_radio, match sys.bt_radio {
            Some(true) => "On".into(),
            Some(false) => "Off".into(),
            None => "Not available".into(),
        }),
        ('\u{E708}', "Dark Mode", Some(ui.cx.dark), if ui.cx.dark { "On".into() } else { "Off".into() }),
    ];
    for (i, (glyph, title, on, sub)) in rows.iter().enumerate() {
        let y = tile.top + 8.0 * s + i as f32 * 46.0 * s;
        let cy = y + 21.0 * s;
        if ui.circle_toggle(tile.left + 26.0 * s, cy, 15.0 * s, *glyph, on.unwrap_or(false)) {
            match i {
                0 => {
                    if let Some(v) = on {
                        ui.cx.sys.send(Job::SetRadio(RadioSel::Wifi, !v));
                        sys.wifi_radio = Some(!v);
                    }
                }
                1 => {
                    if let Some(v) = on {
                        ui.cx.sys.send(Job::SetRadio(RadioSel::Bluetooth, !v));
                        sys.bt_radio = Some(!v);
                    }
                }
                _ => ui.cx.sys.send(Job::DarkMode(!ui.cx.dark)),
            }
        }
        let text_r = rect(tile.left + 48.0 * s, y + 3.0 * s, col - 54.0 * s, 20.0 * s);
        ui.label(title, 13.0, 600, text_r, -1);
        ui.dim_label(sub, 11.5, rect(text_r.left, y + 21.0 * s, text_r.right - text_r.left, 18.0 * s), -1);
        // The Wi-Fi row title opens the network list, like the chevron on macOS.
        if i == 0 && ui.clicked(rect(text_r.left, y, text_r.right - text_r.left, 42.0 * s)) {
            *switch_to = Some(Kind::Wifi);
        }
        if i == 1 && ui.clicked(rect(text_r.left, y, text_r.right - text_r.left, 42.0 * s)) {
            *switch_to = Some(Kind::Bluetooth);
        }
    }

    // Two action tiles on the right.
    let x2 = pad + col + gap;
    let actions: [(char, &str); 2] = [('\u{E72E}', "Lock Screen"), ('\u{E722}', "Screenshot")];
    for (i, (glyph, title)) in actions.iter().enumerate() {
        let r = rect(x2, pad + i as f32 * 80.0 * s, col, 70.0 * s);
        ui.panel(r, 14.0);
        let hot = ui.hover(r);
        let ring = ui.ink(if hot { 0.2 } else { 0.14 });
        ui.cx.gfx.fill_circle(r.left + 26.0 * s, r.top + 35.0 * s, 15.0 * s, ring);
        let c = ui.ink(0.85);
        ui.glyph(*glyph, 13.5, rect(r.left + 11.0 * s, r.top + 20.0 * s, 30.0 * s, 30.0 * s), c);
        ui.label(title, 13.0, 600, rect(r.left + 48.0 * s, r.top, col - 52.0 * s, 70.0 * s), -1);
        if ui.clicked(r) {
            match i {
                0 => ui.cx.cmds.push(Cmd::Lock),
                _ => status::send_keys(&[windows::Win32::UI::Input::KeyboardAndMouse::VK_LWIN, windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT, windows::Win32::UI::Input::KeyboardAndMouse::VK_S]),
            }
            *close = true;
        }
    }

    let mut y = pad + 150.0 * s + gap;
    if let Some(b) = sys.brightness {
        let r = rect(pad, y, w - pad * 2.0, 64.0 * s);
        ui.panel(r, 14.0);
        ui.label("Display", 13.0, 600, rect(r.left + 12.0 * s, r.top + 6.0 * s, 200.0 * s, 20.0 * s), -1);
        if let Some(v) = ui.slider(1, rect(r.left + 12.0 * s, r.top + 32.0 * s, r.right - r.left - 24.0 * s, 22.0 * s), b, Some('\u{E706}')) {
            sys.brightness = Some(v);
            ui.cx.sys.send(Job::SetBrightness(v));
        }
        y += 74.0 * s;
    }

    let r = rect(pad, y, w - pad * 2.0, 64.0 * s);
    ui.panel(r, 14.0);
    ui.label("Sound", 13.0, 600, rect(r.left + 12.0 * s, r.top + 6.0 * s, 200.0 * s, 20.0 * s), -1);
    if ui.clicked(rect(r.left, r.top, r.right - r.left, 28.0 * s)) {
        *switch_to = Some(Kind::Sound);
    }
    let g = volume_glyph(st);
    if let Some(v) = ui.slider(2, rect(r.left + 12.0 * s, r.top + 32.0 * s, r.right - r.left - 24.0 * s, 22.0 * s), st.volume, Some(g)) {
        st.set_volume(v);
    }
    y += 74.0 * s;

    if let Some(m) = sys.media.clone() {
        let r = rect(pad, y, w - pad * 2.0, 64.0 * s);
        ui.panel(r, 14.0);
        let text_w = r.right - r.left - 130.0 * s;
        ui.label(&m.title, 13.0, 600, rect(r.left + 14.0 * s, r.top + 12.0 * s, text_w, 20.0 * s), -1);
        ui.dim_label(&m.artist, 12.0, rect(r.left + 14.0 * s, r.top + 32.0 * s, text_w, 18.0 * s), -1);
        let buttons = [('\u{E892}', MediaCmd::Previous), (if m.playing { '\u{E769}' } else { '\u{E768}' }, MediaCmd::PlayPause), ('\u{E893}', MediaCmd::Next)];
        for (i, (g, cmd)) in buttons.iter().enumerate() {
            let br = rect(r.right - 120.0 * s + i as f32 * 36.0 * s, r.top + 14.0 * s, 34.0 * s, 36.0 * s);
            if ui.hover(br) {
                let c = ui.ink(0.1);
                ui.cx.gfx.fill_rrect(br, 8.0 * s, c);
            }
            let c = ui.ink(0.9);
            ui.glyph(*g, 15.0, br, c);
            if ui.clicked(br) {
                ui.cx.sys.send(Job::Media2(*cmd));
                if let (MediaCmd::PlayPause, Some(media)) = (cmd, &mut sys.media) {
                    media.playing = !media.playing;
                }
            }
        }
    }
}

// --------------------------------------------------------------------- Wi-Fi

fn draw_wifi(ui: &mut Ui, sys: &mut SysState, w: f32, password: &mut Option<(WifiNet, String)>, close: &mut bool) {
    let s = ui.s();
    header(ui, w, "Wi-Fi");
    let mut y = 54.0 * s;
    if let Some(on) = sys.wifi_radio {
        if ui.switch(w - 54.0 * s, 17.0 * s, on) {
            ui.cx.sys.send(Job::SetRadio(RadioSel::Wifi, !on));
            sys.wifi_radio = Some(!on);
            if !on {
                ui.cx.sys.send(Job::Wifi { scan: true });
            }
        }
    }
    ui.separator(12.0 * s, w - 12.0 * s, 50.0 * s);

    if sys.wifi_radio == Some(false) || !sys.wifi.adapter {
        let msg = if sys.wifi.adapter || sys.wifi_radio.is_some() { "Wi-Fi is turned off." } else { "No Wi-Fi adapter found." };
        ui.dim_label(msg, 12.5, rect(16.0 * s, y, w - 32.0 * s, 32.0 * s), -1);
        y += 32.0 * s;
    } else {
        let row = |ui: &mut Ui, net: &WifiNet, y: f32, current: bool| -> bool {
            let r = rect(6.0 * s, y, w - 12.0 * s, if current { 34.0 } else { 30.0 } * s);
            let hit = ui.row(r);
            let rad = 12.0 * s;
            let bg = if current { ui.fade(ACCENT) } else { ui.ink(0.12) };
            ui.cx.gfx.fill_circle(r.left + 22.0 * s, r.top + (r.bottom - r.top) / 2.0, rad, bg);
            let fg = if current { rgba(1.0, 1.0, 1.0, ui.a) } else { ui.ink(0.85) };
            ui.glyph(wifi_glyph(net.signal), 12.0, rect(r.left + 10.0 * s, r.top, 24.0 * s, r.bottom - r.top), fg);
            ui.label(&net.ssid, 13.0, if current { 600 } else { 400 }, rect(r.left + 44.0 * s, r.top, w - 110.0 * s, r.bottom - r.top), -1);
            if net.secured {
                let c = ui.ink(0.45);
                ui.glyph('\u{E72E}', 11.0, rect(r.right - 28.0 * s, r.top, 20.0 * s, r.bottom - r.top), c);
            }
            hit
        };
        if let Some(cur) = sys.wifi.current().cloned() {
            row(ui, &cur, y, true);
            y += 34.0 * s + 10.0 * s;
        }
        ui.dim_label("Other Networks", 11.5, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1);
        y += 24.0 * s;
        let others: Vec<WifiNet> = sys.wifi.networks.iter().filter(|n| !n.connected).take(8).cloned().collect();
        for net in others {
            if row(ui, &net, y, false) {
                sys.wifi_error.clear();
                if !net.joinable() {
                    ui.cx.cmds.push(Cmd::Launch("ms-settings:network-wifi".into()));
                    *close = true;
                } else if net.needs_password() {
                    *password = Some((net.clone(), String::new()));
                } else {
                    sys.joining = Some(net.ssid.clone());
                    ui.cx.sys.send(Job::WifiConnect(net.clone(), None));
                }
            }
            y += 30.0 * s;
            if password.as_ref().is_some_and(|(p, _)| p.ssid == net.ssid) {
                let (_, pw) = password.as_ref().unwrap();
                let field = rect(44.0 * s, y + 4.0 * s, w - 130.0 * s, 28.0 * s);
                let bg = ui.ink(0.08);
                ui.cx.gfx.fill_rrect(field, 6.0 * s, bg);
                let ring = ui.fade(ACCENT);
                ui.cx.gfx.stroke_rrect(field, 6.0 * s, 1.5 * s, ring);
                let shown = if pw.is_empty() { String::new() } else { "•".repeat(pw.chars().count()) };
                let tr = rect(field.left + 8.0 * s, field.top, field.right - field.left - 16.0 * s, 28.0 * s);
                if shown.is_empty() {
                    ui.dim_label("Password", 12.5, tr, -1);
                } else {
                    ui.label(&shown, 12.5, 400, tr, -1);
                }
                let ok = pw.chars().count() >= 8;
                if ui.button(rect(w - 80.0 * s, y + 4.0 * s, 66.0 * s, 28.0 * s), "Join", ok) && ok {
                    let (n, p) = password.take().unwrap();
                    sys.joining = Some(n.ssid.clone());
                    ui.cx.sys.send(Job::WifiConnect(n, Some(p)));
                }
                y += 44.0 * s;
            }
        }
        if let Some(j) = &sys.joining {
            ui.dim_label(&format!("Joining “{j}”…"), 12.0, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1);
            y += 24.0 * s;
        } else if !sys.wifi_error.is_empty() {
            let msg = sys.wifi_error.clone();
            ui.text(&msg, 12.0, 400, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1, rgba(1.0, 0.35, 0.3, 1.0));
            y += 24.0 * s;
        }
    }
    if footer(ui, w, y, "Wi-Fi Settings…") {
        ui.cx.cmds.push(Cmd::Launch("ms-settings:network-wifi".into()));
        *close = true;
    }
}

// ----------------------------------------------------------------- Bluetooth

const BT_MAX: usize = 10;

fn draw_bluetooth(ui: &mut Ui, sys: &mut SysState, w: f32, close: &mut bool) {
    let s = ui.s();
    header(ui, w, "Bluetooth");
    let mut y = 54.0 * s;
    if let Some(on) = sys.bt_radio {
        if ui.switch(w - 54.0 * s, 17.0 * s, on) {
            ui.cx.sys.send(Job::SetRadio(RadioSel::Bluetooth, !on));
            sys.bt_radio = Some(!on);
            if !on {
                ui.cx.sys.send(Job::Bluetooth);
            }
        }
    }
    ui.separator(12.0 * s, w - 12.0 * s, 50.0 * s);

    let empty = match &sys.bt {
        _ if sys.bt_radio == Some(false) => Some("Bluetooth is turned off."),
        None => Some("Looking for devices…"),
        Some(d) if d.is_empty() => Some("No paired devices."),
        Some(_) => None,
    };
    if let Some(msg) = empty {
        ui.dim_label(msg, 12.5, rect(16.0 * s, y, w - 32.0 * s, 32.0 * s), -1);
        y += 32.0 * s;
    } else {
        ui.dim_label("Devices", 11.5, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1);
        y += 24.0 * s;
        let devices: Vec<BtDevice> = sys.bt.iter().flatten().take(BT_MAX).cloned().collect();
        for dev in devices {
            let r = rect(6.0 * s, y, w - 12.0 * s, 38.0 * s);
            let busy = sys.bt_busy.as_ref().filter(|(id, _)| *id == dev.id).map(|(_, op)| *op);
            // Reconnect button on connected audio devices; checked before the
            // row so one click does not trigger both.
            let re = rect(r.right - 34.0 * s, r.top + 5.0 * s, 28.0 * s, 28.0 * s);
            let show_re = dev.connected && dev.audio && busy.is_none();
            let re_hit = show_re && ui.clicked(re);
            let hit = ui.row(r);
            if show_re {
                if ui.hover(re) {
                    let c = ui.ink(0.12);
                    ui.cx.gfx.fill_rrect(re, 6.0 * s, c);
                }
                let c = ui.ink(0.7);
                ui.glyph('\u{E72C}', 11.5, re, c);
            }
            let bg = if dev.connected { ui.fade(ACCENT) } else { ui.ink(0.12) };
            ui.cx.gfx.fill_circle(r.left + 22.0 * s, r.top + 19.0 * s, 13.0 * s, bg);
            let fg = if dev.connected { rgba(1.0, 1.0, 1.0, ui.a) } else { ui.ink(0.85) };
            ui.glyph(dev.glyph, 12.0, rect(r.left + 9.0 * s, r.top, 26.0 * s, 38.0 * s), fg);
            let tw = w - 100.0 * s;
            ui.label(&dev.name, 13.0, if dev.connected { 600 } else { 400 }, rect(r.left + 44.0 * s, r.top + 2.0 * s, tw, 20.0 * s), -1);
            let sub = match busy {
                Some(BtOp::Connect) => "Connecting…",
                Some(BtOp::Disconnect) => "Disconnecting…",
                Some(BtOp::Reconnect) => "Reconnecting…",
                None if dev.connected => "Connected",
                None => "Not Connected",
            };
            ui.dim_label(sub, 11.5, rect(r.left + 44.0 * s, r.top + 19.0 * s, tw, 17.0 * s), -1);
            y += 40.0 * s;

            let op = if re_hit {
                Some(BtOp::Reconnect)
            } else if hit {
                Some(if dev.connected { BtOp::Disconnect } else { BtOp::Connect })
            } else {
                None
            };
            // One request at a time.
            if let (Some(op), None) = (op, &sys.bt_busy) {
                sys.bt_error.clear();
                // Anything connected can be disconnected; connecting needs an audio driver.
                if dev.audio || op == BtOp::Disconnect {
                    sys.bt_busy = Some((dev.id.clone(), op));
                    ui.cx.sys.send(Job::BtAction(dev.clone(), op));
                } else {
                    // Connecting a mouse / keyboard: only Settings (or the device) can.
                    ui.cx.cmds.push(Cmd::Launch("ms-settings:bluetooth".into()));
                    *close = true;
                }
            }
        }
    }
    if !sys.bt_error.is_empty() {
        let msg = sys.bt_error.clone();
        ui.text(&msg, 12.0, 400, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1, rgba(1.0, 0.35, 0.3, 1.0));
        y += 24.0 * s;
    }
    if footer(ui, w, y, "Bluetooth Settings…") {
        ui.cx.cmds.push(Cmd::Launch("ms-settings:bluetooth".into()));
        *close = true;
    }
}

// --------------------------------------------------------------------- Sound

fn draw_sound(ui: &mut Ui, st: &mut Status, sys: &mut SysState, w: f32, close: &mut bool) {
    let s = ui.s();
    header(ui, w, "Sound");
    ui.separator(12.0 * s, w - 12.0 * s, 50.0 * s);
    let mut y = 56.0 * s;
    let g = volume_glyph(st);
    let slider = rect(16.0 * s, y + 6.0 * s, w - 32.0 * s, 24.0 * s);
    if ui.clicked(rect(slider.left, slider.top, 26.0 * s, 24.0 * s)) {
        st.set_mute(!st.muted);
    }
    if let Some(v) = ui.slider(3, slider, st.volume, Some(g)) {
        st.set_volume(v);
    }
    y += 40.0 * s;
    ui.dim_label("Output", 11.5, rect(16.0 * s, y, w - 32.0 * s, 24.0 * s), -1);
    y += 24.0 * s;
    if sys.devices.is_empty() {
        ui.dim_label("No output devices", 12.5, rect(16.0 * s, y, w - 32.0 * s, 30.0 * s), -1);
        y += 30.0 * s;
    }
    for dev in sys.devices.clone() {
        let r = rect(6.0 * s, y, w - 12.0 * s, 30.0 * s);
        if ui.row(r) && !dev.default {
            ui.cx.sys.send(Job::SetAudioDevice(dev.id.clone()));
            for d in sys.devices.iter_mut() {
                d.default = d.id == dev.id;
            }
        }
        if dev.default {
            let c = ui.fade(ACCENT);
            ui.glyph('\u{E73E}', 11.0, rect(r.left + 6.0 * s, r.top, 20.0 * s, 30.0 * s), c);
        }
        ui.label(&dev.name, 13.0, 400, rect(r.left + 30.0 * s, r.top, w - 60.0 * s, 30.0 * s), -1);
        y += 30.0 * s;
    }
    if footer(ui, w, y, "Sound Settings…") {
        ui.cx.cmds.push(Cmd::Launch("ms-settings:sound".into()));
        *close = true;
    }
}

// ------------------------------------------------------------------- Battery

fn draw_battery(ui: &mut Ui, st: &Status, w: f32, close: &mut bool) {
    let s = ui.s();
    header(ui, w, "Battery");
    if let Some((pct, _)) = st.battery {
        ui.label(&format!("{pct}%"), 14.0, 600, rect(w - 90.0 * s, 12.0 * s, 74.0 * s, 32.0 * s), 1);
    }
    ui.separator(12.0 * s, w - 12.0 * s, 50.0 * s);
    let source = if st.on_battery { "Power Source: Battery" } else { "Power Source: Power Adapter" };
    ui.dim_label(source, 12.5, rect(16.0 * s, 56.0 * s, w - 32.0 * s, 24.0 * s), -1);
    let detail = match (st.battery, st.battery_secs) {
        (Some((100, true)), _) => "Fully Charged".to_string(),
        (Some((_, true)), _) => "Charging".to_string(),
        (_, Some(secs)) => format!("{}:{:02} remaining", secs / 3600, secs / 60 % 60),
        _ => "Calculating…".to_string(),
    };
    ui.dim_label(&detail, 12.5, rect(16.0 * s, 80.0 * s, w - 32.0 * s, 24.0 * s), -1);
    if footer(ui, w, 104.0 * s, "Battery Settings…") {
        ui.cx.cmds.push(Cmd::Launch("ms-settings:batterysaver".into()));
        *close = true;
    }
}

// ------------------------------------------------------------------ Calendar

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)) as u32
}

fn draw_calendar(ui: &mut Ui, w: f32, offset: &mut i32, close: &mut bool) {
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let s = ui.s();
    let today = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let total = today.wYear as i32 * 12 + today.wMonth as i32 - 1 + *offset;
    let (year, month) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    ui.label(&format!("{} {}", MONTHS[month as usize - 1], year), 14.0, 700, rect(16.0 * s, 12.0 * s, w - 100.0 * s, 32.0 * s), -1);
    for (i, (g, d)) in [('\u{E76B}', -1), ('\u{E76C}', 1)].iter().enumerate() {
        let r = rect(w - 72.0 * s + i as f32 * 30.0 * s, 14.0 * s, 28.0 * s, 28.0 * s);
        if ui.hover(r) {
            let c = ui.ink(0.1);
            ui.cx.gfx.fill_rrect(r, 6.0 * s, c);
        }
        let c = ui.ink(0.75);
        ui.glyph(*g, 11.0, r, c);
        if ui.clicked(r) {
            *offset += d;
        }
    }
    let cell = (w - 24.0 * s) / 7.0;
    let top = 46.0 * s;
    for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
        ui.dim_label(d, 11.0, rect(12.0 * s + i as f32 * cell, top, cell, 22.0 * s), 0);
    }
    let first_dow = (days_from_civil(year, month, 1) + 4).rem_euclid(7) as usize;
    let days = days_in_month(year, month);
    let is_this_month = *offset == 0;
    for day in 1..=days {
        let idx = first_dow + day as usize - 1;
        let (c, r) = (idx % 7, idx / 7);
        let cr = rect(12.0 * s + c as f32 * cell, top + 22.0 * s + r as f32 * 32.0 * s, cell, 32.0 * s);
        if is_this_month && day == today.wDay as u32 {
            let cx0 = (cr.left + cr.right) / 2.0;
            ui.cx.gfx.fill_circle(cx0, (cr.top + cr.bottom) / 2.0, 13.0 * s, rgba(1.0, 0.27, 0.23, ui.a));
            ui.text(&day.to_string(), 12.5, 700, cr, 0, rgba(1.0, 1.0, 1.0, 1.0));
        } else {
            let weekend = c == 0 || c == 6;
            let col = ui.ink(if weekend { 0.5 } else { 0.9 });
            ui.text(&day.to_string(), 12.5, 400, cr, 0, col);
        }
    }
    let y = top + 22.0 * s + 6.0 * 32.0 * s;
    if footer(ui, w, y, "Notifications…") {
        status::send_win_key(windows::Win32::UI::Input::KeyboardAndMouse::VK_N);
        *close = true;
    }
    let r = rect(6.0 * s, y + 40.0 * s, w - 12.0 * s, 28.0 * s);
    if ui.row(r) {
        ui.cx.cmds.push(Cmd::Launch("ms-settings:dateandtime".into()));
        *close = true;
    }
    ui.label("Date & Time Settings…", 13.0, 400, rect(r.left + 8.0 * s, r.top, r.right - r.left - 16.0 * s, 28.0 * s), -1);
}
