//! Top menu bar: system menu, active app name, window menu, other apps'
//! tray icons and status items that open our own panels.

use std::collections::HashMap;

use windows::core::GUID;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::ABE_TOP;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::render::anim::{lerp, Spring};
use crate::render::gfx::Surface;
use crate::ui::menu::{Anchor, Menu, MenuItem};
use crate::ui::popover::{Kind, Popover, SysState};
use crate::ui::tooltip::Tooltip;
use crate::util::{lp_x, rect, rgba, WM_MOUSELEAVE};
use crate::win::glass::Glass;
use crate::win::status::{self, Status};
use crate::win::sysctl::SysEvent;
use crate::win::trayhost::{self, TrayMsg, NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_STATE, NIF_TIP};
use crate::win::tracker::{self, window_pid};
use crate::win::{appbar, icons};

const TAG_LOGO: u32 = 10;
const TAG_APP: u32 = 11;
const TAG_WINDOW: u32 = 12;
const TIP_TIMER: usize = 7;

#[derive(Clone, Copy, PartialEq)]
enum Zone {
    Logo,
    App,
    Window,
    Tray(usize),
    Wifi,
    Volume,
    Battery,
    Control,
    Clock,
}

impl Zone {
    fn tag(self) -> Option<u32> {
        match self {
            Zone::Logo => Some(TAG_LOGO),
            Zone::App => Some(TAG_APP),
            Zone::Window => Some(TAG_WINDOW),
            _ => None,
        }
    }

    fn panel(self) -> Option<Kind> {
        match self {
            Zone::Wifi => Some(Kind::Wifi),
            Zone::Volume => Some(Kind::Sound),
            Zone::Battery => Some(Kind::Battery),
            Zone::Control => Some(Kind::Control),
            Zone::Clock => Some(Kind::Calendar),
            _ => None,
        }
    }
}

/// An icon another app put in the notification area.
pub struct TrayItem {
    owner: HWND,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    pub tip: String,
    hidden: bool,
    bmp: Option<ID2D1Bitmap>,
    pid: u32,
    /// Executable file name, lowercase (for the "hide" list in Settings).
    pub exe: String,
    /// Version-4 icons without NIF_SHOWTIP draw their own hover popup.
    showtip: bool,
    /// Windows' own Bluetooth icon: a left click opens our Bluetooth panel.
    bt: bool,
}

/// Window class of the shell's Bluetooth notification-area icon.
fn is_bluetooth_icon(owner: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(owner, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n]).starts_with("BluetoothNotificationAreaIconWindowClass")
}

/// The bar on one display. Every display gets the same content, like macOS.
struct Bar {
    glass: Glass,
    hwnd: HWND,
    surf: Surface,
    /// Screen position and width.
    x: i32,
    y: i32,
    w: i32,
    /// The display it sits on.
    mon: RECT,
    /// Clickable spans from the last render: x0, x1, zone.
    zones: Vec<(f32, f32, Zone)>,
    hover: Option<Zone>,
    tracking: bool,
    shown: bool,
}

impl Bar {
    fn new(cx: &Ctx, mon: RECT, h: i32) -> windows::core::Result<Self> {
        let (x, y, w) = (mon.left, mon.top, mon.right - mon.left);
        let mut glass = Glass::new(bar_tint(cx.dark));
        glass.set_rect(x, y, w, h, 0);
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, Some(glass.hwnd));
        unsafe {
            let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
        }
        let surf = Surface::new(&cx.gfx, hwnd, w as u32, h as u32)?;
        appbar::register(glass.hwnd, ABE_TOP, RECT { left: x, top: y, right: x + w, bottom: y + h });
        Ok(Self { glass, hwnd, surf, x, y, w, mon, zones: Vec::new(), hover: None, tracking: false, shown: false })
    }

    fn set_visible(&mut self, show: bool) -> bool {
        if self.shown == show {
            return false;
        }
        self.shown = show;
        self.glass.show(show);
        unsafe {
            let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        true
    }
}

