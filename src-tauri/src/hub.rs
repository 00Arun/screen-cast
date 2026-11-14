//! WebRTC fan-out: one encoder, many viewers. Each viewer gets its own PeerConnection
//! (DTLS-SRTP encrypted) carrying one H.264 video track and one Opus audio track.
//! Viewers are receive-only: there is no data channel and nothing they send is used.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use rtc::{
    interceptor::Registry,
    media::Sample,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            interceptor_registry::register_default_interceptors,
            media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS},
            RTCConfigurationBuilder,
        },
        sdp::RTCSessionDescription,
    },
    rtp_transceiver::rtp_sender::{
        RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
        RTCRtpEncodingParameters, RtpCodecKind,
    },
};
use rtc::statistics::{report::RTCStatsReportEntry, stats::RTCStatsType};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::{
    media_stream::{track_local::static_sample::TrackLocalStaticSample, track_local::TrackLocal},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        RTCPeerConnectionState,
    },
    rtp_transceiver::RtpSender,
    runtime::default_runtime,
};

use crate::server::lan_ip;

fn debug(msg: &str) {
    if std::env::var_os("SC_DEBUG").is_some() {
        eprintln!("[hub] {msg}");
    }
}

pub struct Packet {
    pub data: Bytes,
    pub key: bool,
    pub at: Instant,
}

type Pc = Arc<dyn PeerConnection>;

pub struct Hub {
    video: broadcast::Sender<Arc<Packet>>,
    audio: broadcast::Sender<Arc<Packet>>,
    keyframe: AtomicBool,
    live: AtomicBool,
    peers: Mutex<HashMap<String, (u64, Pc)>>,
    seq: AtomicU64,
    on_change: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

struct Events {
    gathered: mpsc::UnboundedSender<()>,
    state: watch::Sender<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug(&format!("peer state: {state}"));
        let _ = self.state.send(state);
    }
}

fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![
                RTCPFeedback { typ: "nack".into(), parameter: "".into() },
                RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
            ],
        },
        payload_type: 102,
        ..Default::default()
    }
}

fn audio_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

fn make_track(kind: RtpCodecKind, codec: &RTCRtpCodecParameters) -> Result<(Arc<TrackLocalStaticSample>, u32), String> {
    let ssrc = rand::random::<u32>();
    let t = TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "screencast".to_string(),
            format!("screencast-{kind}"),
            format!("screencast-{kind}"),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )
    .map_err(|e| e.to_string())?;
    Ok((Arc::new(t), ssrc))
}

