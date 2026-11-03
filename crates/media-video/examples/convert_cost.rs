//! Cost of NV12 → RGBA for one 1080p picture (the viewer's CPU path).
fn main() {
    let (w, h) = (1920, 1080);
    let nv12 = turbo_pancake_media_video::synthetic_nv12(w, h, 7);
    let mut out = Vec::new();
    turbo_pancake_media_video::nv12_to_rgba(w, h, &nv12, &mut out);
    let n = 60;
    let t = std::time::Instant::now();
    for _ in 0..n {
        turbo_pancake_media_video::nv12_to_rgba(w, h, &nv12, &mut out);
    }
    println!(
        "nv12_to_rgba 1920x1080: {:.2} ms per picture",
        t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
    );
}
