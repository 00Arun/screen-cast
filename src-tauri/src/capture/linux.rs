//! Linux: capture this app's own X11 window (the app forces GDK's X11 backend, so this also
//! works on Wayland desktops through XWayland), crop to the browser area, encode with
//! OpenH264; audio only from this app's own playback streams (PulseAudio / PipeWire-Pulse
//! per-stream monitor), never other apps.

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

        start_app_audio(hub, running.clone());
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

/// `true` if `pid` is this process or one of its descendants (WebKit's web/GPU processes).
fn in_our_tree(pid: u32) -> bool {
    let me = std::process::id();
    let mut p = pid;
    for _ in 0..32 {
        if p == me {
            return true;
        }
        if p <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else { return false };
        // Fields after the ")" of the command name: state ppid ...
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { return false };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|v| v.parse().ok()) else { return false };
        p = ppid;
    }
    false
}

/// Newest playback stream ("sink input") that belongs to this app, from `pactl`.
fn our_sink_input() -> Option<u32> {
    let out = std::process::Command::new("pactl").args(["list", "sink-inputs"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best = None;
    for block in text.split("Sink Input #").skip(1) {
        let Some(index) = block.lines().next().and_then(|l| l.trim().parse::<u32>().ok()) else { continue };
        let ours = block.lines().any(|l| {
            let l = l.trim();
            ["application.process.id = ", "pipewire.sec.pid = "].iter().any(|key| {
                l.strip_prefix(key)
                    .and_then(|v| v.trim_matches('"').parse::<u32>().ok())
                    .is_some_and(in_our_tree)
            })
        });
        if ours {
            best = Some(best.map_or(index, |b: u32| b.max(index)));
        }
    }
    best
}

/// Records only this app's playback streams with `parec --monitor-stream`, so other apps'
/// sound (calls, music, notifications) is never cast. Follows new streams as they appear.
fn start_app_audio(hub: Arc<Hub>, running: Arc<AtomicBool>) {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;

    std::thread::spawn(move || {
        let Ok(mut sink) = AudioSink::new(hub) else { return };
        let mut current: Option<(u32, Child)> = None;
        let (tx, rx) = mpsc::channel::<(u32, Vec<f32>)>();
        let mut last_scan = Instant::now() - Duration::from_secs(10);
        while running.load(Ordering::Relaxed) {
            if last_scan.elapsed() > Duration::from_secs(1) {
                last_scan = Instant::now();
                let wanted = our_sink_input();
                if wanted != current.as_ref().map(|(i, _)| *i) {
                    if let Some((_, mut child)) = current.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    if let Some(index) = wanted {
                        let spawned = Command::new("parec")
                            .args([
                                &format!("--monitor-stream={index}"),
                                "--raw",
                                "--format=float32le",
                                "--rate=48000",
                                "--channels=2",
                                "--latency-msec=20",
                            ])
                            .stdout(Stdio::piped())
                            .stderr(Stdio::null())
                            .spawn();
                        match spawned {
                            Ok(mut child) => {
                                if let Some(mut out) = child.stdout.take() {
                                    let tx = tx.clone();
                                    std::thread::spawn(move || {
                                        let mut buf = vec![0u8; 960 * 2 * 4];
                                        while let Ok(n) = out.read(&mut buf) {
                                            if n == 0 {
                                                break;
                                            }
                                            let pcm = buf[..n - n % 4]
                                                .chunks_exact(4)
                                                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                                .collect();
                                            if tx.send((index, pcm)).is_err() {
                                                break;
                                            }
                                        }
                                    });
                                }
                                current = Some((index, child));
                            }
                            Err(e) => eprintln!("audio capture needs parec (pulseaudio-utils): {e}"),
                        }
                    }
                }
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((index, pcm)) if current.as_ref().is_some_and(|(i, _)| *i == index) => sink.push(&pcm, 2, 48_000),
                _ => {}
            }
        }
        if let Some((_, mut child)) = current.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    });
}
