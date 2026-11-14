//! Captures ONLY this app's own window (the browser area below the toolbar) with
//! ScreenCaptureKit, encodes it with VideoToolbox H.264 + Opus and hands the packets to the
//! WebRTC hub. Other apps, windows, notifications and the rest of the desktop are never captured.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use screencapturekit::{
    cg::CGRect,
    prelude::*,
    stream::configuration::pixel_format::PixelFormat,
};

use crate::{
    audio::{self, OpusEncoder},
    encoder::{
        CFRelease, CFRetain, CFTypeRef, CMSampleBufferGetImageBuffer, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, H264Encoder,
    },
    hub::Hub,
};

const FPS: u32 = 30;
const MAX_W: f64 = 1920.0;
const MAX_H: f64 = 1080.0;
const BITRATE: i32 = 6_000_000;

/// A retained CVPixelBuffer.
struct PixelBuf(CFTypeRef);
unsafe impl Send for PixelBuf {}
impl Drop for PixelBuf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

struct Video {
    enc: Option<H264Encoder>,
    last: Option<(PixelBuf, Instant)>,
    last_pts: i64,
}

struct Pipeline {
    hub: Arc<Hub>,
    video: Mutex<Video>,
    audio: Mutex<Option<OpusEncoder>>,
    start: Instant,
    running: AtomicBool,
}

impl Pipeline {
    fn encode(&self, v: &mut Video, pb: CFTypeRef) {
        let (w, h) = unsafe { (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb)) };
        if w < 16 || h < 16 {
            return;
        }
        let mut fresh = false;
        if v.enc.as_ref().map_or(true, |e| e.width != w || e.height != h) {
            v.enc = None; // flush the old one first
            let hub = self.hub.clone();
            match H264Encoder::new(w, h, FPS, BITRATE, Box::new(move |data, key| hub.push_video(data, key))) {
                Ok(e) => {
                    v.enc = Some(e);
                    fresh = true;
                }
                Err(e) => {
                    eprintln!("{e}");
                    return;
                }
            }
        }
        let key = self.hub.take_keyframe_request() || fresh;
        let mut pts = self.start.elapsed().as_micros() as i64;
        if pts <= v.last_pts {
            pts = v.last_pts + 1;
        }
        v.last_pts = pts;
        if let Some(enc) = v.enc.as_mut() {
            enc.encode(pb, pts, key);
        }
    }

    fn on_video(&self, sb: CFTypeRef) {
        let pb = unsafe { CMSampleBufferGetImageBuffer(sb) };
        if pb.is_null() {
            return; // idle / blank frame: nothing changed on screen
        }
        let pb = PixelBuf(unsafe { CFRetain(pb) });
        let mut v = self.video.lock().unwrap();
        self.encode(&mut v, pb.0);
        v.last = Some((pb, Instant::now()));
    }

    /// When the picture is static ScreenCaptureKit sends no frames; repeat the last one so
    /// newly joined viewers get a picture and key frames keep flowing.
    fn on_idle_tick(&self) {
        let mut v = self.video.lock().unwrap();
        let stale = v.last.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_millis(500));
        if stale || (self.hub.keyframe_pending() && v.last.is_some()) {
            let pb = v.last.as_ref().map(|(p, _)| unsafe { CFRetain(p.0) }).unwrap();
            let pb = PixelBuf(pb);
            self.encode(&mut v, pb.0);
            if let Some((_, t)) = v.last.as_mut() {
                *t = Instant::now();
            }
        }
    }

    fn on_audio(&self, sb: CFTypeRef) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static PEAK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pcm = audio::pcm_from_sample(sb);
        let p = pcm.as_ref().map(|p| p.iter().fold(0f32, |m, v| m.max(v.abs()))).unwrap_or(0.0);
        PEAK.fetch_max((p * 1000.0) as u32, Ordering::Relaxed);
        if n % 200 == 0 {
            let peak = PEAK.swap(0, Ordering::Relaxed) as f32 / 1000.0;
            dbg_log(&format!("audio buffers={n} samples={:?} max_peak={peak:.3}", pcm.as_ref().map(|p| p.len())));
        }
        let Some(pcm) = pcm else { return };
        let mut a = self.audio.lock().unwrap();
        if a.is_none() {
            match OpusEncoder::new() {
                Ok(e) => *a = Some(e),
                Err(e) => {
                    eprintln!("{e}");
                    return;
                }
            }
        }
        for pkt in a.as_mut().unwrap().push(&pcm) {
            self.hub.push_audio(pkt);
        }
    }
}

struct Handler(Arc<Pipeline>);

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !self.0.running.load(Ordering::Relaxed) {
            return;
        }
        let sb = sample.as_ptr() as CFTypeRef;
        match of_type {
            SCStreamOutputType::Screen => self.0.on_video(sb),
            SCStreamOutputType::Audio => self.0.on_audio(sb),
            _ => {}
        }
    }
}

fn dbg_log(msg: &str) {
    if std::env::var_os("SC_DEBUG").is_some() {
        eprintln!("[capture] {msg}");
    }
}

fn even(v: f64) -> u32 {
    ((v.round() as u32) / 2 * 2).max(16)
}

