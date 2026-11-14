//! Linux: capture this app's own X11 window (the app forces GDK's X11 backend, so this also
//! works on Wayland desktops through XWayland), crop to the browser area, encode with
//! OpenH264; audio from the PulseAudio / PipeWire monitor of the default output.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use x11rb::{
    connection::Connection,
    protocol::{
        composite::{self, ConnectionExt as _},
        xproto::{ConnectionExt as _, ImageFormat},
    },
};

use super::{
    soft::{Frame, SoftPipeline, FPS},
    Target,
};
use crate::{audio::AudioSink, hub::Hub};

pub struct Capture {
    pipeline: Arc<SoftPipeline>,
    running: Arc<AtomicBool>,
}

impl Capture {
    pub fn start(hub: Arc<Hub>, target: &Target, _win_w: f64, _win_h: f64, toolbar: f64) -> Result<Self, String> {
        let (conn, _) = x11rb::connect(None).map_err(|e| format!("X11: {e}"))?;
        let xid = target.xid;
        // Make sure the window has an off-screen buffer we can read (a no-op if the
        // desktop already composites).
        conn.composite_query_version(0, 4).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        conn.composite_redirect_window(xid, composite::Redirect::AUTOMATIC).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        let toolbar_px = (toolbar * target.scale).round() as usize;
        let pipeline = SoftPipeline::new(hub.clone());
        let running = Arc::new(AtomicBool::new(true));

        {
            let pipeline = pipeline.clone();
            let running = running.clone();
            std::thread::spawn(move || {
                let interval = Duration::from_secs_f64(1.0 / FPS as f64);
                while running.load(Ordering::Relaxed) {
                    let t0 = Instant::now();
                    match grab(&conn, xid) {
                        Ok((data, w, h)) => {
                            let crop = (0, toolbar_px, w, h.saturating_sub(toolbar_px));
                            pipeline.push(Frame { data, width: w, height: h, stride: w * 4, crop });
                        }
                        Err(e) => {
                            eprintln!("X11 capture: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                    if let Some(rest) = interval.checked_sub(t0.elapsed()) {
                        std::thread::sleep(rest);
                    }
                }
            });
        }

        start_monitor_audio(hub, running.clone());
        Ok(Self { pipeline, running })
    }

    pub fn stop(self) {
        self.running.store(false, Ordering::Relaxed);
        self.pipeline.stop();
    }
}

/// One frame of the window as BGRX bytes.
fn grab(conn: &impl Connection, xid: u32) -> Result<(Vec<u8>, usize, usize), String> {
    let geo = conn.get_geometry(xid).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    let pixmap = conn.generate_id().map_err(|e| e.to_string())?;
    conn.composite_name_window_pixmap(xid, pixmap).map_err(|e| e.to_string())?;
    let image = conn
        .get_image(ImageFormat::Z_PIXMAP, pixmap, 0, 0, geo.width, geo.height, !0)
        .map_err(|e| e.to_string())?
        .reply();
    let _ = conn.free_pixmap(pixmap);
    let image = image.map_err(|e| e.to_string())?;
    let (w, h) = (geo.width as usize, geo.height as usize);
    if image.depth < 24 || image.data.len() < w * h * 4 {
        return Err(format!("unsupported window format (depth {})", image.depth));
    }
    Ok((image.data, w, h))
}

/// Records what the speakers play (default sink monitor) via PulseAudio or PipeWire-Pulse.
fn start_monitor_audio(hub: Arc<Hub>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        use libpulse_binding::{sample::{Format, Spec}, stream::Direction};
        use libpulse_simple_binding::Simple;

        let spec = Spec { format: Format::F32le, channels: 2, rate: 48_000 };
        let simple = match Simple::new(
            None,
            "Screen Cast",
            Direction::Record,
            Some("@DEFAULT_MONITOR@"),
            "cast audio",
            &spec,
            None,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("audio capture disabled: {e}");
                return;
            }
        };
        let Ok(mut sink) = AudioSink::new(hub) else { return };
        let mut bytes = vec![0u8; 960 * 2 * 4]; // 20 ms
        let mut samples = vec![0f32; 960 * 2];
        while running.load(Ordering::Relaxed) {
            if simple.read(&mut bytes).is_err() {
                break;
            }
            for (i, c) in bytes.chunks_exact(4).enumerate() {
                samples[i] = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            }
            sink.push(&samples, 2, 48_000);
        }
    });
}
