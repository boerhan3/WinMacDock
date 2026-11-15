//! System controls that are slow or blocking (WMI, WinRT, Wi-Fi). They run
//! on one background thread; results come back to the UI thread as
//! `WM_APP_SYS` messages carrying a boxed `SysEvent`.

// IPolicyConfig below keeps the COM method names.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows::core::{w, BSTR, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Media::Control::{GlobalSystemMediaTransportControlsSessionManager as MediaManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};
use windows::Win32::System::Variant::{VariantToString, VariantToUInt32, VARIANT, VT_I4, VT_UI1};
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_APP, WM_SETTINGCHANGE};

use crate::util::{from_wide, wide};
use crate::win::bluetooth::{self, BtDevice, BtOp};
use crate::win::wifi::{self, WifiInfo, WifiNet};

pub const WM_APP_SYS: u32 = WM_APP + 10;

#[derive(Clone, Copy, PartialEq)]
pub enum RadioSel {
    Wifi,
    Bluetooth,
}

#[derive(Clone, Copy)]
pub enum MediaCmd {
    PlayPause,
    Next,
    Previous,
}

pub enum Job {
    Brightness,
    SetBrightness(f32),
    Radios,
    SetRadio(RadioSel, bool),
    Media,
    Media2(MediaCmd),
    Wifi { scan: bool },
    WifiConnect(WifiNet, Option<String>),
    Bluetooth,
    BtAction(BtDevice, BtOp),
    AudioDevices,
    SetAudioDevice(String),
    DarkMode(bool),
}

#[derive(Clone, Default)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub playing: bool,
}

#[derive(Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub default: bool,
}

pub enum SysEvent {
    Brightness(Option<f32>),
    Radios { wifi: Option<bool>, bluetooth: Option<bool> },
    Media(Option<MediaInfo>),
    Wifi(WifiInfo),
    /// Outcome of a join attempt: empty on success, otherwise a message.
    WifiJoin(String),
    Bluetooth(Vec<BtDevice>),
    /// Outcome of a connect / disconnect: empty on success, otherwise a message.
    BtDone(String),
    /// A connect / disconnect has finished (the panel stops showing progress).
    BtSettled,
    AudioDevices(Vec<AudioDevice>),
    /// The default output device changed; reopen the volume endpoint.
    AudioDefaultChanged,
}

pub struct Worker {
    tx: Sender<Job>,
}

impl Worker {
    pub fn new(main: HWND) -> Self {
        let (tx, rx) = channel();
        let main = main.0 as isize;
        std::thread::spawn(move || run(rx, HWND(main as *mut _)));
        Self { tx }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

#[derive(Default)]
struct State {
    wmi: Option<IWbemServices>,
    media: Option<MediaManager>,
    radios: Vec<Radio>,
}

fn run(rx: Receiver<Job>, main: HWND) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut st = State::default();
    let post = |ev: SysEvent| unsafe {
        let b = Box::into_raw(Box::new(ev));
        if PostMessageW(Some(main), WM_APP_SYS, WPARAM(0), LPARAM(b as isize)).is_err() {
            drop(Box::from_raw(b));
        }
    };
    while let Ok(first) = rx.recv() {
        // Coalesce a burst of jobs: only the last brightness value matters.
        let mut jobs = vec![first];
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        let last_bright = jobs.iter().rposition(|j| matches!(j, Job::SetBrightness(_)));
        for (i, job) in jobs.into_iter().enumerate() {
            match job {
                Job::Brightness => post(SysEvent::Brightness(get_brightness(&mut st))),
                Job::SetBrightness(v) => {
                    if Some(i) == last_bright {
                        set_brightness(&mut st, v);
                    }
                }
                Job::Radios => post(read_radios(&mut st)),
                Job::SetRadio(which, on) => {
                    set_radio(&mut st, which, on);
                    post(read_radios(&mut st));
                }
                Job::Media => post(SysEvent::Media(read_media(&mut st))),
                Job::Media2(cmd) => {
                    media_cmd(&mut st, cmd);
                    std::thread::sleep(std::time::Duration::from_millis(350));
                    post(SysEvent::Media(read_media(&mut st)));
                }
                Job::Wifi { scan } => {
                    post(SysEvent::Wifi(wifi::list(scan)));
                    if scan {
                        // Scan results trickle in over a couple of seconds.
                        std::thread::sleep(std::time::Duration::from_millis(2500));
                        post(SysEvent::Wifi(wifi::list(false)));
                    }
                }
                Job::WifiConnect(net, password) => {
                    let msg = wifi::connect(&net, password.as_deref()).err().unwrap_or_default();
                    post(SysEvent::WifiJoin(msg));
                    std::thread::sleep(std::time::Duration::from_millis(3000));
                    post(SysEvent::Wifi(wifi::list(false)));
                }
                Job::Bluetooth => post(SysEvent::Bluetooth(bluetooth::list())),
                Job::BtAction(dev, op) => {
                    let msg = bluetooth::act(&dev, op).err().unwrap_or_default();
                    let failed = !msg.is_empty();
                    post(SysEvent::BtDone(msg));
                    if !failed {
                        // The link comes up (or drops) a few seconds after the request.
                        for wait in [2500, 3000] {
                            std::thread::sleep(std::time::Duration::from_millis(wait));
                            post(SysEvent::Bluetooth(bluetooth::list()));
                        }
                    }
                    post(SysEvent::BtSettled);
                }
                Job::AudioDevices => post(SysEvent::AudioDevices(audio_devices())),
                Job::SetAudioDevice(id) => {
                    set_default_audio(&id);
                    post(SysEvent::AudioDefaultChanged);
                    post(SysEvent::AudioDevices(audio_devices()));
                }
                Job::DarkMode(dark) => set_dark_mode(dark),
            }
        }
    }
}

// ---------------------------------------------------------------- brightness

fn var_i32(v: i32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_I4;
        (*var.Anonymous.Anonymous).Anonymous.lVal = v;
    }
    var
}

