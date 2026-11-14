//! Audio: captured PCM -> 48 kHz stereo -> 20 ms Opus packets for WebRTC.
//! macOS gets PCM from ScreenCaptureKit; Windows (WASAPI loopback) and Linux
//! (PulseAudio/PipeWire monitor) feed `AudioSink` directly.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use std::sync::Arc;

use crate::hub::Hub;

#[cfg(target_os = "macos")]
pub use mac::pcm_from_sample;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME: usize = 960; // 20 ms @ 48 kHz
const CHANNELS: usize = 2;

#[cfg(target_os = "macos")]
mod mac {
#![allow(non_snake_case, non_upper_case_globals)]
use std::ffi::c_void;

use crate::encoder::{CFRelease, CFTypeRef, CMSampleBufferGetFormatDescription};

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    mSampleRate: f64,
    mFormatID: u32,
    mFormatFlags: u32,
    mBytesPerPacket: u32,
    mFramesPerPacket: u32,
    mBytesPerFrame: u32,
    mChannelsPerFrame: u32,
    mBitsPerChannel: u32,
    mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    mNumberChannels: u32,
    mDataByteSize: u32,
    mData: *mut c_void,
}

#[repr(C)]
struct AudioBufferList8 {
    mNumberBuffers: u32,
    mBuffers: [AudioBuffer; 8],
}

const kAudioFormatFlagIsFloat: u32 = 1 << 0;
const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;
const kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment: u32 = 1 << 0;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMAudioFormatDescriptionGetStreamBasicDescription(fd: CFTypeRef) -> *const AudioStreamBasicDescription;
    fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb: CFTypeRef,
        size_needed_out: *mut usize,
        list_out: *mut c_void,
        list_size: usize,
        block_alloc: CFTypeRef,
        mem_alloc: CFTypeRef,
        flags: u32,
        block_out: *mut CFTypeRef,
    ) -> i32;
}

/// Interleaved stereo f32 samples from a CMSampleBuffer (or None if unsupported).
fn why(msg: String) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    if std::env::var_os("SC_DEBUG").is_some() && N.fetch_add(1, Ordering::Relaxed) < 5 {
        eprintln!("[audio] {msg}");
    }
}

pub fn pcm_from_sample(sb: CFTypeRef) -> Option<Vec<f32>> {
    unsafe {
        let fd = CMSampleBufferGetFormatDescription(sb);
        if fd.is_null() {
            why("no format description".into());
            return None;
        }
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd);
        if asbd.is_null() {
            why("no stream description".into());
            return None;
        }
        let asbd = *asbd;
        if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
            why(format!("unsupported format flags={:#x} bits={} ch={} rate={}", asbd.mFormatFlags, asbd.mBitsPerChannel, asbd.mChannelsPerFrame, asbd.mSampleRate));
            return None;
        }
        // Ask CoreMedia how big the AudioBufferList must be, then provide exactly that
        // (8-byte aligned storage).
        let mut needed: usize = 0;
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb,
            &mut needed,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        if needed == 0 {
            needed = std::mem::size_of::<AudioBufferList8>();
        }
        let mut storage = vec![0u64; needed.div_ceil(8).max(std::mem::size_of::<AudioBufferList8>() / 8)];
        let mut block: CFTypeRef = std::ptr::null();
        let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb,
            std::ptr::null_mut(),
            storage.as_mut_ptr() as *mut c_void,
            needed,
            std::ptr::null(),
            std::ptr::null(),
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        );
        if st != 0 {
            why(format!("buffer list status {st} needed={needed} flags={:#x} ch={}", asbd.mFormatFlags, asbd.mChannelsPerFrame));
            return None;
        }
        let list = &*(storage.as_ptr() as *const AudioBufferList8);
        let nbuf = (list.mNumberBuffers as usize).min(8);
        let mut out = Vec::new();
        if asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 && nbuf > 0 {
            // Planar: one buffer per channel.
            let frames = list.mBuffers[0].mDataByteSize as usize / 4;
            let ch = |i: usize| -> &[f32] {
                let b = list.mBuffers[i.min(nbuf - 1)];
                if b.mData.is_null() {
                    return &[];
                }
                std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
            };
            let (l, r) = (ch(0), ch(1));
            out.reserve(frames * 2);
            for i in 0..frames {
                out.push(*l.get(i).unwrap_or(&0.0));
                out.push(*r.get(i).unwrap_or(&0.0));
            }
        } else if nbuf > 0 {
            let b = list.mBuffers[0];
            if b.mData.is_null() {
                if !block.is_null() {
                    CFRelease(block);
                }
                return None;
            }
            let s = std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
            let chans = (asbd.mChannelsPerFrame as usize).max(1);
            for frame in s.chunks(chans) {
                let l = frame[0];
                let r = *frame.get(1).unwrap_or(&l);
                out.push(l);
                out.push(r);
            }
        }
        if !block.is_null() {
            CFRelease(block);
        }
        Some(out)
    }
}

}

