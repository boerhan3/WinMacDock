//! Wi-Fi networks through the Native Wifi (WLAN) API.

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::*;

use crate::util::{from_wide, wide};

#[derive(Clone, Default)]
pub struct WifiNet {
    pub ssid: String,
    /// 0–100.
    pub signal: u32,
    pub secured: bool,
    pub connected: bool,
    /// Saved Windows profile for this network, if any.
    pub profile: Option<String>,
    auth: i32,
    cipher: i32,
}

#[derive(Clone, Default)]
pub struct WifiInfo {
    /// False when there is no Wi-Fi adapter.
    pub adapter: bool,
    pub networks: Vec<WifiNet>,
}

impl WifiInfo {
    pub fn current(&self) -> Option<&WifiNet> {
        self.networks.iter().find(|n| n.connected)
    }
}

impl WifiNet {
    /// Whether we can join it ourselves (open network or a personal / home
    /// security type we know how to describe in a profile).
    pub fn joinable(&self) -> bool {
        self.profile.is_some() || !self.secured || auth_name(self.auth).is_some()
    }

    pub fn needs_password(&self) -> bool {
        self.profile.is_none() && self.secured
    }
}

fn auth_name(auth: i32) -> Option<&'static str> {
    match auth {
        4 => Some("WPAPSK"),
        7 => Some("WPA2PSK"),
        9 => Some("WPA3SAE"),
        _ => None,
    }
}

struct Session {
    handle: HANDLE,
    iface: GUID,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.handle, None);
        }
    }
}

fn open() -> Option<Session> {
    unsafe {
        let (mut version, mut handle) = (0u32, HANDLE::default());
        if WlanOpenHandle(2, None, &mut version, &mut handle) != 0 {
            return None;
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, None, &mut list) != 0 || list.is_null() {
            WlanCloseHandle(handle, None);
            return None;
        }
        let iface = ((*list).dwNumberOfItems > 0).then(|| (*list).InterfaceInfo[0].InterfaceGuid);
        WlanFreeMemory(list as *const _);
        match iface {
            Some(iface) => Some(Session { handle, iface }),
            None => {
                WlanCloseHandle(handle, None);
                None
            }
        }
    }
}

/// Visible networks, strongest first; the connected one leads.
pub fn list(scan: bool) -> WifiInfo {
    let Some(sess) = open() else { return WifiInfo::default() };
    let mut nets: Vec<WifiNet> = Vec::new();
    unsafe {
        if scan {
            WlanScan(sess.handle, &sess.iface, None, None, None);
        }
        let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
        if WlanGetAvailableNetworkList(sess.handle, &sess.iface, 0, None, &mut list) == 0 && !list.is_null() {
            let count = (*list).dwNumberOfItems as usize;
            let first = (*list).Network.as_ptr();
            for i in 0..count {
                let n = &*first.add(i);
                let len = (n.dot11Ssid.uSSIDLength as usize).min(32);
                let ssid = String::from_utf8_lossy(&n.dot11Ssid.ucSSID[..len]).to_string();
                if ssid.is_empty() {
                    continue; // hidden network
                }
                let profile = from_wide(&n.strProfileName);
                let net = WifiNet {
                    ssid,
                    signal: n.wlanSignalQuality,
                    secured: n.bSecurityEnabled.as_bool(),
                    connected: n.dwFlags & 1 != 0,
                    profile: (!profile.is_empty()).then_some(profile),
                    auth: n.dot11DefaultAuthAlgorithm.0,
                    cipher: n.dot11DefaultCipherAlgorithm.0,
                };
                // The same SSID shows up once per profile / BSS type; keep the best entry.
                match nets.iter_mut().find(|x| x.ssid == net.ssid) {
                    Some(x) => {
                        x.connected |= net.connected;
                        x.signal = x.signal.max(net.signal);
                        if x.profile.is_none() {
                            x.profile = net.profile;
                        }
                    }
                    None => nets.push(net),
                }
            }
            WlanFreeMemory(list as *const _);
        }
    }
    nets.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.signal.cmp(&a.signal)));
    WifiInfo { adapter: true, networks: nets }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Join a network, saving a new profile first when Windows has none.
pub fn connect(net: &WifiNet, password: Option<&str>) -> Result<(), String> {
    let sess = open().ok_or("No Wi-Fi adapter")?;
    let profile = match &net.profile {
        Some(p) => p.clone(),
        None => {
            let ssid = xml_escape(&net.ssid);
            let security = if !net.secured {
                "<authEncryption><authentication>open</authentication><encryption>none</encryption><useOneX>false</useOneX></authEncryption>".to_string()
            } else {
                let auth = auth_name(net.auth).ok_or("This network type must be joined from Windows Settings")?;
                let enc = if net.cipher == 2 { "TKIP" } else { "AES" };
                let key = xml_escape(password.ok_or("Password required")?);
                format!(
                    "<authEncryption><authentication>{auth}</authentication><encryption>{enc}</encryption><useOneX>false</useOneX></authEncryption>\
                     <sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{key}</keyMaterial></sharedKey>"
                )
            };
            let xml = format!(
                "<?xml version=\"1.0\"?><WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\">\
                 <name>{ssid}</name><SSIDConfig><SSID><name>{ssid}</name></SSID></SSIDConfig>\
                 <connectionType>ESS</connectionType><connectionMode>auto</connectionMode>\
                 <MSM><security>{security}</security></MSM></WLANProfile>"
            );
            let xml_w = wide(&xml);
            let mut reason = 0u32;
            let rc = unsafe { WlanSetProfile(sess.handle, &sess.iface, 0, PCWSTR(xml_w.as_ptr()), PCWSTR::null(), true, None, &mut reason) };
            if rc != 0 {
                return Err(format!("Could not save the network (error {rc}, reason {reason})"));
            }
            net.ssid.clone()
        }
    };
    let profile_w = wide(&profile);
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: PCWSTR(profile_w.as_ptr()),
        pDot11Ssid: std::ptr::null_mut(),
        pDesiredBssidList: std::ptr::null_mut(),
        dot11BssType: dot11_BSS_type_infrastructure,
        dwFlags: 0,
    };
    let rc = unsafe { WlanConnect(sess.handle, &sess.iface, &params, None) };
    if rc == 0 { Ok(()) } else { Err(format!("Could not connect (error {rc})")) }
}

