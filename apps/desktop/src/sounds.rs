//! UI sounds (ADR 0021): short cues synthesized here at start of use — no sound files,
//! no audio library. Every sound is an original design made of a few enveloped tones
//! (soft attack, exponential decay, a short release to exact silence, so nothing clicks),
//! 24 kHz mono 16-bit, rendered as an in-memory WAV and cached until the volume changes.
//!
//! Playback goes through one tiny worker thread fed by a channel, so callers on the UI
//! thread, the call thread or a Tokio runtime never wait. On Windows the worker plays with
//! `PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT)` (looped with `SND_LOOP`), i.e. on
//! the default output device and inside our own process — so screen-share system audio,
//! which captures everything except our process tree, never picks the cues up.
//!
//! `PlaySoundW` plays one sound per process at a time. While the incoming-call ringtone
//! loops, other cues are dropped (a message must not cut the ringtone); the softer
//! outgoing ringback is interrupted by a cue and resumes when the cue has played.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sample rate of every cue.
pub const RATE: u32 = 24_000;
/// No sample of any cue at any volume exceeds this (-3 dBFS).
pub const PEAK: f32 = 0.707;
/// Loudest cue at 100 % volume (about -4.4 dBFS), leaving headroom below [`PEAK`].
const FULL_SCALE: f32 = 0.6;
/// Period of the incoming-call ringtone loop.
pub const RING_PERIOD: Duration = Duration::from_millis(2000);
/// Period of the outgoing ringback loop.
pub const RINGBACK_PERIOD: Duration = Duration::from_millis(3000);
/// A ringtone nobody stopped (a lost update) ends on its own after this.
pub const LOOP_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sound {
    /// New message: a soft two-note pluck.
    Message,
    /// Incoming call (looped): two friendly two-tone bursts, then a pause.
    IncomingRing,
    /// Our call is waiting for the others (looped, softer).
    OutgoingRing,
    /// We joined a call: rising.
    CallJoin,
    /// We left a call: falling.
    CallLeave,
    /// Someone joined our call: a softer rise.
    PeerJoin,
    /// Someone left our call: a softer fall.
    PeerLeave,
    Mute,
    Unmute,
    ShareStart,
    ShareStop,
    /// The connection to the server was lost.
    Disconnected,
    /// It came back.
    Reconnected,
}

impl Sound {
    pub const ALL: [Sound; 13] = [
        Sound::Message,
        Sound::IncomingRing,
        Sound::OutgoingRing,
        Sound::CallJoin,
        Sound::CallLeave,
        Sound::PeerJoin,
        Sound::PeerLeave,
        Sound::Mute,
        Sound::Unmute,
        Sound::ShareStart,
        Sound::ShareStop,
        Sound::Disconnected,
        Sound::Reconnected,
    ];

    /// Name for `TURBO_PANCAKE_ALERT` lines.
    pub fn name(self) -> &'static str {
        match self {
            Sound::Message => "message",
            Sound::IncomingRing => "incoming_ring",
            Sound::OutgoingRing => "outgoing_ring",
            Sound::CallJoin => "call_join",
            Sound::CallLeave => "call_leave",
            Sound::PeerJoin => "peer_join",
            Sound::PeerLeave => "peer_leave",
            Sound::Mute => "mute",
            Sound::Unmute => "unmute",
            Sound::ShareStart => "share_start",
            Sound::ShareStop => "share_stop",
            Sound::Disconnected => "disconnected",
            Sound::Reconnected => "reconnected",
        }
    }

    pub fn looped(self) -> bool {
        matches!(self, Sound::IncomingRing | Sound::OutgoingRing)
    }

    /// Loudness of this cue relative to the loudest (ringtone, message).
    fn level(self) -> f32 {
        match self {
            Sound::Message | Sound::IncomingRing => 1.0,
            Sound::CallJoin | Sound::CallLeave => 0.85,
            Sound::Disconnected | Sound::Reconnected => 0.8,
            Sound::PeerJoin | Sound::PeerLeave => 0.7,
            Sound::ShareStart | Sound::ShareStop => 0.7,
            Sound::OutgoingRing => 0.55,
            Sound::Mute | Sound::Unmute => 0.55,
        }
    }
}

