//! Decoded pictures shown from the GPU (`win::Presenter`): encoder → DXVA decoder → video
//! processor → swap chain of a window that is never shown, with what was drawn read back
//! and compared with the source picture. No window appears, nothing takes the focus.

#![cfg(windows)]
// Win32 window creation for the swap chain (the workspace denies unsafe by default).
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use turbo_pancake_media_video::win::{
    EncoderInput, H264Decoder, H264Encoder, Presenter, create_device, startup,
};
use turbo_pancake_media_video::{EncoderConfig, VideoError, synthetic_nv12};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
};
use windows::core::w;

/// A top-level window of `width` × `height` that is created but never shown.
fn hidden_window(width: i32, height: i32) -> HWND {
    // SAFETY: plain window creation with a system class; destroyed by the test.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("turbo-pancake present test"),
            WS_POPUP,
            0,
            0,
            width,
            height,
            None,
            None,
            None,
            None,
        )
        .expect("window")
    }
}

/// The grey level (0–255, full range) the presenter should draw for a studio-range luma.
fn full_range(y: u8) -> i32 {
    ((i32::from(y) - 16) * 255 / 219).clamp(0, 255)
}

struct Shown {
    count: u64,
    last: Option<(u32, u32, Vec<u8>)>,
}

/// Encodes `frames` synthetic pictures and shows every decoded one in a hidden window of
/// `window` size; returns what the presenter drew last, or `None` when this machine has
/// no hardware decoder / encoder (CI runners).
fn show(size: (u32, u32), window: (i32, i32), frames: u32) -> Option<Shown> {
    startup().expect("Media Foundation");
    let (device, _context, hardware) = create_device().expect("D3D11 device");
    if !hardware {
        println!("SKIP: no hardware D3D11 device");
        return None;
    }
    let mut config = EncoderConfig::new(size.0, size.1, 30, 4000);
    config.gop_frames = 0;
    let mut encoder = match H264Encoder::new(&device, &config) {
        Ok(e) => e,
        Err(VideoError::NoCodec(what)) => {
            println!("SKIP: no H.264 {what} on this system");
            return None;
        }
        Err(e) => panic!("encoder: {e}"),
    };
    let mut decoder = H264Decoder::new(Some(&device)).expect("decoder");
    if !decoder.is_hardware() {
        println!("SKIP: the decoder does not decode on the GPU here");
        return None;
    }
    let hwnd = hidden_window(window.0, window.1);
    let presenter = Presenter::new(decoder.device().expect("decoder device"), hwnd.0 as isize)
        .expect("presenter");
    let shown = Rc::new(RefCell::new(Shown {
        count: 0,
        last: None,
    }));
    let (sink_shown, presenter) = (shown.clone(), Rc::new(RefCell::new(presenter)));
    let sink_presenter = presenter.clone();
    decoder.set_surface_sink(Some(Box::new(
        move |surface, index, visible| match sink_presenter
            .borrow_mut()
            .present_and_read(surface, index, visible)
        {
            Ok(Some(picture)) => {
                let mut s = sink_shown.borrow_mut();
                s.count += 1;
                s.last = Some(picture);
                true
            }
            Ok(None) => false,
            Err(e) => panic!("present: {e}"),
        },
    )));

    let duration = 10_000_000 / 30;
    let mut decoded = Vec::new();
    for n in 0..frames {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !encoder.ready().expect("encoder events") {
            assert!(Instant::now() < deadline, "encoder never asked for input");
            std::thread::sleep(Duration::from_micros(500));
        }
        let bytes = synthetic_nv12(size.0, size.1, n);
        encoder
            .encode(
                EncoderInput::Nv12(&bytes),
                i64::from(n) * duration,
                duration,
            )
            .expect("encode");
        while let Some(frame) = encoder.poll_output().expect("encoder output") {
            decoder
                .decode(&frame.data, frame.timestamp)
                .expect("decode");
            decoded.extend(std::iter::from_fn(|| decoder.poll_output()));
        }
    }
    for frame in encoder.drain().expect("encoder drain") {
        decoder
            .decode(&frame.data, frame.timestamp)
            .expect("decode");
        decoded.extend(std::iter::from_fn(|| decoder.poll_output()));
    }
    decoded.extend(decoder.drain().expect("decoder drain"));
    assert_eq!(decoded.len(), frames as usize, "every frame decoded");
    // Shown from the GPU: never copied to the CPU by the decoder.
    assert!(decoded.iter().all(|f| f.nv12.is_empty()));
    assert_eq!(presenter.borrow().presented(), u64::from(frames));
    decoder.set_surface_sink(None);
    drop(decoder);
    drop(presenter);
    // SAFETY: the window created above; the swap chain is gone.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    Rc::try_unwrap(shown).ok().map(RefCell::into_inner)
}

/// One BGRA pixel as (r, g, b).
fn pixel(picture: &(u32, u32, Vec<u8>), x: u32, y: u32) -> (i32, i32, i32) {
    let i = ((y * picture.0 + x) * 4) as usize;
    let p = &picture.2[i..i + 4];
    (i32::from(p[2]), i32::from(p[1]), i32::from(p[0]))
}

fn assert_grey(picture: &(u32, u32, Vec<u8>), x: u32, y: u32, expected: i32) {
    let (r, g, b) = pixel(picture, x, y);
    for (name, v) in [("r", r), ("g", g), ("b", b)] {
        assert!(
            (v - expected).abs() <= 16,
            "pixel ({x},{y}) {name}={v}, expected about {expected} (rgb {r},{g},{b})"
        );
    }
}

#[test]
fn every_decoded_picture_is_drawn_with_the_right_colours() {
    let (w, h, frames) = (640u32, 360u32, 30u32);
    let Some(shown) = show((w, h), (w as i32, h as i32), frames) else {
        return;
    };
    assert_eq!(shown.count, u64::from(frames));
    let picture = shown.last.expect("a picture was drawn");
    assert_eq!((picture.0, picture.1), (w, h));
    // The last picture (n = frames − 1) of `synthetic_nv12`: a bright square on a
    // diagonal grey gradient.
    let n = frames - 1;
    let side = h / 4;
    let (x0, y0) = ((n * 8) % (w - side), (n * 4) % (h - side));
    assert_grey(&picture, x0 + side / 2, y0 + side / 2, full_range(235));
    let gradient = |x: u32, y: u32| full_range((16 + (x + y) * 160 / (w + h)) as u8);
    for (x, y) in [(20, 20), (600, 20), (40, 330), (600, 330)] {
        assert_grey(&picture, x, y, gradient(x, y));
    }
}

#[test]
fn the_picture_is_scaled_into_the_window_and_letterboxed() {
    // A 16:9 picture in a square window: bars above and below, picture in the middle.
    let Some(shown) = show((640, 360), (400, 400), 10) else {
        return;
    };
    let picture = shown.last.expect("a picture was drawn");
    assert_eq!((picture.0, picture.1), (400, 400));
    // 400 × 225 picture, centred: rows 87..312.
    for (x, y) in [(10, 10), (200, 40), (390, 395), (200, 360)] {
        assert_eq!(pixel(&picture, x, y), (0, 0, 0), "bar at ({x},{y})");
    }
    let (r, g, b) = pixel(&picture, 200, 200);
    assert!(
        r > 20 && (r - g).abs() <= 16 && (g - b).abs() <= 16,
        "{r},{g},{b}"
    );
}
