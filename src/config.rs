use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub dock: DockCfg,
    pub menubar: MenuBarCfg,
    pub spotlight: SpotlightCfg,
    pub traffic_lights: TrafficCfg,
    pub wallpaper: WallpaperCfg,
    pub launchpad: LaunchpadCfg,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct LaunchpadCfg {
    /// Apps shown in Launchpad, in order (`shell:AppsFolder\<id>`). Empty by default:
    /// the user picks them with "Add Apps".
    /// A `folder:<id>` entry places the folder with that id at that position.
    pub apps: Vec<String>,
    pub folders: Vec<FolderCfg>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct FolderCfg {
    pub id: u32,
    pub name: String,
    pub apps: Vec<String>,
}

impl LaunchpadCfg {
    pub fn folder_key(id: u32) -> String {
        format!("folder:{id}")
    }

    pub fn folder_id(key: &str) -> Option<u32> {
        key.strip_prefix("folder:")?.parse().ok()
    }

    pub fn folder_mut(&mut self, id: u32) -> Option<&mut FolderCfg> {
        self.folders.iter_mut().find(|f| f.id == id)
    }

    /// Is this app anywhere in Launchpad (top level or in a folder)?
    pub fn contains(&self, launch: &str) -> bool {
        self.apps.iter().chain(self.folders.iter().flat_map(|f| f.apps.iter())).any(|l| l.eq_ignore_ascii_case(launch))
    }

    /// Take an app out of Launchpad wherever it is.
    pub fn remove_app(&mut self, launch: &str) {
        self.apps.retain(|l| !l.eq_ignore_ascii_case(launch));
        for f in &mut self.folders {
            f.apps.retain(|l| !l.eq_ignore_ascii_case(launch));
        }
        self.tidy();
    }

    /// Dissolve folders with fewer than two apps (like macOS) and drop dangling entries.
    pub fn tidy(&mut self) {
        let mut i = 0;
        while i < self.apps.len() {
            if let Some(id) = Self::folder_id(&self.apps[i]) {
                match self.folders.iter().position(|f| f.id == id) {
                    Some(fi) if self.folders[fi].apps.len() >= 2 => {}
                    Some(fi) => {
                        let f = self.folders.remove(fi);
                        self.apps.remove(i);
                        if let Some(last) = f.apps.into_iter().next() {
                            self.apps.insert(i, last);
                            i += 1;
                        }
                        continue;
                    }
                    None => {
                        self.apps.remove(i);
                        continue;
                    }
                }
            }
            i += 1;
        }
        let apps = &self.apps;
        self.folders.retain(|f| apps.iter().any(|k| Self::folder_id(k) == Some(f.id)));
    }

    /// Put `a` and the app at `b` into a new folder that takes `b`'s place.
    pub fn make_folder(&mut self, b: &str, a: &str) -> u32 {
        let id = self.folders.iter().map(|f| f.id).max().unwrap_or(0) + 1;
        self.apps.retain(|l| l != a);
        let pos = self.apps.iter().position(|l| l == b).unwrap_or(self.apps.len());
        self.apps.retain(|l| l != b);
        self.apps.insert(pos.min(self.apps.len()), Self::folder_key(id));
        self.folders.push(FolderCfg { id, name: "Folder".into(), apps: vec![b.to_string(), a.to_string()] });
        id
    }
}

/// Move `key` within `list` so it lands where `to` currently is.
pub fn move_to(list: &mut Vec<String>, key: &str, to: &str) {
    let (Some(from), Some(dest)) = (list.iter().position(|l| l == key), list.iter().position(|l| l == to)) else { return };
    let k = list.remove(from);
    list.insert(dest, k);
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct DockCfg {
    /// Icon size in logical pixels.
    pub icon_size: f32,
    /// Maximum scale of the icon under the cursor (1.0 disables magnification).
    pub magnification: f32,
    pub autohide: bool,
    /// Executable paths, .lnk paths, or `shell:AppsFolder\<AppUserModelID>`.
    pub pinned: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MenuBarCfg {
    pub enabled: bool,
    pub height: f32,
    /// Use 24-hour clock.
    pub clock_24h: bool,
    /// Show other apps' notification-area icons in the menu bar.
    pub tray_icons: bool,
    /// Executable names whose tray icons are hidden.
    pub tray_hidden: Vec<String>,
    /// Show "85%" next to the battery icon.
    pub battery_percent: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct SpotlightCfg {
    pub enabled: bool,
    /// "alt+space", "ctrl+space" or "win+space"-style hotkey.
    pub hotkey: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct TrafficCfg {
    pub enabled: bool,
    /// Executable file names (lowercase) that never get the overlay.
    pub exclude: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct WallpaperCfg {
    /// "off", "shader" or "video".
    pub mode: String,
    /// Built-in shader name: "aurora", "dunes" or "tide".
    pub shader: String,
    /// Path to an MP4/MOV file for video mode.
    pub video: String,
    pub pause_on_battery: bool,
    /// How fast animated wallpapers move: 1 = very calm, 5 = normal, 12 = lively.
    pub speed: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dock: DockCfg::default(),
            menubar: MenuBarCfg::default(),
            spotlight: SpotlightCfg::default(),
            traffic_lights: TrafficCfg::default(),
            wallpaper: WallpaperCfg::default(),
            launchpad: LaunchpadCfg::default(),
        }
    }
}

impl Default for DockCfg {
    fn default() -> Self {
        Self {
            icon_size: 48.0,
            magnification: 1.7,
            autohide: false,
            pinned: vec![
                r"C:\Windows\explorer.exe".into(),
                r"shell:AppsFolder\MSEdge".into(),
                r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into(),
                r"shell:AppsFolder\Microsoft.WindowsStore_8wekyb3d8bbwe!App".into(),
                r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel".into(),
            ],
        }
    }
}

impl Default for MenuBarCfg {
    fn default() -> Self {
        Self { enabled: true, height: 28.0, clock_24h: true, tray_icons: true, tray_hidden: Vec::new(), battery_percent: false }
    }
}

impl Default for SpotlightCfg {
    fn default() -> Self {
        Self { enabled: true, hotkey: "alt+space".into() }
    }
}

impl Default for TrafficCfg {
    fn default() -> Self {
        Self { enabled: true, exclude: Vec::new() }
    }
}

impl Default for WallpaperCfg {
    fn default() -> Self {
        Self { mode: "shader".into(), shader: "aurora".into(), video: String::new(), pause_on_battery: true, speed: 5.0 }
    }
}

pub fn path() -> PathBuf {
    util::data_dir().join("config.toml")
}

impl Config {
    /// Load the config, writing the defaults on first run.
    pub fn load() -> Self {
        match std::fs::read_to_string(path()) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    util::log(&format!("config.toml invalid, using defaults: {e}"));
                    Self::default()
                }
            },
            Err(_) => {
                let cfg = Self::default();
                cfg.save();
                cfg
            }
        }
    }

    pub fn save(&self) {
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path(), text);
        }
    }
}

/// Part of the configuration that changed, so only that part is rebuilt.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Section {
    Dock,
    MenuBar,
    TrayFilter,
    Spotlight,
    Traffic,
    Wallpaper,
    All,
}
