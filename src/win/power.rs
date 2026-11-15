//! Sleep, hibernate, restart, shut down and sign out.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY};
use windows::Win32::System::Power::{GetPwrCapabilities, SetSuspendState, SYSTEM_POWER_CAPABILITIES};
use windows::Win32::System::Shutdown::{ExitWindowsEx, EWX_LOGOFF, EWX_POWEROFF, EWX_REBOOT, EXIT_WINDOWS_FLAGS, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO};

use crate::util::wide;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Sleep,
    Hibernate,
    Restart,
    ShutDown,
    LogOut,
}

/// Whether this PC can hibernate (enabled in power options, with a hibernation file).
pub fn can_hibernate() -> bool {
    let mut caps = SYSTEM_POWER_CAPABILITIES::default();
    unsafe { GetPwrCapabilities(&mut caps) && caps.SystemS4 && caps.HiberFilePresent }
}

fn enable_shutdown_privilege() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return;
        }
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_SHUTDOWN_NAME, &mut luid).is_ok() {
            let mut tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, ..Default::default() };
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
            let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

/// Carry out `action`. Restart, shut down and log out ask first (on their
/// own thread, so the Dock keeps working while the question is open).
pub fn run(action: Action) {
    match action {
        Action::Sleep => unsafe {
            let _ = SetSuspendState(false, false, false);
        },
        Action::Hibernate => unsafe {
            let _ = SetSuspendState(true, false, false);
        },
        _ => {
            std::thread::spawn(move || {
                let (question, flags): (&str, EXIT_WINDOWS_FLAGS) = match action {
                    Action::Restart => ("Are you sure you want to restart your computer now?", EWX_REBOOT),
                    Action::ShutDown => ("Are you sure you want to shut down your computer now?", EWX_POWEROFF),
                    _ => ("Are you sure you want to quit all applications and log out now?", EWX_LOGOFF),
                };
                let text = wide(question);
                let answer = unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), w!("WinMacDock"), MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND) };
                if answer == IDYES {
                    enable_shutdown_privilege();
                    unsafe {
                        let _ = ExitWindowsEx(flags, SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED);
                    }
                }
            });
        }
    }
}
