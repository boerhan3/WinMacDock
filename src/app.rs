//! Application state, window classes, message loop and command dispatch.

use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};
use std::time::Instant;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN};
use windows::Win32::UI::Shell::{SHEmptyRecycleBinW, ABN_FULLSCREENAPP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, Config, Section};
use crate::render::anim;
use crate::render::gfx::Gfx;
use crate::ui::dock::Dock;
use crate::ui::launchpad::Launchpad;
use crate::ui::menu::{Anchor, Menu, MenuItem};
use crate::ui::menubar::MenuBar;
use crate::ui::minfx::{self, MinFx};
use crate::ui::preview::Preview;
use crate::ui::settings::{self, Extra, Settings, WM_APP_PICKED};
use crate::ui::spotlight::Spotlight;
use crate::ui::traffic::{Traffic, WM_APP_TRAFFIC};
use crate::ui::wallpaper::{self, Wallpaper};
use crate::util;
use crate::win::appbar::WM_APPBAR;
use crate::win::apps::{self, AppEntry};
use crate::win::icons::{self, IconLoader, LoadedIcon, WM_APP_ICON};
use crate::win::outside::{self, WM_APP_OUTSIDE};
use crate::win::status::{self, WM_APP_STATUS};
use crate::win::sysctl::{SysEvent, Worker, WM_APP_SYS};
use crate::win::taskbar;
use crate::win::tracker::{self, AppKey, Tracker};
use crate::win::trayhost::{self, WM_APP_TRAY};

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
/// Sent by a second `winmacdock.exe --<command>` process; `wParam` is the command.
const WM_APP_REMOTE: u32 = WM_APP + 5;
/// The background app scan finished; `lParam` is a `Box<Vec<AppEntry>>`.
const WM_APP_APPS: u32 = WM_APP + 6;
/// A window started minimizing; `wParam` is the window.
const WM_APP_MINSTART: u32 = WM_APP + 12;
const MAIN_TITLE: PCWSTR = w!("WinMacDockMain");
const TIMER_REFRESH: usize = 1;
const TIMER_CLOCK: usize = 2;
const TIMER_WALLPAPER: usize = 3;
const TIMER_TRIM: usize = 4;
const TIMER_TRAY: usize = 5;
const TIMER_DISPLAY: usize = 6;
/// Refreshes the picture of the window in front (for the minimize effect).
const TIMER_SNAP: usize = 8;
const TIMER_SNAP_FG: usize = 9;

/// Milliseconds since the last keyboard or mouse input.
fn user_idle_ms() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut li).as_bool() {
            return u32::MAX;
        }
        windows::Win32::System::SystemInformation::GetTickCount().wrapping_sub(li.dwTime)
    }
}
const HOTKEY_QUIT: i32 = 1;
const HOTKEY_SPOTLIGHT: i32 = 2;
const CLASS: PCWSTR = w!("WinMacDockWindow");
const MENU_TAG_APP: u32 = 30;

#[derive(Clone)]
pub enum Cmd {
    QuitSelf,
    OpenConfig,
    ReloadConfig,
    OpenSettings,
    CloseSettings,
    ConfigChanged(Section),
    /// Start a target (a new window when the app already runs).
    Launch(String),
    /// Bring the app forward if it runs, otherwise start it.
    Open(String),
    Pin(String),
    Unpin(String),
    CloseApp(AppKey),
    Minimize(HWND),
    /// Minimize all of an app's windows: (front window, which gets the effect; all windows).
    MinimizeAll(HWND, Vec<HWND>),
    Activate(HWND),
    BringAllToFront(AppKey),
    ToggleDockHiding,
    /// Put an app in Launchpad or take it out.
    ToggleLaunchpad(String),
    /// Context menu for an installed app at a screen point.
    AppMenu(String, i32, i32),
    EmptyTrash,
    Launchpad,
    Spotlight,
    Power(crate::win::power::Action),
    Lock,
    HideApp(AppKey),
    ZoomFg,
    CloseFg,
}

/// Shared services every UI component draws on.
pub struct Ctx {
    pub gfx: Gfx,
    pub cfg: Config,
    /// DPI scale (1.0 at 96 DPI).
    pub s: f32,
    /// Primary screen size in pixels.
    pub sw: i32,
    pub sh: i32,
    pub dark: bool,
    /// False when Windows' "Animation effects" setting is off.
    pub animate: bool,
    pub tracker: Tracker,
    pub main: HWND,
    pub hinst: HINSTANCE,
    /// Commands queued by components, run after the current message/frame.
    pub cmds: Vec<Cmd>,
    /// Installed apps for Launchpad / Spotlight, scanned off the UI thread.
    pub apps: Vec<AppEntry>,
    pub icon_loader: IconLoader,
    /// Background thread for slow system calls.
    pub sys: Worker,
    /// Animation time of the live wallpaper, so overlays can show the same picture.
    pub wallpaper_time: f32,
    apps_scanned: Option<Instant>,
    apps_scanning: bool,
}