pub struct MenuBar {
    bars: Vec<Bar>,
    /// The bar the pointer last used; menus and panels open on it.
    cur: usize,
    pub h: i32,
    name: String,
    prev_name: String,
    fade: Spring,
    names: HashMap<String, String>,
    pub status: Status,
    pub sys: SysState,
    pub popover: Popover,
    pub tray: Vec<TrayItem>,
    tooltip: Tooltip,
    /// Tray icon whose own popup (NIN_POPUPOPEN) is open.
    popup_open: Option<usize>,
    clock: String,
    dirty: bool,
    /// Last time the reserved space had to be claimed again.
    reserved_at: Option<std::time::Instant>,
}

fn bar_tint(dark: bool) -> u32 {
    if dark { 0x50_20_20_22 } else { 0x58_F4_F4_F4 }
}

impl MenuBar {
    pub fn new(cx: &mut Ctx) -> windows::core::Result<Self> {
        let h = (cx.cfg.menubar.height.clamp(20.0, 48.0) * cx.s).round() as i32;
        let mut bars = Vec::new();
        for mon in crate::win::monitors::list() {
            match Bar::new(cx, mon, h) {
                Ok(b) => bars.push(b),
                // Only the main display's bar is essential.
                Err(e) if bars.is_empty() => return Err(e),
                Err(_) => {}
            }
        }
        let primary = (bars[0].x, bars[0].y, bars[0].w);
        let popover = Popover::new(cx);
        let mut bar = Self {
            bars,
            cur: 0,
            h,
            name: String::new(),
            prev_name: String::new(),
            fade: Spring::new(1.0, 300.0, 34.0),
            names: HashMap::new(),
            status: Status::new(),
            sys: SysState::default(),
            popover,
            tray: Vec::new(),
            tooltip: Tooltip::new(cx),
            popup_open: None,
            clock: String::new(),
            dirty: true,
            reserved_at: None,
        };
        bar.update_app(cx);
        bar.fade.snap(1.0);
        bar.set_visible(None);
        bar.tick_clock(cx);
        if cx.cfg.menubar.tray_icons {
            let (x, y, w) = primary;
            trayhost::start(cx.main, RECT { left: x, top: y, right: x + w, bottom: y + h });
        }
        Ok(bar)
    }

    pub fn shutdown(&self) {
        trayhost::stop();
        for b in &self.bars {
            appbar::unregister(b.glass.hwnd);
        }
    }

    /// Show every bar except the one on a display where a fullscreen app runs.
    pub fn set_visible(&mut self, fullscreen_on: Option<RECT>) {
        for b in &mut self.bars {
            if b.set_visible(fullscreen_on != Some(b.mon)) {
                self.dirty = true;
            }
        }
    }

    /// Make sure maximized windows still stop below the bar; claim the space
    /// again if Windows dropped it. Returns true when it had to.
    pub fn ensure_reserved(&mut self) -> bool {
        let lost = appbar::work_area().top < self.bars[0].y + self.h;
        // Re-registering triggers another layout notification; never loop on it.
        if lost && self.reserved_at.is_none_or(|t| t.elapsed().as_secs() >= 5) {
            self.reserved_at = Some(std::time::Instant::now());
            for b in &self.bars {
                appbar::register(b.glass.hwnd, ABE_TOP, RECT { left: b.x, top: b.y, right: b.x + b.w, bottom: b.y + self.h });
            }
            return true;
        }
        false
    }

    pub fn theme_changed(&mut self, cx: &Ctx) {
        for b in &mut self.bars {
            b.glass.set_tint(bar_tint(cx.dark));
        }
        self.popover.theme_changed(cx);
        self.dirty = true;
    }

    /// Refresh the clock; returns milliseconds until the next minute.
    pub fn tick_clock(&mut self, cx: &Ctx) -> u32 {
        let (text, next) = status::clock_text(cx.cfg.menubar.clock_24h);
        self.clock = text;
        self.status.refresh();
        // Drop icons whose app has gone away without removing them.
        self.tray.retain(|t| unsafe { IsWindow(Some(t.owner)).as_bool() });
        self.dirty = true;
        next
    }

