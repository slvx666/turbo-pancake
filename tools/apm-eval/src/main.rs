//! WebRTC APM evaluation (ADR 0006): CPU per 10 ms frame (48 kHz mono) for several configs, echo
//! reduction (single talk), near-end preservation (double talk), noise suppression.
use std::time::Instant;
use turbo_pancake_media_audio::testsignal::{Rng, rms, speech_like};
use webrtc_audio_processing::Processor;
use webrtc_audio_processing_config::{
    Config, EchoCanceller, GainController, GainController2, HighPassFilter, NoiseSuppression,
};

const RATE: usize = 48_000;
const F: usize = RATE / 100;

fn cfg(aec: bool, ns: bool, agc: bool) -> Config {
    Config {
        echo_canceller: aec.then(EchoCanceller::default),
        noise_suppression: ns.then(NoiseSuppression::default),
        high_pass_filter: (aec || ns).then(HighPassFilter::default),
        gain_controller: agc.then(|| GainController::GainController2(GainController2::default())),
        ..Default::default()
    }
}

/// Runs render/capture through APM; returns (output, us per capture frame incl. render).
fn run(
    config: Config,
    render: &[f32],
    capture: &[f32],
) -> (Vec<f32>, f64, Option<f64>, Option<u32>) {
    let ap = Processor::new(RATE as u32).unwrap();
    ap.set_config(config);
    let mut out = Vec::with_capacity(capture.len());
    let mut r = vec![0f32; F];
    let mut c = vec![0f32; F];
    let t = Instant::now();
    for (rf, cf) in render.chunks_exact(F).zip(capture.chunks_exact(F)) {
        r.copy_from_slice(rf);
        ap.process_render_frame([&mut r[..]]).unwrap();
        c.copy_from_slice(cf);
        ap.process_capture_frame([&mut c[..]]).unwrap();
        out.extend_from_slice(&c);
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / (capture.len() / F) as f64;
    let st = ap.get_stats();
    (out, us, st.echo_return_loss_enhancement, st.delay_ms)
}

fn db(a: f32, b: f32) -> f32 {
    20.0 * (a / b.max(1e-9)).log10()
}

fn main() {
    let secs = 20.0;
    let far = speech_like(RATE as u32, secs, 1);
    // A different talker: generated at 36 kHz and played at 48 kHz (pitch, formants and
    // syllable rate x4/3, pauses at other times), so it is not correlated with `far`.
    let mut near: Vec<f32> = speech_like(36_000, secs * 4.0 / 3.0, 2)
        .iter()
        .map(|v| v * 0.8)
        .collect();
    near.truncate(far.len());
    let n = far.len();
    let delay = RATE * 40 / 1000;
    let mut rng = Rng::new(9);
    let noise: Vec<f32> = (0..n).map(|_| (rng.unit() - 0.5) * 0.02).collect();
    // Echo path: 40 ms delay, -6 dB, plus a weak 2nd reflection.
    let echo: Vec<f32> = (0..n)
        .map(|i| {
            let a = if i >= delay {
                far[i - delay] * 0.5
            } else {
                0.0
            };
            let b = if i >= delay + 480 {
                far[i - delay - 480] * 0.15
            } else {
                0.0
            };
            a + b
        })
        .collect();
    let silent = vec![0f32; n];
    let corr: f64 = far
        .iter()
        .zip(&near)
        .map(|(a, b)| (*a as f64) * (*b as f64))
        .sum::<f64>()
        / (far.iter().map(|a| (*a as f64).powi(2)).sum::<f64>().sqrt()
            * near.iter().map(|a| (*a as f64).powi(2)).sum::<f64>().sqrt());
    println!("{{\"far_near_correlation\":{corr:.3}}}");
    let tail = n / 2; // measure after convergence

    // CPU
    let cap_st: Vec<f32> = (0..n).map(|i| echo[i] + noise[i] * 0.1).collect();
    for (name, aec, ns, agc) in [
        ("ns", false, true, false),
        ("aec3+ns+hpf", true, true, false),
        ("aec3+ns+hpf+agc2", true, true, true),
    ] {
        let mut v: Vec<f64> = (0..5)
            .map(|_| run(cfg(aec, ns, agc), &far, &cap_st).1)
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{{\"cpu\":\"{name}\",\"us_per_10ms_median\":{:.1},\"min\":{:.1},\"max\":{:.1},\"core_pct\":{:.2}}}",
            v[2],
            v[0],
            v[4],
            v[2] / 100.0
        );
    }
    // Single talk: only echo (+ tiny noise) at the mic.
    let (out, _, erle, d) = run(cfg(true, true, false), &far, &cap_st);
    println!(
        "{{\"single_talk\":true,\"echo_in_db\":{:.1},\"out_vs_in_db\":{:.1},\"erle_stat\":{:?},\"delay_ms\":{:?}}}",
        db(rms(&cap_st[tail..]), 1.0),
        db(rms(&out[tail..]), rms(&cap_st[tail..])),
        erle,
        d
    );
    // Double talk: echo + near speech. Near-end preservation = level of residual vs clean near.
    let cap_dt: Vec<f32> = (0..n).map(|i| echo[i] + near[i] + noise[i] * 0.1).collect();
    let (out_dt, _, _, _) = run(cfg(true, true, false), &far, &cap_dt);
    // Delay-compensated projection of the output on the clean near-end talker (search
    // 0..10 ms lag; APM's filters add a small delay) + plain level comparison.
    let tn = |i: usize| (i as f32 / RATE as f32) * 4.0 / 3.0;
    let (mut best_gain, mut best_lag, mut best_resid) = (0f64, 0usize, 0f32);
    for lag in 0..480 {
        let (mut xy, mut yy) = (0f64, 0f64);
        for i in tail..n - lag {
            xy += out_dt[i + lag] as f64 * near[i] as f64;
            yy += (near[i] as f64).powi(2);
        }
        let g = xy / yy;
        if g > best_gain {
            let resid: Vec<f32> = (tail..n - lag)
                .map(|i| out_dt[i + lag] - g as f32 * near[i])
                .collect();
            best_gain = g;
            best_lag = lag;
            best_resid = db(rms(&resid), rms(&near[tail..]));
        }
    }
    let act: Vec<usize> = (tail..n).filter(|&i| tn(i) % 2.0 < 1.6).collect();
    let lvl = |x: &[f32]| rms(&act.iter().map(|&i| x[i]).collect::<Vec<_>>());
    println!(
        "{{\"double_talk\":true,\"near_gain\":{best_gain:.3},\"lag_ms\":{:.2},\"residual_vs_near_db\":{best_resid:.1},\"out_vs_near_level_db\":{:.1},\"echo_vs_near_in_db\":{:.1}}}",
        best_lag as f32 / 48.0,
        db(lvl(&out_dt), lvl(&near)),
        db(rms(&echo[tail..]), rms(&near[tail..]))
    );
    // Near talking while the far end pauses (no echo) vs both talking (double talk).
    let tf = |i: usize| i as f32 / RATE as f32;
    let seg = |pred: &dyn Fn(usize) -> bool| -> f32 {
        let idx: Vec<usize> = (tail..n).filter(|&i| pred(i)).collect();
        db(
            rms(&idx.iter().map(|&i| out_dt[i]).collect::<Vec<_>>()),
            rms(&idx.iter().map(|&i| near[i]).collect::<Vec<_>>()),
        )
    };
    let near_only = seg(&|i| tn(i) % 2.0 < 1.6 && tf(i) % 2.0 > 1.75);
    let both = seg(&|i| tn(i) % 2.0 < 1.6 && tf(i) % 2.0 < 1.6);
    println!(
        "{{\"double_talk_levels\":true,\"out_vs_near_when_far_silent_db\":{near_only:.1},\"out_vs_near_when_both_talk_db\":{both:.1}}}"
    );
    // Noise suppression: near speech + noise (SNR ~ 10 dB), no echo; measure in pauses.
    let cap_ns: Vec<f32> = (0..n).map(|i| near[i] + noise[i] * 2.0).collect();
    let (out_ns, _, _, _) = run(cfg(false, true, false), &silent, &cap_ns);
    let tn = |i: usize| (i as f32 / RATE as f32) * 4.0 / 3.0;
    let pauses: Vec<usize> = (tail..n).filter(|&i| tn(i) % 2.0 > 1.75).collect();
    let pin: Vec<f32> = pauses.iter().map(|&i| cap_ns[i]).collect();
    let pout: Vec<f32> = pauses.iter().map(|&i| out_ns[i]).collect();
    let speech: Vec<usize> = (tail..n).filter(|&i| tn(i) % 2.0 < 1.6).collect();
    let sin: Vec<f32> = speech.iter().map(|&i| cap_ns[i]).collect();
    let sout: Vec<f32> = speech.iter().map(|&i| out_ns[i]).collect();
    println!(
        "{{\"ns\":true,\"noise_floor_reduction_db\":{:.1},\"speech_level_change_db\":{:.1}}}",
        db(rms(&pout), rms(&pin)),
        db(rms(&sout), rms(&sin))
    );
}
