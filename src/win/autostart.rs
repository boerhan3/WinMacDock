//! "Start at login" through the per-user Run key (the installer uses the same value).

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::*;

use crate::util::wide;

const KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("WinMacDock");

const SERIALIZE: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Serialize");

/// Windows waits several seconds after sign-in before starting startup apps.
/// True when that wait is turned off (for this user, for all startup apps).
pub fn fast_startup() -> bool {
    let mut v = 1u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, SERIALIZE, w!("StartupDelayInMSec"), RRF_RT_REG_DWORD, None, Some(&mut v as *mut _ as *mut _), Some(&mut size)).is_ok() && v == 0
    }
}

pub fn set_fast_startup(on: bool) {
    unsafe {
        if on {
            let zero = 0u32;
            for name in [w!("StartupDelayInMSec"), w!("WaitForIdleState")] {
                let _ = RegSetKeyValueW(HKEY_CURRENT_USER, SERIALIZE, name, REG_DWORD.0, Some(&zero as *const _ as *const _), 4);
            }
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, SERIALIZE, w!("StartupDelayInMSec"));
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, SERIALIZE, w!("WaitForIdleState"));
        }
    }
}

pub fn enabled() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, KEY, VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() }
}

pub fn set(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let data = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, KEY, VALUE, REG_SZ.0, Some(data.as_ptr() as *const _), (data.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, VALUE);
        }
    }
}