impl Ctx {
    /// Refresh the installed-app list in the background if it is stale.
    pub fn request_apps(&mut self) {
        if self.apps_scanning || self.apps_scanned.is_some_and(|t| t.elapsed().as_secs() < 60) {
            return;
        }
        self.apps_scanning = true;
        let main = self.main.0 as isize;
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let list = Box::into_raw(Box::new(apps::list()));
            if PostMessageW(Some(HWND(main as *mut _)), WM_APP_APPS, WPARAM(0), LPARAM(list as isize)).is_err() {
                drop(Box::from_raw(list));
            }
            CoUninitialize();
        });
    }
}

pub struct App {
    pub cx: Ctx,
    pub dock: Dock,
    pub menu: Menu,
    pub menubar: Option<MenuBar>,
    pub launchpad: Launchpad,
    pub spotlight: Spotlight,
    pub wallpaper: Option<Wallpaper>,
    pub traffic: Option<Traffic>,
    pub minfx: MinFx,
    pub preview: Preview,
    pub settings: Settings,
    hotkey_ok: bool,
    shellhook_msg: u32,
    taskbar_created_msg: u32,
    last_frame: Instant,
    idle: bool,
    busy: bool,
    /// Frame intervals (ms) of the current animation, when `--frame-stats` is on.
    frame_stats: Option<Vec<f32>>,
    /// Time spent inside frame() (ms), for the same stats.
    work_stats: Vec<f32>,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub fn main_hwnd() -> HWND {
    HWND(MAIN_HWND.load(Ordering::Relaxed) as *mut _)
}

fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default() }
}

/// Create a hidden borderless window of our class.
pub fn create_window(ex: WINDOW_EX_STYLE, owner: Option<HWND>) -> HWND {
    unsafe { CreateWindowExW(ex, CLASS, w!(""), WS_POPUP, 0, 0, 0, 0, owner, None, Some(hinstance()), None).expect("CreateWindowExW failed") }
}

fn system_dark() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut _ as *mut _),
            Some(&mut size),
        );
    }
    value == 0
}

pub struct RunOptions {
    pub open_settings: bool,
    pub frame_stats: bool,
}

pub fn run(opts: RunOptions) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        // Single instance.
        let _mutex = CreateMutexW(None, true, w!("WinMacDock.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        // First thing: get the Windows taskbar out of sight while we load.
        taskbar::hide();

        let hinst = hinstance();
        let procs: [(PCWSTR, WNDPROC); 2] = [(CLASS, Some(wndproc)), (settings::CLASS, Some(settings::proc))];
        for (class, proc) in procs {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_DBLCLKS,
                lpfnWndProc: proc,
                hInstance: hinst,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
        }

        let main = create_window(WS_EX_TOOLWINDOW, None);
        let _ = SetWindowTextW(main, MAIN_TITLE);
        MAIN_HWND.store(main.0 as isize, Ordering::Relaxed);

        let mut cx = Ctx {
            gfx: Gfx::new()?,
            cfg: Config::load(),
            s: GetDpiForSystem() as f32 / 96.0,
            sw: GetSystemMetrics(SM_CXSCREEN),
            sh: GetSystemMetrics(SM_CYSCREEN),
            dark: system_dark(),
            animate: anim::system_animations_enabled(),
            tracker: Tracker::new(),
            main,
            hinst,
            cmds: Vec::new(),
            apps: Vec::new(),
            icon_loader: IconLoader::new(main),
            sys: Worker::new(main),
            wallpaper_time: 0.0,
            apps_scanned: None,
            apps_scanning: false,
        };
        cx.request_apps();
        cx.tracker.refresh();
        let menu = Menu::new(&cx);
        let dock = Dock::new(&mut cx)?;
        let menubar = if cx.cfg.menubar.enabled { Some(MenuBar::new(&mut cx)?) } else { None };
        let wallpaper = Wallpaper::new(&cx);
        let traffic = if cx.cfg.traffic_lights.enabled { Traffic::new(&cx).ok() } else { None };
        let minfx = MinFx::new();
        let preview = Preview::new(&cx);
        let launchpad = Launchpad::new(&cx);
        let spotlight = Spotlight::new(&cx);
        let settings = Settings::new(&cx);

        let app = Box::into_raw(Box::new(App {
            cx,
            dock,
            menu,
            menubar,
            launchpad,
            spotlight,
            wallpaper,
            traffic,
            minfx,
            preview,
            settings,
            hotkey_ok: false,
            shellhook_msg: RegisterWindowMessageW(w!("SHELLHOOK")),
            taskbar_created_msg: RegisterWindowMessageW(w!("TaskbarCreated")),
            last_frame: Instant::now(),
            idle: true,
            busy: false,
            frame_stats: opts.frame_stats.then(Vec::new),
            work_stats: Vec::new(),
        }));
        APP.store(app, Ordering::Release);

        taskbar::hide();
        if (*app).cx.animate {
            minfx::disable_system_animation();
        }
        taskbar::watch();
        let _ = RegisterShellHookWindow(main);
        // Foreground changes and (un)cloaking are not reported by the shell hook.
        for (lo, hi) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZESTART),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ] {
            SetWinEventHook(lo, hi, None, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        }
        // Emergency exit: Ctrl+Alt+Shift+Q.
        let _ = RegisterHotKey(Some(main), HOTKEY_QUIT, MOD_CONTROL | MOD_ALT | MOD_SHIFT, 'Q' as u32);
        (*app).register_spotlight();
        (*app).start_timers();
        let _ = WTSRegisterSessionNotification(main, NOTIFY_FOR_THIS_SESSION);
        SetTimer(Some(main), TIMER_TRIM, 8000, None);
        if opts.open_settings {
            (*app).cx.cmds.push(Cmd::OpenSettings);
            (*app).run_cmds();
        }

        let mut msg = MSG::default();
        'outer: loop {
            if (*app).needs_frame() {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        break 'outer;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                (*app).busy = true;
                (*app).frame();
                (*app).busy = false;
                // One wait per frame, aligned to the compositor's vsync.
                let _ = DwmFlush();
            } else {
                if !(*app).idle {
                    (*app).flush_frame_stats();
                }
                (*app).idle = true;
                if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if let Some(wp) = &(*app).wallpaper {
            wp.shutdown();
        }
        if let Some(t) = &mut (*app).traffic {
            t.shutdown();
        }
        (*app).dock.shutdown();
        if let Some(bar) = &(*app).menubar {
            bar.shutdown();
        }
        // Signing out or restarting with "start at sign-in" on: leave the taskbar
        // hidden so the next sign-in goes straight to WinMacDock.
        if !SESSION_ENDING.load(Ordering::Relaxed) || !crate::win::autostart::enabled() {
            taskbar::restore();
        }
        Ok(())
    }
}