fn var_u8(v: u8) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI1;
        (*var.Anonymous.Anonymous).Anonymous.bVal = v;
    }
    var
}

fn wmi(st: &mut State) -> Option<IWbemServices> {
    if st.wmi.is_none() {
        st.wmi = unsafe {
            (|| -> windows::core::Result<IWbemServices> {
                let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
                let svc = loc.ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
                CoSetProxyBlanket(&svc, 10, 0, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;
                Ok(svc)
            })()
            .ok()
        };
    }
    st.wmi.clone()
}

fn first_object(svc: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
    unsafe {
        let en = svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None).ok()?;
        let mut objs = [None];
        let mut n = 0u32;
        let _ = en.Next(WBEM_INFINITE, &mut objs, &mut n);
        objs[0].take()
    }
}

/// Built-in display brightness (0–1); `None` for external monitors / desktops.
fn get_brightness(st: &mut State) -> Option<f32> {
    let svc = wmi(st)?;
    let obj = first_object(&svc, "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE")?;
    unsafe {
        let mut v = VARIANT::default();
        obj.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
        VariantToUInt32(&v).ok().map(|b| b as f32 / 100.0)
    }
}

fn set_brightness(st: &mut State, value: f32) {
    let Some(svc) = wmi(st) else { return };
    let Some(obj) = first_object(&svc, "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active=TRUE") else { return };
    unsafe {
        let _ = (|| -> windows::core::Result<()> {
            let mut path_v = VARIANT::default();
            obj.Get(w!("__PATH"), 0, &mut path_v, None, None)?;
            let mut buf = [0u16; 512];
            VariantToString(&path_v, &mut buf)?;
            let path = BSTR::from_wide(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
            let mut class = None;
            svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)?;
            let class = class.ok_or(windows::core::Error::from(E_FAIL))?;
            let (mut input, mut output) = (None, None);
            class.GetMethod(w!("WmiSetBrightness"), 0, &mut input, &mut output)?;
            let params = input.ok_or(windows::core::Error::from(E_FAIL))?.SpawnInstance(0)?;
            params.Put(w!("Timeout"), 0, &var_i32(1), 0)?;
            params.Put(w!("Brightness"), 0, &var_u8((value.clamp(0.0, 1.0) * 100.0).round() as u8), 0)?;
            svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None)
        })();
    }
}

// -------------------------------------------------------------------- radios

fn load_radios(st: &mut State) {
    if !st.radios.is_empty() {
        return;
    }
    let _ = Radio::RequestAccessAsync().and_then(|op| op.get());
    if let Ok(list) = Radio::GetRadiosAsync().and_then(|op| op.get()) {
        let n = list.Size().unwrap_or(0);
        st.radios = (0..n).filter_map(|i| list.GetAt(i).ok()).collect();
    }
}

fn kind_of(sel: RadioSel) -> RadioKind {
    match sel {
        RadioSel::Wifi => RadioKind::WiFi,
        RadioSel::Bluetooth => RadioKind::Bluetooth,
    }
}

