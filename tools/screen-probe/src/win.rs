//! Windows: Windows.Graphics.Capture → D3D11 video processor (BGRA → NV12, scale) →
//! hardware H.264 Media Foundation MFT (async, low latency, CBR). Everything stays on the
//! GPU; the CPU only sees encoded bytes.

#![allow(unsafe_code)]

use std::mem::ManuallyDrop;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{FILETIME, HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, Interface, PWSTR};

use crate::RunArgs;

struct Monitor {
    handle: HMONITOR,
    name: String,
    rect: RECT,
    primary: bool,
    refresh_hz: u32,
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed below, alive for the call.
        let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        list.push(hmon);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into the vector that outlives the call.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }
    handles
        .into_iter()
        .map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            let mut mode = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: both structs are sized and owned here.
            unsafe {
                let _ = GetMonitorInfoW(handle, &mut info.monitorInfo as *mut MONITORINFO);
                let _ = EnumDisplaySettingsW(
                    windows::core::PCWSTR(info.szDevice.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                );
            }
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
            Monitor {
                handle,
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & 1 != 0,
                refresh_hz: mode.dmDisplayFrequency,
            }
        })
        .collect()
}

fn monitor_json(index: usize, m: &Monitor) -> Value {
    json!({
        "index": index,
        "name": m.name,
        "primary": m.primary,
        "left": m.rect.left,
        "top": m.rect.top,
        "width": m.rect.right - m.rect.left,
        "height": m.rect.bottom - m.rect.top,
        "refresh_hz": m.refresh_hz,
    })
}

/// Hardware MFTs of a category producing (encoders) or consuming (decoders) `subtype`.
fn hardware_mfts(
    category: windows::core::GUID,
    subtype: windows::core::GUID,
    encoder: bool,
) -> Vec<(String, IMFActivate)> {
    let info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx fills a CoTaskMem array of `count` activates that we take ownership of.
    unsafe {
        let (input, output) = if encoder {
            (None, Some(&info as *const _))
        } else {
            (Some(&info as *const _), None)
        };
        if MFTEnumEx(
            category,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            input,
            output,
            &mut array,
            &mut count,
        )
        .is_err()
            || array.is_null()
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(activate) = std::ptr::read(array.add(i)) {
                out.push((
                    attribute_string(&activate, &MFT_FRIENDLY_NAME_Attribute),
                    activate,
                ));
            }
        }
        CoTaskMemFree(Some(array as *const _));
        out
    }
}

fn attribute_string(attrs: &IMFActivate, key: &windows::core::GUID) -> String {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: the string is allocated by MF and freed here.
    unsafe {
        if attrs.GetAllocatedString(key, &mut value, &mut len).is_err() {
            return "?".into();
        }
        let s = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const _));
        s
    }
}

pub fn list() -> Result<Value> {
    mf_startup()?;
    let monitors: Vec<Value> = monitors()
        .iter()
        .enumerate()
        .map(|(i, m)| monitor_json(i, m))
        .collect();
    let codecs = [
        ("h264", MFVideoFormat_H264),
        ("hevc", MFVideoFormat_HEVC),
        ("av1", MFVideoFormat_AV1),
    ];
    let names = |category, encoder| -> Value {
        codecs
            .iter()
            .map(|(name, subtype)| {
                let list: Vec<String> = hardware_mfts(category, *subtype, encoder)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                (name.to_string(), json!(list))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    Ok(json!({
        "os": "windows",
        "monitors": monitors,
        "hardware_encoders": names(MFT_CATEGORY_VIDEO_ENCODER, true),
        "hardware_decoders": names(MFT_CATEGORY_VIDEO_DECODER, false),
        "adapter": adapter_name().unwrap_or_default(),
    }))
}

fn mf_startup() -> Result<()> {
    // SAFETY: plain library initialisation; balanced by process exit.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: standard device creation with out-params owned here.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
    }
    let device = device.ok_or_else(|| anyhow!("no device"))?;
    // The encoder MFT uses the device from its own threads.
    let mt: ID3D11Multithread = device.cast()?;
    // SAFETY: COM call on a live interface.
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok((device, context.ok_or_else(|| anyhow!("no context"))?))
}

fn adapter_name() -> Result<String> {
    let (device, _) = create_device()?;
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: COM calls on live interfaces.
    let desc = unsafe { dxgi.GetAdapter()?.GetDesc()? };
    let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
    Ok(String::from_utf16_lossy(&desc.Description[..len]))
}

/// 100 ns ticks of the performance counter, the clock of `SystemRelativeTime`.
fn qpc_100ns() -> i64 {
    let mut counter = 0i64;
    let mut freq = 0i64;
    // SAFETY: out-params owned here.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (counter as i128 * 10_000_000 / freq as i128) as i64
}

fn process_cpu() -> Duration {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: out-params owned here.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Duration::from_nanos((t(k) + t(u)) * 100)
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: descriptor and out-param owned here.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
    tex.ok_or_else(|| anyhow!("no texture"))
}

/// GPU colour conversion + scaling from the captured BGRA copy into NV12 textures.
struct Converter {
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        src: (u32, u32),
        args: &RunArgs,
        pool: usize,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: args.fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src.0,
            InputHeight: src.1,
            OutputFrameRate: rate,
            OutputWidth: args.width,
            OutputHeight: args.height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: COM calls with descriptors owned here; views reference live textures.
        unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .context("video processor enumerator")?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .context("video processor")?;
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view = None;
            video_device.CreateVideoProcessorInputView(
                source,
                &enumerator,
                &in_desc,
                Some(&mut input_view),
            )?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut outputs = Vec::new();
            for _ in 0..pool {
                let tex = texture(
                    device,
                    args.width,
                    args.height,
                    DXGI_FORMAT_NV12,
                    D3D11_BIND_RENDER_TARGET,
                )?;
                let mut view = None;
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &out_desc,
                    Some(&mut view),
                )?;
                outputs.push((tex, view.ok_or_else(|| anyhow!("no output view"))?));
            }
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            Ok(Self {
                video_context,
                processor,
                input_view: input_view.ok_or_else(|| anyhow!("no input view"))?,
                outputs,
            })
        }
    }

    fn convert(&self, slot: usize) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.outputs[slot % self.outputs.len()];
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        // SAFETY: the stream holds a cloned view reference, released right after the call.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt")?;
        Ok(tex.clone())
    }
}

