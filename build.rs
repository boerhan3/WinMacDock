use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("WinMacDock").dpi_awareness(DpiAwareness::PerMonitorV2)).expect("unable to embed manifest");

        // App icon and the version details shown in Explorer / Task Manager.
        let mut res = winresource::WindowsResource::new();
        // Complete version details: security software trusts files that say what they are.
        res.set_icon("assets/app.ico")
            .set("FileDescription", "WinMacDock - macOS-style Dock and menu bar for Windows")
            .set("ProductName", "WinMacDock")
            .set("CompanyName", "WinMacDock")
            .set("LegalCopyright", "Copyright (c) 2026 WinMacDock")
            .set("OriginalFilename", "winmacdock.exe")
            .set("InternalName", "winmacdock")
            .set("Comments", "Desktop shell add-on: Dock, menu bar, Launchpad, live wallpaper");
        res.compile().expect("unable to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
