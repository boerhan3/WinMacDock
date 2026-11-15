//! Time-based animation primitives. Everything advances by real elapsed
//! seconds, so motion is identical at 60, 120 or 144 Hz.

/// Damped spring. Retargeting mid-flight keeps position and velocity, so
/// interrupted animations reverse without a jump.
#[derive(Clone, Copy)]
pub struct Spring {
    pub v: f32,
    pub vel: f32,
    pub target: f32,
    stiffness: f32,
    damping: f32,
}

impl Spring {
    pub fn new(v: f32, stiffness: f32, damping: f32) -> Self {
        Self { v, vel: 0.0, target: v, stiffness, damping }
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    pub fn snap(&mut self, v: f32) {
        self.v = v;
        self.target = v;
        self.vel = 0.0;
    }

    pub fn active(&self) -> bool {
        (self.v - self.target).abs() > 0.0005 || self.vel.abs() > 0.005
    }

    /// Advance by `dt` seconds. Returns true while still moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        if !self.active() {
            self.v = self.target;
            self.vel = 0.0;
            return false;
        }
        // Fixed substeps keep the integration stable on long frames.
        let mut left = dt.min(0.1);
        while left > 0.0 {
            let h = left.min(0.004);
            let acc = self.stiffness * (self.target - self.v) - self.damping * self.vel;
            self.vel += acc * h;
            self.v += self.vel * h;
            left -= h;
        }
        true
    }
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// True when Windows' "Animation effects" setting is on.
pub fn system_animations_enabled() -> bool {
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut on = BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut on as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    on.as_bool()
}