    pub fn status_changed(&mut self) {
        self.status.refresh();
        self.popover.refresh();
        self.dirty = true;
    }

    pub fn sys_event(&mut self, ev: SysEvent) {
        if self.sys.apply(ev) {
            self.status.reopen_volume();
        }
        self.popover.refresh();
        self.dirty = true;
    }

    /// Follow the foreground app.
    pub fn update_app(&mut self, cx: &Ctx) {
        let name = match cx.tracker.fg_key() {
            Some(key) => {
                let target = key.launch_target();
                self.names.entry(target.clone()).or_insert_with(|| icons::display_name(&target)).clone()
            }
            None => "Desktop".to_string(),
        };
        if name != self.name {
            self.prev_name = std::mem::replace(&mut self.name, name);
            self.fade.snap(if cx.animate { 0.0 } else { 1.0 });
            self.fade.set(1.0);
            self.dirty = true;
        }
    }

    // ------------------------------------------------------------ tray icons

    /// Returns true when something visible changed.
    pub fn tray_messages(&mut self, cx: &Ctx, msgs: Vec<TrayMsg>) -> bool {
        let mut visible_change = false;
        for m in msgs {
            let guid = (m.flags & NIF_GUID != 0 && m.guid != GUID::zeroed()).then_some(m.guid);
            let owner = HWND(m.owner as *mut _);
            let idx = self.tray.iter().position(|t| match (guid, t.guid) {
                (Some(g), Some(tg)) => g == tg,
                _ => t.owner == owner && t.uid == m.uid,
            });
            match (m.op, idx) {
                (2, Some(i)) => {
                    self.tray.remove(i);
                    visible_change = true;
                }
                (2, None) => {}
                (4, Some(i)) => self.tray[i].version = m.version,
                (4, None) => {}
                (_, i) => {
                    // NIM_ADD for a known icon behaves like NIM_MODIFY.
                    let i = match i {
                        Some(i) => i,
                        None if m.op == 0 => {
                            let pid = window_pid(owner);
                            let exe = tracker::process_info(pid).0.map(|p| p.rsplit('\\').next().unwrap_or("").to_lowercase()).unwrap_or_default();
                            visible_change = true;
                            self.tray.push(TrayItem { owner, uid: m.uid, guid, callback: 0, version: 0, tip: String::new(), hidden: false, bmp: None, pid, exe, showtip: false, bt: is_bluetooth_icon(owner) });
                            self.tray.len() - 1
                        }
                        None => continue,
                    };
                    let t = &mut self.tray[i];
                    if m.flags & NIF_MESSAGE != 0 {
                        t.callback = m.callback;
                    }
                    if m.flags & NIF_TIP != 0 {
                        t.tip = m.tip;
                    }
                    if m.flags & 0x80 != 0 {
                        t.showtip = true;
                    }
                    if m.flags & NIF_STATE != 0 && m.state_mask & 1 != 0 && t.hidden != (m.state & 1 != 0) {
                        t.hidden = m.state & 1 != 0;
                        visible_change = true;
                    }
                    if m.flags & NIF_ICON != 0 {
                        t.bmp = m.icon.and_then(|(w, h, px)| cx.gfx.create_bitmap(w, h, &px));
                        visible_change = true;
                    }
                }
            }
        }
        // Tooltip-only updates (download speeds, levels...) need no redraw.
        self.dirty |= visible_change;
        visible_change
    }

    /// Visible tray items, in display order.
    fn shown_tray(&self, cx: &Ctx) -> Vec<usize> {
        if !cx.cfg.menubar.tray_icons {
            return Vec::new();
        }
        (0..self.tray.len())
            .filter(|&i| {
                let t = &self.tray[i];
                !t.hidden && t.bmp.is_some() && !cx.cfg.menubar.tray_hidden.iter().any(|x| x.eq_ignore_ascii_case(&t.exe))
            })
            .collect()
    }

    /// The bar the pointer is on.
    fn bar(&self) -> &Bar {
        &self.bars[self.cur.min(self.bars.len() - 1)]
    }

