//! Tracks the top-level application windows and which one is in front.

use std::collections::HashMap;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util::from_wide;

/// Identity of an application: explicit AppUserModelID and/or executable
/// path, both lowercase. Two keys match when either field agrees.
#[derive(Clone, Default, Debug)]
pub struct AppKey {
    pub aumid: Option<String>,
    pub exe: Option<String>,
}

impl AppKey {
    pub fn matches(&self, other: &AppKey) -> bool {
        matches!((&self.aumid, &other.aumid), (Some(a), Some(b)) if a == b)
            || matches!((&self.exe, &other.exe), (Some(a), Some(b)) if a == b)
    }

    /// Something the shell can launch or load an icon from.
    pub fn launch_target(&self) -> String {
        match (&self.exe, &self.aumid) {
            // Packaged apps: the AppsFolder entry has the proper name and a high-res icon.
            (_, Some(id)) if id.contains('!') => format!("shell:AppsFolder\\{id}"),
            (Some(exe), _) => exe.clone(),
            (None, Some(id)) => format!("shell:AppsFolder\\{id}"),
            _ => String::new(),
        }
    }
}

pub struct Win {
    pub hwnd: HWND,
    pub key: AppKey,
}

pub struct Tracker {
    /// Application windows in z-order, topmost first.
    pub wins: Vec<Win>,
    /// Last foreground window that is not ours.
    pub fg: HWND,
    own_pid: u32,
    cache: HashMap<isize, AppKey>,
}

impl Tracker {
    pub fn new() -> Self {
        Self { wins: Vec::new(), fg: HWND::default(), own_pid: unsafe { GetCurrentProcessId() }, cache: HashMap::new() }
    }

    pub fn refresh(&mut self) {
        let mut hwnds: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM(&mut hwnds as *mut _ as isize));
        }
        let mut wins = Vec::new();
        let mut cache = HashMap::new();
        for h in hwnds {
            if !self.is_app_window(h) {
                continue;
            }
            let id = h.0 as isize;
            let key = self.cache.get(&id).cloned().unwrap_or_else(|| window_key(h));
            cache.insert(id, key.clone());
            wins.push(Win { hwnd: h, key });
        }
        self.wins = wins;
        self.cache = cache;

        let fg = unsafe { GetForegroundWindow() };
        if !fg.is_invalid() && window_pid(fg) != self.own_pid {
            self.fg = fg;
        }
    }

    fn is_app_window(&self, h: HWND) -> bool {
        unsafe {
            if !IsWindowVisible(h).as_bool() {
                return false;
            }
            if GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
                return false;
            }
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
                return false;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
            if cloaked != 0 {
                return false;
            }
            if GetWindowTextLengthW(h) == 0 {
                return false;
            }
            if window_pid(h) == self.own_pid {
                return false;
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(h, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            !matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Windows.UI.Core.CoreWindow")
        }
    }

    pub fn windows_of<'a>(&'a self, key: &'a AppKey) -> impl Iterator<Item = &'a Win> {
        self.wins.iter().filter(move |w| w.key.matches(key))
    }

    pub fn is_running(&self, key: &AppKey) -> bool {
        self.windows_of(key).next().is_some()
    }

    /// Key of the app owning the foreground window, if it is a tracked app.
    pub fn fg_key(&self) -> Option<&AppKey> {
        self.wins.iter().find(|w| w.hwnd == self.fg).map(|w| &w.key)
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lp: LPARAM) -> BOOL {
    let list = &mut *(lp.0 as *mut Vec<HWND>);
    list.push(hwnd);
    TRUE
}

pub fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    pid
}

/// Executable path and, for packaged (Store/MSIX) apps, the package AUMID.
pub fn process_info(pid: u32) -> (Option<String>, Option<String>) {
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return (None, None) };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let path = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
            .is_ok()
            .then(|| String::from_utf16_lossy(&buf[..len as usize]));
        let mut id = [0u16; 256];
        let mut id_len = id.len() as u32;
        let aumid = (GetApplicationUserModelId(proc, &mut id_len, Some(PWSTR(id.as_mut_ptr()))) == ERROR_SUCCESS)
            .then(|| from_wide(&id))
            .filter(|s| !s.is_empty());
        let _ = CloseHandle(proc);
        (path, aumid)
    }
}

fn explicit_aumid(h: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(h).ok()?;
        let pv = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let p = PropVariantToStringAlloc(&pv).ok()?;
        let s = from_wide(p.as_wide());
        CoTaskMemFree(Some(p.0 as *const _));
        (!s.is_empty()).then(|| s.to_lowercase())
    }
}

fn window_key(h: HWND) -> AppKey {
    let (exe, packaged) = process_info(window_pid(h));
    let exe = exe.map(|p| p.to_lowercase());
    // UWP windows are hosted by ApplicationFrameHost; only the window's AUMID identifies them.
    let hosted = exe.as_deref().map(|e| e.ends_with("applicationframehost.exe")).unwrap_or(false);
    let aumid = explicit_aumid(h).or(if hosted { None } else { packaged.map(|s| s.to_lowercase()) });
    AppKey { aumid, exe: if hosted { None } else { exe } }
}

/// Bring a window to the front, restoring it if minimized.
pub fn activate(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = SetForegroundWindow(h);
    }
}

/// Window title.
pub fn title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// Bring all of an app's windows forward, keeping their relative order.
pub fn bring_all_to_front(wins: &[HWND]) {
    let visible: Vec<HWND> = wins.iter().copied().filter(|&h| unsafe { !IsIconic(h).as_bool() }).collect();
    if visible.is_empty() {
        if let Some(&h) = wins.first() {
            activate(h);
        }
        return;
    }
    unsafe {
        // `wins` is topmost-first: raise from the back so the first ends on top.
        for &h in visible.iter().skip(1).rev() {
            let _ = SetWindowPos(h, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
    activate(visible[0]);
}
