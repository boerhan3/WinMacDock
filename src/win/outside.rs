//! Reports mouse clicks that land outside our open popups (menus, panels),
//! which do not take focus and so never get a deactivation message.
//!
//! While a popup is open, a small thread checks the mouse buttons every
//! 25 ms. (A system-wide mouse hook would also work, but security software
//! treats global input hooks as suspicious.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app;

/// Posted to the main window; `wParam` is the slot that was clicked outside of.
pub const WM_APP_OUTSIDE: u32 = WM_APP + 2;

pub const SLOT_MENU: usize = 0;
pub const SLOT_POPOVER: usize = 1;

static RUNNING: AtomicBool = AtomicBool::new(false);
static RECTS: Mutex<[Option<RECT>; 2]> = Mutex::new([None, None]);

/// Start reporting clicks outside `rect` (screen pixels) for `slot`.
pub fn watch(slot: usize, rect: RECT, _hinst: HINSTANCE) {
    if let Ok(mut r) = RECTS.lock() {
        r[slot] = Some(rect);
    }
    if !RUNNING.swap(true, Ordering::AcqRel) {
        let main = app::main_hwnd().0 as isize;
        std::thread::spawn(move || poll(main));
    }
}

pub fn unwatch(slot: usize) {
    if let Ok(mut r) = RECTS.lock() {
        r[slot] = None;
    }
}

fn button_down() -> bool {
    // Bit 15: held now; bit 0: pressed since the last check (catches quick clicks).
    unsafe { [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|&b| GetAsyncKeyState(b.0 as i32) as u16 & 0x8001 != 0) }
}

fn poll(main: isize) {
    let mut was_down = button_down();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(25));
        let rects = match RECTS.lock() {
            Ok(r) => *r,
            Err(_) => [None, None],
        };
        if rects.iter().all(|r| r.is_none()) {
            RUNNING.store(false, Ordering::Release);
            // A popup may have opened again just now; keep watching for it.
            if RECTS.lock().map(|r| r.iter().all(|x| x.is_none())).unwrap_or(true) || RUNNING.swap(true, Ordering::AcqRel) {
                return;
            }
            continue;
        }
        let down = button_down();
        if down && !was_down {
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
            }
            for (slot, r) in rects.iter().enumerate() {
                if let Some(r) = r {
                    if !(pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom) {
                        unsafe {
                            let _ = PostMessageW(Some(HWND(main as *mut _)), WM_APP_OUTSIDE, WPARAM(slot), LPARAM(0));
                        }
                    }
                }
            }
        }
        was_down = down;
    }
}
