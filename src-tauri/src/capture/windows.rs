//! Windows: capture this app's own window with Windows.Graphics.Capture (Windows 10 1903+),
//! crop to the browser area, encode with OpenH264; system audio via WASAPI loopback.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use windows::{
    core::{factory, Interface},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{HMODULE, HWND, POINT, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Dxgi::IDXGIDevice,
            Gdi::ClientToScreen,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RoInitialize, RO_INIT_MULTITHREADED,
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
};

use super::{
    soft::{Frame, SoftPipeline},
    Target,
};
use crate::{audio::AudioSink, hub::Hub};

/// D3D objects used from the capture callback thread (guarded by a mutex).
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}
unsafe impl Send for Gpu {}

/// The WinRT Direct3D device wrapper is agile (thread-safe) but not marked `Send` by the
/// bindings; the capture callback runs on a thread-pool thread.
struct Agile<T>(T);
unsafe impl<T> Send for Agile<T> {}
unsafe impl<T> Sync for Agile<T> {}
impl<T> Agile<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

pub struct Capture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    pipeline: Arc<SoftPipeline>,
    audio_running: Arc<AtomicBool>,
}

// WinRT capture objects are agile (usable from any thread).
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

/// Where the client area's toolbar-free part sits inside the captured window image.
fn crop_for(hwnd: HWND, toolbar_px: usize, img_w: usize, img_h: usize) -> (usize, usize, usize, usize) {
    unsafe {
        let mut frame = RECT::default();
        let mut client = RECT::default();
        let mut origin = POINT { x: 0, y: 0 };
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
            && GetClientRect(hwnd, &mut client).is_ok()
            && ClientToScreen(hwnd, &mut origin).as_bool();
        if !ok {
            return (0, toolbar_px, img_w, img_h.saturating_sub(toolbar_px));
        }
        let x = (origin.x - frame.left).max(0) as usize;
        let y = (origin.y - frame.top).max(0) as usize + toolbar_px;
        let w = (client.right - client.left).max(0) as usize;
        let h = ((client.bottom - client.top).max(0) as usize).saturating_sub(toolbar_px);
        (x, y, w, h)
    }
}

impl Capture {
    pub fn start(hub: Arc<Hub>, target: &Target, _win_w: f64, _win_h: f64, toolbar: f64) -> Result<Self, String> {
        let e = |e: windows::core::Error| format!("Windows capture: {e}");
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("Window capture needs Windows 10 version 1903 or newer".into());
        }
        let hwnd = HWND(target.hwnd as *mut _);
        let toolbar_px = (toolbar * target.scale).round() as usize;

        // Direct3D device shared by the capture pool and our CPU read-back.
        let (device, context) = unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(e)?;
            (device.ok_or("no D3D device")?, context.ok_or("no D3D context")?)
        };
        let dxgi: IDXGIDevice = device.cast().map_err(e)?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(e)?
            .cast()
            .map_err(e)?;

        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(e)?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(e)?;
        let size = item.Size().map_err(e)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(e)?;
        let session = pool.CreateCaptureSession(&item).map_err(e)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false); // Windows 11: no yellow border

        let pipeline = SoftPipeline::new(hub.clone());
        let gpu = Arc::new(Mutex::new(Gpu { device, context, staging: None }));
        let last_size = Arc::new(Mutex::new(size));
        let hwnd_raw = target.hwnd;

        {
            let pipeline = pipeline.clone();
            let d3d = Agile(d3d.clone());
            pool.FrameArrived(&windows::Foundation::TypedEventHandler::new(
                move |sender: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                    let Some(pool) = sender.as_ref() else { return Ok(()) };
                    let frame = pool.TryGetNextFrame()?;
                    let content: SizeInt32 = frame.ContentSize()?;
                    {
                        let mut last = last_size.lock().unwrap();
                        if content.Width != last.Width || content.Height != last.Height {
                            // Window resized: recreate the pool buffers at the new size.
                            *last = content;
                            pool.Recreate(d3d.get(), DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
                            return Ok(());
                        }
                    }
                    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { texture.GetDesc(&mut desc) };

                    let mut g = gpu.lock().unwrap();
                    if g.staging.as_ref().map_or(true, |(_, w, h)| *w != desc.Width || *h != desc.Height) {
                        let mut sd = desc;
                        sd.Usage = D3D11_USAGE_STAGING;
                        sd.BindFlags = 0;
                        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                        sd.MiscFlags = 0;
                        sd.MipLevels = 1;
                        sd.ArraySize = 1;
                        sd.SampleDesc.Count = 1;
                        let mut tex = None;
                        unsafe { g.device.CreateTexture2D(&sd, None, Some(&mut tex))? };
                        g.staging = tex.map(|t| (t, desc.Width, desc.Height));
                    }
                    let Some((staging, w, h)) = g.staging.clone() else { return Ok(()) };
                    unsafe { g.context.CopyResource(&staging, &texture) };
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    unsafe { g.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
                    let stride = mapped.RowPitch as usize;
                    let (w, h) = (w as usize, (content.Height as usize).min(h as usize));
                    let width = (content.Width as usize).min(w);
                    let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, stride * h) }.to_vec();
                    unsafe { g.context.Unmap(&staging, 0) };
                    drop(g);

                    let crop = crop_for(HWND(hwnd_raw as *mut _), toolbar_px, width, h);
                    pipeline.push(Frame { data, width, height: h, stride, crop });
                    Ok(())
                },
            ))
            .map_err(e)?;
        }
        session.StartCapture().map_err(e)?;

        let audio_running = Arc::new(AtomicBool::new(true));
        start_loopback_audio(hub, audio_running.clone());

        Ok(Self { session, pool, pipeline, audio_running })
    }

    pub fn stop(self) {
        self.audio_running.store(false, Ordering::Relaxed);
        self.pipeline.stop();
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// WASAPI loopback of the default output device (what the speakers play). The stream lives
/// on its own thread because cpal streams are not `Send` on every platform.
fn start_loopback_audio(hub: Arc<Hub>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let host = cpal::default_host();
            let device = host.default_output_device().ok_or("no audio output device")?;
            let config = device.default_output_config().map_err(|e| e.to_string())?;
            if config.sample_format() != cpal::SampleFormat::F32 {
                return Err(format!("unsupported audio format {:?}", config.sample_format()));
            }
            let channels = config.channels() as usize;
            let rate = config.sample_rate().0;
            let mut sink = AudioSink::new(hub)?;
            // Using an *output* device as input makes cpal open a WASAPI loopback stream.
            let stream = device
                .build_input_stream(
                    &config.config(),
                    move |data: &[f32], _| sink.push(data, channels, rate),
                    |err| eprintln!("audio loopback: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("audio capture disabled: {e}");
        }
    });
}
