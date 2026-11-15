//! Connected displays, in screen pixels (primary first).

use windows::core::BOOL;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn collect(mon: HMONITOR, _hdc: HDC, _r: *mut RECT, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<(RECT, bool)>);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
        out.push((mi.rcMonitor, mi.dwFlags & MONITORINFOF_PRIMARY != 0));
    }
    TRUE
}

/// Every display's rectangle; the primary one comes first.
pub fn list() -> Vec<RECT> {
    let mut found: Vec<(RECT, bool)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut found as *mut _ as isize));
    }
    found.sort_by_key(|m| !m.1);
    let mut out: Vec<RECT> = found.into_iter().map(|m| m.0).collect();
    if out.is_empty() {
        unsafe {
            out.push(RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) });
        }
    }
    out
}

/// The bounding box of all displays.
pub fn virtual_rect() -> RECT {
    unsafe {
        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        RECT { left: x, top: y, right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN), bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN) }
    }
}

/// The display containing (or nearest to) a screen point.
pub fn at(x: i32, y: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            return mi.rcMonitor;
        }
        RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }
    }
}

/// The display a window is mostly on.
pub fn of_window(hwnd: HWND) -> RECT {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            return mi.rcMonitor;
        }
        RECT::default()
    }
}

/// Clamp a `w`×`h` box at (x, y) inside the display that holds (x, y).
pub fn clamp_box(x: i32, y: i32, w: i32, h: i32, margin: i32) -> (i32, i32) {
    let m = at(x, y);
    (x.clamp(m.left + margin, (m.right - w - margin).max(m.left + margin)), y.clamp(m.top + margin, (m.bottom - h - margin).max(m.top + margin)))
}
