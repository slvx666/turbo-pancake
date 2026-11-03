//! H.264 encoder MFT. Hardware MFTs (NVENC, AMF, QSV) are asynchronous and driven by their
//! event queue; the Microsoft software encoder (part of the OS, ADR 0009) is synchronous.
//! Both look the same to the caller: `ready` → `encode` → `poll_output`.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};

use super::{OsContext, device_manager, pack, sample_bytes};
use crate::{EncodedFrame, EncoderConfig, VideoError};

/// A frame to encode: an NV12 texture on the encoder's device, or tight NV12 bytes
/// (`width * height * 3 / 2`).
pub enum EncoderInput<'a> {
    Texture(&'a ID3D11Texture2D),
    Nv12(&'a [u8]),
}

pub struct H264Encoder {
    name: String,
    hardware: bool,
    asynchronous: bool,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    /// The MFT reads D3D11 surfaces directly.
    takes_textures: bool,
    provides_samples: bool,
    output_size: u32,
    /// Async MFT: inputs it asked for and has not received yet.
    credits: u32,
    output: VecDeque<EncodedFrame>,
    staging: Option<ID3D11Texture2D>,
    device: ID3D11Device,
    config: EncoderConfig,
    applied: Vec<&'static str>,
    _manager: Option<IMFDXGIDeviceManager>,
}

fn enum_encoders(flags: MFT_ENUM_FLAG) -> Vec<(String, IMFActivate)> {
    let info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx fills a CoTaskMem array of `count` activates we take ownership of.
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&info),
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
                out.push((friendly_name(&activate), activate));
            }
        }
        CoTaskMemFree(Some(array as *const _));
        out
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: the string is allocated by MF and freed here.
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .is_err()
        {
            return "?".into();
        }
        let s = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const _));
        s
    }
}

