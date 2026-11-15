//! Paired Bluetooth devices and connecting / disconnecting them.
//!
//! The list comes from the device association endpoints (classic and LE).
//! Audio devices (headphones, speakers, headsets) are connected and
//! disconnected through their audio driver's one-shot KS properties, the
//! same request the Windows Settings "Connect" / "Disconnect" buttons send.
//! Other devices (mice, keyboards) connect by themselves; Windows offers no
//! public call to force those, so the panel sends them to Settings.

use windows::core::{Interface, HSTRING, IInspectable};
use windows::Devices::Bluetooth::{BluetoothDevice, BluetoothLEDevice};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind};
use windows::Foundation::IPropertyValue;
use windows::Win32::Devices::FunctionDiscovery::PKEY_DeviceInterface_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER, KSPROPERTY_ONESHOT_DISCONNECT, KSPROPERTY_ONESHOT_RECONNECT, KSPROPERTY_TYPE_GET, KSPROPSETID_BtAudio};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows_collections::IIterable;

use crate::util::from_wide;

#[derive(Clone, Default, PartialEq)]
pub struct BtDevice {
    /// Association endpoint id.
    pub id: String,
    pub name: String,
    /// 12 hex digits, upper case, no separators.
    pub address: String,
    pub connected: bool,
    /// Segoe Fluent / MDL2 icon.
    pub glyph: char,
    /// An audio device we can connect and disconnect ourselves.
    pub audio: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BtOp {
    Connect,
    Disconnect,
    Reconnect,
}

const P_CONNECTED: &str = "System.Devices.Aep.IsConnected";
const P_ADDRESS: &str = "System.Devices.Aep.DeviceAddress";
const P_MAJOR: &str = "System.Devices.Aep.Bluetooth.Cod.Major";
const P_MINOR: &str = "System.Devices.Aep.Bluetooth.Cod.Minor";
const P_LE_CAT: &str = "System.Devices.Aep.Bluetooth.Le.Appearance.Category";
const P_LE_SUB: &str = "System.Devices.Aep.Bluetooth.Le.Appearance.Subcategory";

const G_BT: char = '\u{E702}';
const G_HEADPHONE: char = '\u{E7F6}';
const G_SPEAKER: char = '\u{E7F5}';
const G_PHONE: char = '\u{E8EA}';
const G_PC: char = '\u{E7F8}';
const G_KEYBOARD: char = '\u{E765}';
const G_MOUSE: char = '\u{E962}';
const G_GAMEPAD: char = '\u{E7FC}';

fn prop(map: &windows_collections::IMapView<HSTRING, IInspectable>, key: &str) -> Option<IPropertyValue> {
    map.Lookup(&HSTRING::from(key)).ok().and_then(|v| v.cast::<IPropertyValue>().ok())
}

fn classic_glyph(major: u16, minor: u16) -> char {
    match major {
        1 => G_PC,
        2 => G_PHONE,
        4 => match minor {
            1 | 2 | 6 => G_HEADPHONE, // headset, hands-free, headphones
            _ => G_SPEAKER,
        },
        5 if minor & 0x10 != 0 => G_KEYBOARD,
        5 if minor & 0x20 != 0 => G_MOUSE,
        5 if matches!(minor & 0x0F, 1 | 2) => G_GAMEPAD,
        _ => G_BT,
    }
}

fn le_glyph(cat: u16, sub: u16) -> char {
    match cat {
        1 => G_PHONE,
        2 => G_PC,
        15 => match sub {
            1 => G_KEYBOARD,
            2 => G_MOUSE,
            3 | 4 => G_GAMEPAD,
            _ => G_BT,
        },
        // Audio sink / source, earbuds, hearing aids.
        33 | 34 | 37 | 40 | 41 => G_HEADPHONE,
        _ => G_BT,
    }
}

fn query(selector: windows::core::Result<HSTRING>, le: bool) -> Vec<BtDevice> {
    let Ok(selector) = selector else { return Vec::new() };
    let keys: Vec<HSTRING> = [P_CONNECTED, P_ADDRESS, P_MAJOR, P_MINOR, P_LE_CAT, P_LE_SUB].iter().map(|k| HSTRING::from(*k)).collect();
    let props: IIterable<HSTRING> = keys.into();
    let Ok(list) = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(&selector, &props, DeviceInformationKind::AssociationEndpoint).and_then(|op| op.get()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..list.Size().unwrap_or(0) {
        let Ok(info) = list.GetAt(i) else { continue };
        let name = info.Name().map(|n| n.to_string()).unwrap_or_default();
        if name.trim().is_empty() {
            continue;
        }
        let Ok(map) = info.Properties() else { continue };
        let connected = prop(&map, P_CONNECTED).and_then(|v| v.GetBoolean().ok()).unwrap_or(false);
        let address: String = prop(&map, P_ADDRESS).and_then(|v| v.GetString().ok()).map(|s| s.to_string()).unwrap_or_default().chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_uppercase();
        let u16_of = |k| prop(&map, k).and_then(|v| v.GetUInt16().ok().or_else(|| v.GetUInt32().ok().map(|x| x as u16)).or_else(|| v.GetUInt8().ok().map(u16::from))).unwrap_or(0);
        let glyph = if le { le_glyph(u16_of(P_LE_CAT), u16_of(P_LE_SUB)) } else { classic_glyph(u16_of(P_MAJOR), u16_of(P_MINOR)) };
        out.push(BtDevice { id: info.Id().map(|s| s.to_string()).unwrap_or_default(), name, address, connected, glyph, audio: false });
    }
    out
}

/// Paired devices, connected ones first.
pub fn list() -> Vec<BtDevice> {
    let mut devs = query(BluetoothDevice::GetDeviceSelectorFromPairingState(true), false);
    // Dual-mode devices (most headphones) show up once per protocol; keep the classic one.
    for d in query(BluetoothLEDevice::GetDeviceSelectorFromPairingState(true), true) {
        if !devs.iter().any(|c| c.name == d.name || (!d.address.is_empty() && c.address == d.address)) {
            devs.push(d);
        }
    }
    let audio = audio_controls();
    for d in devs.iter_mut() {
        d.audio = audio.iter().any(|a| a.matches(d));
    }
    devs.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    devs
}

// ------------------------------------------------------------- audio control

struct AudioCtl {
    /// KS filter device path, upper case.
    path: String,
    /// Device-interface name, the Bluetooth device's own name.
    name: String,
    ctl: IKsControl,
}

impl AudioCtl {
    fn matches(&self, d: &BtDevice) -> bool {
        (d.address.len() == 12 && self.path.contains(&d.address)) || self.name == d.name
    }
}

fn take_pwstr(p: windows::core::PWSTR) -> String {
    unsafe {
        let s = from_wide(p.as_wide());
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// KS controls of every Bluetooth audio driver, connected or not.
fn audio_controls() -> Vec<AudioCtl> {
    let mut out: Vec<AudioCtl> = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(col) = en.EnumAudioEndpoints(eAll, DEVICE_STATE(DEVICE_STATEMASK_ALL)) else { return out };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(topo) = dev.Activate::<IDeviceTopology>(CLSCTX_ALL, None) else { continue };
            let Ok(conn) = topo.GetConnector(0) else { continue };
            let Ok(ks_id) = conn.GetDeviceIdConnectedTo().map(take_pwstr) else { continue };
            let path = ks_id.to_uppercase();
            // BTHENUM (A2DP) and BTHHFENUM (hands-free) drivers.
            if !path.contains("BTH") || out.iter().any(|a| a.path == path) {
                continue;
            }
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_DeviceInterface_FriendlyName))
                .and_then(|v| PropVariantToStringAlloc(&v))
                .map(take_pwstr)
                .unwrap_or_default();
            let Ok(ks_dev) = en.GetDevice(&windows::core::HSTRING::from(ks_id.as_str())) else { continue };
            let Ok(ctl) = ks_dev.Activate::<IKsControl>(CLSCTX_ALL, None) else { continue };
            out.push(AudioCtl { path, name, ctl });
        }
    }
    out
}