pub struct OpusEncoder {
    enc: *mut audiopus_sys::OpusEncoder,
    pending: Vec<f32>,
    packet: Vec<u8>,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let mut err = 0;
            let enc = audiopus_sys::opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS as i32,
                audiopus_sys::OPUS_APPLICATION_AUDIO as i32,
                &mut err,
            );
            if enc.is_null() || err != 0 {
                return Err(format!("opus init failed ({err})"));
            }
            audiopus_sys::opus_encoder_ctl(enc, audiopus_sys::OPUS_SET_BITRATE_REQUEST as i32, 128_000i32);
            Ok(Self { enc, pending: Vec::with_capacity(FRAME * CHANNELS * 4), packet: vec![0u8; 4000] })
        }
    }

    /// Feed interleaved stereo samples; returns complete 20 ms Opus packets.
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(pcm);
        let mut packets = Vec::new();
        let step = FRAME * CHANNELS;
        // Avoid unbounded latency if we fall behind.
        if self.pending.len() > step * 25 {
            let drop_n = self.pending.len() - step * 5;
            self.pending.drain(..drop_n);
        }
        while self.pending.len() >= step {
            let n = unsafe {
                audiopus_sys::opus_encode_float(
                    self.enc,
                    self.pending.as_ptr(),
                    FRAME as i32,
                    self.packet.as_mut_ptr(),
                    self.packet.len() as i32,
                )
            };
            self.pending.drain(..step);
            if n > 0 {
                packets.push(self.packet[..n as usize].to_vec());
            }
        }
        packets
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_encoder_destroy(self.enc) }
    }
}

/// Converts any captured PCM (f32, N channels, any rate) to 48 kHz stereo, encodes Opus and
/// hands packets to the hub.
pub struct AudioSink {
    opus: OpusEncoder,
    hub: Arc<Hub>,
    input: Vec<f32>, // interleaved stereo at the source rate
    pos: f64,
    out: Vec<f32>,
}

impl AudioSink {
    pub fn new(hub: Arc<Hub>) -> Result<Self, String> {
        Ok(Self { opus: OpusEncoder::new()?, hub, input: Vec::new(), pos: 0.0, out: Vec::new() })
    }

    pub fn push(&mut self, data: &[f32], channels: usize, rate: u32) {
        let channels = channels.max(1);
        for frame in data.chunks(channels) {
            let l = frame[0];
            let r = *frame.get(1).unwrap_or(&l);
            self.input.push(l);
            self.input.push(r);
        }
        self.out.clear();
        if rate == SAMPLE_RATE {
            self.out.extend_from_slice(&self.input);
            self.input.clear();
        } else {
            // Linear-interpolation resampler, stateful across calls.
            let step = rate as f64 / SAMPLE_RATE as f64;
            let frames = self.input.len() / 2;
            while self.pos + 1.0 < frames as f64 {
                let i = self.pos as usize;
                let t = (self.pos - i as f64) as f32;
                for c in 0..2 {
                    let a = self.input[i * 2 + c];
                    let b = self.input[(i + 1) * 2 + c];
                    self.out.push(a + (b - a) * t);
                }
                self.pos += step;
            }
            let used = (self.pos as usize).min(frames);
            self.input.drain(..used * 2);
            self.pos -= used as f64;
        }
        for pkt in self.opus.push(&self.out) {
            self.hub.push_audio(pkt);
        }
    }
}
