//! Native ad / tracker / malware / local-network blocking inside the browser engine.
//!
//! | OS      | Mechanism                                                                |
//! |---------|--------------------------------------------------------------------------|
//! | macOS   | WebKit content-rule lists (compiled from the filter lists, cached)       |
//! | Windows | WebView2 `WebResourceRequested`: every request checked by the engine     |
//! | Linux   | WebKitGTK user content filters (same rule format as macOS)               |

use std::sync::Arc;

use crate::adblock::Blocker;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod linux;

pub fn install(webview: &tauri::Webview, blocker: Arc<Blocker>) {
    #[cfg(target_os = "macos")]
    {
        let _ = blocker;
        macos::install(webview);
    }
    #[cfg(windows)]
    windows::install(webview, blocker);
    #[cfg(target_os = "linux")]
    {
        let _ = blocker;
        linux::install(webview);
    }
}
