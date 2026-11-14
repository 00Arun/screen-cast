fn main() {
    // The ScreenCaptureKit bindings contain Swift code; point the linker at the Swift runtime
    // support libraries of whichever toolchain (Xcode or Command Line Tools) is installed.
    if let Ok(out) = std::process::Command::new("xcrun").args(["-f", "swiftc"]).output() {
        let swiftc = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some(bin) = std::path::Path::new(&swiftc).parent() {
            let lib = bin.join("../lib/swift/macosx");
            if lib.exists() {
                println!("cargo:rustc-link-search=native={}", lib.display());
            }
        }
    }

    // Every app command requires an explicit permission; only the toolbar capability grants them.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_state", "navigate", "go_back", "go_forward", "reload", "toggle_cast", "new_pin",
            "show_qr", "qr_info", "show_about", "about_info", "open_developer_site",
        ]),
    ))
    .expect("failed to run tauri-build");
}
