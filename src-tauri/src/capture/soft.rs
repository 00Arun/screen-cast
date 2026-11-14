//! Software video path used on Windows and Linux (and testable on macOS):
//! BGRA frame -> crop to the browser area -> downscale -> I420 -> OpenH264 -> hub.
//! Also repeats the last frame while the picture is static so viewers who join late still
//! get a picture and key frames keep flowing.

#![allow(dead_code)]

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use openh264::{
    encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, UsageType},
    formats::YUVBuffer,
    OpenH264API,
};

use crate::hub::Hub;

pub const FPS: u32 = 30;
/// Software encoding is CPU bound: cap at 720p for smooth 30 fps on ordinary machines.
const MAX_W: usize = 1280;
const MAX_H: usize = 720;
const BITRATE: u32 = 4_000_000;

/// One captured frame in BGRA (or BGRX) byte order.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    /// Region (x, y, w, h) in pixels to send — the browser area below the toolbar.
    pub crop: (usize, usize, usize, usize),
}

struct State {
    enc: Option<(Encoder, usize, usize)>,
    yuv: Vec<u8>,
    last: Option<(Frame, Instant)>,
}

pub struct SoftPipeline {
    hub: Arc<Hub>,
    state: Mutex<State>,
    pub running: AtomicBool,
}

impl SoftPipeline {
    pub fn new(hub: Arc<Hub>) -> Arc<Self> {
        let p = Arc::new(Self {
            hub,
            state: Mutex::new(State { enc: None, yuv: Vec::new(), last: None }),
            running: AtomicBool::new(true),
        });
        let idle = p.clone();
        std::thread::spawn(move || {
            while idle.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                idle.tick();
            }
        });
        p
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn push(&self, frame: Frame) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        let mut st = self.state.lock().unwrap();
        self.encode(&mut st, &frame);
        st.last = Some((frame, Instant::now()));
    }

    fn tick(&self) {
        let mut st = self.state.lock().unwrap();
        let stale = st.last.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_millis(500));
        if stale || (self.hub.keyframe_pending() && st.last.is_some()) {
            let (frame, _) = st.last.take().unwrap();
            self.encode(&mut st, &frame);
            st.last = Some((frame, Instant::now()));
        }
    }

    fn encode(&self, st: &mut State, f: &Frame) {
        let (cx, cy, cw, ch) = clamp_crop(f);
        if cw < 16 || ch < 16 {
            return;
        }
        let scale = (MAX_W as f64 / cw as f64).min(MAX_H as f64 / ch as f64).min(1.0);
        let ow = ((cw as f64 * scale) as usize / 2 * 2).max(16);
        let oh = ((ch as f64 * scale) as usize / 2 * 2).max(16);

        let mut fresh = false;
        if st.enc.as_ref().map_or(true, |(_, w, h)| *w != ow || *h != oh) {
            let cfg = EncoderConfig::new()
                .bitrate(BitRate::from_bps(BITRATE))
                .max_frame_rate(FrameRate::from_hz(FPS as f32))
                .usage_type(UsageType::CameraVideoRealTime)
                .complexity(Complexity::Low)
                .intra_frame_period(IntraFramePeriod::from_num_frames(FPS * 2))
                .skip_frames(false);
            match Encoder::with_api_config(OpenH264API::from_source(), cfg) {
                Ok(e) => {
                    st.enc = Some((e, ow, oh));
                    fresh = true;
                }
                Err(e) => {
                    eprintln!("openh264: {e}");
                    return;
                }
            }
        }

        bgra_to_i420(f, (cx, cy, cw, ch), ow, oh, &mut st.yuv);
        let key = self.hub.take_keyframe_request() || fresh;
        let (enc, _, _) = st.enc.as_mut().unwrap();
        if key {
            enc.force_intra_frame();
        }
        let buf = YUVBuffer::from_vec(std::mem::take(&mut st.yuv), ow, oh);
        match enc.encode(&buf) {
            Ok(bs) => {
                let is_key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
                let data = bs.to_vec();
                if !data.is_empty() {
                    self.hub.push_video(data, is_key);
                }
            }
            Err(e) => eprintln!("openh264 encode: {e}"),
        }
        // Reuse the allocation next time.
        st.yuv = Vec::new();
    }
}

