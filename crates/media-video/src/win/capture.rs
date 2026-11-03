//! Windows.Graphics.Capture of a whole monitor (spec §5.1: the selected monitor, never a
//! window) and GPU conversion BGRA → NV12 with scaling (D3D11 video processor). The CPU
//! never touches pixels: the captured surface is copied into our own texture (so the
//! capture pool gets its buffer back at once) and converted on the GPU for the encoder.

use std::mem::ManuallyDrop;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, Interface};

use super::OsContext;
use crate::{MonitorInfo, VideoError};

fn handles() -> Vec<HMONITOR> {
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
}

/// The monitors, in the system's enumeration order (index = position in this list).
pub fn monitors() -> Vec<MonitorInfo> {
    handles()
        .into_iter()
        .enumerate()
        .map(|(index, handle)| {
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
            let r = info.monitorInfo.rcMonitor;
            MonitorInfo {
                index,
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                refresh_hz: mode.dmDisplayFrequency,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            }
        })
        .collect()
}

pub(crate) fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D, VideoError> {
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
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.os("CreateTexture2D")?;
    tex.ok_or(VideoError::NoCodec("texture"))
}

/// GPU colour conversion + scaling from a BGRA texture into a small pool of NV12 textures.
pub struct Converter {
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    /// `source` (BGRA, `src` size) → NV12 `dst` size; `pool` textures rotate so the
    /// encoder can still read one while the next is written.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        src: (u32, u32),
        dst: (u32, u32),
        fps: u32,
        pool: usize,
    ) -> Result<Self, VideoError> {
        let video_device: ID3D11VideoDevice = device.cast().os("ID3D11VideoDevice")?;
        let video_context: ID3D11VideoContext1 = context.cast().os("ID3D11VideoContext1")?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src.0,
            InputHeight: src.1,
            OutputFrameRate: rate,
            OutputWidth: dst.0,
            OutputHeight: dst.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: COM calls with descriptors owned here; views reference live textures.
        unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .os("video processor enumerator")?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .os("video processor")?;
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
            video_device
                .CreateVideoProcessorInputView(source, &enumerator, &in_desc, Some(&mut input_view))
                .os("input view")?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut outputs = Vec::new();
            for _ in 0..pool.max(1) {
                let tex = texture(
                    device,
                    dst.0,
                    dst.1,
                    DXGI_FORMAT_NV12,
                    D3D11_BIND_RENDER_TARGET,
                )?;
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&tex, &enumerator, &out_desc, Some(&mut view))
                    .os("output view")?;
                outputs.push((tex, view.ok_or(VideoError::NoCodec("output view"))?));
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
                input_view: input_view.ok_or(VideoError::NoCodec("input view"))?,
                outputs,
                next: 0,
            })
        }
    }

    /// Converts the current source picture into the next pool texture.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D, VideoError> {
        let (tex, view) = &self.outputs[self.next % self.outputs.len()];
        self.next += 1;
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
        // SAFETY: releases the reference taken above exactly once.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.os("VideoProcessorBlt")?;
        Ok(tex.clone())
    }
}

/// A running capture of one monitor. New frames are signalled to whoever waits in
/// [`MonitorCapture::wait`]; [`MonitorCapture::take_latest`] copies the newest one into the
/// capture's own texture and drops older ones unseen.
pub struct MonitorCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    latest: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    size: (u32, u32),
    signal: Arc<(Mutex<u64>, Condvar)>,
    seen: u64,
    pub border_disabled: bool,
}

impl MonitorCapture {
    pub fn start(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        monitor: usize,
    ) -> Result<Self, VideoError> {
        let handle = *handles()
            .get(monitor)
            .ok_or(VideoError::InvalidConfig("no such monitor"))?;
        let dxgi: IDXGIDevice = device.cast().os("IDXGIDevice")?;
        // SAFETY: WinRT interop calls on live interfaces.
        let (item, winrt_device) = unsafe {
            let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .and_then(|d| d.cast())
                .os("WinRT device")?;
            let interop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .os("capture interop")?;
            let item: GraphicsCaptureItem = interop
                .CreateForMonitor(handle)
                .os("capture item for monitor")?;
            (item, winrt_device)
        };
        let size = item.Size().os("capture size")?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .os("frame pool")?;
        let session = pool.CreateCaptureSession(&item).os("capture session")?;
        let border_disabled = session.SetIsBorderRequired(false).is_ok();
        let _ = session.SetIsCursorCaptureEnabled(true);
        let signal = Arc::new((Mutex::new(0u64), Condvar::new()));
        let handler_signal = signal.clone();
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            let (count, cv) = &*handler_signal;
            if let Ok(mut c) = count.lock() {
                *c += 1;
            }
            cv.notify_one();
            Ok(())
        }))
        .os("FrameArrived")?;
        let (w, h) = (size.Width as u32, size.Height as u32);
        let latest = texture(
            device,
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET,
        )?;
        session.StartCapture().os("StartCapture")?;
        Ok(Self {
            pool,
            session,
            latest,
            context: context.clone(),
            size: (w, h),
            signal,
            seen: 0,
            border_disabled,
        })
    }

    /// Captured picture size (the monitor's resolution).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The texture [`MonitorCapture::take_latest`] copies into (input of a [`Converter`]).
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.latest
    }

    /// Waits up to `timeout` for a frame newer than the last one taken.
    pub fn wait(&mut self, timeout: Duration) {
        let (count, cv) = &*self.signal;
        let Ok(guard) = count.lock() else {
            return;
        };
        if *guard != self.seen {
            return;
        }
        let _ = cv.wait_timeout(guard, timeout);
    }

    /// Copies the newest captured frame (if any arrived) into [`MonitorCapture::texture`].
    /// Returns its capture time (100 ns units of the performance counter) and how many
    /// frames arrived since the last call. Errors when the monitor's resolution changed.
    pub fn take_latest(&mut self) -> Result<Option<(i64, u64)>, VideoError> {
        if let Ok(c) = self.signal.0.lock() {
            self.seen = *c;
        }
        let mut newest = None;
        let mut arrived = 0;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            arrived += 1;
            let content = frame.ContentSize().os("ContentSize")?;
            if (content.Width as u32, content.Height as u32) != self.size {
                return Err(VideoError::InvalidConfig(
                    "the monitor's resolution changed during the share",
                ));
            }
            let access: IDirect3DDxgiInterfaceAccess =
                frame.Surface().and_then(|s| s.cast()).os("frame surface")?;
            // SAFETY: GPU copy between two live textures of the same size and format.
            unsafe {
                let tex: ID3D11Texture2D = access.GetInterface().os("frame texture")?;
                self.context.CopyResource(&self.latest, &tex);
            }
            newest = Some(frame.SystemRelativeTime().os("frame time")?.Duration);
        }
        Ok(newest.map(|t| (t, arrived)))
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
