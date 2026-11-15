//! Installed applications, as listed in the shell's AppsFolder (covers
//! classic desktop apps and Store apps alike).

use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::*;

use crate::util::from_wide;

pub struct AppEntry {
    pub name: String,
    pub lower: String,
    /// `shell:AppsFolder\<AppUserModelID>`
    pub launch: String,
}

pub fn list() -> Vec<AppEntry> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) else { return out };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None::<&IBindCtx>, &BHID_EnumItems) else { return out };
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            if items.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else { break };
            let text = |kind| {
                item.GetDisplayName(kind).ok().map(|p| {
                    let s = from_wide(p.as_wide());
                    CoTaskMemFree(Some(p.0 as *const _));
                    s
                })
            };
            let (Some(name), Some(id)) = (text(SIGDN_NORMALDISPLAY), text(SIGDN_PARENTRELATIVEPARSING)) else { continue };
            let lower = name.to_lowercase();
            let id_lower = id.to_lowercase();
            // Start-menu clutter: uninstallers, manuals, web links.
            let junk = lower.contains("uninstall")
                || [".url", ".chm", ".txt", ".htm", ".html", ".pdf", ".rtf", ".ini"].iter().any(|ext| id_lower.ends_with(ext));
            if name.is_empty() || junk {
                continue;
            }
            out.push(AppEntry { name, lower, launch: format!("shell:AppsFolder\\{id}") });
        }
    }
    out.sort_by(|a, b| a.lower.cmp(&b.lower));
    out.dedup_by(|a, b| a.lower == b.lower);
    out
}

/// Fuzzy match score of `query` (lowercase) against a lowercase name; higher is better.
pub fn score(query: &str, name: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let len = name.chars().count() as i32;
    if name.starts_with(query) {
        return Some(1000 - len);
    }
    if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(query)) {
        return Some(700 - len);
    }
    if let Some(pos) = name.find(query) {
        return Some(400 - pos as i32);
    }
    // Subsequence: every query character appears in order.
    let mut chars = name.chars();
    query.chars().all(|q| chars.any(|c| c == q)).then_some(100 - len)
}
