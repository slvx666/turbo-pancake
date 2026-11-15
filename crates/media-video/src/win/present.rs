//! Shows decoded pictures on the GPU: the decoder's NV12 surface goes through the D3D11
//! video processor (colour conversion and scaling) straight into the swap chain of a
//! window. Nothing is read back to the CPU and nothing passes the UI thread, so the rate
//! on screen is the decode rate (spec §5.1: up to 1440p120, minimal copying), not the
//! rate of a CPU conversion plus a software-rendered widget.
//!
//! The presenter belongs to the thread that decodes. The window belongs to the UI; the
//! presenter only reads its client size and resizes the swap chain to it.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::Interface;

use super::{ID3D11Device, ID3D11Texture2D, OsContext};
use crate::VideoError;

/// The video processor for one (source size → target size) pair.
struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    source: (u32, u32),
    target: (u32, u32),
}

pub struct Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    swap_chain: IDXGISwapChain1,
    hwnd: HWND,
    /// Size of the swap chain's buffers.
    size: (u32, u32),
    processor: Option<Processor>,
    presented: u64,
}

/// The picture rectangle inside a `target` area that keeps the `source` aspect ratio
/// (centred, bars on two sides): `(left, top, right, bottom)`.
pub fn letterbox(source: (u32, u32), target: (u32, u32)) -> (i32, i32, i32, i32) {
    let (sw, sh) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (tw, th) = (u64::from(target.0), u64::from(target.1));
    // Fit the width, or the height, whichever leaves the picture inside the target.
    let (w, h) = if tw * sh <= th * sw {
        (tw, (tw * sh / sw).max(1))
    } else {
        ((th * sw / sh).max(1), th)
    };
    let (left, top) = ((tw - w.min(tw)) / 2, (th - h.min(th)) / 2);
    (
        left as i32,
        top as i32,
        (left + w.min(tw)) as i32,
        (top + h.min(th)) as i32,
    )
}

