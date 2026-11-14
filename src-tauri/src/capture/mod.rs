//! Captures ONLY this app's own window (the browser area below the toolbar) and feeds the
//! encoded stream to the WebRTC hub.
//!
//! | OS      | Capture                         | Video encoder        | Audio                     |
//! |---------|---------------------------------|----------------------|---------------------------|
//! | macOS   | ScreenCaptureKit (own window)   | VideoToolbox (HW)    | ScreenCaptureKit          |
//! | Windows | Windows.Graphics.Capture (HWND) | OpenH264 (software)  | WASAPI loopback           |
//! | Linux   | X11 / XWayland (own window)     | OpenH264 (software)  | PulseAudio/PipeWire monitor |

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Capture;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::Capture;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Capture;

pub mod soft;

/// Native handle of the window to capture, resolved on the UI thread.
#[derive(Clone)]
pub struct Target {
    #[allow(dead_code)]
    pub title: String,
    /// Device pixels per logical point.
    #[allow(dead_code)]
    pub scale: f64,
    #[cfg(windows)]
    pub hwnd: isize,
    #[cfg(target_os = "linux")]
    pub xid: u32,
}

pub fn target(window: &tauri::Window, title: &str) -> Result<Target, String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let hwnd = window.hwnd().map_err(|e| e.to_string())?;
        return Ok(Target { title: title.into(), scale, hwnd: hwnd.0 as isize });
    }
    #[cfg(target_os = "linux")]
    {
        // GTK objects may only be touched on the main thread.
        let (tx, rx) = std::sync::mpsc::channel();
        let w = window.clone();
        window
            .run_on_main_thread(move || {
                let xid = (|| -> Result<u32, String> {
                    use gtk::prelude::*;
                    let gtk_win = w.gtk_window().map_err(|e| e.to_string())?;
                    let gdk_win = gtk_win.window().ok_or("window not realized")?;
                    let x11 = gdk_win
                        .downcast::<gdkx11::X11Window>()
                        .map_err(|_| "Screen Cast must run on X11/XWayland to cast".to_string())?;
                    Ok(x11.xid() as u32)
                })();
                let _ = tx.send(xid);
            })
            .map_err(|e| e.to_string())?;
        let xid = rx.recv().map_err(|e| e.to_string())??;
        return Ok(Target { title: title.into(), scale, xid });
    }
    #[cfg(target_os = "macos")]
    Ok(Target { title: title.into(), scale })
}