/// Names of the hardware H.264 encoders on this system.
pub fn hardware_encoder_names() -> Vec<String> {
    enum_encoders(MFT_ENUM_FLAG_HARDWARE)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

impl H264Encoder {
    /// First working hardware encoder; the OS software encoder unless
    /// `config.require_hardware`.
    pub fn new(device: &ID3D11Device, config: &EncoderConfig) -> Result<Self, VideoError> {
        config.validate()?;
        let mut last = None;
        let hardware = if config.software_only {
            Vec::new()
        } else {
            enum_encoders(MFT_ENUM_FLAG_HARDWARE)
        };
        for (name, activate) in hardware {
            match Self::open(device, config, name, &activate, true) {
                Ok(e) => return Ok(e),
                Err(e) => last = Some(e),
            }
        }
        if !config.require_hardware {
            for (name, activate) in enum_encoders(MFT_ENUM_FLAG_SYNCMFT) {
                match Self::open(device, config, name, &activate, false) {
                    Ok(e) => return Ok(e),
                    Err(e) => last = Some(e),
                }
            }
        }
        Err(last.unwrap_or(VideoError::NoCodec("encoder")))
    }

    fn open(
        device: &ID3D11Device,
        config: &EncoderConfig,
        name: String,
        activate: &IMFActivate,
        hardware: bool,
    ) -> Result<Self, VideoError> {
        // SAFETY: COM calls on live interfaces with values owned here.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().os("activate encoder")?;
            let attrs = transform.GetAttributes().os("encoder attributes")?;
            let asynchronous = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if asynchronous {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .os("async unlock")?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut manager = None;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 || hardware {
                let m = device_manager(device)?;
                if transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .is_ok()
                {
                    manager = Some(m);
                } else if hardware {
                    return Err(VideoError::Os {
                        context: "encoder",
                        message: format!("{name} rejected the D3D11 device"),
                    });
                }
            }

            let out = MFCreateMediaType().os("media type")?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .os("type")?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .os("type")?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_kbps * 1000)
                .os("type")?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))
                .os("type")?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps, 1))
                .os("type")?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
                .os("type")?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .os("type")?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .os("type")?;
            transform
                .SetOutputType(0, &out, 0)
                .os("encoder output type")?;

            let input = MFCreateMediaType().os("media type")?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .os("type")?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .os("type")?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))
                .os("type")?;
            input
                .SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps, 1))
                .os("type")?;
            input
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
                .os("type")?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .os("type")?;
            transform
                .SetInputType(0, &input, 0)
                .os("encoder input type")?;

            let codec_api = transform.cast::<ICodecAPI>().ok();
            let mut applied = Vec::new();
            if let Some(api) = &codec_api {
                let settings: [(&'static str, GUID, VARIANT); 5] = [
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
                        VARIANT::from(config.bitrate_kbps * 1000),
                    ),
                    (
                        "gop",
                        CODECAPI_AVEncMPVGOPSize,
                        VARIANT::from(config.gop_frames),
                    ),
                    (
                        "no_b_frames",
                        CODECAPI_AVEncMPVDefaultBPictureCount,
                        VARIANT::from(0u32),
                    ),
                ];
                for (label, key, value) in settings {
                    if api.SetValue(&key, &value).is_ok() {
                        applied.push(label);
                    }
                }
            }

            let info = transform.GetOutputStreamInfo(0).os("output stream info")?;
            let events = if asynchronous {
                Some(transform.cast().os("event generator")?)
            } else {
                None
            };
            // The Microsoft software encoder rejects a flush before streaming; not fatal.
            let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .os("begin streaming")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .os("start of stream")?;
            Ok(Self {
                name,
                hardware,
                asynchronous,
                transform,
                events,
                codec_api,
                takes_textures: manager.is_some(),
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                output_size: info
                    .cbSize
                    .max(config.width * config.height * 3 / 2)
                    .max(1 << 16),
                credits: 0,
                output: VecDeque::new(),
                staging: None,
                device: device.clone(),
                config: config.clone(),
                applied,
                _manager: manager,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Encoder settings the MFT accepted (`low_latency`, `cbr`, …), for reports.
    pub fn settings_applied(&self) -> &[&'static str] {
        &self.applied
    }

    /// Processes pending encoder events; true when it can take a frame now. A caller that
    /// gets `false` skips this frame (the newest frame wins, nothing queues up).
    pub fn ready(&mut self) -> Result<bool, VideoError> {
        self.pump()?;
        Ok(!self.asynchronous || self.credits > 0)
    }

    fn pump(&mut self) -> Result<(), VideoError> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            // SAFETY: COM call on a live interface.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).os("encoder GetEvent"),
            };
            // SAFETY: as above.
            let kind = unsafe { event.GetType() }.os("event type")? as i32;
            if kind == METransformNeedInput.0 {
                self.credits += 1;
            } else if kind == METransformHaveOutput.0 {
                self.take_output()?;
            }
        }
    }

    /// Queues one frame. `timestamp` and `duration` are in 100 ns units.
    pub fn encode(
        &mut self,
        input: EncoderInput<'_>,
        timestamp: i64,
        duration: i64,
    ) -> Result<(), VideoError> {
        if !self.ready()? {
            return Err(VideoError::Os {
                context: "encode",
                message: "encoder is not ready for input".into(),
            });
        }
        let sample = self.make_sample(input)?;
        // SAFETY: COM calls; the sample owns its buffer.
        unsafe {
            sample.SetSampleTime(timestamp).os("sample time")?;
            sample.SetSampleDuration(duration).os("sample duration")?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .os("ProcessInput")?;
        }
        if self.asynchronous {
            self.credits -= 1;
        } else {
            while self.take_output()? {}
        }
        Ok(())
    }

    fn make_sample(&mut self, input: EncoderInput<'_>) -> Result<IMFSample, VideoError> {
        let frame_len = (self.config.width * self.config.height * 3 / 2) as usize;
        // SAFETY: COM calls; buffers are locked only for the copy.
        unsafe {
            let sample = MFCreateSample().os("MFCreateSample")?;
            match input {
                EncoderInput::Texture(tex) if self.takes_textures => {
                    let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)
                        .os("MFCreateDXGISurfaceBuffer")?;
                    sample.AddBuffer(&buffer).os("AddBuffer")?;
                }
                EncoderInput::Texture(tex) => {
                    let bytes = self.read_back(tex)?;
                    sample.AddBuffer(&memory_buffer(&bytes)?).os("AddBuffer")?;
                }
                EncoderInput::Nv12(bytes) => {
                    if bytes.len() != frame_len {
                        return Err(VideoError::InvalidConfig("NV12 frame size mismatch"));
                    }
                    sample.AddBuffer(&memory_buffer(bytes)?).os("AddBuffer")?;
                }
            }
            Ok(sample)
        }
    }

    /// GPU → CPU copy of an NV12 texture with tight rows (software encoder path).
    fn read_back(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<u8>, VideoError> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        // SAFETY: COM calls on live interfaces; the mapping is read within its pitch/rows.
        unsafe {
            if self.staging.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w as u32,
                    Height: h as u32,
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
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .os("staging texture")?;
                self.staging = t;
            }
            let staging = self
                .staging
                .as_ref()
                .ok_or(VideoError::NoCodec("staging"))?;
            let context = self.device.GetImmediateContext().os("context")?;
            context.CopyResource(staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .os("Map")?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            let mut out = Vec::with_capacity(w * h * 3 / 2);
            // Luma rows, then the interleaved UV rows that follow `h` padded rows.
            for row in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w));
            }
            for row in 0..h / 2 {
                out.extend_from_slice(std::slice::from_raw_parts(base.add((h + row) * pitch), w));
            }
            context.Unmap(staging, 0);
            Ok(out)
        }
    }

    /// Next encoded access unit, if any.
    pub fn poll_output(&mut self) -> Result<Option<EncodedFrame>, VideoError> {
        self.pump()?;
        Ok(self.output.pop_front())
    }

    /// The next encoded frame will be a keyframe (IDR).
    pub fn request_keyframe(&mut self) -> Result<(), VideoError> {
        self.set_value(CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32))
    }

    /// Changes the target bitrate while encoding.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), VideoError> {
        self.set_value(CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(kbps * 1000))?;
        self.config.bitrate_kbps = kbps;
        Ok(())
    }

    fn set_value(&self, key: GUID, value: VARIANT) -> Result<(), VideoError> {
        let api = self
            .codec_api
            .as_ref()
            .ok_or(VideoError::NoCodec("encoder control"))?;
        // SAFETY: COM call with a VARIANT owned here.
        unsafe { api.SetValue(&key, &value) }.os("ICodecAPI::SetValue")
    }

    /// Flushes the remaining frames out of the encoder (end of a share).
    pub fn drain(&mut self) -> Result<Vec<EncodedFrame>, VideoError> {
        // SAFETY: COM call on a live interface.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .os("drain")?;
        }
        if self.asynchronous {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let events = self.events.clone().ok_or(VideoError::NoCodec("events"))?;
            while std::time::Instant::now() < deadline {
                // SAFETY: COM call on a live interface.
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => {
                        // SAFETY: as above.
                        let kind = unsafe { event.GetType() }.os("event type")? as i32;
                        if kind == METransformHaveOutput.0 {
                            self.take_output()?;
                        } else if kind == METransformNeedInput.0 {
                            self.credits += 1;
                        } else if kind == METransformDrainComplete.0 {
                            break;
                        }
                    }
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(e) => return Err(e).os("encoder GetEvent"),
                }
            }
        } else {
            while self.take_output()? {}
        }
        Ok(self.output.drain(..).collect())
    }

    /// One ProcessOutput call; true when it produced a frame.
    fn take_output(&mut self) -> Result<bool, VideoError> {
        // SAFETY: the output buffer struct owns the sample/events refs we take out of it.
        unsafe {
            let sample = if self.provides_samples {
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
                    let ty = self
                        .transform
                        .GetOutputAvailableType(0, 0)
                        .os("output type")?;
                    self.transform
                        .SetOutputType(0, &ty, 0)
                        .os("set output type")?;
                    return Ok(false);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) => return Err(e).os("ProcessOutput"),
            }
            let Some(sample) = sample else {
                return Ok(false);
            };
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            self.output.push_back(EncodedFrame {
                data: sample_bytes(&sample)?,
                timestamp: sample.GetSampleTime().unwrap_or(0),
                keyframe,
            });
            Ok(true)
        }
    }
}

impl Drop for H264Encoder {
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
            if self.asynchronous
                && let Ok(shutdown) = self.transform.cast::<IMFShutdown>()
            {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

pub(crate) fn memory_buffer(bytes: &[u8]) -> Result<IMFMediaBuffer, VideoError> {
    // SAFETY: the buffer is at least `bytes.len()` long and locked only for the copy.
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32).os("MFCreateMemoryBuffer")?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None).os("Lock")?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = buffer.Unlock();
        buffer
            .SetCurrentLength(bytes.len() as u32)
            .os("SetCurrentLength")?;
        Ok(buffer)
    }
}