/// Hardware H.264 encoder MFT driven through its async event queue.
struct Encoder {
    name: String,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    provides_samples: bool,
    output_size: u32,
    applied: Vec<String>,
    _manager: IMFDXGIDeviceManager,
}

fn set_codec_value(api: &ICodecAPI, key: &windows::core::GUID, value: VARIANT) -> bool {
    // SAFETY: COM call with a VARIANT owned here.
    unsafe { api.SetValue(key, &value).is_ok() }
}

impl Encoder {
    fn new(device: &ID3D11Device, args: &RunArgs) -> Result<Self> {
        let (name, activate) = hardware_mfts(MFT_CATEGORY_VIDEO_ENCODER, MFVideoFormat_H264, true)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no hardware H.264 encoder MFT"))?;
        // SAFETY: COM calls on live interfaces with values owned here.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("activate encoder")?;
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("no DXGI manager"))?;
            manager.ResetDevice(device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("encoder rejected the D3D11 device")?;

            let pack = |hi: u32, lo: u32| ((hi as u64) << 32) | lo as u64;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, args.bitrate_kbps * 1000)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(args.width, args.height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(args.fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            transform
                .SetOutputType(0, &out, 0)
                .context("encoder output type")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(args.width, args.height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(args.fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetInputType(0, &input, 0)
                .context("encoder input type")?;

            let mut applied = Vec::new();
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let settings: [(&str, windows::core::GUID, VARIANT); 4] = [
                    (
                        "low_latency",
                        CODECAPI_AVLowLatencyMode,
                        VARIANT::from(true),
                    ),
                    (
                        "cbr",
                        CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    ),
                    (
                        "mean_bitrate",
                        CODECAPI_AVEncCommonMeanBitRate,
                        VARIANT::from(args.bitrate_kbps * 1000),
                    ),
                    (
                        "gop",
                        CODECAPI_AVEncMPVGOPSize,
                        VARIANT::from(args.fps * args.gop_seconds),
                    ),
                ];
                for (label, key, value) in settings {
                    if set_codec_value(&api, &key, value) {
                        applied.push(label.to_string());
                    }
                }
            }

            let info = transform.GetOutputStreamInfo(0)?;
            let events: IMFMediaEventGenerator = transform.cast()?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                name,
                transform,
                events,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                output_size: info.cbSize.max(1 << 20),
                applied,
                _manager: manager,
            })
        }
    }

    /// Next queued event type, without waiting.
    fn poll_event(&self) -> Result<Option<i32>> {
        // SAFETY: COM call on a live interface.
        match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => Ok(Some(unsafe { event.GetType()? } as i32)),
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => Ok(None),
            Err(e) => Err(e).context("encoder GetEvent"),
        }
    }

    fn submit(&self, nv12: &ID3D11Texture2D, time: i64, duration: i64) -> Result<()> {
        // SAFETY: the sample keeps its own reference to the texture.
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")
        }
    }

    /// One encoded access unit: (sample time, bytes, keyframe).
    fn take_output(&self) -> Result<Option<(i64, u32, bool)>> {
        // SAFETY: the output buffer struct owns the sample/events refs we take out of it.
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let ty = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &ty, 0)?;
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            Ok(Some((
                sample.GetSampleTime()?,
                sample.GetTotalLength()?,
                key,
            )))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: COM calls on a live interface.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

