//! Notification-area host. Apps add tray icons by sending WM_COPYDATA to
//! the first window of class `Shell_TrayWnd`. We register such a window
//! (on its own thread, above Explorer's) so those messages reach us, and
//! forward every message to Explorer's real taskbar so nothing breaks.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::tracker::window_pid;

/// Posted to the main window when tray messages are queued.
pub const WM_APP_TRAY: u32 = WM_APP + 9;
/// `wParam` of our own "TaskbarCreated" broadcasts, so we can ignore them.
pub const BROADCAST_TAG: usize = 0x574D44;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
pub const NIF_MESSAGE: u32 = 1;
pub const NIF_ICON: u32 = 2;
pub const NIF_TIP: u32 = 4;
pub const NIF_STATE: u32 = 8;
pub const NIF_GUID: u32 = 0x20;

/// NOTIFYICONDATAW as the shell sends it between processes: handles are 32 bit.
#[repr(C)]
#[derive(Clone, Copy)]
struct Nid32 {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TrayData {
    signature: u32,
    message: u32,
    nid: Nid32,
}

pub struct TrayMsg {
    pub op: u32,
    pub owner: isize,
    pub uid: u32,
    pub flags: u32,
    pub callback: u32,
    pub guid: GUID,
    pub tip: String,
    pub state: u32,
    pub state_mask: u32,
    pub version: u32,
    /// Icon pixels (premultiplied BGRA) when NIF_ICON was set.
    pub icon: Option<(u32, u32, Vec<u8>)>,
}

static QUEUE: Mutex<Vec<TrayMsg>> = Mutex::new(Vec::new());
static TRAY: AtomicIsize = AtomicIsize::new(0);
static MAIN: AtomicIsize = AtomicIsize::new(0);
static EXPLORER: AtomicIsize = AtomicIsize::new(0);

pub fn take_messages() -> Vec<TrayMsg> {
    QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

fn ours() -> HWND {
    HWND(TRAY.load(Ordering::Relaxed) as *mut _)
}

/// Explorer's own taskbar window (where everything is forwarded to).
pub fn explorer_tray() -> Option<HWND> {
    let cached = HWND(EXPLORER.load(Ordering::Relaxed) as *mut _);
    unsafe {
        if !cached.is_invalid() && IsWindow(Some(cached)).as_bool() {
            return Some(cached);
        }
        let me = std::process::id();
        let mut prev: Option<HWND> = None;
        while let Ok(h) = FindWindowExW(None, prev, w!("Shell_TrayWnd"), PCWSTR::null()) {
            if h.is_invalid() {
                break;
            }
            if window_pid(h) != me {
                EXPLORER.store(h.0 as isize, Ordering::Relaxed);
                return Some(h);
            }
            prev = Some(h);
        }
    }
    None
}

/// Start hosting. `rect` is where apps should believe the taskbar is (the
/// menu bar), so they place their tray popups near our icons.
pub fn start(main: HWND, rect: RECT) {
    if !ours().is_invalid() {
        return;
    }
    MAIN.store(main.0 as isize, Ordering::Relaxed);
    let (l, t, r, b) = (rect.left, rect.top, rect.right, rect.bottom);
    std::thread::spawn(move || unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: hinst,
            lpszClassName: w!("Shell_TrayWnd"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("Shell_TrayWnd"), w!(""), WS_POPUP, l, t, r - l, b - t, None, None, Some(hinst), None) else {
            return;
        };
        TRAY.store(hwnd.0 as isize, Ordering::Relaxed);
        raise_and_announce();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
        let _ = TRAY.compare_exchange(hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
    });
}

pub fn stop() {
    // Forget it right away so a new host can start before this one is gone.
    let h = HWND(TRAY.swap(0, Ordering::Relaxed) as *mut _);
    if !h.is_invalid() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Put our window ahead of Explorer's and ask every app to re-add its icons.
pub fn raise_and_announce() {
    let h = ours();
    if h.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(BROADCAST_TAG), LPARAM(0));
    }
}

/// Make sure apps still find us first. Explorer re-orders its taskbar now
/// and then; another tray replacement owning the slot is left alone.
pub fn ensure_first() {
    let h = ours();
    if h.is_invalid() {
        return;
    }
    unsafe {
        let Ok(first) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) else { return };
        if first == h {
            return;
        }
        if Some(first) == explorer_tray() {
            raise_and_announce();
        } else {
            crate::util::log("tray: another program is hosting tray icons; not competing with it");
        }
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            // Explorer keeps working as before: it gets every message too.
            let mut forwarded = 0usize;
            if let Some(ex) = explorer_tray() {
                SendMessageTimeoutW(ex, WM_COPYDATA, wp, lp, SMTO_ABORTIFHUNG, 3000, Some(&mut forwarded));
            }
            if cds.dwData == 1 && !cds.lpData.is_null() && cds.cbData as usize >= 8 + 24 {
                let mut data: TrayData = std::mem::zeroed();
                let n = (cds.cbData as usize).min(std::mem::size_of::<TrayData>());
                std::ptr::copy_nonoverlapping(cds.lpData as *const u8, &mut data as *mut _ as *mut u8, n);
                return LRESULT(queue(data) as isize);
            }
            LRESULT(forwarded as isize)
        }
        // Commands that tools send to "the taskbar" (Show Desktop, Minimize All,
        // Explorer's private WM_USER messages, ...) belong to Explorer.
        WM_COMMAND | WM_USER..=0xBFFF => forward(hwnd, msg, wp, lp),
        m if m >= 0xC000 && m != RegisterWindowMessageW(w!("TaskbarCreated")) => forward(hwnd, msg, wp, lp),
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Pass a message on to Explorer's taskbar, synchronously if it was sent.
unsafe fn forward(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(ex) = explorer_tray() else { return DefWindowProcW(hwnd, msg, wp, lp) };
    if InSendMessage().as_bool() {
        let mut result = 0usize;
        SendMessageTimeoutW(ex, msg, wp, lp, SMTO_ABORTIFHUNG, 3000, Some(&mut result));
        LRESULT(result as isize)
    } else {
        let _ = PostMessageW(Some(ex), msg, wp, lp);
        LRESULT(0)
    }
}

/// Parse one Shell_NotifyIcon call. Returns the BOOL the caller expects.
unsafe fn queue(data: TrayData) -> bool {
    let nid = data.nid;
    let op = data.message;
    if !matches!(op, NIM_ADD | NIM_MODIFY | NIM_DELETE | NIM_SETVERSION) {
        return true;
    }
    // The app may destroy its icon right after the call: copy the pixels now.
    let icon = if nid.flags & NIF_ICON != 0 && nid.hicon != 0 {
        icon_pixels(HICON(nid.hicon as i32 as isize as *mut _))
    } else {
        None
    };
    let tip_len = nid.tip.iter().position(|&c| c == 0).unwrap_or(nid.tip.len());
    let msg = TrayMsg {
        op,
        owner: nid.hwnd as i32 as isize,
        uid: nid.uid,
        flags: nid.flags,
        callback: nid.callback,
        guid: nid.guid,
        tip: String::from_utf16_lossy(&nid.tip[..tip_len]),
        state: nid.state,
        state_mask: nid.state_mask,
        version: nid.version,
        icon,
    };
    if let Ok(mut q) = QUEUE.lock() {
        q.push(msg);
    }
    let _ = PostMessageW(Some(HWND(MAIN.load(Ordering::Relaxed) as *mut _)), WM_APP_TRAY, WPARAM(0), LPARAM(0));
    op != NIM_SETVERSION || nid.version <= 4
}

/// Copy an icon (of any process) into premultiplied BGRA pixels.
pub fn icon_pixels(hicon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;
        let color = info.hbmColor;
        let mask = info.hbmMask;
        let cleanup = || {
            if !color.is_invalid() {
                let _ = DeleteObject(color.into());
            }
            if !mask.is_invalid() {
                let _ = DeleteObject(mask.into());
            }
        };
        if color.is_invalid() {
            cleanup();
            return None; // monochrome icon
        }
        let mut bm = BITMAP::default();
        GetObjectW(color.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 || w > 256 || h > 256 {
            cleanup();
            return None;
        }
        let read = |bmp: HBITMAP| -> Option<Vec<u8>> {
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let dc = GetDC(None);
            let rows = GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            (rows > 0).then_some(buf)
        };
        let mut px = read(color);
        if let Some(buf) = &mut px {
            if buf.chunks_exact(4).all(|p| p[3] == 0) {
                // No alpha channel: transparency comes from the AND mask.
                if let Some(m) = read(mask) {
                    for (p, mp) in buf.chunks_exact_mut(4).zip(m.chunks_exact(4)) {
                        p[3] = if mp[0] == 0 { 255 } else { 0 };
                        if p[3] == 0 {
                            p[0] = 0;
                            p[1] = 0;
                            p[2] = 0;
                        }
                    }
                }
            } else {
                // Icon colour data is straight alpha; Direct2D wants premultiplied.
                for p in buf.chunks_exact_mut(4) {
                    let a = p[3] as u32;
                    for c in &mut p[..3] {
                        *c = ((*c as u32 * a + 127) / 255) as u8;
                    }
                }
            }
        }
        cleanup();
        px.map(|p| (w as u32, h as u32, p))
    }
}