fn make_config(win_w: f64, win_h: f64, toolbar: f64) -> SCStreamConfiguration {
    let cw = win_w.max(16.0);
    let ch = (win_h - toolbar).max(16.0);
    // Capture at up to 2x (Retina) detail, capped at 1080p, keeping aspect ratio.
    let scale = (MAX_W / cw).min(MAX_H / ch).min(2.0);
    SCStreamConfiguration::new()
        .with_width(even(cw * scale))
        .with_height(even(ch * scale))
        .with_source_rect(CGRect::new(0.0, toolbar, cw, ch))
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_fps(FPS)
        .with_queue_depth(8)
        .with_shows_cursor(false)
        .with_scales_to_fit(true)
        .with_captures_audio(false)
}

/// Audio-only stream. WebKit plays sound from its own helper process (not this app's
/// window), so window-scoped audio is silent. Instead capture the display's audio while
/// EXCLUDING every other app: sound from Music, Spotify, other browsers, etc. is not cast.
fn audio_config() -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_fps(1)
        .with_queue_depth(3)
        .with_shows_cursor(false)
        .with_captures_audio(true)
        .with_sample_rate(audio::SAMPLE_RATE as i32)
        .with_channel_count(2)
        .with_excludes_current_process_audio(false)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub struct Capture {
    streams: Arc<Mutex<Vec<SCStream>>>,
    pipeline: Arc<Pipeline>,
}

impl Capture {
    pub fn start(hub: Arc<Hub>, target: &super::Target, win_w: f64, win_h: f64, toolbar: f64) -> Result<Self, String> {
        const PERMISSION: &str = "Screen Cast needs permission: System Settings → Privacy & Security → \
Screen & System Audio Recording → turn on Screen Cast, then quit and reopen the app.";

        // Check first so macOS shows its permission prompt once, not on every Cast click.
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            unsafe { CGRequestScreenCaptureAccess() };
            return Err(PERMISSION.to_string());
        }
        let content = SCShareableContent::get().map_err(|_| PERMISSION.to_string())?;
        let pid = std::process::id() as i32;
        let ours: Vec<SCWindow> = content
            .windows()
            .into_iter()
            .filter(|w| w.owning_application().is_some_and(|a| a.process_id() == pid))
            .collect();
        let window = ours
            .iter()
            .find(|w| w.title().as_deref() == Some(target.title.as_str()))
            .or_else(|| {
                ours.iter().max_by(|a, b| {
                    let area = |w: &SCWindow| w.frame().size.width * w.frame().size.height;
                    area(a).total_cmp(&area(b))
                })
            })
            .ok_or_else(|| "Could not find the Screen Cast window to capture".to_string())?;

        let pipeline = Arc::new(Pipeline {
            hub,
            video: Mutex::new(Video { enc: None, last: None, last_pts: 0 }),
            audio: Mutex::new(None),
            start: Instant::now(),
            running: AtomicBool::new(true),
        });

        // Video: only this window, cropped to the browser area.
        let filter = SCContentFilter::create()
            .with_window(window)
            .build()
            .map_err(|e| format!("capture filter: {e:?}"))?;
        let mut video = SCStream::new(&filter, &make_config(win_w, win_h, toolbar))
            .map_err(|e| format!("capture: {e:?}"))?;
        video
            .add_output_handler(Handler(pipeline.clone()), SCStreamOutputType::Screen)
            .map_err(|e| format!("capture: {e:?}"))?;
        video.start_capture().map_err(|_| PERMISSION.to_string())?;
        let mut streams = vec![video];

        // Audio: display audio minus every other application.
        let wf = window.frame();
        let (cx, cy) = (wf.origin.x + wf.size.width / 2.0, wf.origin.y + wf.size.height / 2.0);
        let displays = content.displays();
        let display = displays
            .iter()
            .find(|d| {
                let f = d.frame();
                cx >= f.origin.x && cx < f.origin.x + f.size.width && cy >= f.origin.y && cy < f.origin.y + f.size.height
            })
            .or(displays.first());
        if let Some(display) = display {
            let apps = content.applications();
            let others: Vec<&SCRunningApplication> = apps.iter().filter(|a| a.process_id() != pid).collect();
            for a in &apps {
                dbg_log(&format!("app pid={} {} [{}]", a.process_id(), a.application_name(), a.bundle_identifier()));
            }
            let audio_filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .with_excluding_applications(&others, &[])
                .build();
            match audio_filter.and_then(|f| SCStream::new(&f, &audio_config())) {
                Ok(mut a) => {
                    if a.add_output_handler(Handler(pipeline.clone()), SCStreamOutputType::Audio).is_ok()
                        && a.start_capture().is_ok()
                    {
                        dbg_log("audio stream started");
                        streams.push(a);
                    } else {
                        eprintln!("audio capture could not start");
                    }
                }
                Err(e) => eprintln!("audio capture: {e:?}"),
            }
        }

        let p = pipeline.clone();
        std::thread::spawn(move || {
            while p.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                p.on_idle_tick();
            }
        });

        Ok(Self { streams: Arc::new(Mutex::new(streams)), pipeline })
    }

    pub fn stop(self) {
        self.pipeline.running.store(false, Ordering::Relaxed);
        let streams = self.streams.clone();
        std::thread::spawn(move || {
            for s in streams.lock().unwrap().iter() {
                let _ = s.stop_capture();
            }
        });
    }
}
