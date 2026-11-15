#![windows_subsystem = "windows"]

mod app;
mod config;
mod util;
mod render {
    pub mod anim;
    pub mod gfx;
}
mod win {
    pub mod appbar;
    pub mod apps;
    pub mod autostart;
    pub mod desktop;
    pub mod glass;
    pub mod icons;
    pub mod monitors;
    pub mod outside;
    pub mod power;
    pub mod status;
    pub mod sysctl;
    pub mod taskbar;
    pub mod tracker;
    pub mod trayhost;
    pub mod wifi;
    pub mod bluetooth;
}
mod ui {
    pub mod backdrop;
    pub mod dock;
    pub mod launchpad;
    pub mod menu;
    pub mod menubar;
    pub mod minfx;
    pub mod popover;
    pub mod preview;
    pub mod settings;
    pub mod spotlight;
    pub mod tooltip;
    pub mod traffic;
    pub mod wallpaper;
    pub mod widgets;
}

fn main() {
    // Inherited from an editor's terminal (VS Code sets it), this makes every
    // Electron app we launch run as plain Node.js and exit at once.
    std::env::remove_var("ELECTRON_RUN_AS_NODE");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if let Some(i) = args.iter().position(|a| a == "--render-frames") {
        let a = |k: usize| args.get(i + k).cloned().unwrap_or_default();
        let num = |k: usize, d: f32| a(k).parse::<f32>().unwrap_or(d);
        let r = ui::wallpaper::render_frames(&a(1), &a(2), num(3, 1080.0) as u32, num(4, 1920.0) as u32, num(5, 30.0) as u32, num(6, 40.0), num(7, 0.2));
        if let Err(e) = r {
            util::log(&format!("render-frames: {e}"));
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--render-wallpapers") {
        let dir = args.get(i + 1).cloned().unwrap_or_else(|| ".".into());
        if let Err(e) = ui::wallpaper::render_all_to(&dir) {
            util::log(&format!("render-wallpapers: {e}"));
        }
        return;
    }

    // `--restore` puts the Windows taskbar back after a crash or forced kill.
    if has("--restore") {
        win::taskbar::restore();
        return;
    }
    // `--launchpad`, `--spotlight`, `--settings`, `--minimize`, `--quit`
    // control an instance that is already running.
    if let Some(switch) = args.iter().find(|a| matches!(a.as_str(), "--launchpad" | "--spotlight" | "--settings" | "--reload" | "--minimize" | "--quit")) {
        if app::remote(switch) || switch != "--settings" {
            return;
        }
        // `--settings` with nothing running: start normally and open Settings.
    }

    // Hard crashes (access violations etc.) must not leave the taskbar hidden.
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
        unsafe extern "system" fn on_crash(info: *const EXCEPTION_POINTERS) -> i32 {
            win::taskbar::restore();
            if let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
                util::log(&format!("crash: code {:#x} at {:p}", rec.ExceptionCode.0, rec.ExceptionAddress));
            }
            0 // EXCEPTION_CONTINUE_SEARCH: let Windows report it as usual
        }
        SetUnhandledExceptionFilter(Some(on_crash));
    }
    std::panic::set_hook(Box::new(|info| {
        win::taskbar::restore();
        util::log(&format!("panic: {info}"));
    }));
    let opts = app::RunOptions { open_settings: has("--settings"), frame_stats: has("--frame-stats") };
    if let Err(e) = app::run(opts) {
        win::taskbar::restore();
        util::log(&format!("fatal: {e}"));
    }
}
