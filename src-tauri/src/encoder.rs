//! Hardware H.264 encoder (Apple VideoToolbox) — minimal raw FFI.
//!
//! Input: CVPixelBuffers from ScreenCaptureKit. Output: Annex-B access units (SPS/PPS
//! prepended on key frames), Constrained Baseline, no B-frames — decodable by every browser.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl CMTime {
    pub fn micros(us: i64) -> Self {
        CMTime { value: us, timescale: 1_000_000, flags: 1, epoch: 0 }
    }
    pub fn invalid() -> Self {
        CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 }
    }
}

const kCFNumberSInt32Type: isize = 3;
const kCFNumberFloat64Type: isize = 6;
const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");

type VTOutputCallback =
    unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, CFTypeRef);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFTypeArrayCallBacks: c_void;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        kcb: *const c_void,
        vcb: *const c_void,
    ) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, ty: isize, value: *const c_void) -> CFTypeRef;
    fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, n: isize, cb: *const c_void) -> CFTypeRef;
    fn CFArrayGetCount(a: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFTypeRef;
    pub fn CMSampleBufferGetImageBuffer(sb: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: CFTypeRef, create: u8) -> CFTypeRef;
    pub fn CMSampleBufferGetFormatDescription(sb: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetDataBuffer(sb: CFTypeRef) -> CFTypeRef;
    pub fn CMSampleBufferDataIsReady(sb: CFTypeRef) -> u8;
    fn CMBlockBufferGetDataLength(bb: CFTypeRef) -> usize;
    fn CMBlockBufferCopyDataBytes(bb: CFTypeRef, off: usize, len: usize, dest: *mut c_void) -> i32;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        fd: CFTypeRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> i32;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub fn CVPixelBufferGetWidth(pb: CFTypeRef) -> usize;
    pub fn CVPixelBufferGetHeight(pb: CFTypeRef) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFTypeRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFTypeRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFTypeRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFTypeRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFTypeRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFTypeRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFTypeRef;
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFTypeRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFTypeRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        compressed_alloc: CFTypeRef,
        callback: VTOutputCallback,
        refcon: *mut c_void,
        out: *mut CFTypeRef,
    ) -> i32;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> i32;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFTypeRef) -> i32;
    fn VTCompressionSessionEncodeFrame(
        session: CFTypeRef,
        image: CFTypeRef,
        pts: CMTime,
        duration: CMTime,
        props: CFTypeRef,
        src_refcon: *mut c_void,
        flags_out: *mut u32,
    ) -> i32;
    fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> i32;
    fn VTCompressionSessionInvalidate(session: CFTypeRef);
}

/// Owned CoreFoundation object, released on drop.
struct Cf(CFTypeRef);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

unsafe fn cf_i32(v: i32) -> Cf {
    Cf(CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, &v as *const i32 as *const c_void))
}
unsafe fn cf_f64(v: f64) -> Cf {
    Cf(CFNumberCreate(std::ptr::null(), kCFNumberFloat64Type, &v as *const f64 as *const c_void))
}
unsafe fn cf_dict(keys: &[CFTypeRef], values: &[CFTypeRef]) -> Cf {
    Cf(CFDictionaryCreate(
        std::ptr::null(),
        keys.as_ptr(),
        values.as_ptr(),
        keys.len() as isize,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    ))
}

pub type OutputFn = Box<dyn Fn(Vec<u8>, bool) + Send + Sync>;

struct Ctx {
    out: OutputFn,
}

pub struct H264Encoder {
    session: CFTypeRef,
    ctx: *mut Ctx,
    force_key: Cf,
    pub width: usize,
    pub height: usize,
}

unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(width: usize, height: usize, fps: u32, bitrate: i32, out: OutputFn) -> Result<Self, String> {
        unsafe {
            let ctx = Box::into_raw(Box::new(Ctx { out }));
            let spec = cf_dict(
                &[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder],
                &[kCFBooleanTrue],
            );
            let mut session: CFTypeRef = std::ptr::null();
            let st = VTCompressionSessionCreate(
                std::ptr::null(),
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                spec.0,
                std::ptr::null(),
                std::ptr::null(),
                on_encoded,
                ctx as *mut c_void,
                &mut session,
            );
            if st != 0 || session.is_null() {
                drop(Box::from_raw(ctx));
                return Err(format!("VideoToolbox encoder could not start ({st})"));
            }
            let set = |k: CFTypeRef, v: CFTypeRef| VTSessionSetProperty(session, k, v);
            set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel);
            set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
            let br = cf_i32(bitrate);
            set(kVTCompressionPropertyKey_AverageBitRate, br.0);
            // Hard cap: 1.5x average bitrate over any 1 s window (bytes, seconds).
            let bytes = cf_i32(bitrate / 8 * 3 / 2);
            let secs = cf_f64(1.0);
            let limits = Cf(CFArrayCreate(
                std::ptr::null(),
                [bytes.0, secs.0].as_ptr(),
                2,
                &kCFTypeArrayCallBacks,
            ));
            set(kVTCompressionPropertyKey_DataRateLimits, limits.0);
            let kfi = cf_f64(4.0);
            set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kfi.0);
            let fr = cf_i32(fps as i32);
            set(kVTCompressionPropertyKey_ExpectedFrameRate, fr.0);
            VTCompressionSessionPrepareToEncodeFrames(session);

            let force_key = cf_dict(&[kVTEncodeFrameOptionKey_ForceKeyFrame], &[kCFBooleanTrue]);
            Ok(Self { session, ctx, force_key, width, height })
        }
    }

    /// Encode one CVPixelBuffer. Output arrives asynchronously via the callback.
    pub fn encode(&mut self, pixel_buffer: CFTypeRef, pts_us: i64, key: bool) {
        unsafe {
            let props = if key { self.force_key.0 } else { std::ptr::null() };
            VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                CMTime::micros(pts_us),
                CMTime::invalid(),
                props,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            drop(Box::from_raw(self.ctx));
        }
    }
}

unsafe extern "C" fn on_encoded(refcon: *mut c_void, _src: *mut c_void, status: i32, _flags: u32, sb: CFTypeRef) {
    if status != 0 || sb.is_null() || CMSampleBufferDataIsReady(sb) == 0 {
        return;
    }
    let ctx = &*(refcon as *const Ctx);

    // Key frame unless the "NotSync" attachment is true.
    let mut key = true;
    let atts = CMSampleBufferGetSampleAttachmentsArray(sb, 0);
    if !atts.is_null() && CFArrayGetCount(atts) > 0 {
        let d = CFArrayGetValueAtIndex(atts, 0);
        let not_sync = CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
        if !not_sync.is_null() && CFBooleanGetValue(not_sync) != 0 {
            key = false;
        }
    }

    let mut out = Vec::with_capacity(64 * 1024);
    let mut nal_len_size: i32 = 4;
    if key {
        let fd = CMSampleBufferGetFormatDescription(sb);
        let mut count = 0usize;
        let mut ptr: *const u8 = std::ptr::null();
        let mut size = 0usize;
        if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, 0, &mut ptr, &mut size, &mut count, &mut nal_len_size) == 0 {
            for i in 0..count {
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    fd, i, &mut ptr, &mut size, std::ptr::null_mut(), std::ptr::null_mut(),
                ) == 0
                {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, size));
                }
            }
        }
    }

    let bb = CMSampleBufferGetDataBuffer(sb);
    if bb.is_null() {
        return;
    }
    let total = CMBlockBufferGetDataLength(bb);
    let mut avcc = vec![0u8; total];
    if CMBlockBufferCopyDataBytes(bb, 0, total, avcc.as_mut_ptr() as *mut c_void) != 0 {
        return;
    }
    // AVCC (length-prefixed NAL units) -> Annex-B (start codes).
    let n = nal_len_size.clamp(1, 4) as usize;
    let mut i = 0usize;
    while i + n <= avcc.len() {
        let mut len = 0usize;
        for b in &avcc[i..i + n] {
            len = (len << 8) | *b as usize;
        }
        i += n;
        if len == 0 || i + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    (ctx.out)(out, key);
}