fn read_radios(st: &mut State) -> SysEvent {
    load_radios(st);
    let state = |sel| st.radios.iter().find(|r| r.Kind().ok() == Some(kind_of(sel))).and_then(|r| r.State().ok()).map(|s| s == RadioState::On);
    SysEvent::Radios { wifi: state(RadioSel::Wifi), bluetooth: state(RadioSel::Bluetooth) }
}

fn set_radio(st: &mut State, sel: RadioSel, on: bool) {
    load_radios(st);
    if let Some(r) = st.radios.iter().find(|r| r.Kind().ok() == Some(kind_of(sel))) {
        let _ = r.SetStateAsync(if on { RadioState::On } else { RadioState::Off }).and_then(|op| op.get());
    }
}

// --------------------------------------------------------------------- media

fn manager(st: &mut State) -> Option<MediaManager> {
    if st.media.is_none() {
        st.media = MediaManager::RequestAsync().and_then(|op| op.get()).ok();
    }
    st.media.clone()
}

fn read_media(st: &mut State) -> Option<MediaInfo> {
    let session = manager(st)?.GetCurrentSession().ok()?;
    let props = session.TryGetMediaPropertiesAsync().and_then(|op| op.get()).ok()?;
    let playing = session.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()).map(|s| s == PlaybackStatus::Playing).unwrap_or(false);
    let title = props.Title().map(|t| t.to_string()).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    Some(MediaInfo { title, artist: props.Artist().map(|t| t.to_string()).unwrap_or_default(), playing })
}

fn media_cmd(st: &mut State, cmd: MediaCmd) {
    let Some(session) = manager(st).and_then(|m| m.GetCurrentSession().ok()) else { return };
    let op = match cmd {
        MediaCmd::PlayPause => session.TryTogglePlayPauseAsync(),
        MediaCmd::Next => session.TrySkipNextAsync(),
        MediaCmd::Previous => session.TrySkipPreviousAsync(),
    };
    let _ = op.and_then(|op| op.get());
}

// --------------------------------------------------------------------- audio

fn take_pwstr(p: PWSTR) -> String {
    unsafe {
        let s = from_wide(p.as_wide());
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn audio_devices() -> Vec<AudioDevice> {
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return Vec::new() };
        let default = en.GetDefaultAudioEndpoint(eRender, eConsole).and_then(|d| d.GetId()).map(take_pwstr).unwrap_or_default();
        let Ok(col) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(id) = dev.GetId().map(take_pwstr) else { continue };
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|v| PropVariantToStringAlloc(&v))
                .map(take_pwstr)
                .unwrap_or_else(|_| "Speakers".into());
            out.push(AudioDevice { default: id == default, id, name });
        }
        out
    }
}

/// Undocumented but stable since Vista; the only way to change the default
/// output device (used by the Windows sound settings themselves).
#[windows_core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: windows_core::IUnknown {
    fn GetMixFormat(&self, id: PCWSTR, fmt: *mut *mut c_void) -> HRESULT;
    fn GetDeviceFormat(&self, id: PCWSTR, default: i32, fmt: *mut *mut c_void) -> HRESULT;
    fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
    fn SetDeviceFormat(&self, id: PCWSTR, fmt: *mut c_void, mix: *mut c_void) -> HRESULT;
    fn GetProcessingPeriod(&self, id: PCWSTR, default: i32, a: *mut i64, b: *mut i64) -> HRESULT;
    fn SetProcessingPeriod(&self, id: PCWSTR, a: *mut i64) -> HRESULT;
    fn GetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn SetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn GetPropertyValue(&self, id: PCWSTR, key: *const c_void, v: *mut c_void) -> HRESULT;
    fn SetPropertyValue(&self, id: PCWSTR, key: *const c_void, v: *mut c_void) -> HRESULT;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: i32) -> HRESULT;
    fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
}

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn set_default_audio(id: &str) {
    let idw = wide(id);
    unsafe {
        if let Ok(cfg) = CoCreateInstance::<_, IPolicyConfig>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) {
            for role in 0..3 {
                let _ = cfg.SetDefaultEndpoint(PCWSTR(idw.as_ptr()), role);
            }
        }
    }
}

// ----------------------------------------------------------------- dark mode

fn set_dark_mode(dark: bool) {
    let light: u32 = if dark { 0 } else { 1 };
    let key = w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    unsafe {
        for name in [w!("AppsUseLightTheme"), w!("SystemUsesLightTheme")] {
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_DWORD.0, Some(&light as *const _ as *const c_void), 4);
        }
        let area = wide("ImmersiveColorSet");
        let mut res = 0usize;
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, WPARAM(0), LPARAM(area.as_ptr() as isize), SMTO_ABORTIFHUNG, 300, Some(&mut res));
    }
}
