//! A blurred picture of the current wallpaper, for full-screen overlays
//! (Launchpad). It does not depend on Windows' transparency effects, which
//! turn off with Energy Saver and leave the system blur a flat grey.
//!
//! The trick: make a tiny version of the wallpaper (64 px wide) and let
//! Direct2D stretch it with cubic filtering. The result is a smooth, heavy
//! blur for almost no work.

use windows::core::PCWSTR;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

use crate::app::Ctx;
use crate::ui::wallpaper;

const W: u32 = 64;

/// The wallpaper as currently seen on the desktop, blurred, for a `sw`×`sh` screen.
pub fn wallpaper(cx: &Ctx, sw: i32, sh: i32) -> Option<ID2D1Bitmap> {
    let h = ((W as f32 * sh as f32 / sw.max(1) as f32).round() as u32).max(1);
    if cx.cfg.wallpaper.mode == "shader" {
        // The live wallpaper is a shader: render the same scene, tiny, at the same moment.
        if let Some(bmp) = wallpaper::snapshot(cx, &cx.cfg.wallpaper.shader, cx.wallpaper_time, W, h) {
            return Some(bmp);
        }
    }
    let pixels = image_pixels(W, h)?;
    cx.gfx.create_bitmap(W, h, &pixels)
}

/// Windows' own wallpaper image, cropped to fill and shrunk to `w`×`h` (BGRA).
fn image_pixels(w: u32, h: u32) -> Option<Vec<u8>> {
    let mut path = [0u16; 520];
    unsafe {
        SystemParametersInfoW(SPI_GETDESKWALLPAPER, path.len() as u32, Some(path.as_mut_ptr() as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
        if path[0] == 0 {
            return None;
        }
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let dec = factory.CreateDecoderFromFilename(PCWSTR(path.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut iw, mut ih) = (0u32, 0u32);
        frame.GetSize(&mut iw, &mut ih).ok()?;
        if iw == 0 || ih == 0 {
            return None;
        }
        // Crop to the screen's shape, like the "Fill" wallpaper position.
        let (ia, ta) = (iw as f32 / ih as f32, w as f32 / h as f32);
        let crop = if ia > ta {
            let cw = (ih as f32 * ta) as i32;
            WICRect { X: (iw as i32 - cw) / 2, Y: 0, Width: cw.max(1), Height: ih as i32 }
        } else {
            let ch = (iw as f32 / ta) as i32;
            WICRect { X: 0, Y: (ih as i32 - ch) / 2, Width: iw as i32, Height: ch.max(1) }
        };
        let clip = factory.CreateBitmapClipper().ok()?;
        clip.Initialize(&frame, &crop).ok()?;
        let scaler = factory.CreateBitmapScaler().ok()?;
        // Fant averages every source pixel: a clean base for the blur.
        scaler.Initialize(&clip, w, h, WICBitmapInterpolationModeFant).ok()?;
        let conv = factory.CreateFormatConverter().ok()?;
        conv.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut buf).ok()?;
        Some(buf)
    }
}