impl Presenter {
    /// A swap chain on `hwnd` (a window handle as an integer) with `device`, which must
    /// be the device the decoder decodes on.
    pub fn new(device: &ID3D11Device, hwnd: isize) -> Result<Self, VideoError> {
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // SAFETY: COM calls on live interfaces; descriptors and out-params owned here.
        unsafe {
            let context = device.GetImmediateContext().os("device context")?;
            let video_device: ID3D11VideoDevice = device.cast().os("ID3D11VideoDevice")?;
            let video_context: ID3D11VideoContext1 = context.cast().os("ID3D11VideoContext1")?;
            let size = client_size(hwnd).unwrap_or((16, 16));
            let dxgi: IDXGIDevice = device.cast().os("IDXGIDevice")?;
            let adapter: IDXGIAdapter = dxgi.GetAdapter().os("adapter")?;
            let factory: IDXGIFactory2 = adapter.GetParent().os("DXGI factory")?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .os("swap chain")?;
            // The window is the UI's: no Alt+Enter handling by DXGI.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            Ok(Self {
                device: device.clone(),
                context,
                video_device,
                video_context,
                swap_chain,
                hwnd,
                size,
                processor: None,
                presented: 0,
            })
        }
    }

    /// Pictures presented so far.
    pub fn presented(&self) -> u64 {
        self.presented
    }

    /// Size of the area the pictures are drawn into (the window's client area).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Draws the visible part (`visible`, from the top-left corner) of slice `index` of
    /// the NV12 `surface`, letterboxed on black, and presents it without waiting for the
    /// display. `Ok(false)`: the window has no area right now (minimised), nothing shown.
    pub fn present(
        &mut self,
        surface: &ID3D11Texture2D,
        index: u32,
        visible: (u32, u32),
    ) -> Result<bool, VideoError> {
        Ok(self.draw(surface, index, visible, false)?.is_some())
    }

    /// [`Presenter::present`], returning what was drawn as tight BGRA rows (tests and
    /// diagnostics: one GPU → CPU copy per call).
    pub fn present_and_read(
        &mut self,
        surface: &ID3D11Texture2D,
        index: u32,
        visible: (u32, u32),
    ) -> Result<Option<(u32, u32, Vec<u8>)>, VideoError> {
        Ok(self
            .draw(surface, index, visible, true)?
            .map(|bytes| (self.size.0, self.size.1, bytes)))
    }

    fn draw(
        &mut self,
        surface: &ID3D11Texture2D,
        index: u32,
        visible: (u32, u32),
        read: bool,
    ) -> Result<Option<Vec<u8>>, VideoError> {
        let Some(size) = client_size(self.hwnd) else {
            return Ok(None);
        };
        // SAFETY: COM calls on live interfaces; views and descriptors owned here, the
        // stream's surface reference is released exactly once after the blit.
        unsafe {
            if size != self.size {
                // No reference to a buffer may be held across the resize; none is.
                self.processor = None;
                self.swap_chain
                    .ResizeBuffers(
                        0,
                        size.0,
                        size.1,
                        DXGI_FORMAT_UNKNOWN,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                    .os("resize swap chain")?;
                self.size = size;
            }
            if self
                .processor
                .as_ref()
                .is_none_or(|p| p.source != visible || p.target != size)
            {
                self.processor = Some(self.create_processor(visible, size)?);
            }
            let Some(p) = self.processor.as_ref() else {
                return Ok(None);
            };
            let back: ID3D11Texture2D = self.swap_chain.GetBuffer(0).os("back buffer")?;
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: index,
                    },
                },
            };
            let mut input_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    surface,
                    &p.enumerator,
                    &in_desc,
                    Some(&mut input_view),
                )
                .os("decoded surface view")?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &back,
                    &p.enumerator,
                    &out_desc,
                    Some(&mut output_view),
                )
                .os("back buffer view")?;
            let output_view = output_view.ok_or(VideoError::NoCodec("back buffer view"))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(input_view),
                ..Default::default()
            };
            let blit = self.video_context.VideoProcessorBlt(
                &p.processor,
                &output_view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            blit.os("VideoProcessorBlt")?;
            let bytes = if read {
                Some(self.read_back(&back)?)
            } else {
                None
            };
            drop(output_view);
            drop(back);
            // Interval 0: never wait for the display; the compositor shows the newest.
            self.swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .os("Present")?;
            self.presented += 1;
            Ok(Some(bytes.unwrap_or_default()))
        }
    }

    fn create_processor(
        &self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> Result<Processor, VideoError> {
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: target.0,
            OutputHeight: target.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: COM calls with descriptors owned here.
        unsafe {
            let enumerator = self
                .video_device
                .CreateVideoProcessorEnumerator(&desc)
                .os("video processor enumerator")?;
            let processor = self
                .video_device
                .CreateVideoProcessor(&enumerator, 0)
                .os("video processor")?;
            let vc = &self.video_context;
            vc.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            vc.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            vc.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            // Only the visible part of the (padded) decoder surface, letterboxed.
            let src = RECT {
                left: 0,
                top: 0,
                right: source.0 as i32,
                bottom: source.1 as i32,
            };
            let (left, top, right, bottom) = letterbox(source, target);
            let dst = RECT {
                left,
                top,
                right,
                bottom,
            };
            let whole = RECT {
                left: 0,
                top: 0,
                right: target.0 as i32,
                bottom: target.1 as i32,
            };
            vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            Ok(Processor {
                enumerator,
                processor,
                source,
                target,
            })
        }
    }

    /// The back buffer as tight BGRA rows.
    fn read_back(&self, back: &ID3D11Texture2D) -> Result<Vec<u8>, VideoError> {
        let (w, h) = self.size;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        // SAFETY: the staging texture is mapped for the copy and unmapped right after;
        // rows are read within pitch × height of the mapping.
        unsafe {
            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .os("staging texture")?;
            let staging = staging.ok_or(VideoError::NoCodec("staging"))?;
            self.context.CopyResource(&staging, back);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .os("Map")?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for row in 0..h as usize {
                out.extend_from_slice(std::slice::from_raw_parts(
                    base.add(row * pitch),
                    w as usize * 4,
                ));
            }
            self.context.Unmap(&staging, 0);
            Ok(out)
        }
    }
}

/// Client size of the window, `None` when it has no area (or no longer exists).
fn client_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut r = RECT::default();
    // SAFETY: out-param owned here; a stale handle only fails the call.
    unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::letterbox;

    #[test]
    fn letterbox_keeps_the_aspect_ratio_and_centres() {
        // Same shape: the whole area.
        assert_eq!(letterbox((1920, 1080), (1280, 720)), (0, 0, 1280, 720));
        // Wider area: bars left and right.
        assert_eq!(letterbox((1920, 1080), (2000, 720)), (360, 0, 1640, 720));
        // Taller area: bars above and below.
        assert_eq!(letterbox((1920, 1080), (1280, 1000)), (0, 140, 1280, 860));
        // Degenerate sizes never leave the area.
        let (l, t, r, b) = letterbox((0, 0), (10, 10));
        assert!(l >= 0 && t >= 0 && r <= 10 && b <= 10);
    }
}
