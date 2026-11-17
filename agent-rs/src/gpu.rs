// GPU video source: captures the LDPlayer window with Windows Graphics Capture, crops it to the
// Android render area, converts BGRA→NV12 with the D3D11 video processor, and encodes H.264 with
// the GPU's hardware Media Foundation encoder (NVENC / AMF / Quick Sync behind one API, no SDK).
//
// Constraints (checked on a real LDPlayer): the window must be on screen. Covered by other windows
// is fine; minimized or moved off screen and LDPlayer stops drawing. So while streaming we restore
// a minimized window behind all others, and minimize it again when done.
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use windows::core::{Interface, GUID};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

pub enum GpuEvent {
    Size { width: u32, height: u32 },
    Packet { key: bool, pts_us: u64, data: Bytes },
    Closed(Option<String>),
}

pub enum GpuCtl {
    Keyframe,
    Bitrate(u32),
}

pub struct GpuOpts {
    pub top: isize,
    pub render: isize,
    pub bit_rate: u32,
    pub max_fps: u32,
    /// Longest output side; larger windows are scaled down (0 = native).
    pub max_size: u32,
}

pub struct GpuSource {
    ctl: std::sync::mpsc::Sender<GpuCtl>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GpuSource {
    pub fn send(&self, c: GpuCtl) {
        let _ = self.ctl.send(c);
    }
}

impl Drop for GpuSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start(opts: GpuOpts, events: mpsc::UnboundedSender<GpuEvent>) -> GpuSource {
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("gpu-capture".into())
        .spawn(move || {
            let restored = prepare_window(HWND(opts.top as *mut _));
            // Restart the pipeline when the render area changes size (rotation, window resize).
            let result = loop {
                match run(&opts, &events, &ctl_rx, &stop2) {
                    Ok(Exit::Resized) => continue,
                    Ok(Exit::Stopped) => break None,
                    Err(e) => break Some(format!("{e:#}")),
                }
            };
            if restored {
                unsafe {
                    let _ = ShowWindow(HWND(opts.top as *mut _), SW_SHOWMINNOACTIVE);
                }
            }
            let _ = events.send(GpuEvent::Closed(result));
        })
        .expect("spawn gpu thread");
    GpuSource { ctl: ctl_tx, stop, thread: Some(thread) }
}

/// LDPlayer only draws while its window is on screen: restore a minimized window without
/// activating it and push it behind every other window. Returns true when we restored it.
fn prepare_window(top: HWND) -> bool {
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    unsafe {
        let restored = IsIconic(top).as_bool();
        if restored {
            let _ = ShowWindow(top, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(top, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        // Off screen it stops drawing too: bring it back onto the primary monitor (still at the bottom).
        if MonitorFromWindow(top, MONITOR_DEFAULTTONULL).is_invalid() {
            let _ = SetWindowPos(top, Some(HWND_BOTTOM), 40, 40, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
        if restored {
            std::thread::sleep(Duration::from_millis(300));
        }
        restored
    }
}

/// The Android render area inside the captured top-level window, in frame pixels.
fn crop_rect(top: HWND, render: HWND, frame_w: u32, frame_h: u32) -> Option<D3D11_BOX> {
    unsafe {
        let mut bounds = RECT::default();
        DwmGetWindowAttribute(top, DWMWA_EXTENDED_FRAME_BOUNDS, &mut bounds as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).ok()?;
        let mut r = RECT::default();
        GetWindowRect(render, &mut r).ok()?;
        let left = (r.left - bounds.left).clamp(0, frame_w as i32) as u32;
        let top_ = (r.top - bounds.top).clamp(0, frame_h as i32) as u32;
        let right = ((r.right - bounds.left).clamp(0, frame_w as i32) as u32).max(left);
        let bottom = ((r.bottom - bounds.top).clamp(0, frame_h as i32) as u32).max(top_);
        // Even sizes for NV12.
        let w = (right - left) & !1;
        let h = (bottom - top_) & !1;
        (w >= 64 && h >= 64).then_some(D3D11_BOX { left, top: top_, front: 0, right: left + w, bottom: top_ + h, back: 1 })
    }
}

enum Exit {
    Resized,
    Stopped,
}

/// Latest captured frame, already cropped into `staging` by the WGC callback.
struct Shared {
    fresh: bool,
    resized: bool,
    error: Option<String>,
}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_UI4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { ulVal: v } }),
        },
    }
}

fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VARENUM(VT_BOOL.0),
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 }) },
            }),
        },
    }
}