/// One enveloped tone; `glide` sweeps the pitch linearly to that frequency.
#[derive(Clone, Copy)]
struct Note {
    at: f32,
    freq: f32,
    dur: f32,
    gain: f32,
    glide: Option<f32>,
}

const fn n(at: f32, freq: f32, dur: f32, gain: f32) -> Note {
    Note {
        at,
        freq,
        dur,
        gain,
        glide: None,
    }
}

const fn sweep(at: f32, from: f32, to: f32, dur: f32, gain: f32) -> Note {
    Note {
        at,
        freq: from,
        dur,
        gain,
        glide: Some(to),
    }
}

// Equal-tempered pitches used below (Hz).
const C5: f32 = 523.25;
const E5: f32 = 659.26;
const G5: f32 = 783.99;
const A5: f32 = 880.00;
const B5: f32 = 987.77;
const C6: f32 = 1046.50;
const E6: f32 = 1318.51;
const A4: f32 = 440.00;
const E4: f32 = 329.63;

/// The score of a cue and its total length (seconds; a looped cue's period).
fn score(sound: Sound) -> (Vec<Note>, f32) {
    match sound {
        Sound::Message => (vec![n(0.0, A5, 0.22, 0.8), n(0.075, E6, 0.30, 0.7)], 0.42),
        Sound::IncomingRing => {
            let burst = |t: f32| {
                [
                    n(t, G5, 0.16, 0.9),
                    n(t + 0.12, B5, 0.16, 0.9),
                    n(t + 0.24, G5, 0.16, 0.9),
                    n(t + 0.36, B5, 0.22, 0.9),
                ]
            };
            let mut v = burst(0.0).to_vec();
            v.extend(burst(0.70));
            (v, RING_PERIOD.as_secs_f32())
        }
        Sound::OutgoingRing => (
            vec![n(0.0, C5, 0.45, 0.8), n(0.28, E5, 0.60, 0.7)],
            RINGBACK_PERIOD.as_secs_f32(),
        ),
        Sound::CallJoin => (
            vec![
                n(0.0, C5, 0.20, 0.8),
                n(0.08, E5, 0.20, 0.8),
                n(0.16, G5, 0.32, 0.85),
            ],
            0.55,
        ),
        Sound::CallLeave => (
            vec![
                n(0.0, G5, 0.20, 0.85),
                n(0.08, E5, 0.20, 0.8),
                n(0.16, C5, 0.34, 0.8),
            ],
            0.56,
        ),
        Sound::PeerJoin => (vec![n(0.0, E5, 0.18, 0.8), n(0.09, A5, 0.28, 0.8)], 0.42),
        Sound::PeerLeave => (vec![n(0.0, A5, 0.18, 0.8), n(0.09, E5, 0.28, 0.8)], 0.42),
        Sound::Mute => (vec![n(0.0, B5, 0.05, 0.9), n(0.055, G5, 0.07, 0.9)], 0.15),
        Sound::Unmute => (vec![n(0.0, G5, 0.05, 0.9), n(0.055, B5, 0.07, 0.9)], 0.15),
        Sound::ShareStart => (
            vec![sweep(0.0, E5, C6, 0.16, 0.7), n(0.15, C6, 0.25, 0.8)],
            0.45,
        ),
        Sound::ShareStop => (
            vec![sweep(0.0, C6, E5, 0.16, 0.7), n(0.15, E5, 0.25, 0.8)],
            0.45,
        ),
        Sound::Disconnected => (vec![n(0.0, A4, 0.30, 0.9), n(0.16, E4, 0.45, 0.9)], 0.66),
        Sound::Reconnected => (vec![n(0.0, E5, 0.18, 0.8), n(0.10, A5, 0.32, 0.85)], 0.48),
    }
}

/// Raised-cosine ramp 0 → 1 over `0..len`.
fn ramp(t: f32, len: f32) -> f32 {
    if t >= len {
        1.0
    } else if t <= 0.0 {
        0.0
    } else {
        0.5 - 0.5 * (std::f32::consts::PI * t / len).cos()
    }
}