fn oneshot(ctls: &[&AudioCtl], id: i32) -> bool {
    let mut any = false;
    for a in ctls {
        let mut p = KSIDENTIFIER::default();
        unsafe {
            p.Anonymous.Anonymous.Set = KSPROPSETID_BtAudio;
            p.Anonymous.Anonymous.Id = id as u32;
            p.Anonymous.Anonymous.Flags = KSPROPERTY_TYPE_GET;
            let mut ret = 0u32;
            any |= a.ctl.KsProperty(&p, std::mem::size_of::<KSIDENTIFIER>() as u32, std::ptr::null_mut(), 0, &mut ret).is_ok();
        }
    }
    any
}

/// Connect, disconnect or reconnect a device. Connecting needs an audio
/// driver to ask; disconnecting also drops the radio link itself, so
/// devices whose audio is idle (or that have none) really go offline.
pub fn act(dev: &BtDevice, op: BtOp) -> Result<(), String> {
    let all = audio_controls();
    let mine: Vec<&AudioCtl> = all.iter().filter(|a| a.matches(dev)).collect();
    let disconnect = || -> Result<(), String> {
        let audio = oneshot(&mine, KSPROPERTY_ONESHOT_DISCONNECT.0);
        match drop_link(&dev.address) {
            Ok(()) => Ok(()),
            Err(_) if audio => Ok(()),
            Err(e) => Err(format!("Could not disconnect “{}” ({e})", dev.name)),
        }
    };
    let connect = || -> Result<(), String> {
        if mine.is_empty() {
            Err(format!("“{}” can only be connected from Bluetooth Settings", dev.name))
        } else if oneshot(&mine, KSPROPERTY_ONESHOT_RECONNECT.0) {
            Ok(())
        } else {
            Err(format!("Could not reach “{}”", dev.name))
        }
    };
    match op {
        BtOp::Connect => connect(),
        BtOp::Disconnect => disconnect(),
        BtOp::Reconnect => {
            let _ = disconnect();
            std::thread::sleep(std::time::Duration::from_millis(2000));
            connect()
        }
    }
}

/// Drop the radio link (ACL) to a device, whatever profiles it uses.
fn drop_link(address: &str) -> Result<(), String> {
    use windows::Win32::Devices::Bluetooth::{BluetoothFindFirstRadio, BluetoothFindRadioClose, BLUETOOTH_FIND_RADIO_PARAMS};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::IO::DeviceIoControl;
    const IOCTL_BTH_DISCONNECT_DEVICE: u32 = 0x0041_000C;
    let addr = u64::from_str_radix(address, 16).map_err(|_| "Unknown device address".to_string())?;
    unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS { dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32 };
        let mut radio = HANDLE::default();
        let find = BluetoothFindFirstRadio(&params, &mut radio).map_err(|e| format!("No Bluetooth radio ({e})"))?;
        let _ = BluetoothFindRadioClose(find);
        let r = DeviceIoControl(radio, IOCTL_BTH_DISCONNECT_DEVICE, Some(&addr as *const u64 as *const _), 8, None, 0, None, None);
        let _ = CloseHandle(radio);
        r.map_err(|e| format!("{e}"))
    }
}