fn set_codec(api: &ICodecAPI, key: &GUID, value: VARIANT, name: &str) {
    if let Err(e) = unsafe { api.SetValue(key, &value) } {
        tracing::debug!("encoder: {name} not supported ({e})");
    }
}

struct Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    provides_samples: bool,
    out_size: u32,
    name: String,
}

impl Encoder {
    fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bit_rate: u32) -> Result<Self> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                bail!("no hardware H.264 encoder");
            }
            let list = std::slice::from_raw_parts_mut(activates, count as usize);
            let activate = list[0].take().ok_or_else(|| anyhow!("empty encoder entry"))?;
            for a in list.iter_mut() {
                drop(a.take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

            let mut name = String::new();
            let mut buf = [0u16; 256];
            let mut len = 0u32;
            if activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
                name = String::from_utf16_lossy(&buf[..len as usize]);
            }
            let mft: IMFTransform = activate.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                set_codec(c, &CODECAPI_AVLowLatencyMode, variant_bool(true), "low latency");
                set_codec(c, &CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bit_rate), "bit rate");
                // Long GOP: key frames come on demand (viewer joins, packet loss) instead of every
                // few seconds, which keeps the stream free of periodic size spikes.
                set_codec(c, &CODECAPI_AVEncMPVGOPSize, variant_u32(fps * 10), "GOP size");
                set_codec(c, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "B frames");
            }

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bit_rate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            mft.SetOutputType(0, &out, 0).context("SetOutputType")?;

            let mut set_input = false;
            for i in 0.. {
                let Ok(t) = mft.GetInputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    t.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
                    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                    mft.SetInputType(0, &t, 0).context("SetInputType")?;
                    set_input = true;
                    break;
                }
            }
            if !set_input {
                bail!("encoder has no NV12 input");
            }
            let info = mft.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = mft.cast()?;
            Ok(Self { mft, events, codec, provides_samples, out_size: info.cbSize.max(1 << 20), name })
        }
    }

    fn set_bitrate(&self, bps: u32) {
        if let Some(c) = &self.codec {
            set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bps), "bit rate");
        }
    }

    fn force_keyframe(&self) {
        if let Some(c) = &self.codec {
            set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1), "force key frame");
        }
    }

    /// Pulls one encoded access unit: (annex B bytes, key frame, time in 100 ns units).
    fn take_output(&self) -> Result<Option<(Vec<u8>, bool, i64)>> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(None);
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(e.into());
            }
            let Some(sample) = sample else { return Ok(None) };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let time = sample.GetSampleTime().unwrap_or(0);
            Ok(Some((data, key, time)))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned nothing"))
}

fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

fn run(opts: &GpuOpts, events: &mpsc::UnboundedSender<GpuEvent>, ctl: &std::sync::mpsc::Receiver<GpuCtl>, stop: &AtomicBool) -> Result<Exit> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let result = run_inner(opts, events, ctl, stop);
    unsafe {
        let _ = MFShutdown();
    }
    result
}

