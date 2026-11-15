//! Places a window behind the desktop icons, where the wallpaper lives.

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn child(parent: HWND, class: PCWSTR) -> Option<HWND> {
    unsafe { FindWindowExW(Some(parent), None, class, PCWSTR::null()).ok().filter(|h| !h.is_invalid()) }
}

unsafe extern "system" fn find_icons_host(hwnd: HWND, lp: LPARAM) -> BOOL {
    if child(hwnd, w!("SHELLDLL_DefView")).is_some() {
        *(lp.0 as *mut HWND) = hwnd;
        return FALSE;
    }
    TRUE
}

/// Re-parent `hwnd` into the desktop, below the icons. Returns false if the
/// shell's desktop windows could not be found.
pub fn attach(hwnd: HWND, w: i32, h: i32) -> bool {
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) else { return false };
        // Undocumented but long-standing: asks Progman to split the wallpaper
        // layer (a WorkerW) from the icon layer.
        let mut result = 0usize;
        SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(0x1), SMTO_NORMAL, 1000, Some(&mut result));

        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & !WS_POPUP.0) | WS_CHILD.0;
        let show = SWP_NOACTIVATE | SWP_SHOWWINDOW;

        // Windows 11 24H2 and later: icons (DefView) and the wallpaper WorkerW
        // are both children of Progman; we slot in between them.
        if let (Some(defview), Some(workerw)) = (child(progman, w!("SHELLDLL_DefView")), child(progman, w!("WorkerW"))) {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
            // Children of this Progman only render when layered.
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) | WS_EX_LAYERED.0 as isize;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
            if SetParent(hwnd, Some(progman)).is_err() {
                return false;
            }
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(hwnd, Some(defview), 0, 0, w, h, show);
            let _ = SetWindowPos(workerw, Some(hwnd), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            return true;
        }

        // Older layout: the icons live in a top-level WorkerW (or Progman)
        // and the next WorkerW sibling behind it is the wallpaper layer.
        let mut icons_host = HWND::default();
        let _ = EnumWindows(Some(find_icons_host), LPARAM(&mut icons_host as *mut _ as isize));
        if icons_host.is_invalid() {
            return false;
        }
        let Ok(workerw) = FindWindowExW(None, Some(icons_host), w!("WorkerW"), PCWSTR::null()) else { return false };
        if workerw.is_invalid() {
            return false;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        if SetParent(hwnd, Some(workerw)).is_err() {
            return false;
        }
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, show | SWP_NOZORDER);
        true
    }
}

/// Windows re-stacks the desktop layers now and then (e.g. on a wallpaper
/// slideshow change). Put `hwnd` back directly beneath the icons if needed.
pub fn keep_order(hwnd: HWND) {
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) else { return };
        let Some(defview) = child(progman, w!("SHELLDLL_DefView")) else { return };
        if GetParent(hwnd).ok() != Some(progman) {
            return; // older layout: we are the only child of our WorkerW
        }
        if GetWindow(hwnd, GW_HWNDPREV).ok() != Some(defview) {
            let _ = SetWindowPos(hwnd, Some(defview), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}
