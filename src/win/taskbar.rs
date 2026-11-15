//! Hide the Windows taskbar while we run and put it back afterwards.
//! The original auto-hide state is saved to disk so `--restore` can undo
//! everything even after a crash.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util;

fn state_file() -> std::path::PathBuf {
    util::data_dir().join("taskbar.state")
}

fn taskbars() -> Vec<HWND> {
    let mut out = Vec::new();
    unsafe {
        for class in [w!("Shell_TrayWnd"), w!("Shell_SecondaryTrayWnd")] {
            let mut prev: Option<HWND> = None;
            while let Ok(h) = FindWindowExW(None, prev, class, PCWSTR::null()) {
                if h.is_invalid() {
                    break;
                }
                // Our own tray host uses the same class name.
                if crate::win::tracker::window_pid(h) != std::process::id() {
                    out.push(h);
                }
                prev = Some(h);
            }
        }
    }
    out
}

fn appbar_state(set: Option<u32>) -> u32 {
    unsafe {
        let Some(tray) = crate::win::trayhost::explorer_tray() else { return 0 };
        let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: tray, ..Default::default() };
        match set {
            Some(state) => {
                abd.lParam = LPARAM(state as isize);
                SHAppBarMessage(ABM_SETSTATE, &mut abd);
                state
            }
            None => SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32,
        }
    }
}

pub fn hide() {
    // Keep the state from a previous crashed run; it is the real original.
    if !state_file().exists() {
        let _ = std::fs::write(state_file(), appbar_state(None).to_string());
    }
    // Auto-hide releases the work area the taskbar reserves.
    appbar_state(Some(ABS_AUTOHIDE));
    rehide();
}

/// Explorer occasionally re-shows the taskbar; call this to hide it again.
pub fn rehide() {
    unsafe {
        for h in taskbars() {
            if IsWindowVisible(h).as_bool() {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }
}

pub fn restore() {
    // Also undo our change to Windows' minimize animation (crash, --restore, exit).
    crate::ui::minfx::restore_system_animation();
    let saved = std::fs::read_to_string(state_file()).ok().and_then(|s| s.trim().parse::<u32>().ok());
    appbar_state(Some(saved.unwrap_or(0)));
    unsafe {
        for h in taskbars() {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
    }
    let _ = std::fs::remove_file(state_file());
}

static WATCH: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Explorer re-shows its taskbar when the pointer touches the screen edge
/// (auto-hide). Watch Explorer for that and hide it again at once.
pub fn watch() {
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    let Some(tray) = crate::win::trayhost::explorer_tray() else { return };
    let pid = crate::win::tracker::window_pid(tray);
    unsafe {
        let old = WATCH.swap(0, std::sync::atomic::Ordering::Relaxed);
        if old != 0 {
            let _ = UnhookWinEvent(HWINEVENTHOOK(old as *mut _));
        }
        let hook = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_show), pid, 0, WINEVENT_OUTOFCONTEXT);
        WATCH.store(hook.0 as isize, std::sync::atomic::Ordering::Relaxed);
    }
}

unsafe extern "system" fn on_show(_hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _child: i32, _thread: u32, _time: u32) {
    if id_object != 0 || hwnd.is_invalid() {
        return;
    }
    let mut class = [0u16; 32];
    let n = GetClassNameW(hwnd, &mut class) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if (class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd") && crate::win::tracker::window_pid(hwnd) != std::process::id() {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}
