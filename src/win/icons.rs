//! Icons and display names from the Windows shell (works for .exe, .lnk
//! and `shell:AppsFolder\<AUMID>` items, including Store apps).

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::EnhancedStorage::{PKEY_FileDescription, PKEY_Link_TargetParsingPath, PKEY_Software_ProductName};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::*;

use crate::render::gfx::Gfx;
use crate::util::{from_wide, wide};

fn item(name: &str) -> Option<IShellItem2> {
    let w = wide(name);
    unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok() }
}

fn take_string(p: windows::core::PWSTR) -> String {
    unsafe {
        let s = from_wide(p.as_wide());
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Load the icon of a shell item as a `px`-sized square bitmap.
pub fn load(gfx: &Gfx, name: &str, px: i32) -> Option<ID2D1Bitmap> {
    let (w, h, buf) = load_pixels(name, px)?;
    gfx.create_bitmap(w, h, &buf)
}

fn load_pixels(name: &str, px: i32) -> Option<(u32, u32, Vec<u8>)> {
    let wname = wide(name);
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wname.as_ptr()), None).ok()?;
        let hbm = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY).ok()?;
        let mut bm = BITMAP::default();
        GetObjectW(hbm.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            let _ = DeleteObject(hbm.into());
            return None;
        }
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        let rows = GetDIBits(hdc, hbm, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(None, hdc);
        let _ = DeleteObject(hbm.into());
        if rows == 0 {
            return None;
        }
        // Icons without an alpha channel come back with alpha 0 everywhere.
        if buf.chunks_exact(4).all(|p| p[3] == 0) {
            for p in buf.chunks_exact_mut(4) {
                p[3] = 255;
            }
        } else if buf.chunks_exact(4).any(|p| p[0].max(p[1]).max(p[2]) > p[3]) {
            // Straight (not premultiplied) alpha: a colour brighter than its
            // alpha is impossible when premultiplied. Direct2D wants
            // premultiplied pixels, otherwise soft edges turn into a light halo.
            for p in buf.chunks_exact_mut(4) {
                let a = p[3] as u32;
                for c in &mut p[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
        Some((w as u32, h as u32, buf))
    }
}

/// Human-readable name of a shell item.
pub fn display_name(name: &str) -> String {
    let fallback = || {
        let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
        base.trim_end_matches(".exe").trim_end_matches(".lnk").to_string()
    };
    let Some(it) = item(name) else { return fallback() };
    unsafe {
        if name.to_lowercase().ends_with(".exe") {
            if let Ok(p) = it.GetString(&PKEY_FileDescription) {
                let s = take_string(p);
                // Electron apps often leave the generic description in place.
                if s.trim() == "Electron" {
                    if let Ok(p) = it.GetString(&PKEY_Software_ProductName) {
                        let product = take_string(p);
                        if !product.trim().is_empty() {
                            return product;
                        }
                    }
                }
                if !s.trim().is_empty() {
                    return s;
                }
            }
        }
        match it.GetDisplayName(SIGDN_NORMALDISPLAY) {
            Ok(p) => {
                let s = take_string(p);
                let s = s.trim_end_matches(".exe").trim_end_matches(".lnk").to_string();
                if s.is_empty() { fallback() } else { s }
            }
            Err(_) => fallback(),
        }
    }
}

/// Executable a shortcut or AppsFolder entry points at (lowercase), if any.
pub fn link_target(name: &str) -> Option<String> {
    let it = item(name)?;
    unsafe {
        let p = it.GetString(&PKEY_Link_TargetParsingPath).ok()?;
        let s = take_string(p).to_lowercase();
        s.ends_with(".exe").then_some(s)
    }
}

/// Posted to the main window when a background icon load finishes;
/// `lParam` is a `Box<LoadedIcon>`.
pub const WM_APP_ICON: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 7;

pub struct LoadedIcon {
    /// Caller-defined tag identifying who asked and for what.
    pub id: u64,
    pub w: u32,
    pub h: u32,
    /// Premultiplied BGRA; empty when the icon could not be loaded.
    pub pixels: Vec<u8>,
}

/// Loads icons on a worker thread so slow shell lookups never stall a frame.
pub struct IconLoader {
    tx: std::sync::mpsc::Sender<(u64, String, i32)>,
}

impl IconLoader {
    pub fn new(main: windows::Win32::Foundation::HWND) -> Self {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        let (tx, rx) = std::sync::mpsc::channel::<(u64, String, i32)>();
        let main = main.0 as isize;
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            for (id, name, px) in rx {
                let (w, h, pixels) = load_pixels(&name, px).unwrap_or((0, 0, Vec::new()));
                let boxed = Box::into_raw(Box::new(LoadedIcon { id, w, h, pixels }));
                if PostMessageW(Some(HWND(main as *mut _)), WM_APP_ICON, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                    drop(Box::from_raw(boxed));
                }
            }
        });
        Self { tx }
    }

    pub fn request(&self, id: u64, name: &str, px: i32) {
        let _ = self.tx.send((id, name.to_string(), px));
    }
}

/// Pack (consumer, generation, index) into a request id.
pub fn icon_id(consumer: u8, generation: u32, index: usize) -> u64 {
    ((consumer as u64) << 56) | (((generation & 0xFF_FFFF) as u64) << 32) | (index as u64 & 0xFFFF_FFFF)
}

pub fn split_icon_id(id: u64) -> (u8, u32, usize) {
    ((id >> 56) as u8, ((id >> 32) & 0xFF_FFFF) as u32, (id & 0xFFFF_FFFF) as usize)
}
