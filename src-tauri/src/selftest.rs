//! `screen_cast --selftest [out.h264]`: end-to-end check of the media pipeline without the
//! GUI or screen-recording permission. Synthetic frames -> VideoToolbox H.264 -> Opus ->
//! WebRTC hub -> a webrtc-rs client acting as a phone, which saves the received video.

use std::{
    ffi::c_void,
    fs::File,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use rtc::{
    interceptor::Registry,
    media::io::{h26x_writer::H26xWriter, Writer},
    peer_connection::{
        configuration::{
            interceptor_registry::register_default_interceptors,
            media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS},
            RTCConfigurationBuilder,
        },
        sdp::RTCSessionDescription,
    },
    rtp_transceiver::{
        rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind},
        RTCRtpTransceiverDirection, RTCRtpTransceiverInit,
    },
};
use webrtc::{
    media_stream::track_remote::{TrackRemote, TrackRemoteEvent},
    peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState},
    runtime::default_runtime,
};

use crate::{audio::OpusEncoder, encoder::*, hub::Hub, server::lan_ip};

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(a: CFTypeRef, w: usize, h: usize, fmt: u32, attrs: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CFTypeRef, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CFTypeRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: CFTypeRef, plane: usize) -> usize;
}

fn frame(w: usize, h: usize, t: usize) -> CFTypeRef {
    unsafe {
        let mut pb: CFTypeRef = std::ptr::null();
        CVPixelBufferCreate(std::ptr::null(), w, h, u32::from_be_bytes(*b"420v"), std::ptr::null(), &mut pb);
        CVPixelBufferLockBaseAddress(pb, 0);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            let rows = CVPixelBufferGetHeightOfPlane(pb, plane);
            for y in 0..rows {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                for (x, p) in row.iter_mut().enumerate() {
                    *p = if plane == 0 { ((x + y + t * 4) % 220 + 16) as u8 } else { 128 };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, 0);
        pb
    }
}

struct Client {
    gathered: tokio::sync::mpsc::UnboundedSender<()>,
    video: Arc<AtomicUsize>,
    audio: Arc<AtomicUsize>,
    writer: Arc<Mutex<Option<H26xWriter<File>>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Client {
    async fn on_ice_gathering_state_change(&self, s: RTCIceGatheringState) {
        if s == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(());
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let kind = track.kind().await;
        let counter = if kind == RtpCodecKind::Video { self.video.clone() } else { self.audio.clone() };
        let writer = self.writer.clone();
        tokio::spawn(async move {
            while let Some(evt) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(p) = evt {
                    counter.fetch_add(1, Ordering::Relaxed);
                    if kind == RtpCodecKind::Video {
                        if let Some(w) = writer.lock().unwrap().as_mut() {
                            let _ = w.write_rtp(&p);
                        }
                    }
                }
            }
        });
    }
}

pub async fn run(out_path: String) -> Result<(), String> {
    let hub = Hub::new();
    hub.set_live(true);

    // Producer: 30 fps synthetic video + 440 Hz tone.
    let h = hub.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        let hub2 = h.clone();
        let mut enc = H264Encoder::new(1280, 720, 30, 4_000_000, Box::new(move |d, k| hub2.push_video(d, k))).unwrap();
        let mut opus = OpusEncoder::new().unwrap();
        let mut phase = 0f32;
        for t in 0..(30 * 8) {
            let pb = frame(1280, 720, t);
            enc.encode(pb, start.elapsed().as_micros() as i64, h.take_keyframe_request());
            unsafe { CFRelease(pb) };
            let mut pcm = Vec::with_capacity(1600 * 2);
            for _ in 0..1600 {
                let s = (phase * std::f32::consts::TAU).sin() * 0.2;
                phase = (phase + 440.0 / 48_000.0) % 1.0;
                pcm.push(s);
                pcm.push(s);
            }
            for pkt in opus.push(&pcm) {
                h.push_audio(pkt);
            }
            std::thread::sleep(Duration::from_millis(33));
        }
    });

    // Consumer: a receive-only client, like the viewer page in a phone browser.
    let e = |e: rtc::shared::error::Error| e.to_string();
    let mut media = MediaEngine::default();
    media
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.into(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )
        .map_err(e)?;
    media
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.into(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RtpCodecKind::Audio,
        )
        .map_err(e)?;
    let registry = register_default_interceptors(Registry::new(), &mut media).map_err(e)?;
    let (gtx, mut grx) = tokio::sync::mpsc::unbounded_channel();
    let video = Arc::new(AtomicUsize::new(0));
    let audio = Arc::new(AtomicUsize::new(0));
    let writer = Arc::new(Mutex::new(Some(H26xWriter::new(File::create(&out_path).map_err(|e| e.to_string())?, false))));
    let ip = lan_ip().ok_or("no LAN ip")?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Client { gathered: gtx, video: video.clone(), audio: audio.clone(), writer: writer.clone() }))
        .with_runtime(default_runtime().ok_or("no runtime")?)
        .with_udp_addrs(vec![format!("{ip}:0")])
        .build()
        .await
        .map_err(e)?;
    for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
        pc.add_transceiver_from_kind(
            kind,
            Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default() }),
        )
        .await
        .map_err(e)?;
    }
    let offer = pc.create_offer(None).await.map_err(e)?;
    pc.set_local_description(offer).await.map_err(e)?;
    let _ = tokio::time::timeout(Duration::from_secs(3), grx.recv()).await;
    let offer_sdp = pc.local_description().await.ok_or("no offer")?.sdp;

    let t0 = Instant::now();
    let answer = hub.answer("selftest".into(), offer_sdp).await?;
    println!("answer ready in {:?}, viewers = {}", t0.elapsed(), hub.viewer_count());
    pc.set_remote_description(RTCSessionDescription::answer(answer).map_err(e)?).await.map_err(e)?;

    tokio::time::sleep(Duration::from_secs(6)).await;
    let (v, a) = (video.load(Ordering::Relaxed), audio.load(Ordering::Relaxed));
    println!("received RTP packets: video = {v}, audio = {a}");
    if let Some(mut w) = writer.lock().unwrap().take() {
        let _ = w.close();
    }
    let _ = pc.close().await;
    hub.set_live(false);
    if v == 0 || a == 0 {
        return Err("no media received".into());
    }
    println!("saved received video to {out_path}");
    Ok(())
}
