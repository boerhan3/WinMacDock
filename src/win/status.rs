//! System status shown in the menu bar: battery, volume, network, clock.

use windows::core::{implement, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::Endpoints::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::Networking::NetworkListManager::{INetworkListManager, NetworkListManager};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::app;

/// Posted to the main window when the volume changes.
pub const WM_APP_STATUS: u32 = WM_APP + 4;

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback;

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        // Called on an audio thread; just wake the UI thread.
        unsafe {
            let _ = PostMessageW(Some(app::main_hwnd()), WM_APP_STATUS, WPARAM(0), LPARAM(0));
        }
        Ok(())
    }
}

pub struct Status {
    /// Percent and whether on AC power; `None` on desktops.
    pub battery: Option<(u8, bool)>,
    pub on_battery: bool,
    /// Estimated seconds of battery left, when Windows knows.
    pub battery_secs: Option<u32>,
    pub volume: f32,
    pub muted: bool,
    pub online: bool,
    endpoint: Option<IAudioEndpointVolume>,
    _callback: Option<IAudioEndpointVolumeCallback>,
    nlm: Option<INetworkListManager>,
}

impl Status {
    pub fn new() -> Self {
        let (endpoint, callback) = unsafe { open_volume() }.map(|(e, c)| (Some(e), Some(c))).unwrap_or((None, None));
        let nlm = unsafe { CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok() };
        let mut s = Self { battery: None, on_battery: false, battery_secs: None, volume: 0.0, muted: false, online: true, endpoint, _callback: callback, nlm };
        s.refresh();
        s
    }

    pub fn refresh(&mut self) {
        unsafe {
            let mut sps = SYSTEM_POWER_STATUS::default();
            if GetSystemPowerStatus(&mut sps).is_ok() {
                let has_battery = sps.BatteryFlag & 128 == 0 && sps.BatteryLifePercent <= 100;
                self.battery = has_battery.then_some((sps.BatteryLifePercent, sps.ACLineStatus == 1));
                self.on_battery = sps.ACLineStatus == 0;
                self.battery_secs = (sps.BatteryLifeTime != u32::MAX && self.on_battery).then_some(sps.BatteryLifeTime);
            }
            if let Some(ep) = &self.endpoint {
                if let Ok(v) = ep.GetMasterVolumeLevelScalar() {
                    self.volume = v;
                }
                if let Ok(m) = ep.GetMute() {
                    self.muted = m.as_bool();
                }
            }
            if let Some(nlm) = &self.nlm {
                if let Ok(c) = nlm.IsConnectedToInternet() {
                    self.online = c.as_bool();
                }
            }
        }
    }
}

impl Status {
    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(ep) = &self.endpoint {
            unsafe {
                let _ = ep.SetMasterVolumeLevelScalar(self.volume, std::ptr::null());
                if self.muted && self.volume > 0.0 {
                    let _ = ep.SetMute(false, std::ptr::null());
                    self.muted = false;
                }
            }
        }
    }

    pub fn set_mute(&mut self, mute: bool) {
        self.muted = mute;
        if let Some(ep) = &self.endpoint {
            unsafe {
                let _ = ep.SetMute(mute, std::ptr::null());
            }
        }
    }

    /// The default output device changed: follow it.
    pub fn reopen_volume(&mut self) {
        if let (Some(ep), Some(cb)) = (&self.endpoint, &self._callback) {
            unsafe {
                let _ = ep.UnregisterControlChangeNotify(cb);
            }
        }
        let (endpoint, callback) = unsafe { open_volume() }.map(|(e, c)| (Some(e), Some(c))).unwrap_or((None, None));
        self.endpoint = endpoint;
        self._callback = callback;
        self.refresh();
    }
}

unsafe fn open_volume() -> Result<(IAudioEndpointVolume, IAudioEndpointVolumeCallback)> {
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
    let callback: IAudioEndpointVolumeCallback = VolumeCallback.into();
    endpoint.RegisterControlChangeNotify(&callback)?;
    Ok((endpoint, callback))
}

/// "Thu 12 Nov  21:26" and the milliseconds left until the next minute.
pub fn clock_text(h24: bool) -> (String, u32) {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = unsafe { GetLocalTime() };
    let day = DAYS[(t.wDayOfWeek as usize) % 7];
    let month = MONTHS[(t.wMonth as usize + 11) % 12];
    let time = if h24 {
        format!("{:02}:{:02}", t.wHour, t.wMinute)
    } else {
        let h = if t.wHour % 12 == 0 { 12 } else { t.wHour % 12 };
        format!("{}:{:02} {}", h, t.wMinute, if t.wHour < 12 { "AM" } else { "PM" })
    };
    let left = (60 - t.wSecond as u32) * 1000 - t.wMilliseconds as u32 + 30;
    (format!("{day} {} {month}  {time}", t.wDay), left)
}

/// Press a key combination (all keys down in order, then up in reverse).
pub fn send_keys(keys: &[VIRTUAL_KEY]) {
    let make = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() } },
    };
    let mut inputs: Vec<INPUT> = keys.iter().map(|&k| make(k, false)).collect();
    inputs.extend(keys.iter().rev().map(|&k| make(k, true)));
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Press Win + `key` (opens Quick Settings, Notification Center, ...).
pub fn send_win_key(key: VIRTUAL_KEY) {
    let make = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
        },
    };
    let inputs = [make(VK_LWIN, false), make(key, false), make(key, true), make(VK_LWIN, true)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// True when running on battery power.
pub fn on_battery() -> bool {
    let mut sps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut sps).is_ok() && sps.ACLineStatus == 0 }
}