    /// Forward a mouse event on a tray icon to the app that owns it.
    /// `x` is in bar pixels.
    fn tray_notify(&self, i: usize, mouse_msg: u32, x: i32) {
        let Some(t) = self.tray.get(i) else { return };
        if t.callback == 0 {
            return;
        }
        let (x, y) = (self.bar().x + x, self.bar().y + self.h);
        unsafe {
            // Lets the app bring its menu or window to the front.
            let _ = AllowSetForegroundWindow(t.pid);
            let send = |msg: u32| {
                let (wp, lp) = if t.version >= 4 {
                    (WPARAM(((y as u16 as usize) << 16) | x as u16 as usize), LPARAM(((t.uid as u16 as isize) << 16) | msg as u16 as isize))
                } else {
                    (WPARAM(t.uid as usize), LPARAM(msg as isize))
                };
                let _ = PostMessageW(Some(t.owner), t.callback, wp, lp);
            };
            send(mouse_msg);
            if t.version >= 4 {
                match mouse_msg {
                    WM_LBUTTONUP => send(0x400), // NIN_SELECT
                    WM_RBUTTONUP => send(WM_CONTEXTMENU),
                    _ => {}
                }
            }
        }
    }

    /// Hovering a tray icon long enough: show its tooltip, or let the app
    /// show its own popup when it asked to (version 4 without NIF_SHOWTIP).
    fn start_tip(&mut self, cx: &mut Ctx, i: usize) {
        let Some((x0, x1)) = self.zone_span(Zone::Tray(i)) else { return };
        let Some(t) = self.tray.get(i) else { return };
        let cxp = ((x0 + x1) / 2.0) as i32;
        if t.version >= 4 && !t.showtip {
            self.tray_notify(i, 0x406, cxp); // NIN_POPUPOPEN
            self.popup_open = Some(i);
        } else {
            let tip = t.tip.clone();
            let (bx, by) = (self.bar().x, self.bar().y);
            self.tooltip.show(cx, &tip, bx + cxp, by + self.h + (6.0 * cx.s) as i32);
        }
    }

    fn end_tip(&mut self) {
        unsafe {
            for b in &self.bars {
                let _ = KillTimer(Some(b.hwnd), TIP_TIMER);
            }
        }
        self.tooltip.hide();
        if let Some(i) = self.popup_open.take() {
            let x = self.zone_span(Zone::Tray(i)).map(|z| ((z.0 + z.1) / 2.0) as i32).unwrap_or(0);
            self.tray_notify(i, 0x407, x); // NIN_POPUPCLOSE
        }
    }

    // ----------------------------------------------------------------- menus

    fn zone_at(&self, x: f32) -> Option<Zone> {
        self.bar().zones.iter().find(|(x0, x1, _)| x >= *x0 && x < *x1).map(|z| z.2)
    }

    fn zone_span(&self, zone: Zone) -> Option<(f32, f32)> {
        self.bar().zones.iter().find(|z| z.2 == zone).map(|z| (z.0, z.1))
    }

