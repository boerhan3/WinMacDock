//! Frosted-glass backdrop window. A Windows.UI.Composition visual tree
//! paints the blurred desktop (host backdrop) plus a tint, clipped to an
//! anti-aliased rounded rectangle. UI content is drawn by a separate
//! transparent window owned by (and therefore always above) this one.

use std::cell::RefCell;

use windows::core::{Interface, Result, BOOL};
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::{CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Color;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{CompositionColorBrush, CompositionRoundedRectangleGeometry, Compositor, ContainerVisual};
use windows_numerics::Vector2;

use crate::app;

thread_local! {
    static COMPOSITOR: RefCell<Option<(Compositor, DispatcherQueueController)>> = const { RefCell::new(None) };
}

/// One compositor per process, created on first use. It needs a
/// DispatcherQueue on the UI thread.
fn compositor() -> Result<Compositor> {
    COMPOSITOR.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            let options = DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_NONE,
            };
            let queue = unsafe { CreateDispatcherQueueController(options)? };
            *c = Some((Compositor::new()?, queue));
        }
        Ok(c.as_ref().unwrap().0.clone())
    })
}

fn color(tint: u32) -> Color {
    Color { A: (tint >> 24) as u8, B: (tint >> 16) as u8, G: (tint >> 8) as u8, R: tint as u8 }
}

struct Visuals {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    tint: CompositionColorBrush,
    shape: CompositionRoundedRectangleGeometry,
}

pub struct Glass {
    pub hwnd: HWND,
    vis: Option<Visuals>,
    rect: (i32, i32, i32, i32),
    radius: i32,
    visible: bool,
}

impl Glass {
    /// `tint` is 0xAABBGGRR, laid over the blurred backdrop.
    pub fn new(tint: u32) -> Self {
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, None);
        let vis = match build(hwnd, tint) {
            Ok(v) => Some(v),
            Err(e) => {
                crate::util::log(&format!("glass backdrop unavailable: {e}"));
                None
            }
        };
        Self { hwnd, vis, rect: (0, 0, 0, 0), radius: -1, visible: false }
    }

    pub fn set_tint(&self, tint: u32) {
        if let Some(v) = &self.vis {
            let _ = v.tint.SetColor(color(tint));
        }
    }

    /// Fade the whole backdrop (0.0 – 1.0).
    pub fn set_opacity(&self, opacity: f32) {
        if let Some(v) = &self.vis {
            let _ = v.root.SetOpacity(opacity.clamp(0.0, 1.0));
        }
    }

    /// Move/resize (screen pixels) and round the corners. No-op when unchanged.
    pub fn set_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32) {
        if self.rect == (x, y, w, h) && self.radius == radius {
            return;
        }
        let resized = self.rect.2 != w || self.rect.3 != h || self.radius != radius;
        self.rect = (x, y, w, h);
        self.radius = radius;
        if resized {
            if let Some(v) = &self.vis {
                let _ = v.shape.SetSize(Vector2 { X: w as f32, Y: h as f32 });
                let _ = v.shape.SetCornerRadius(Vector2 { X: radius as f32, Y: radius as f32 });
            }
        }
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
        }
    }

    pub fn show(&mut self, show: bool) {
        if self.visible == show {
            return;
        }
        self.visible = show;
        unsafe {
            let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }

    /// Move to the top of the topmost band (its owned content window follows).
    pub fn raise(&self) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

impl Drop for Glass {
    fn drop(&mut self) {
        // Also destroys the content window it owns.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn build(hwnd: HWND, tint: u32) -> Result<Visuals> {
    let comp = compositor()?;
    unsafe {
        // Lets the host backdrop brush sample what is behind this window.
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, &on as *const _ as *const _, 4);
    }
    let interop: ICompositorDesktopInterop = comp.cast()?;
    let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false)? };

    let root = comp.CreateContainerVisual()?;
    let fill = Vector2 { X: 1.0, Y: 1.0 };
    root.SetRelativeSizeAdjustment(fill)?;

    let blur = comp.CreateSpriteVisual()?;
    blur.SetRelativeSizeAdjustment(fill)?;
    blur.SetBrush(&comp.CreateHostBackdropBrush()?)?;
    root.Children()?.InsertAtBottom(&blur)?;

    let tint_brush = comp.CreateColorBrushWithColor(color(tint))?;
    let tint_visual = comp.CreateSpriteVisual()?;
    tint_visual.SetRelativeSizeAdjustment(fill)?;
    tint_visual.SetBrush(&tint_brush)?;
    root.Children()?.InsertAtTop(&tint_visual)?;

    let shape = comp.CreateRoundedRectangleGeometry()?;
    let clip = comp.CreateGeometricClipWithGeometry(&shape)?;
    root.SetClip(&clip)?;

    target.SetRoot(&root)?;
    Ok(Visuals { _target: target, root, tint: tint_brush, shape })
}
