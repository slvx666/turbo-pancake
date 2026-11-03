//! H.264 decoder: the Microsoft H.264 decoder MFT (part of the OS, ADR 0009). With a D3D11
//! device manager it decodes on the GPU through DXVA; without one, on the CPU.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use super::encoder::memory_buffer;
use super::{ID3D11Device, ID3D11Texture2D, OsContext, device_manager};
use crate::{DecodedFrame, VideoError};

pub struct H264Decoder {
    transform: IMFTransform,
    dxva: bool,
    provides_samples: bool,
    output_size: u32,
    /// Allocated (padded) picture size and visible size of the current output type.
    alloc: (u32, u32),
    visible: (u32, u32),
    output: VecDeque<DecodedFrame>,
    /// DXVA path: the decoder's device and our CPU-readable copy of its output surface.
    device: Option<ID3D11Device>,
    staging: Option<(ID3D11Texture2D, (u32, u32))>,
    _manager: Option<IMFDXGIDeviceManager>,
}

impl H264Decoder {
    /// `device`: decode on this GPU through DXVA when the decoder accepts it.
    pub fn new(device: Option<&ID3D11Device>) -> Result<Self, VideoError> {
        // SAFETY: COM calls on live interfaces with values owned here.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|_| VideoError::NoCodec("decoder"))?;
            let attrs = transform.GetAttributes().os("decoder attributes")?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            let mut manager = None;
            if let Some(device) = device
                && attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0
            {
                let m = device_manager(device)?;
                if transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .is_ok()
                {
                    manager = Some(m);
                }
            }

            let input = MFCreateMediaType().os("media type")?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .os("type")?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .os("type")?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .os("type")?;
            transform
                .SetInputType(0, &input, 0)
                .os("decoder input type")?;

            let mut decoder = Self {
                transform,
                dxva: manager.is_some(),
                provides_samples: false,
                output_size: 0,
                alloc: (0, 0),
                visible: (0, 0),
                output: VecDeque::new(),
                device: manager.as_ref().and(device.cloned()),
                staging: None,
                _manager: manager,
            };
            decoder.choose_output_type()?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .os("begin streaming")?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .os("start of stream")?;
            Ok(decoder)
        }
    }

    /// True when the decoder runs on the GPU (DXVA).
    pub fn is_hardware(&self) -> bool {
        self.dxva
    }

    fn choose_output_type(&mut self) -> Result<(), VideoError> {
        // SAFETY: COM calls on a live interface.
        unsafe {
            let mut index = 0;
            loop {
                let ty = match self.transform.GetOutputAvailableType(0, index) {
                    Ok(t) => t,
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => {
                        return Err(VideoError::NoCodec("decoder NV12 output"));
                    }
                    // Before the first SPS there may be no output types yet.
                    Err(e) if e.code() == MF_E_TRANSFORM_TYPE_NOT_SET => return Ok(()),
                    Err(e) => return Err(e).os("decoder output type"),
                };
                if ty.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.transform
                        .SetOutputType(0, &ty, 0)
                        .os("set decoder output type")?;
                    let size = ty.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                    self.alloc = ((size >> 32) as u32, size as u32);
                    self.visible = aperture(&ty).unwrap_or(self.alloc);
                    let info = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .os("output stream info")?;
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    self.output_size = info.cbSize;
                    return Ok(());
                }
                index += 1;
            }
        }
    }

    /// Feeds one Annex B access unit; decoded pictures become available from `poll_output`.
    pub fn decode(&mut self, data: &[u8], timestamp: i64) -> Result<(), VideoError> {
        // SAFETY: COM calls; the sample owns its buffer.
        unsafe {
            let sample = MFCreateSample().os("MFCreateSample")?;
            sample.AddBuffer(&memory_buffer(data)?).os("AddBuffer")?;
            sample.SetSampleTime(timestamp).os("sample time")?;
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        if !self.take_output()? {
                            return Err(e).os("decoder ProcessInput");
                        }
                    }
                    Err(e) => return Err(e).os("decoder ProcessInput"),
                }
            }
        }
        while self.take_output()? {}
        Ok(())
    }

    pub fn poll_output(&mut self) -> Option<DecodedFrame> {
        self.output.pop_front()
    }

    /// Flushes pictures still held by the decoder.
    pub fn drain(&mut self) -> Result<Vec<DecodedFrame>, VideoError> {
        // SAFETY: COM call on a live interface.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .os("drain")?;
        }
        while self.take_output()? {}
        Ok(self.output.drain(..).collect())
    }

    /// One ProcessOutput call; true when it produced a picture or changed the format.
    fn take_output(&mut self) -> Result<bool, VideoError> {
        // SAFETY: the output buffer struct owns the sample/events refs we take out of it.
        unsafe {
            let sample = if self.provides_samples || self.output_size == 0 {
                None
            } else {
                let s = MFCreateSample().os("MFCreateSample")?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.output_size).os("buffer")?)
                    .os("AddBuffer")?;
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
                    self.choose_output_type()?;
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) => return Err(e).os("decoder ProcessOutput"),
            }
            let Some(sample) = sample else {
                return Ok(false);
            };
            let frame = self.picture(&sample)?;
            self.output.push_back(frame);
            Ok(true)
        }
    }

    /// Copies the visible NV12 picture of `sample` into tight rows.
    fn picture(&mut self, sample: &IMFSample) -> Result<DecodedFrame, VideoError> {
        let (w, h) = (self.visible.0 as usize, self.visible.1 as usize);
        let alloc_h = self.alloc.1.max(self.visible.1) as usize;
        // SAFETY: the buffer is locked while read, within pitch × rows of the allocation.
        unsafe {
            let buffer = sample.GetBufferByIndex(0).os("GetBufferByIndex")?;
            let mut nv12 = Vec::with_capacity(w * h * 3 / 2);
            let mut copy = |base: *const u8, pitch: usize| {
                for row in 0..h {
                    nv12.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w));
                }
                let uv = base.add(alloc_h * pitch);
                for row in 0..h / 2 {
                    nv12.extend_from_slice(std::slice::from_raw_parts(uv.add(row * pitch), w));
                }
            };
            if let (Ok(dxgi), Some(device)) = (buffer.cast::<IMFDXGIBuffer>(), self.device.clone())
            {
                // Copying the decoder's surface into our own staging texture and mapping it
                // takes a few ms; IMF2DBuffer::Lock2D on the same surface took ≈ 55 ms per
                // 1080p frame on the dev GPU.
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                    .os("decoded surface")?;
                let surface = ID3D11Texture2D::from_raw(raw);
                let index = dxgi.GetSubresourceIndex().os("subresource index")?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                surface.GetDesc(&mut desc);
                let size = (desc.Width, desc.Height);
                if self.staging.as_ref().map(|(_, s)| *s) != Some(size) {
                    let staging_desc = D3D11_TEXTURE2D_DESC {
                        Width: size.0,
                        Height: size.1,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_NV12,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut t = None;
                    device
                        .CreateTexture2D(&staging_desc, None, Some(&mut t))
                        .os("staging texture")?;
                    self.staging = t.map(|t| (t, size));
                }
                let (staging, _) = self
                    .staging
                    .as_ref()
                    .ok_or(VideoError::NoCodec("staging"))?;
                let context = device.GetImmediateContext().os("context")?;
                context.CopySubresourceRegion(staging, 0, 0, 0, 0, &surface, index, None);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .os("Map")?;
                let pitch = mapped.RowPitch as usize;
                let base = mapped.pData as *const u8;
                for row in 0..h {
                    nv12.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w));
                }
                let uv = base.add(size.1 as usize * pitch);
                for row in 0..h / 2 {
                    nv12.extend_from_slice(std::slice::from_raw_parts(uv.add(row * pitch), w));
                }
                context.Unmap(staging, 0);
            } else if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut base = std::ptr::null_mut();
                let mut pitch = 0i32;
                two_d.Lock2D(&mut base, &mut pitch).os("Lock2D")?;
                copy(base, pitch.unsigned_abs() as usize);
                let _ = two_d.Unlock2D();
            } else {
                let mut base = std::ptr::null_mut();
                buffer.Lock(&mut base, None, None).os("Lock")?;
                copy(base, self.alloc.0.max(self.visible.0) as usize);
                let _ = buffer.Unlock();
            }
            Ok(DecodedFrame {
                width: w as u32,
                height: h as u32,
                timestamp: sample.GetSampleTime().unwrap_or(0),
                nv12,
            })
        }
    }
}

/// Visible size from `MF_MT_MINIMUM_DISPLAY_APERTURE` (an `MFVideoArea`: two `MFOffset`s,
/// then `SIZE { cx, cy }`).
fn aperture(ty: &IMFMediaType) -> Option<(u32, u32)> {
    let mut blob = [0u8; 16];
    // SAFETY: the blob is copied into a buffer of the struct's size.
    unsafe {
        ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, None)
            .ok()?;
    }
    let cx = i32::from_le_bytes(blob[8..12].try_into().ok()?);
    let cy = i32::from_le_bytes(blob[12..16].try_into().ok()?);
    (cx > 0 && cy > 0).then_some((cx as u32, cy as u32))
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: COM calls on a live interface.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}