    fn open_menu(&mut self, cx: &mut Ctx, menu: &mut Menu, zone: Zone) {
        let Some(tag) = zone.tag() else { return };
        let Some((x0, _)) = self.zone_span(zone) else { return };
        self.popover.close(cx);
        let fg_key = cx.tracker.fg_key().cloned();
        let items = match zone {
            Zone::Logo => {
                use crate::win::power::Action;
                let mut items = vec![
                MenuItem::disabled(concat!("WinMacDock ", env!("CARGO_PKG_VERSION"))),
                MenuItem::sep(),
                MenuItem::new("System Settings…", Cmd::Launch("ms-settings:".into())),
                MenuItem::new("App Store…", Cmd::Launch("ms-windows-store:".into())),
                MenuItem::sep(),
                MenuItem::new("WinMacDock Settings…", Cmd::OpenSettings),
                MenuItem::sep(),
                MenuItem::new("Sleep", Cmd::Power(Action::Sleep)),
                ];
                if crate::win::power::can_hibernate() {
                    items.push(MenuItem::new("Hibernate", Cmd::Power(Action::Hibernate)));
                }
                items.extend([
                    MenuItem::new("Restart…", Cmd::Power(Action::Restart)),
                    MenuItem::new("Shut Down…", Cmd::Power(Action::ShutDown)),
                    MenuItem::sep(),
                    MenuItem::new("Lock Screen", Cmd::Lock),
                    MenuItem::new("Log Out…", Cmd::Power(Action::LogOut)),
                    MenuItem::sep(),
                    MenuItem::new("Quit WinMacDock", Cmd::QuitSelf),
                ]);
                items
            }
            Zone::App => match fg_key {
                Some(key) => vec![
                    MenuItem::new(&format!("Hide {}", self.name), Cmd::HideApp(key.clone())),
                    MenuItem::sep(),
                    MenuItem::new(&format!("Quit {}", self.name), Cmd::CloseApp(key)),
                ],
                None => vec![MenuItem::disabled("No active app")],
            },
            Zone::Window => match fg_key {
                Some(key) => {
                    let mut items = vec![
                        MenuItem::new("Minimize", Cmd::Minimize(cx.tracker.fg)),
                        MenuItem::new("Zoom", Cmd::ZoomFg),
                        MenuItem::sep(),
                        MenuItem::new("Bring All to Front", Cmd::BringAllToFront(key.clone())),
                        MenuItem::sep(),
                    ];
                    for w in cx.tracker.windows_of(&key) {
                        items.push(MenuItem::check(&tracker::title(w.hwnd), Cmd::Activate(w.hwnd), w.hwnd == cx.tracker.fg));
                    }
                    items.push(MenuItem::sep());
                    items.push(MenuItem::new("Close Window", Cmd::CloseFg));
                    items
                }
                None => vec![MenuItem::disabled("No active window")],
            },
            _ => return,
        };
        let (bx, by) = (self.bar().x, self.bar().y);
        menu.show(cx, items, bx + x0 as i32, by + self.h + (2.0 * cx.s) as i32, Anchor::Below, tag);
        self.dirty = true;
    }

    /// The Bluetooth tray icon, which we answer ourselves on a left click.
    fn is_bt(&self, zone: Option<Zone>) -> bool {
        matches!(zone, Some(Zone::Tray(i)) if self.tray.get(i).is_some_and(|t| t.bt))
    }