/// Mixes the score into float samples (unnormalized). Every note starts and ends at
/// exactly zero, so the whole buffer does too.
fn render(notes: &[Note], total: f32) -> Vec<f32> {
    const ATTACK: f32 = 0.006;
    const RELEASE: f32 = 0.020;
    let len = (total * RATE as f32).round() as usize;
    let mut out = vec![0.0f32; len];
    let dt = 1.0 / RATE as f32;
    for note in notes {
        let start = (note.at * RATE as f32).round() as usize;
        let count = (note.dur * RATE as f32).round() as usize;
        // Decay to about -30 dB over the note: soft, bell-like.
        let tau = note.dur / 3.5;
        let mut phase = 0.0f32;
        for i in 0..count {
            let Some(slot) = out.get_mut(start + i) else {
                break;
            };
            let t = i as f32 * dt;
            let freq = match note.glide {
                Some(to) => note.freq + (to - note.freq) * (t / note.dur),
                None => note.freq,
            };
            phase += std::f32::consts::TAU * freq * dt;
            if phase > std::f32::consts::TAU * 64.0 {
                phase -= std::f32::consts::TAU * 64.0;
            }
            let env = ramp(t, ATTACK) * (-t / tau).exp() * ramp(note.dur - t - dt, RELEASE);
            // Fundamental with a little second and third harmonic: round, not a pure beep.
            let wave = phase.sin() + 0.22 * (2.0 * phase).sin() + 0.07 * (3.0 * phase).sin();
            *slot += note.gain * env * wave;
        }
    }
    out
}

/// Amplitude factor of the 0–100 volume setting (squared: even steps sound even).
pub fn volume_gain(volume: u8) -> f32 {
    let v = f32::from(volume.min(100)) / 100.0;
    v * v
}

/// Mono 16-bit PCM samples of a cue at this volume; `None` at volume 0.
pub fn samples(sound: Sound, volume: u8) -> Option<Vec<i16>> {
    let gain = volume_gain(volume);
    if gain <= 0.0 {
        return None;
    }
    let (notes, total) = score(sound);
    let raw = render(&notes, total);
    let max = raw.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if max <= 0.0 {
        return None;
    }
    let target = (FULL_SCALE * sound.level() * gain).min(PEAK);
    let scale = target / max;
    Some(
        raw.iter()
            .map(|s| (s * scale * 32767.0).round().clamp(-32767.0, 32767.0) as i16)
            .collect(),
    )
}

/// A RIFF/WAVE file (PCM, mono, 16-bit, [`RATE`]) holding these samples.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// How long a WAV made by [`wav`] plays.
pub fn wav_duration(wav: &[u8]) -> Duration {
    let samples = wav.len().saturating_sub(44) / 2;
    Duration::from_secs_f64(samples as f64 / f64::from(RATE))
}

/// At most one event per `min` interval (message sounds while a chat floods in).
#[derive(Debug, Clone)]
pub struct RateLimiter {
    min: Duration,
    last: Option<Instant>,
}

impl RateLimiter {
    pub fn new(min: Duration) -> Self {
        Self { min, last: None }
    }

    /// True (and remembered) when the previous allowed event is at least `min` ago.
    pub fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|l| now.saturating_duration_since(l) < self.min)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// Where the worker sends finished WAVs: the OS player, or nothing (silent runs, tests).
pub trait AudioOut: Send {
    /// Plays this WAV at once, replacing whatever plays. The worker keeps the buffer
    /// alive until the next `play` or `stop` returns.
    fn play(&mut self, wav: &[u8], looped: bool);
    fn stop(&mut self);
}

/// Plays nothing (silent mode).
pub struct NoAudio;

impl AudioOut for NoAudio {
    fn play(&mut self, _wav: &[u8], _looped: bool) {}
    fn stop(&mut self) {}
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod os {
    use windows::Win32::Media::Audio::{
        PlaySoundW, SND_ASYNC, SND_LOOP, SND_MEMORY, SND_NODEFAULT,
    };
    use windows::core::PCWSTR;

    /// `PlaySoundW` from memory: the default output device, our own process.
    pub struct PlaySound;

    impl super::AudioOut for PlaySound {
        fn play(&mut self, wav: &[u8], looped: bool) {
            let mut flags = SND_MEMORY | SND_ASYNC | SND_NODEFAULT;
            if looped {
                flags |= SND_LOOP;
            }
            // SAFETY: with SND_MEMORY the "name" points at a whole WAV image; the worker
            // keeps that buffer alive until the next PlaySoundW call has replaced it.
            let _ = unsafe { PlaySoundW(PCWSTR(wav.as_ptr().cast()), None, flags) };
        }

        fn stop(&mut self) {
            // A null sound stops what plays (and releases our buffer).
            let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_NODEFAULT) };
        }
    }
}