async fn payload_type(sender: &Arc<dyn RtpSender>) -> Option<u8> {
    sender.get_parameters().await.ok()?.rtp_parameters.codecs.first().map(|c| c.payload_type)
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            video: broadcast::channel(240).0,
            audio: broadcast::channel(500).0,
            keyframe: AtomicBool::new(false),
            live: AtomicBool::new(false),
            peers: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            on_change: OnceLock::new(),
        })
    }

    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_change.set(f);
    }

    fn changed(&self) {
        if let Some(f) = self.on_change.get() {
            f();
        }
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    pub fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::Relaxed);
        if !live {
            self.close_all();
        }
    }

    pub fn viewer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    pub fn has_viewer(&self, token: &str) -> bool {
        self.peers.lock().unwrap().contains_key(token)
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }
    pub fn keyframe_pending(&self) -> bool {
        self.keyframe.load(Ordering::Relaxed)
    }
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe.swap(false, Ordering::Relaxed)
    }

    pub fn push_video(&self, data: Vec<u8>, key: bool) {
        let _ = self.video.send(Arc::new(Packet { data: Bytes::from(data), key, at: Instant::now() }));
    }

    #[cfg(test)]
    pub fn subscribe_video_for_test(&self) -> broadcast::Receiver<Arc<Packet>> {
        self.video.subscribe()
    }

    pub fn push_audio(&self, data: Vec<u8>) {
        let _ = self.audio.send(Arc::new(Packet { data: Bytes::from(data), key: true, at: Instant::now() }));
    }

    pub fn close_all(&self) {
        let peers: Vec<Pc> = self.peers.lock().unwrap().drain().map(|(_, (_, pc))| pc).collect();
        for pc in peers {
            tokio::spawn(async move {
                let _ = pc.close().await;
            });
        }
        self.changed();
    }

    /// Accepts a viewer's SDP offer and returns the SDP answer (non-trickle ICE).
    pub async fn answer(self: &Arc<Self>, token: String, offer: String) -> Result<String, String> {
        let e = |e: rtc::shared::error::Error| e.to_string();
        let vcodec = video_codec();
        let acodec = audio_codec();
        let mut media = MediaEngine::default();
        media.register_codec(vcodec.clone(), RtpCodecKind::Video).map_err(e)?;
        media.register_codec(acodec.clone(), RtpCodecKind::Audio).map_err(e)?;
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(e)?;

        let (gathered_tx, mut gathered_rx) = mpsc::unbounded_channel();
        let (state_tx, mut state_rx) = watch::channel(RTCPeerConnectionState::New);
        let ip = lan_ip().ok_or("no local network")?;

        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Events { gathered: gathered_tx, state: state_tx }))
            .with_runtime(default_runtime().ok_or("no runtime")?)
            .with_udp_addrs(vec![format!("{ip}:0")])
            .build()
            .await
            .map_err(e)?;
        let pc: Pc = Arc::new(pc);

        let (vtrack, vssrc) = make_track(RtpCodecKind::Video, &vcodec)?;
        let (atrack, assrc) = make_track(RtpCodecKind::Audio, &acodec)?;
        let vsender = pc.add_track(vtrack.clone() as Arc<dyn TrackLocal>).await.map_err(e)?;
        let asender = pc.add_track(atrack.clone() as Arc<dyn TrackLocal>).await.map_err(e)?;

        let desc = RTCSessionDescription::offer(offer).map_err(e)?;
        pc.set_remote_description(desc).await.map_err(e)?;
        let answer = pc.create_answer(None).await.map_err(e)?;
        pc.set_local_description(answer).await.map_err(e)?;
        let _ = tokio::time::timeout(Duration::from_secs(3), gathered_rx.recv()).await;
        let sdp = pc.local_description().await.ok_or("no answer")?.sdp;
        debug(&format!("answer sdp:\n{sdp}"));

        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some((_, old)) = self.peers.lock().unwrap().insert(token.clone(), (id, pc.clone())) {
            tokio::spawn(async move {
                let _ = old.close().await;
            });
        }
        self.changed();

        let hub = self.clone();
        tokio::spawn(async move {
            hub.stream_to(&mut state_rx, (vtrack, vssrc, vsender), (atrack, assrc, asender)).await;
            let removed = {
                let mut peers = hub.peers.lock().unwrap();
                if peers.get(&token).is_some_and(|(i, _)| *i == id) {
                    peers.remove(&token);
                    true
                } else {
                    false
                }
            };
            let _ = pc.close().await;
            if removed {
                hub.changed();
            }
        });
        Ok(sdp)
    }

    async fn stream_to(
        &self,
        state: &mut watch::Receiver<RTCPeerConnectionState>,
        (vtrack, vssrc, vsender): (Arc<TrackLocalStaticSample>, u32, Arc<dyn RtpSender>),
        (atrack, assrc, asender): (Arc<TrackLocalStaticSample>, u32, Arc<dyn RtpSender>),
    ) {
        use RTCPeerConnectionState as S;
        // Wait (max 20 s) for the connection to come up.
        let connected = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match *state.borrow_and_update() {
                    S::Connected => return true,
                    S::Failed | S::Closed => return false,
                    _ => {}
                }
                if state.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        debug(&format!("connected = {connected}"));
        if !connected {
            return;
        }

        let (vpt, apt) = (payload_type(&vsender).await, payload_type(&asender).await);
        debug(&format!("payload types video={vpt:?} audio={apt:?}"));
        let (Some(vpt), Some(apt)) = (vpt, apt) else {
            return;
        };

        let mut vrx = self.video.subscribe();
        let mut arx = self.audio.subscribe();
        self.request_keyframe();
        let mut have_key = false;
        let mut last_video: Option<Instant> = None;
        // Viewers report lost pictures with PLI/FIR; answer with a key frame right away
        // instead of letting the picture freeze until the next periodic key frame.
        let mut feedback = tokio::time::interval(Duration::from_millis(250));
        let mut loss_reports = 0u32;

        loop {
            tokio::select! {
                _ = feedback.tick() => {
                    if let Ok(report) = vsender.get_stats(Instant::now()).await {
                        let n: u32 = report
                            .iter_by_type(RTCStatsType::OutboundRTP)
                            .filter_map(|e| match e {
                                RTCStatsReportEntry::OutboundRtp(s) => Some(s.pli_count + s.fir_count),
                                _ => None,
                            })
                            .sum();
                        if n > loss_reports {
                            debug(&format!("viewer asked for key frame ({n})"));
                            self.request_keyframe();
                        }
                        loss_reports = n;
                    }
                }
                v = vrx.recv() => match v {
                    Ok(p) => {
                        if !have_key {
                            if !p.key { continue; }
                            have_key = true;
                        }
                        let duration = last_video
                            .map(|t| p.at.saturating_duration_since(t))
                            .unwrap_or(Duration::from_millis(33))
                            .clamp(Duration::from_millis(1), Duration::from_millis(500));
                        last_video = Some(p.at);
                        let sample = Sample { data: p.data.clone(), duration, ..Sample::new(p.at) };
                        if let Err(e) = vtrack.sample_writer(vssrc, vpt).write_sample(&sample).await {
                            debug(&format!("video write failed: {e}"));
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        have_key = false;
                        self.request_keyframe();
                    }
                    Err(_) => break,
                },
                a = arx.recv() => match a {
                    Ok(p) => {
                        let sample = Sample { data: p.data.clone(), duration: Duration::from_millis(20), ..Sample::new(p.at) };
                        if atrack.sample_writer(assrc, apt).write_sample(&sample).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                },
                r = state.changed() => {
                    if r.is_err() { break; }
                    if matches!(*state.borrow(), S::Failed | S::Closed) { break; }
                }
            }
        }
    }
}