    fn open_panel(&mut self, cx: &mut Ctx, menu: &mut Menu, zone: Zone) {
        let kind = match zone {
            Zone::Tray(_) if self.is_bt(Some(zone)) => Some(Kind::Bluetooth),
            z => z.panel(),
        };
        let Some(kind) = kind else { return };
        if self.popover.visible && self.popover.kind == Some(kind) {
            self.popover.close(cx);
            return;
        }
        if self.popover.just_closed(kind) {
            return;
        }
        menu.close(cx);
        let (bx, by, bw) = (self.bar().x, self.bar().y, self.bar().w);
        let right = bx + self.zone_span(zone).map(|z| z.1 as i32).unwrap_or(bw);
        // Control Center and the calendar line up with the screen edge like on macOS.
        let right = if matches!(kind, Kind::Control | Kind::Calendar) { bx + bw - (6.0 * cx.s) as i32 } else { right };
        self.popover.show(cx, kind, right, by + self.h + (4.0 * cx.s) as i32);
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, menu: &mut Menu, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if let Some(r) = self.popover.on_msg(cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        let bi = self.bars.iter().position(|b| b.hwnd == hwnd)?;
        let x = lp_x(lp) as f32;
        if matches!(msg, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP) && bi != self.cur {
            // Moved onto another display's bar.
            self.end_tip();
            self.cur = bi;
            self.dirty = true;
        }
        match msg {
            WM_MOUSEMOVE => {
                if !self.bars[bi].tracking {
                    self.bars[bi].tracking = true;
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                let zone = self.zone_at(x);
                if zone != self.bars[bi].hover {
                    self.end_tip();
                    if let Some(Zone::Tray(_)) = zone {
                        unsafe {
                            SetTimer(Some(hwnd), TIP_TIMER, 550, None);
                        }
                    }
                    self.bars[bi].hover = zone;
                    self.dirty = true;
                    // With a menu already open, sliding across titles switches menus.
                    if let Some(z) = zone {
                        let open_tag = menu.owner_tag;
                        if menu.visible && (TAG_LOGO..=TAG_WINDOW).contains(&open_tag) && z.tag().is_some_and(|t| t != open_tag) {
                            self.open_menu(cx, menu, z);
                        }
                    }
                }
                if let Some(Zone::Tray(i)) = zone {
                    self.tray_notify(i, WM_MOUSEMOVE, x as i32);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIP_TIMER => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIP_TIMER);
                }
                if let (true, Some(Zone::Tray(i))) = (bi == self.cur, self.bars[bi].hover) {
                    self.start_tip(cx, i);
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.end_tip();
                self.bars[bi].tracking = false;
                if self.bars[bi].hover.take().is_some() {
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                match self.zone_at(x) {
                    Some(z @ (Zone::Logo | Zone::App | Zone::Window)) => {
                        let tag = z.tag().unwrap_or(0);
                        if menu.visible && menu.owner_tag == tag {
                            menu.close(cx);
                        } else if !menu.just_closed(tag) {
                            self.open_menu(cx, menu, z);
                        }
                    }
                    Some(z @ Zone::Tray(_)) if self.is_bt(Some(z)) => {
                        self.end_tip();
                        self.open_panel(cx, menu, z);
                    }
                    Some(Zone::Tray(i)) => {
                        self.end_tip();
                        menu.close(cx);
                        self.popover.close(cx);
                        self.tray_notify(i, msg, x as i32);
                    }
                    Some(z) => self.open_panel(cx, menu, z),
                    None => {}
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP => {
                let zone = self.zone_at(x);
                if msg == WM_LBUTTONUP && self.is_bt(zone) {
                    return Some(LRESULT(0));
                }
                if msg == WM_RBUTTONDOWN && self.is_bt(zone) {
                    self.popover.close(cx);
                }
                if let Some(Zone::Tray(i)) = zone {
                    self.tray_notify(i, msg, x as i32);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.bars.iter().any(|b| b.shown) && (self.dirty || self.fade.active() || self.popover.needs_frame())
    }

    pub fn frame(&mut self, cx: &mut Ctx, menu: &Menu, dt: f32) {
        self.popover.frame(cx, &mut self.status, &mut self.sys, dt);
        if !self.bars.iter().any(|b| b.shown) || !(self.dirty || self.fade.active()) {
            return;
        }
        self.dirty = false;
        self.fade.tick(dt);
        let mut bars = std::mem::take(&mut self.bars);
        for (i, b) in bars.iter_mut().enumerate() {
            if b.shown {
                self.draw_bar(cx, menu, b, i == self.cur);
            }
        }
        self.bars = bars;
    }

    /// Paint one display's bar. Open menus/panels are highlighted only on
    /// the bar they were opened from.
    fn draw_bar(&self, cx: &mut Ctx, menu: &Menu, b: &mut Bar, current: bool) {
        let s = cx.s;
        let h = self.h as f32;
        let w = b.w as f32;
        let dark = cx.dark;
        let ink = move |a: f32| if dark { rgba(1.0, 1.0, 1.0, a) } else { rgba(0.0, 0.0, 0.0, a) };
        let fg = ink(if dark { 0.95 } else { 0.88 });
        let open_tag = if menu.visible && current { menu.owner_tag } else { 0 };
        let open_panel = if self.popover.visible && current { self.popover.kind } else { None };

        b.surf.begin(&cx.gfx);
        b.zones.clear();
        let highlight = |cx: &mut Ctx, x0: f32, x1: f32, zone: Zone, hover: Option<Zone>| {
            let active = zone.tag().is_some_and(|t| t == open_tag) || (zone.panel().is_some() && zone.panel() == open_panel);
            if active || hover == Some(zone) {
                let a = if active { 0.2 } else { 0.11 };
                cx.gfx.fill_rrect(rect(x0, 3.0 * s, x1 - x0, h - 6.0 * s), 5.0 * s, ink(a));
            }
        };

        // ---- left side ----
        let mut x = 6.0 * s;
        let logo_w = 34.0 * s;
        highlight(cx, x, x + logo_w, Zone::Logo, b.hover);
        let (lx, ly, d) = (x + logo_w / 2.0, h / 2.0, 6.5 * s);
        cx.gfx.fill_rrect(rect(lx - d, ly - d, d * 2.0, d * 2.0), d * 0.62, fg);
        b.zones.push((x, x + logo_w, Zone::Logo));
        x += logo_w;

        let f = self.fade.v.clamp(0.0, 1.0);
        let new_w = cx.gfx.text_width(&self.name, 13.5 * s, 700);
        let old_w = cx.gfx.text_width(&self.prev_name, 13.5 * s, 700);
        let name_w = lerp(old_w, new_w, f) + 20.0 * s;
        highlight(cx, x, x + name_w, Zone::App, b.hover);
        let tr = rect(x + 10.0 * s, 0.0, name_w, h);
        if f < 0.99 {
            cx.gfx.text(&self.prev_name, 13.5 * s, 700, tr, -1, ink(0.92 * (1.0 - f)));
        }
        cx.gfx.text(&self.name, 13.5 * s, 700, tr, -1, ink(0.95 * f));
        b.zones.push((x, x + name_w, Zone::App));
        x += name_w;

        let win_w = cx.gfx.text_width("Window", 13.5 * s, 400) + 20.0 * s;
        highlight(cx, x, x + win_w, Zone::Window, b.hover);
        cx.gfx.text("Window", 13.5 * s, 400, rect(x + 10.0 * s, 0.0, win_w, h), -1, fg);
        b.zones.push((x, x + win_w, Zone::Window));

        // ---- right side, laid out right to left ----
        let mut r = w - 6.0 * s;
        let clock_w = cx.gfx.text_width(&self.clock, 13.5 * s, 400) + 20.0 * s;
        highlight(cx, r - clock_w, r, Zone::Clock, b.hover);
        cx.gfx.text(&self.clock, 13.5 * s, 400, rect(r - clock_w, 0.0, clock_w, h), 0, fg);
        b.zones.push((r - clock_w, r, Zone::Clock));
        r -= clock_w;

        let iw = 32.0 * s;
        // Control Center: two toggle pills.
        highlight(cx, r - iw, r, Zone::Control, b.hover);
        {
            let (cxm, cym) = (r - iw / 2.0, h / 2.0);
            let (pw, ph) = (15.0 * s, 5.6 * s);
            for (row, knob_right) in [(-1.0f32, false), (1.0, true)] {
                let py = cym + row * 4.2 * s - ph / 2.0;
                cx.gfx.stroke_rrect(rect(cxm - pw / 2.0, py, pw, ph), ph / 2.0, 1.2 * s, fg);
                let kx = if knob_right { cxm + pw / 2.0 - ph / 2.0 } else { cxm - pw / 2.0 + ph / 2.0 };
                cx.gfx.fill_circle(kx, py + ph / 2.0, ph / 2.0, fg);
            }
        }
        b.zones.push((r - iw, r, Zone::Control));
        r -= iw;

        if let Some((pct, charging)) = self.status.battery {
            // Optional percentage written inside a slightly larger battery, like macOS.
            let inside = cx.cfg.menubar.battery_percent;
            let (body_w, body_h) = if inside { (28.0 * s, 13.5 * s) } else { (23.0 * s, 11.5 * s) };
            // Charging with the number shown: the bolt sits beside the battery.
            let bolt_out = charging && inside;
            let bw = body_w + 13.0 * s + if bolt_out { 11.0 * s } else { 0.0 };
            highlight(cx, r - bw, r, Zone::Battery, b.hover);
            let bx = r - bw / 2.0 - body_w / 2.0 - 1.0 * s;
            let by = h / 2.0 - body_h / 2.0;
            cx.gfx.stroke_rrect(rect(bx, by, body_w, body_h), 3.2 * s, 1.0 * s, ink(0.55));
            cx.gfx.fill_rrect(rect(bx + body_w + 1.2 * s, h / 2.0 - 2.0 * s, 1.6 * s, 4.0 * s), 0.8 * s, ink(0.55));
            let level = (pct as f32 / 100.0).clamp(0.05, 1.0);
            let fill = if charging {
                rgba(0.20, 0.82, 0.35, 1.0)
            } else if pct <= 15 {
                rgba(1.0, 0.27, 0.23, 1.0)
            } else {
                fg
            };
            let inset = 1.8 * s;
            if inside {
                // A softer fill keeps the number readable over it.
                let soft = if charging || pct <= 15 { fill } else { ink(0.38) };
                cx.gfx.fill_rrect(rect(bx + inset, by + inset, (body_w - inset * 2.0) * level, body_h - inset * 2.0), 2.0 * s, soft);
                let text_c = if charging || pct <= 15 { rgba(1.0, 1.0, 1.0, 1.0) } else { fg };
                cx.gfx.text(&pct.to_string(), 9.5 * s, 700, rect(bx, by - 0.5 * s, body_w, body_h), 0, text_c);
            } else {
                cx.gfx.fill_rrect(rect(bx + inset, by + inset, (body_w - inset * 2.0) * level, body_h - inset * 2.0), 1.6 * s, fill);
                if charging {
                    // Bolt over the fill: dark outline first so it reads on green and on empty.
                    let br = rect(bx, by, body_w, body_h);
                    for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                        let o = rect(br.left + dx * s * 0.8, br.top + dy * s * 0.8, body_w, body_h);
                        cx.gfx.glyph('\u{E945}', 9.5 * s, o, rgba(0.0, 0.0, 0.0, 0.55));
                    }
                    cx.gfx.glyph('\u{E945}', 9.5 * s, br, rgba(1.0, 1.0, 1.0, 1.0));
                }
            }
            if bolt_out {
                cx.gfx.glyph('\u{E945}', 11.0 * s, rect(bx - 13.0 * s, by - 1.0 * s, 12.0 * s, body_h + 2.0 * s), fg);
            }
            b.zones.push((r - bw, r, Zone::Battery));
            r -= bw;
        }

        highlight(cx, r - iw, r, Zone::Volume, b.hover);
        let vol_glyph = if self.status.muted {
            '\u{E74F}'
        } else if self.status.volume < 0.01 {
            '\u{E992}'
        } else if self.status.volume < 0.34 {
            '\u{E993}'
        } else if self.status.volume < 0.67 {
            '\u{E994}'
        } else {
            '\u{E995}'
        };
        cx.gfx.glyph(vol_glyph, 15.0 * s, rect(r - iw, 0.0, iw, h), fg);
        b.zones.push((r - iw, r, Zone::Volume));
        r -= iw;

        highlight(cx, r - iw, r, Zone::Wifi, b.hover);
        cx.gfx.glyph('\u{E701}', 15.0 * s, rect(r - iw, 0.0, iw, h), if self.status.online { fg } else { ink(0.35) });
        b.zones.push((r - iw, r, Zone::Wifi));
        r -= iw + 4.0 * s;

        // Other apps' tray icons, newest on the left like macOS.
        let tw = 28.0 * s;
        let isz = 16.0 * s;
        for i in self.shown_tray(cx) {
            if r - tw < x + win_w + 40.0 * s {
                break; // out of room
            }
            highlight(cx, r - tw, r, Zone::Tray(i), b.hover);
            if let Some(bmp) = &self.tray[i].bmp {
                cx.gfx.bitmap(bmp, rect(r - tw / 2.0 - isz / 2.0, h / 2.0 - isz / 2.0, isz, isz), 1.0);
            }
            b.zones.push((r - tw, r, Zone::Tray(i)));
            r -= tw;
        }

        b.surf.end(&cx.gfx);
    }
}