/// The real player of this system (`PlaySoundW` on Windows; nothing elsewhere until S9).
pub fn os_audio() -> Box<dyn AudioOut> {
    #[cfg(windows)]
    {
        Box::new(os::PlaySound)
    }
    #[cfg(not(windows))]
    {
        Box::new(NoAudio)
    }
}

enum Cmd {
    Play(Sound),
    Loop(Option<Sound>),
    Volume(u8),
}

/// The cues at one volume, rendered on first use.
struct Cache {
    volume: u8,
    wavs: HashMap<Sound, Option<Arc<[u8]>>>,
}

impl Cache {
    fn get(&mut self, s: Sound) -> Option<Arc<[u8]>> {
        let volume = self.volume;
        self.wavs
            .entry(s)
            .or_insert_with(|| samples(s, volume).map(|v| Arc::from(wav(&v))))
            .clone()
    }
}

struct Worker {
    out: Box<dyn AudioOut>,
    cache: Cache,
    /// Kept alive while the OS plays it.
    current: Option<Arc<[u8]>>,
    looping: Option<Sound>,
    loop_until: Option<Instant>,
    /// A cue interrupted the (outgoing) loop: restart it at this instant.
    resume_at: Option<Instant>,
}

impl Worker {
    fn start_loop(&mut self, s: Sound) {
        self.resume_at = None;
        match self.cache.get(s) {
            Some(w) => {
                self.out.play(&w, true);
                self.current = Some(w);
            }
            None => {
                self.out.stop();
                self.current = None;
            }
        }
    }

    fn handle(&mut self, cmd: Cmd, now: Instant) {
        match cmd {
            Cmd::Play(s) => {
                // The ringtone is never cut by another cue.
                if self.looping == Some(Sound::IncomingRing) {
                    return;
                }
                let Some(w) = self.cache.get(s) else { return };
                self.out.play(&w, false);
                if self.looping.is_some() {
                    self.resume_at = Some(now + wav_duration(&w));
                }
                self.current = Some(w);
            }
            Cmd::Loop(Some(s)) => {
                if self.looping == Some(s) {
                    return;
                }
                self.looping = Some(s);
                self.loop_until = Some(now + LOOP_LIMIT);
                self.start_loop(s);
            }
            Cmd::Loop(None) => {
                if self.looping.take().is_some() {
                    self.loop_until = None;
                    self.resume_at = None;
                    self.out.stop();
                    self.current = None;
                }
            }
            Cmd::Volume(v) => {
                if v != self.cache.volume {
                    self.cache = Cache {
                        volume: v,
                        wavs: HashMap::new(),
                    };
                    if let Some(s) = self.looping {
                        self.start_loop(s);
                    }
                }
            }
        }
    }

    /// Time-driven work: resume an interrupted loop, end a forgotten one.
    fn tick(&mut self, now: Instant) {
        if self.loop_until.is_some_and(|t| now >= t) {
            self.handle(Cmd::Loop(None), now);
        }
        if self.resume_at.is_some_and(|t| now >= t)
            && let Some(s) = self.looping
        {
            self.start_loop(s);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match (self.resume_at, self.loop_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            let cmd = match self.deadline() {
                Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                },
            };
            let now = Instant::now();
            if let Some(c) = cmd {
                self.handle(c, now);
            }
            self.tick(now);
        }
        if self.looping.is_some() || self.current.is_some() {
            self.out.stop();
        }
    }
}

/// Plays cues on its own thread (started on first use); every method returns at once
/// and may be called from any thread.
pub struct Player {
    out: Mutex<Option<Box<dyn AudioOut>>>,
    volume: Mutex<u8>,
    tx: OnceLock<mpsc::Sender<Cmd>>,
}

impl Player {
    pub fn new(out: Box<dyn AudioOut>, volume: u8) -> Self {
        Self {
            out: Mutex::new(Some(out)),
            volume: Mutex::new(volume.min(100)),
            tx: OnceLock::new(),
        }
    }