fn run_inner(opts: &GpuOpts, events: &mpsc::UnboundedSender<GpuEvent>, ctl: &std::sync::mpsc::Receiver<GpuCtl>, stop: &AtomicBool) -> Result<Exit> {
    let top = HWND(opts.top as *mut _);
    let render = HWND(opts.render as *mut _);
    let fps = opts.max_fps.clamp(10, 120);

    // D3D11 device usable by the video processor and Media Foundation, shared with the WGC callback.
    let (device, context) = unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        (device.unwrap(), context.unwrap())
    };
    unsafe {
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }

    // Capture item and the area we crop out of it.
    let item: GraphicsCaptureItem = unsafe { windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?.CreateForWindow(top)? };
    let item_size = item.Size()?;
    let crop = crop_rect(top, render, item_size.Width as u32, item_size.Height as u32).ok_or_else(|| anyhow!("LDPlayer render area not found"))?;
    let (src_w, src_h) = (crop.right - crop.left, crop.bottom - crop.top);
    let (out_w, out_h) = if opts.max_size > 0 && src_w.max(src_h) > opts.max_size {
        let scale = opts.max_size as f64 / src_w.max(src_h) as f64;
        (even((src_w as f64 * scale) as u32), even((src_h as f64 * scale) as u32))
    } else {
        (src_w, src_h)
    };

    let encoder = Encoder::new(&device, out_w, out_h, fps, opts.bit_rate)?;
    tracing::info!("gpu: capturing {src_w}x{src_h} → {out_w}x{out_h} @{fps} with \"{}\"", encoder.name);
    let _ = events.send(GpuEvent::Size { width: out_w, height: out_h });

    // Cropped BGRA copy of the latest frame (written by the capture callback).
    let staging = texture(&device, src_w, src_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
    // NV12 inputs for the encoder; rotated so the encoder can hold on to a few.
    let nv12: Vec<ID3D11Texture2D> = (0..4).map(|_| texture(&device, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)).collect::<Result<_>>()?;

    // BGRA → NV12 (and scaling) on the GPU.
    let vdev: ID3D11VideoDevice = device.cast()?;
    let vctx: ID3D11VideoContext = context.cast()?;
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
        InputWidth: src_w,
        InputHeight: src_h,
        OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
        OutputWidth: out_w,
        OutputHeight: out_h,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let (enumerator, processor, in_view, out_views) = unsafe {
        let enumerator = vdev.CreateVideoProcessorEnumerator(&content)?;
        let processor = vdev.CreateVideoProcessor(&enumerator, 0)?;
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut in_view = None;
        vdev.CreateVideoProcessorInputView(&staging, &enumerator, &in_desc, Some(&mut in_view))?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut views = Vec::new();
        for t in &nv12 {
            let mut v = None;
            vdev.CreateVideoProcessorOutputView(t, &enumerator, &out_desc, Some(&mut v))?;
            views.push(v.unwrap());
        }
        (enumerator, processor, in_view.unwrap(), views)
    };
    let _ = &enumerator;

    // Windows Graphics Capture on a free-threaded frame pool: the callback crops into `staging`.
    let shared = Arc::new((Mutex::new(Shared { fresh: false, resized: false, error: None }), Condvar::new()));
    let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()? };
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item_size)?;
    {
        let shared = shared.clone();
        let context = context.clone();
        let staging = staging.clone();
        let item_size_cb = item_size;
        let (top_i, render_i) = (opts.top, opts.render); // HWND is not Send
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(move |pool, _| {
            let Some(pool) = pool.as_ref() else { return Ok(()) };
            let frame = pool.TryGetNextFrame()?;
            let size = frame.ContentSize()?;
            let (lock, cv) = &*shared;
            let mut st = lock.lock().unwrap();
            if size.Width != item_size_cb.Width || size.Height != item_size_cb.Height {
                st.resized = true;
                cv.notify_all();
                return Ok(());
            }
            match crop_rect(HWND(top_i as *mut _), HWND(render_i as *mut _), size.Width as u32, size.Height as u32) {
                Some(c) if c.right - c.left == src_w && c.bottom - c.top == src_h => {
                    let tex: ID3D11Texture2D = unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
                    unsafe { context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &tex, 0, Some(&c)) };
                    st.fresh = true;
                }
                _ => st.resized = true,
            }
            cv.notify_all();
            Ok(())
        }))?;
    }
    let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;

    let start = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut need_input = 0u32;
    let mut have_frame = false;
    let mut key_pending = true;
    let mut last_input = Instant::now() - Duration::from_secs(10);
    let mut next_view = 0usize;
    let mut frames_out = 0u64;
    let mut last_stats = Instant::now();

    let exit = loop {
        if stop.load(Ordering::SeqCst) {
            break Exit::Stopped;
        }
        while let Ok(c) = ctl.try_recv() {
            match c {
                GpuCtl::Keyframe => key_pending = true,
                GpuCtl::Bitrate(bps) => encoder.set_bitrate(bps),
            }
        }

        // Drain encoder events without blocking.
        loop {
            let ev = match unsafe { encoder.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(anyhow!("encoder event: {e}")),
            };
            match unsafe { ev.GetType()? } {
                t if t == METransformNeedInput.0 as u32 => need_input += 1,
                t if t == METransformHaveOutput.0 as u32 => {
                    if let Some((data, key, time)) = encoder.take_output()? {
                        frames_out += 1;
                        let _ = events.send(GpuEvent::Packet { key, pts_us: (time / 10).max(0) as u64, data: Bytes::from(data) });
                    }
                }
                _ => {}
            }
        }

        // Wait for a new frame (or a reason to re-send the last one).
        let (fresh, resized, error) = {
            let (lock, cv) = &*shared;
            let mut st = lock.lock().unwrap();
            if !st.fresh && !st.resized && st.error.is_none() {
                st = cv.wait_timeout(st, Duration::from_millis(2)).unwrap().0;
            }
            let r = (st.fresh, st.resized, st.error.take());
            st.fresh = false;
            r
        };
        if let Some(e) = error {
            bail!(e);
        }
        if resized {
            tracing::info!("gpu: LDPlayer window size changed, restarting capture");
            break Exit::Resized;
        }
        have_frame |= fresh;
        // A static screen yields no frames; repeat the last one (a few hundred bytes once encoded)
        // so a key frame request is always served and the browser's freeze detector (500 ms)
        // never mistakes a still screen for a stalled stream.
        let repeat = have_frame && (key_pending || last_input.elapsed() > Duration::from_millis(150));
        if need_input > 0 && (fresh || repeat) && last_input.elapsed() >= frame_interval / 2 {
            unsafe {
                let view = &out_views[next_view];
                let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    OutputIndex: 0,
                    InputFrameOrField: 0,
                    PastFrames: 0,
                    FutureFrames: 0,
                    ppPastSurfaces: std::ptr::null_mut(),
                    pInputSurface: ManuallyDrop::new(Some(in_view.clone())),
                    ppFutureSurfaces: std::ptr::null_mut(),
                    ppPastSurfacesRight: std::ptr::null_mut(),
                    pInputSurfaceRight: ManuallyDrop::new(None),
                    ppFutureSurfacesRight: std::ptr::null_mut(),
                };
                let streams = [stream];
                vctx.VideoProcessorBlt(&processor, view, 0, &streams)?;
                let [mut s] = streams;
                ManuallyDrop::drop(&mut s.pInputSurface);

                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12[next_view], 0, false)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime((start.elapsed().as_nanos() / 100) as i64)?;
                sample.SetSampleDuration((frame_interval.as_nanos() / 100) as i64)?;
                if key_pending {
                    encoder.force_keyframe();
                    key_pending = false;
                }
                encoder.mft.ProcessInput(0, &sample, 0)?;
            }
            next_view = (next_view + 1) % out_views.len();
            need_input -= 1;
            last_input = Instant::now();
        }

        if last_stats.elapsed() > Duration::from_secs(60) {
            tracing::debug!("gpu: {frames_out} frames encoded in the last minute");
            frames_out = 0;
            last_stats = Instant::now();
        }
    };
    let _ = session.Close();
    let _ = pool.Close();
    Ok(exit)
}
