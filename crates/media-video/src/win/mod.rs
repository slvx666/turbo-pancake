//! Windows: Media Foundation H.264 encoder/decoder MFTs on a D3D11 device.

#![allow(unsafe_code)]

mod decoder;
mod encoder;

pub use decoder::H264Decoder;
pub use encoder::{EncoderInput, H264Encoder, hardware_encoder_names};

use std::sync::Once;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::Interface;

pub use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use crate::VideoError;

pub(crate) trait OsContext<T> {
    fn os(self, context: &'static str) -> Result<T, VideoError>;
}

impl<T> OsContext<T> for windows::core::Result<T> {
    fn os(self, context: &'static str) -> Result<T, VideoError> {
        self.map_err(|e| VideoError::Os {
            context,
            message: e.message().to_string(),
        })
    }
}

/// Initialises COM (MTA) for the calling thread and Media Foundation for the process.
/// Call on every thread that uses the encoder/decoder.
pub fn startup() -> Result<(), VideoError> {
    static MF: Once = Once::new();
    // SAFETY: plain library initialisation. COM may already be initialised on this thread
    // (S_FALSE / RPC_E_CHANGED_MODE are fine: MF objects here are free-threaded).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut result = Ok(());
    MF.call_once(|| {
        // SAFETY: once per process, never shut down (the process owns MF until exit).
        result = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.os("MFStartup");
    });
    result
}

/// A D3D11 device suitable for capture, conversion and the codecs: video support,
/// multithread protection (MFTs use it from their own threads). Falls back to WARP when
/// there is no hardware adapter (CI runners).
pub fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, bool), VideoError> {
    let mut last = None;
    for (driver, hardware) in [
        (D3D_DRIVER_TYPE_HARDWARE, true),
        (D3D_DRIVER_TYPE_WARP, false),
    ] {
        match create_device_of(driver) {
            Ok((d, c)) => return Ok((d, c, hardware)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or(VideoError::NoCodec("device")))
}

fn create_device_of(
    driver: D3D_DRIVER_TYPE,
) -> Result<(ID3D11Device, ID3D11DeviceContext), VideoError> {
    let mut device = None;
    let mut context = None;
    // SAFETY: standard device creation with out-params owned here.
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .os("D3D11CreateDevice")?;
    }
    let device: ID3D11Device = device.ok_or(VideoError::NoCodec("device"))?;
    let mt: ID3D11Multithread = device.cast().os("ID3D11Multithread")?;
    // SAFETY: COM call on a live interface.
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok((device, context.ok_or(VideoError::NoCodec("device"))?))
}

/// Adapter description of a device (for reports).
pub fn adapter_name(device: &ID3D11Device) -> String {
    let Ok(dxgi) = device.cast::<IDXGIDevice>() else {
        return String::new();
    };
    // SAFETY: COM calls on live interfaces.
    let Ok(desc) = (unsafe { dxgi.GetAdapter().and_then(|a| a.GetDesc()) }) else {
        return String::new();
    };
    let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
    String::from_utf16_lossy(&desc.Description[..len])
}

/// A DXGI device manager bound to `device`, for MFTs that take D3D11 surfaces.
pub(crate) fn device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager, VideoError> {
    let mut token = 0u32;
    let mut manager = None;
    // SAFETY: out-params owned here.
    unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut manager).os("MFCreateDXGIDeviceManager")?;
        let manager = manager.ok_or(VideoError::NoCodec("device manager"))?;
        manager.ResetDevice(device, token).os("ResetDevice")?;
        Ok(manager)
    }
}

pub(crate) fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Copies a sample's bytes (any buffer kind) into a vector.
pub(crate) fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, VideoError> {
    // SAFETY: the buffer is locked for the copy and unlocked right after.
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .os("ConvertToContiguousBuffer")?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).os("Lock")?;
        let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(out)
    }
}

/// 100 ns ticks of the performance counter (the clock of capture timestamps).
pub fn qpc_100ns() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let mut counter = 0i64;
    let mut freq = 0i64;
    // SAFETY: out-params owned here.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (counter as i128 * 10_000_000 / freq.max(1) as i128) as i64
}