fn clamp_crop(f: &Frame) -> (usize, usize, usize, usize) {
    let (x, y, w, h) = f.crop;
    let x = x.min(f.width);
    let y = y.min(f.height);
    (x, y, w.min(f.width - x), h.min(f.height - y))
}

/// Crop + box-filtered downscale + BT.601 limited-range conversion to planar I420.
pub fn bgra_to_i420(f: &Frame, crop: (usize, usize, usize, usize), ow: usize, oh: usize, out: &mut Vec<u8>) {
    let (cx, cy, cw, ch) = crop;
    out.clear();
    out.resize(ow * oh * 3 / 2, 0);
    let (yp, rest) = out.split_at_mut(ow * oh);
    let (up, vp) = rest.split_at_mut(ow * oh / 4);

    // Source pixel (averaged over the box that maps to output pixel ox, oy).
    let px = |ox: usize, oy: usize| -> (u32, u32, u32) {
        let x0 = cx + ox * cw / ow;
        let x1 = (cx + (ox + 1) * cw / ow).max(x0 + 1).min(cx + cw);
        let y0 = cy + oy * ch / oh;
        let y1 = (cy + (oy + 1) * ch / oh).max(y0 + 1).min(cy + ch);
        // Sample at most 2x2 points of the box: good quality, bounded cost.
        let xs = [x0, (x0 + x1 - 1) / 2 + ((x1 - x0) > 1) as usize].map(|v| v.min(x1 - 1));
        let ys = [y0, (y0 + y1 - 1) / 2 + ((y1 - y0) > 1) as usize].map(|v| v.min(y1 - 1));
        let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
        for &sy in &ys {
            let row = sy * f.stride;
            for &sx in &xs {
                let i = row + sx * 4;
                b += f.data[i] as u32;
                g += f.data[i + 1] as u32;
                r += f.data[i + 2] as u32;
            }
        }
        (r / 4, g / 4, b / 4)
    };

    for oy in (0..oh).step_by(2) {
        for ox in (0..ow).step_by(2) {
            let mut su = 0i32;
            let mut sv = 0i32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (r, g, b) = px(ox + dx, oy + dy);
                let (r, g, b) = (r as i32, g as i32, b as i32);
                yp[(oy + dy) * ow + ox + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
                su += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                sv += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            }
            let ci = (oy / 2) * (ow / 2) + ox / 2;
            up[ci] = (su / 4).clamp(0, 255) as u8;
            vp[ci] = (sv / 4).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_path_produces_decodable_h264() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_video_for_test();
        let p = SoftPipeline::new(hub.clone());
        let (w, h) = (1600usize, 1000usize);
        for t in 0..30 {
            let mut data = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    data[i] = ((x + t * 8) % 256) as u8;
                    data[i + 1] = (y % 256) as u8;
                    data[i + 2] = 128;
                    data[i + 3] = 255;
                }
            }
            // 56 px "toolbar" at the top is cropped away.
            p.push(Frame { data, width: w, height: h, stride: w * 4, crop: (0, 56, w, h - 56) });
        }
        p.stop();
        let mut out = Vec::new();
        let mut keys = 0;
        while let Ok(pkt) = rx.try_recv() {
            if pkt.key {
                keys += 1;
            }
            out.extend_from_slice(&pkt.data);
        }
        assert!(keys >= 1, "no key frame");
        assert!(out.starts_with(&[0, 0, 0, 1]) || out.starts_with(&[0, 0, 1]));
        if let Ok(path) = std::env::var("SC_SOFT_OUT") {
            std::fs::write(path, out).unwrap();
        }
    }
}