static SESSION_ENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe extern "system" fn win_event(_hook: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    if id_object == 0 {
        if event == EVENT_SYSTEM_MINIMIZESTART {
            let _ = PostMessageW(Some(main_hwnd()), WM_APP_MINSTART, WPARAM(hwnd.0 as usize), LPARAM(0));
        }
        let _ = PostMessageW(Some(main_hwnd()), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
    }
}

pub unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = APP.load(Ordering::Acquire);
    // Messages sent re-entrantly (while we are already inside a handler) take the default path.
    if !app.is_null() && !(*app).busy {
        (*app).busy = true;
        let handled = (*app).handle(hwnd, msg, wp, lp);
        (*app).run_cmds();
        (*app).busy = false;
        if let Some(r) = handled {
            return r;
        }
    }
    match msg {
        WM_MOUSEACTIVATE if !app.is_null() && hwnd == (*app).settings.hwnd => LRESULT(MA_ACTIVATE as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

impl App {
    fn needs_frame(&self) -> bool {
        self.dock.needs_frame()
            || self.menu.needs_frame()
            || self.launchpad.needs_frame()
            || self.spotlight.needs_frame()
            || self.minfx.needs_frame()
            || self.preview.needs_frame()
            || self.settings.needs_frame()
            || self.traffic.as_ref().is_some_and(|t| t.needs_frame())
            || self.menubar.as_ref().is_some_and(|b| b.needs_frame())
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = if self.idle { 1.0 / 60.0 } else { now.duration_since(self.last_frame).as_secs_f32().min(0.05) };
        if let (Some(stats), false) = (&mut self.frame_stats, self.idle) {
            stats.push(now.duration_since(self.last_frame).as_secs_f32() * 1000.0);
        }
        self.idle = false;
        self.last_frame = now;
        // Keep an auto-hiding Dock up while its right-click menu is open.
        self.dock.set_held(&self.cx, self.menu.visible && self.menu.owner_tag == crate::ui::dock::MENU_TAG_DOCK);
        self.dock.frame(&mut self.cx, dt);
        self.menu.frame(&mut self.cx, dt);
        self.launchpad.frame(&mut self.cx, dt);
        self.spotlight.frame(&mut self.cx, dt);
        self.minfx.frame(&self.cx);
        // A menu (e.g. right-click on a Dock icon) wins over the window
        // previews, which would otherwise pop up on top of it.
        let hovered = if self.menu.visible {
            self.preview.close(&self.cx);
            None
        } else {
            self.dock.hovered_app()
        };
        self.preview.track(&mut self.cx, hovered);
        self.preview.frame(&mut self.cx, dt);
        if let Some(t) = &mut self.traffic {
            t.frame(&mut self.cx);
        }
        if let Some(bar) = &mut self.menubar {
            bar.frame(&mut self.cx, &self.menu, dt);
        }
        if self.settings.needs_frame() {
            let extra = Extra {
                tray_apps: self.menubar.as_ref().map(|b| b.tray.iter().map(|t| (t.exe.clone(), t.tip.clone())).collect()).unwrap_or_default(),
                hotkey_ok: self.hotkey_ok,
            };
            self.settings.frame(&mut self.cx, &extra);
        }
        self.run_cmds();
        if self.frame_stats.is_some() {
            self.work_stats.push(now.elapsed().as_secs_f32() * 1000.0);
        }
    }

    /// Log a summary of the animation that just ended (`--frame-stats`).
    fn flush_frame_stats(&mut self) {
        let Some(stats) = &mut self.frame_stats else { return };
        if stats.len() < 5 {
            stats.clear();
            return;
        }
        let n = stats.len() as f32;
        let avg = stats.iter().sum::<f32>() / n;
        let max = stats.iter().cloned().fold(0.0, f32::max);
        let long = stats.iter().filter(|&&d| d > avg * 1.5 + 2.0).count();
        let work = self.work_stats.iter().sum::<f32>() / self.work_stats.len().max(1) as f32;
        util::log(&format!("frames: {} avg {:.2} ms ({:.0} fps) max {:.2} ms, {} late; drawing {:.2} ms/frame", stats.len(), avg, 1000.0 / avg, max, long, work));
        stats.clear();
        self.work_stats.clear();
    }

    /// Hide the Dock and menu bar only while a fullscreen app (video, game,
    /// presentation) is in front; show them again as soon as it is not.
    fn update_visibility(&mut self) {
        // Only the display with the fullscreen app loses its bar / Dock.
        let fs = if self.launchpad.visible { None } else { fullscreen_monitor(&self.cx) };
        let show = fs.is_none_or(|m| m != self.dock.monitor());
        self.dock.set_visible(show);
        if show {
            // Other always-on-top windows may have been raised over the Dock.
            self.dock.raise();
        }
        if let Some(bar) = &mut self.menubar {
            bar.set_visible(fs);
        }
    }

    fn start_timers(&mut self) {
        let main = self.cx.main;
        unsafe {
            let _ = PostMessageW(Some(main), WM_TIMER, WPARAM(TIMER_CLOCK), LPARAM(0));
            SetTimer(Some(main), TIMER_SNAP, 3000, None);
            if self.wallpaper.is_some() {
                SetTimer(Some(main), TIMER_WALLPAPER, wallpaper::ACTIVE_MS, None);
            }
        }
    }

    fn register_spotlight(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.cx.main), HOTKEY_SPOTLIGHT);
        }
        self.hotkey_ok = false;
        if !self.cx.cfg.spotlight.enabled {
            return;
        }
        if let Some((mods, key)) = parse_hotkey(&self.cx.cfg.spotlight.hotkey) {
            self.hotkey_ok = unsafe { RegisterHotKey(Some(self.cx.main), HOTKEY_SPOTLIGHT, mods | MOD_NOREPEAT, key).is_ok() };
            if !self.hotkey_ok {
                util::log(&format!("spotlight hotkey {} is already taken by another program", self.cx.cfg.spotlight.hotkey));
            }
        }
    }

    /// Save the configuration and rebuild the part that changed.
    fn apply_config(&mut self, sec: Section) {
        self.cx.cfg.save();
        let all = sec == Section::All;
        if all || sec == Section::Dock {
            self.menu.close(&self.cx);
            self.dock.shutdown();
            match Dock::new(&mut self.cx) {
                Ok(d) => self.dock = d,
                Err(e) => util::log(&format!("dock: {e}")),
            }
        }
        if all || sec == Section::MenuBar {
            if let Some(bar) = self.menubar.take() {
                bar.shutdown();
            }
            if self.cx.cfg.menubar.enabled {
                self.menubar = MenuBar::new(&mut self.cx).ok();
                unsafe {
                    let _ = PostMessageW(Some(self.cx.main), WM_TIMER, WPARAM(TIMER_CLOCK), LPARAM(0));
                }
            }
        }
        if sec == Section::TrayFilter {
            if let Some(bar) = &mut self.menubar {
                bar.status_changed();
            }
        }
        if all || sec == Section::Spotlight {
            self.register_spotlight();
        }
        if all || sec == Section::Traffic {
            if let Some(mut t) = self.traffic.take() {
                t.shutdown();
            }
            if self.cx.cfg.traffic_lights.enabled {
                self.traffic = Traffic::new(&self.cx).ok();
                if let Some(t) = &mut self.traffic {
                    t.update(&self.cx);
                }
            }
        }
        if all || sec == Section::Wallpaper {
            if let Some(wp) = self.wallpaper.take() {
                wp.shutdown();
            }
            self.wallpaper = Wallpaper::new(&self.cx);
            if self.wallpaper.is_some() {
                unsafe {
                    SetTimer(Some(self.cx.main), TIMER_WALLPAPER, wallpaper::ACTIVE_MS, None);
                }
            }
        }
        self.settings.refresh();
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd == self.cx.main {
            return self.handle_main(msg, wp, lp);
        }
        if msg == WM_APPBAR {
            // Windows' fullscreen notifications are unreliable (the "ended" one is
            // often never sent), so they only trigger our own check.
            // ABN_POSCHANGED (1): the shell re-laid out the screen edges.
            if wp.0 as u32 == ABN_FULLSCREENAPP || wp.0 == 1 {
                unsafe {
                    SetTimer(Some(self.cx.main), TIMER_REFRESH, 40, None);
                }
            }
            return Some(LRESULT(0));
        }
        if let Some(r) = self.settings.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        if let Some(r) = self.preview.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        if let Some(r) = self.menu.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        if let Some(t) = &mut self.traffic {
            if let Some(r) = t.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
                return Some(r);
            }
        }
        if let Some(r) = self.launchpad.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        if let Some(r) = self.spotlight.on_msg(&mut self.cx, hwnd, msg, wp, lp) {
            return Some(r);
        }
        if let Some(bar) = &mut self.menubar {
            if let Some(r) = bar.on_msg(&mut self.cx, &mut self.menu, hwnd, msg, wp, lp) {
                return Some(r);
            }
        }
        self.dock.on_msg(&mut self.cx, &mut self.menu, hwnd, msg, wp, lp)
    }

    fn handle_main(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        // Shell hook: only window lifetime / activation matter (1 created, 2 destroyed,
        // 4 activated, 13 replaced, 0x8004 activated while fullscreen). Title redraws
        // and taskbar flashes arrive constantly and are ignored.
        if msg == self.shellhook_msg && !matches!(wp.0, 1 | 2 | 4 | 13 | 0x8004) {
            return Some(LRESULT(0));
        }
        if msg == self.shellhook_msg || msg == WM_APP_REFRESH {
            unsafe {
                SetTimer(Some(self.cx.main), TIMER_REFRESH, 40, None);
            }
            return Some(LRESULT(0));
        }
        if msg == self.taskbar_created_msg {
            // Our own broadcast (asking apps to re-add tray icons) is not news.
            if wp.0 != trayhost::BROADCAST_TAG {
                // Explorer restarted.
                taskbar::hide();
                taskbar::watch();
                if let Some(wall) = &self.wallpaper {
                    wall.reattach(&self.cx);
                }
                unsafe {
                    SetTimer(Some(self.cx.main), TIMER_TRAY, 1500, None);
                }
            }
            return Some(LRESULT(0));
        }
        match msg {
            WM_TIMER if wp.0 == TIMER_REFRESH => {
                unsafe {
                    let _ = KillTimer(Some(self.cx.main), TIMER_REFRESH);
                }
                let prev_fg = self.cx.tracker.fg;
                self.cx.tracker.refresh();
                if self.cx.animate && self.cx.tracker.fg != prev_fg {
                    // A new window is in front: picture it for its minimize effect,
                    // once the switch has settled (a picture makes the app redraw,
                    // which would stutter its activation).
                    unsafe {
                        SetTimer(Some(self.cx.main), TIMER_SNAP_FG, 900, None);
                    }
                }
                taskbar::rehide();
                if let Some(bar) = &mut self.menubar {
                    if bar.ensure_reserved() {
                        util::log("menu bar space was lost; reserved it again");
                    }
                }
                self.update_visibility();
                trayhost::ensure_first();
                self.dock.sync(&mut self.cx, true);
                if let Some(bar) = &mut self.menubar {
                    bar.update_app(&self.cx);
                }
                if let Some(t) = &mut self.traffic {
                    t.update(&self.cx);
                }
            }
            WM_TIMER if wp.0 == TIMER_SNAP_FG => {
                unsafe {
                    let _ = KillTimer(Some(self.cx.main), TIMER_SNAP_FG);
                }
                if user_idle_ms() < 300 {
                    // Still clicking or typing: try again a little later.
                    unsafe {
                        SetTimer(Some(self.cx.main), TIMER_SNAP_FG, 600, None);
                    }
                } else if self.cx.animate {
                    minfx::prefetch(self.cx.tracker.fg, &[], self.cx.s);
                }
            }
            WM_TIMER if wp.0 == TIMER_SNAP => {
                // Skipped while the user is busy, so typing never stutters.
                if self.cx.animate && user_idle_ms() > 1000 {
                    // The window in front, plus other visible windows whose picture is old.
                    let others: Vec<HWND> = self.cx.tracker.wins.iter().map(|w| w.hwnd).take(8).collect();
                    minfx::prefetch(self.cx.tracker.fg, &others, self.cx.s);
                }
            }
            WM_DISPLAYCHANGE => unsafe {
                // Displays added, removed or rearranged; changes come in bursts.
                SetTimer(Some(self.cx.main), TIMER_DISPLAY, 800, None);
            },
            WM_TIMER if wp.0 == TIMER_DISPLAY => {
                unsafe {
                    let _ = KillTimer(Some(self.cx.main), TIMER_DISPLAY);
                    self.cx.sw = GetSystemMetrics(SM_CXSCREEN);
                    self.cx.sh = GetSystemMetrics(SM_CYSCREEN);
                }
                self.launchpad.close(&mut self.cx);
                self.apply_config(Section::All);
                self.update_visibility();
                taskbar::rehide();
            }
            WM_TIMER if wp.0 == TIMER_TRAY => unsafe {
                let _ = KillTimer(Some(self.cx.main), TIMER_TRAY);
                trayhost::raise_and_announce();
            },
            WM_TIMER if wp.0 == TIMER_WALLPAPER => {
                if let Some(wall) = &mut self.wallpaper {
                    let next = wall.tick(&self.cx, status::on_battery());
                    self.cx.wallpaper_time = wall.time();
                    unsafe {
                        SetTimer(Some(self.cx.main), TIMER_WALLPAPER, next, None);
                    }
                } else {
                    unsafe {
                        let _ = KillTimer(Some(self.cx.main), TIMER_WALLPAPER);
                    }
                }
            }
            WM_WTSSESSION_CHANGE => {
                if let Some(wall) = &mut self.wallpaper {
                    match wp.0 as u32 {
                        WTS_SESSION_LOCK => wall.locked = true,
                        WTS_SESSION_UNLOCK => wall.locked = false,
                        _ => {}
                    }
                }
            }
            WM_TIMER if wp.0 == TIMER_TRIM => {
                // Startup and Launchpad touch a lot of memory that is never needed again;
                // hand those pages back to Windows.
                unsafe {
                    let _ = KillTimer(Some(self.cx.main), TIMER_TRIM);
                    let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
                }
            }
            WM_TIMER if wp.0 == TIMER_CLOCK => {
                if let Some(bar) = &mut self.menubar {
                    let next = bar.tick_clock(&self.cx);
                    unsafe {
                        SetTimer(Some(self.cx.main), TIMER_CLOCK, next, None);
                    }
                }
            }
            WM_APP_STATUS | WM_POWERBROADCAST => {
                if let Some(bar) = &mut self.menubar {
                    bar.status_changed();
                }
            }
            WM_APP_SYS => {
                let ev = *unsafe { Box::from_raw(lp.0 as *mut SysEvent) };
                if let Some(bar) = &mut self.menubar {
                    bar.sys_event(ev);
                }
            }
            WM_APP_TRAY => {
                let msgs = trayhost::take_messages();
                let changed = self.menubar.as_mut().is_some_and(|bar| bar.tray_messages(&self.cx, msgs));
                if changed {
                    self.settings.refresh();
                }
            }
            WM_APP_PICKED => {
                let (purpose, path) = *unsafe { Box::from_raw(lp.0 as *mut (u32, String)) };
                self.settings.picked(&mut self.cx, purpose, path);
            }
            WM_HOTKEY if wp.0 as i32 == HOTKEY_SPOTLIGHT => self.spotlight.toggle(&mut self.cx),
            WM_HOTKEY if wp.0 as i32 == HOTKEY_QUIT => unsafe { PostQuitMessage(0) },
            WM_HOTKEY if (crate::ui::menu::HOTKEY_MENU_FIRST..crate::ui::menu::HOTKEY_MENU_FIRST + 4).contains(&(wp.0 as i32)) => {
                self.menu.on_key(&mut self.cx, wp.0 as i32)
            }
            WM_APP_APPS => {
                self.cx.apps = *unsafe { Box::from_raw(lp.0 as *mut Vec<AppEntry>) };
                self.cx.apps_scanned = Some(Instant::now());
                self.cx.apps_scanning = false;
                self.launchpad.apps_changed(&mut self.cx);
                self.spotlight.apps_changed(&self.cx);
            }
            WM_APP_ICON => {
                let icon = unsafe { Box::from_raw(lp.0 as *mut LoadedIcon) };
                let bmp = if icon.pixels.is_empty() { None } else { self.cx.gfx.create_bitmap(icon.w, icon.h, &icon.pixels) };
                let (consumer, generation, index) = icons::split_icon_id(icon.id);
                match consumer {
                    crate::ui::launchpad::ICON_CONSUMER => self.launchpad.icon_ready(generation, index, bmp),
                    crate::ui::spotlight::ICON_CONSUMER => self.spotlight.icon_ready(generation, index, bmp),
                    _ => {}
                }
            }
            WM_APP_MINSTART => {
                let hwnd = HWND(wp.0 as *mut _);
                let dest = self.cx.tracker.wins.iter().find(|w| w.hwnd == hwnd).and_then(|w| self.dock.icon_center(&self.cx, &w.key));
                if dest.is_some() {
                    self.dock.peek();
                }
                self.minfx.minimized(&self.cx, hwnd, dest);
            }
            WM_APP_TRAFFIC => {
                if let Some(t) = &mut self.traffic {
                    t.reposition(&self.cx);
                }
            }
            WM_APP_REMOTE => match wp.0 {
                1 => self.cx.cmds.push(Cmd::Launchpad),
                2 => self.spotlight.toggle(&mut self.cx),
                3 => unsafe { PostQuitMessage(0) },
                4 => self.cx.cmds.push(Cmd::Minimize(self.cx.tracker.fg)),
                5 => self.cx.cmds.push(Cmd::OpenSettings),
                6 => self.cx.cmds.push(Cmd::ReloadConfig),
                _ => {}
            },
            WM_APP_OUTSIDE => match wp.0 {
                outside::SLOT_MENU => self.menu.close(&self.cx),
                _ => {
                    if let Some(bar) = &mut self.menubar {
                        bar.popover.close(&self.cx);
                    }
                }
            },
            WM_SETTINGCHANGE => {
                let dark = system_dark();
                self.cx.animate = anim::system_animations_enabled();
                if dark != self.cx.dark {
                    self.cx.dark = dark;
                    self.dock.theme_changed(&self.cx);
                    self.menu.theme_changed(&self.cx);
                    self.settings.theme_changed(&self.cx);
                    self.preview.theme_changed(&self.cx);
                    if let Some(bar) = &mut self.menubar {
                        bar.theme_changed(&self.cx);
                    }
                }
                return None;
            }
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    SESSION_ENDING.store(true, Ordering::Relaxed);
                    if !crate::win::autostart::enabled() {
                        taskbar::restore();
                    }
                }
                return None;
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn run_cmds(&mut self) {
        while !self.cx.cmds.is_empty() {
            let cmds = std::mem::take(&mut self.cx.cmds);
            for cmd in cmds {
                self.run_cmd(cmd);
            }
        }
    }

    fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::QuitSelf => unsafe { PostQuitMessage(0) },
            Cmd::OpenConfig => util::shell_open(&config::path().to_string_lossy()),
            Cmd::ReloadConfig => {
                self.cx.cfg = Config::load();
                self.apply_config(Section::All);
            }
            Cmd::OpenSettings => self.settings.show(&mut self.cx),
            Cmd::CloseSettings => self.settings.hide(),
            Cmd::ConfigChanged(sec) => self.apply_config(sec),
            // Always starts the target: for a running app that usually opens a new window.
            Cmd::Launch(target) => util::shell_open(&target),
            Cmd::Open(target) => {
                // Like macOS: an app that is already running comes forward instead.
                self.cx.tracker.refresh();
                let wins = self.cx.tracker.windows_for_launch(&target);
                if wins.is_empty() {
                    util::shell_open(&target);
                } else {
                    tracker::bring_all_to_front(&wins);
                }
            }
            Cmd::Pin(launch) => {
                self.dock.set_pinned(&mut self.cx, &launch, true);
                self.dock.rebuild(&mut self.cx);
                self.settings.refresh();
            }
            Cmd::Unpin(launch) => {
                self.dock.set_pinned(&mut self.cx, &launch, false);
                self.settings.refresh();
            }
            Cmd::CloseApp(key) => {
                for w in self.cx.tracker.windows_of(&key) {
                    unsafe {
                        let _ = PostMessageW(Some(w.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            Cmd::Minimize(hwnd) => {
                let dest = self.cx.tracker.wins.iter().find(|w| w.hwnd == hwnd).and_then(|w| self.dock.icon_center(&self.cx, &w.key));
                if dest.is_some() {
                    self.dock.peek();
                }
                self.minfx.minimize(&self.cx, hwnd, dest);
            }
            Cmd::MinimizeAll(front, wins) => {
                for &h in wins.iter().filter(|&&h| h != front) {
                    self.minfx.minimize_quietly(h);
                }
                let dest = self.cx.tracker.wins.iter().find(|w| w.hwnd == front).and_then(|w| self.dock.icon_center(&self.cx, &w.key));
                if dest.is_some() {
                    self.dock.peek();
                }
                self.minfx.minimize(&self.cx, front, dest);
            }
            Cmd::Activate(hwnd) => tracker::activate(hwnd),
            Cmd::BringAllToFront(key) => {
                let wins: Vec<HWND> = self.cx.tracker.windows_of(&key).map(|w| w.hwnd).collect();
                tracker::bring_all_to_front(&wins);
            }
            Cmd::ToggleDockHiding => {
                self.cx.cfg.dock.autohide = !self.cx.cfg.dock.autohide;
                self.apply_config(Section::Dock);
            }
            Cmd::AppMenu(launch, x, y) => {
                let pinned = self.cx.cfg.dock.pinned.iter().any(|p| p.eq_ignore_ascii_case(&launch));
                let items = vec![
                    MenuItem::new("Open", Cmd::Launch(launch.clone())),
                    MenuItem::sep(),
                    if pinned { MenuItem::new("Remove from Dock", Cmd::Unpin(launch.clone())) } else { MenuItem::new("Add to Dock", Cmd::Pin(launch.clone())) },
                    MenuItem::new("Remove from Launchpad", Cmd::ToggleLaunchpad(launch)),
                ];
                self.menu.show(&mut self.cx, items, x, y, Anchor::At, MENU_TAG_APP);
            }
            Cmd::ToggleLaunchpad(launch) => self.launchpad.toggle_app(&mut self.cx, &launch),
            Cmd::EmptyTrash => unsafe {
                let _ = SHEmptyRecycleBinW(None, PCWSTR::null(), 0);
            },
            Cmd::Launchpad => {
                self.launchpad.toggle(&mut self.cx);
                unsafe {
                    SetTimer(Some(self.cx.main), TIMER_TRIM, 8000, None);
                }
                // Like macOS, the Dock stays usable on top of Launchpad.
                self.dock.raise();
            }
            Cmd::Spotlight => self.spotlight.toggle(&mut self.cx),
            Cmd::Power(action) => crate::win::power::run(action),
            Cmd::Lock => unsafe {
                let _ = LockWorkStation();
            },
            Cmd::HideApp(key) => {
                for w in self.cx.tracker.windows_of(&key) {
                    unsafe {
                        let _ = ShowWindow(w.hwnd, SW_MINIMIZE);
                    }
                }
            }
            Cmd::ZoomFg => unsafe {
                let fg = self.cx.tracker.fg;
                let sc = if IsZoomed(fg).as_bool() { SC_RESTORE } else { SC_MAXIMIZE };
                let _ = PostMessageW(Some(fg), WM_SYSCOMMAND, WPARAM(sc as usize), LPARAM(0));
            },
            Cmd::CloseFg => unsafe {
                let _ = PostMessageW(Some(self.cx.tracker.fg), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
        }
    }
}

/// Parse "alt+space", "ctrl+shift+k", ... into RegisterHotKey arguments.
fn parse_hotkey(text: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut key = None;
    for part in text.split('+').map(|p| p.trim().to_lowercase()) {
        match part.as_str() {
            "alt" => mods |= MOD_ALT,
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            "space" => key = Some(0x20),
            k if k.len() == 1 => key = k.chars().next().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase() as u32),
            _ => return None,
        }
    }
    key.filter(|_| mods.0 != 0).map(|k| (mods, k))
}

/// Forward a command-line switch to the running instance. Returns false if
/// the switch is unknown or nothing is running.
pub fn remote(switch: &str) -> bool {
    let code = match switch {
        "--launchpad" => 1,
        "--spotlight" => 2,
        "--quit" => 3,
        "--minimize" => 4,
        "--settings" => 5,
        "--reload" => 6,
        _ => return false,
    };
    unsafe {
        match FindWindowW(CLASS, MAIN_TITLE) {
            Ok(h) if !h.is_invalid() => {
                // We were started by the user, so we may hand our right to
                // bring a window to the front over to the running instance.
                let _ = AllowSetForegroundWindow(tracker::window_pid(h));
                PostMessageW(Some(h), WM_APP_REMOTE, WPARAM(code), LPARAM(0)).is_ok()
            }
            _ => false,
        }
    }
}

/// The display on which a fullscreen window (not ours, not the desktop) is in front.
fn fullscreen_monitor(_cx: &Ctx) -> Option<RECT> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || tracker::window_pid(fg) == std::process::id() {
            return None;
        }
        let mut class = [0u16; 32];
        let n = GetClassNameW(fg, &mut class) as usize;
        if matches!(String::from_utf16_lossy(&class[..n]).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(fg, &mut r).ok()?;
        // A maximized window with a resize frame is never fullscreen, even when
        // it covers the whole display (e.g. if the reserved space was lost).
        // Fullscreen video/games drop the frame.
        let style = GetWindowLongPtrW(fg, GWL_STYLE) as u32;
        if IsZoomed(fg).as_bool() && style & WS_THICKFRAME.0 != 0 {
            return None;
        }
        let m = crate::win::monitors::of_window(fg);
        (r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom).then_some(m)
    }
}