    fn send(&self, cmd: Cmd) {
        let tx = self.tx.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let out = self
                .out
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| Box::new(NoAudio));
            let volume = *self.volume.lock().unwrap_or_else(|e| e.into_inner());
            let worker = Worker {
                out,
                cache: Cache {
                    volume,
                    wavs: HashMap::new(),
                },
                current: None,
                looping: None,
                loop_until: None,
                resume_at: None,
            };
            let spawned = std::thread::Builder::new()
                .name("tp-sounds".into())
                .stack_size(256 * 1024)
                .spawn(move || worker.run(rx));
            if let Err(e) = spawned {
                eprintln!("sound thread not started: {e}");
            }
            tx
        });
        let _ = tx.send(cmd);
    }

    pub fn play(&self, s: Sound) {
        if s.looped() {
            self.send(Cmd::Loop(Some(s)));
        } else {
            self.send(Cmd::Play(s));
        }
    }

    /// Starts this looped cue (replacing another loop) or, with `None`, stops the loop.
    pub fn set_loop(&self, s: Option<Sound>) {
        self.send(Cmd::Loop(s));
    }

    pub fn set_volume(&self, volume: u8) {
        let volume = volume.min(100);
        *self.volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
        if self.tx.get().is_some() {
            self.send(Cmd::Volume(volume));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn every_cue_is_a_valid_pcm_wave() {
        for s in Sound::ALL {
            let pcm = samples(s, 100).expect("audible at full volume");
            let w = wav(&pcm);
            assert_eq!(&w[0..4], b"RIFF", "{s:?}");
            assert_eq!(u32_at(&w, 4) as usize, w.len() - 8);
            assert_eq!(&w[8..16], b"WAVEfmt ");
            assert_eq!(u32_at(&w, 16), 16);
            assert_eq!(u16::from_le_bytes([w[20], w[21]]), 1, "PCM");
            assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1, "mono");
            assert_eq!(u32_at(&w, 24), RATE);
            assert_eq!(u32_at(&w, 28), RATE * 2);
            assert_eq!(u16::from_le_bytes([w[34], w[35]]), 16);
            assert_eq!(&w[36..40], b"data");
            assert_eq!(u32_at(&w, 40) as usize, pcm.len() * 2);
            assert_eq!(w.len(), 44 + pcm.len() * 2);
            // Short cues stay short; RAM stays small.
            assert!(wav_duration(&w) <= RINGBACK_PERIOD, "{s:?}");
        }
    }

    #[test]
    fn no_sample_exceeds_the_peak_and_edges_are_silent() {
        let limit = (PEAK * 32767.0) as i32;
        for s in Sound::ALL {
            for volume in [1, 35, 70, 100] {
                let pcm = samples(s, volume).unwrap();
                let max = pcm.iter().map(|v| i32::from(*v).abs()).max().unwrap();
                assert!(max <= limit, "{s:?} at {volume}: {max}");
                // No click at either end (also the seam of a loop).
                assert!(i32::from(pcm[0]).abs() <= 2, "{s:?} starts at {}", pcm[0]);
                let last = *pcm.last().unwrap();
                assert!(i32::from(last).abs() <= 2, "{s:?} ends at {last}");
            }
            assert!(
                samples(s, 100).unwrap().iter().any(|v| v.abs() > 8000),
                "{s:?} audible"
            );
        }
    }

    #[test]
    fn loops_have_their_exact_period() {
        let ring = samples(Sound::IncomingRing, 70).unwrap();
        assert_eq!(ring.len() as u32, RATE * 2);
        assert_eq!(
            wav_duration(&wav(&ring)),
            RING_PERIOD,
            "ringtone period 2 s"
        );
        let back = samples(Sound::OutgoingRing, 70).unwrap();
        assert_eq!(back.len() as u32, RATE * 3);
        // The ringtone pauses: the last half second is silence.
        let tail = &ring[ring.len() - RATE as usize / 2..];
        assert!(tail.iter().all(|v| *v == 0));
    }

    #[test]
    fn volume_scales_and_zero_is_silent() {
        assert!(samples(Sound::Message, 0).is_none());
        let peak = |v| {
            samples(Sound::Message, v)
                .unwrap()
                .iter()
                .map(|s| i32::from(*s).abs())
                .max()
                .unwrap()
        };
        assert!(peak(30) < peak(70) && peak(70) < peak(100));
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(250), 1.0, "clamped");
    }

    #[test]
    fn rate_limiter_allows_one_per_interval() {
        let mut r = RateLimiter::new(Duration::from_millis(700));
        let t0 = Instant::now();
        assert!(r.allow(t0));
        assert!(!r.allow(t0 + Duration::from_millis(100)));
        assert!(!r.allow(t0 + Duration::from_millis(699)));
        assert!(r.allow(t0 + Duration::from_millis(700)));
        assert!(!r.allow(t0 + Duration::from_millis(1000)));
    }

    /// Records what the worker would play (no OS call).
    #[derive(Clone, Default)]
    struct Rec(Arc<Mutex<Vec<String>>>);

    impl AudioOut for Rec {
        fn play(&mut self, wav: &[u8], looped: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("play {} {looped}", wav.len()));
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().push("stop".into());
        }
    }

    fn worker(rec: &Rec, volume: u8) -> Worker {
        Worker {
            out: Box::new(rec.clone()),
            cache: Cache {
                volume,
                wavs: HashMap::new(),
            },
            current: None,
            looping: None,
            loop_until: None,
            resume_at: None,
        }
    }

    #[test]
    fn ringtone_is_not_cut_and_ringback_resumes() {
        let rec = Rec::default();
        let mut w = worker(&rec, 70);
        let t0 = Instant::now();
        let ring_len = wav(&samples(Sound::IncomingRing, 70).unwrap()).len();
        let msg = wav(&samples(Sound::Message, 70).unwrap());
        w.handle(Cmd::Loop(Some(Sound::IncomingRing)), t0);
        w.handle(Cmd::Loop(Some(Sound::IncomingRing)), t0);
        w.handle(Cmd::Play(Sound::Message), t0);
        assert_eq!(*rec.0.lock().unwrap(), [format!("play {ring_len} true")]);
        w.handle(Cmd::Loop(None), t0);
        w.handle(Cmd::Play(Sound::Message), t0);
        assert_eq!(rec.0.lock().unwrap().len(), 3);

        rec.0.lock().unwrap().clear();
        let back_len = wav(&samples(Sound::OutgoingRing, 70).unwrap()).len();
        w.handle(Cmd::Loop(Some(Sound::OutgoingRing)), t0);
        w.handle(Cmd::Play(Sound::Message), t0);
        let resume = w.deadline().unwrap();
        assert_eq!(resume, t0 + wav_duration(&msg));
        w.tick(resume);
        assert_eq!(
            *rec.0.lock().unwrap(),
            [
                format!("play {back_len} true"),
                format!("play {} false", msg.len()),
                format!("play {back_len} true"),
            ]
        );
        // A forgotten loop ends on its own.
        w.tick(t0 + LOOP_LIMIT);
        assert_eq!(rec.0.lock().unwrap().last().unwrap(), "stop");
        assert!(w.looping.is_none() && w.deadline().is_none());
    }

    #[test]
    fn volume_change_rerenders_and_zero_plays_nothing() {
        let rec = Rec::default();
        let mut w = worker(&rec, 0);
        let t0 = Instant::now();
        w.handle(Cmd::Play(Sound::Message), t0);
        assert!(rec.0.lock().unwrap().is_empty(), "volume 0: nothing");
        w.handle(Cmd::Volume(50), t0);
        w.handle(Cmd::Play(Sound::Message), t0);
        assert_eq!(rec.0.lock().unwrap().len(), 1);
        let before = w.cache.get(Sound::Message).unwrap();
        w.handle(Cmd::Volume(90), t0);
        assert_ne!(w.cache.get(Sound::Message).unwrap(), before);
    }

    #[test]
    fn player_never_blocks_the_caller() {
        let rec = Rec::default();
        let p = Player::new(Box::new(rec.clone()), 70);
        let t0 = Instant::now();
        for _ in 0..100 {
            p.play(Sound::Mute);
        }
        p.set_volume(40);
        p.set_loop(Some(Sound::IncomingRing));
        p.set_loop(None);
        assert!(t0.elapsed() < Duration::from_secs(1));
        drop(p);
    }
}
