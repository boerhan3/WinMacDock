//! AppBar registration: reserves screen-edge space so maximized windows
//! stay clear of the menu bar / Dock, and reports fullscreen apps.

use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Callback message; `wParam` is an `ABN_*` code.
pub const WM_APPBAR: u32 = WM_APP + 3;

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: hwnd, uCallbackMessage: WM_APPBAR, ..Default::default() }
}

/// Reserve `rc` (screen pixels) on `edge` (`ABE_TOP` / `ABE_BOTTOM`).
pub fn register(hwnd: HWND, edge: u32, rc: RECT) {
    unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_NEW, &mut abd);
        abd.uEdge = edge;
        abd.rc = rc;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
    }
}

/// The of the primary display's work area.
pub fn work_area() -> RECT {
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut r = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    r
}

pub fn unregister(hwnd: HWND) {
    unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    }
}