fn percentiles(values: &mut [f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    let r = |v: f64| (v * 100.0).round() / 100.0;
    json!({ "n": values.len(), "p50": r(at(0.5)), "p95": r(at(0.95)), "p99": r(at(0.99)), "max": r(at(1.0)) })
}

struct Pending {
    capture_time: Option<i64>,
    submitted: i64,
}

pub fn run(args: &RunArgs) -> Result<Value> {
    if args.fps == 0 || !args.width.is_multiple_of(2) || !args.height.is_multiple_of(2) {
        bail!("fps must be > 0 and the size even");
    }
    mf_startup()?;
    let list = monitors();
    let monitor = list.get(args.monitor).ok_or_else(|| {
        anyhow!(
            "monitor {} not found ({} monitors)",
            args.monitor,
            list.len()
        )
    })?;

    let pattern = if args.pattern {
        let r = monitor.rect;
        let (w, h) = ((r.right - r.left) / 2, (r.bottom - r.top) / 2);
        Some(crate::pattern::Pattern::start(
            r.left + w / 2,
            r.top + h / 2,
            w,
            h,
        )?)
    } else {
        None
    };

    let (device, context) = create_device()?;
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: WinRT interop calls on live interfaces.
    let (item, winrt_device) = unsafe {
        let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop
            .CreateForMonitor(monitor.handle)
            .context("capture item for monitor")?;
        (item, winrt_device)
    };
    let size = item.Size()?;
    let (src_w, src_h) = (size.Width as u32, size.Height as u32);

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let border_off = session.SetIsBorderRequired(false).is_ok();
    let _ = session.SetIsCursorCaptureEnabled(true);

    let signal = Arc::new((Mutex::new(0u64), Condvar::new()));
    let handler_signal = signal.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        let (count, cv) = &*handler_signal;
        *count.lock().unwrap() += 1;
        cv.notify_one();
        Ok(())
    }))?;

    // Own copy of the latest frame, so the pool buffer goes back to the capturer at once.
    let latest = texture(
        &device,
        src_w,
        src_h,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_RENDER_TARGET,
    )?;
    let converter = Converter::new(&device, &context, &latest, (src_w, src_h), args, 6)?;
    let encoder = Encoder::new(&device, args)?;

    // 1 ms timer resolution for the frame pacing waits below.
    // SAFETY: balanced by timeEndPeriod at the end of the function.
    unsafe { timeBeginPeriod(1) };
    session.StartCapture()?;

    let interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let duration_100ns = 10_000_000 / args.fps as i64;
    let started = Instant::now();
    let cpu_start = process_cpu();
    let mut next_tick = started;
    let mut have_frame = false;
    let mut fresh_capture: Option<i64> = None;
    let (mut frames_arrived, mut fresh_used, mut submitted, mut skipped_ticks, mut no_credit) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut credits = 0u32;
    let mut pending = std::collections::HashMap::<i64, Pending>::new();
    let (mut encoded, mut keyframes, mut bytes) = (0u64, 0u64, 0u64);
    let mut first_output: Option<Instant> = None;
    let mut capture_to_encoded = Vec::new();
    let mut encode_ms = Vec::new();
    let mut gpu_convert_ms = Vec::new();
    let run_for = Duration::from_secs(args.seconds);

    let result: Result<()> = (|| {
        while started.elapsed() < run_for {
            // Newest captured frame wins; older ones are dropped unseen.
            while let Ok(frame) = pool.TryGetNextFrame() {
                frames_arrived += 1;
                let content = frame.ContentSize()?;
                if content.Width as u32 != src_w || content.Height as u32 != src_h {
                    bail!("monitor resolution changed during the run");
                }
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                // SAFETY: GPU copy between two live textures of the same size/format.
                unsafe {
                    let tex: ID3D11Texture2D = access.GetInterface()?;
                    context.CopyResource(&latest, &tex);
                }
                fresh_capture = Some(frame.SystemRelativeTime()?.Duration);
                have_frame = true;
            }
            while let Some(kind) = encoder.poll_event()? {
                if kind == METransformNeedInput.0 {
                    credits += 1;
                } else if kind == METransformHaveOutput.0
                    && let Some((time, len, key)) = encoder.take_output()?
                {
                    let now = qpc_100ns();
                    first_output.get_or_insert_with(Instant::now);
                    encoded += 1;
                    bytes += len as u64;
                    keyframes += key as u64;
                    if let Some(p) = pending.remove(&time) {
                        encode_ms.push((now - p.submitted) as f64 / 10_000.0);
                        if let Some(c) = p.capture_time {
                            capture_to_encoded.push((now - c) as f64 / 10_000.0);
                        }
                    }
                }
            }
            let now = Instant::now();
            if have_frame && now >= next_tick {
                if credits == 0 {
                    no_credit += 1;
                } else {
                    let t0 = Instant::now();
                    let nv12 = converter.convert(submitted as usize)?;
                    gpu_convert_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    let time = submitted as i64 * duration_100ns;
                    let capture_time = fresh_capture.take();
                    fresh_used += capture_time.is_some() as u64;
                    pending.insert(
                        time,
                        Pending {
                            capture_time,
                            submitted: qpc_100ns(),
                        },
                    );
                    encoder.submit(&nv12, time, duration_100ns)?;
                    credits -= 1;
                    submitted += 1;
                }
                next_tick += interval;
                if now > next_tick + interval {
                    // Fell behind by more than a frame: skip ahead instead of bursting.
                    let behind = ((now - next_tick).as_secs_f64() / interval.as_secs_f64()) as u64;
                    skipped_ticks += behind;
                    next_tick += interval * behind as u32;
                }
            }
            let (count, cv) = &*signal;
            let wait = next_tick
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1));
            let guard = count.lock().unwrap();
            let _ = cv.wait_timeout(guard, wait).unwrap();
        }
        Ok(())
    })();

    let wall = started.elapsed().as_secs_f64();
    let cpu = (process_cpu() - cpu_start).as_secs_f64();
    let pattern_frames = pattern.as_ref().map(|p| p.frames_drawn());
    drop(pattern);
    let _ = session.Close();
    let _ = pool.Close();
    // SAFETY: matches timeBeginPeriod above.
    unsafe { timeEndPeriod(1) };
    result?;

    let encode_span = first_output
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(wall);
    Ok(json!({
        "os": "windows",
        "tool": "turbo-pancake-screen-probe",
        "version": env!("CARGO_PKG_VERSION"),
        "adapter": adapter_name().unwrap_or_default(),
        "monitor": monitor_json(args.monitor, monitor),
        "capture": { "api": "Windows.Graphics.Capture", "source": [src_w, src_h], "border_disabled": border_off, "cursor": true },
        "encoder": { "name": encoder.name, "codec": "h264", "profile": "high", "settings_applied": encoder.applied, "provides_samples": encoder.provides_samples },
        "requested": { "width": args.width, "height": args.height, "fps": args.fps, "bitrate_kbps": args.bitrate_kbps, "gop_seconds": args.gop_seconds, "seconds": args.seconds },
        "result": {
            "wall_s": (wall * 1000.0).round() / 1000.0,
            "capture_frames_arrived": frames_arrived,
            "capture_fps": (frames_arrived as f64 / wall * 10.0).round() / 10.0,
            "submitted": submitted,
            "submitted_fresh": fresh_used,
            "submitted_repeat": submitted - fresh_used,
            "encoded": encoded,
            "encoded_fps": (encoded as f64 / encode_span * 10.0).round() / 10.0,
            "keyframes": keyframes,
            "bitrate_kbps": (bytes as f64 * 8.0 / encode_span / 1000.0).round(),
            "ticks_without_encoder_credit": no_credit,
            "ticks_skipped_behind": skipped_ticks,
            "in_flight_at_end": pending.len(),
            "latency_ms": {
                "capture_to_encoded_fresh_frames": percentiles(&mut capture_to_encoded),
                "submit_to_encoded": percentiles(&mut encode_ms),
                "gpu_convert_submit_cpu": percentiles(&mut gpu_convert_ms),
            },
            "process_cpu_percent_of_one_core": (cpu / wall * 1000.0).round() / 10.0,
        },
        "pattern_frames_drawn": pattern_frames,
        "notes": [
            "Windows.Graphics.Capture delivers a frame only when the monitor content changes; on a static screen the encoder repeats the last frame (submitted_repeat), as a real share would.",
            "capture_to_encoded uses the capture timestamp (SystemRelativeTime) of frames encoded for the first time; it excludes network, decode and display.",
        ],
    }))
}
