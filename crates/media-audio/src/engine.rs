//! Audio engine: one worker thread that owns the microphone and speaker streams and
//! runs the voice pipeline between them and the network.
//!
//! ```text
//! mic ring ─▶ VoiceSender ─▶ outgoing channel ─▶ transport
//! transport ─▶ PacketSink ─▶ per-peer VoiceReceiver (48 kHz) ─▶ mix ─▶ resample ─▶ speaker ring
//! ```
//!
//! Decoding is paced by the output device: the worker renders a 20 ms tick whenever the
//! speaker ring holds less than the prefill target, so the device clock drives playout
//! and the jitter buffer absorbs drift against the remote sender. Without a working
//! output device the worker ticks on the wall clock instead, so receivers keep draining.
//!
//! Every channel between the engine and the rest of the client is bounded and
//! non-blocking on the engine side (spec §3): a full outgoing channel drops the frame and
//! counts it. UI-facing calls go through a bounded command channel and never wait for
//! the worker; [`AudioEngine::status`] reads a snapshot the worker publishes every 50 ms.
//!
//! A stream that reports a fatal error (unplugged device) is dropped and reopened: first
//! the selected device, then the system default, then retried every second. Device I/O is
//! abstracted by [`AudioIo`] so the engine runs against [`CpalIo`] in the client and
//! [`crate::sim::SimIo`] in tests and headless runs.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};

use crate::device::{StreamHealth, open_capture, open_playback};
use crate::dsp::{DSP_AVAILABLE, DspConfig, DspStats, VoiceDsp};
use crate::pipeline::{ReceiverStats, VoiceReceiver, VoiceSender};
use crate::resample::MonoResampler;
use crate::{AudioError, CODEC_RATE, EncoderConfig, FRAME_MS, FRAME_SAMPLES, JitterConfig};

/// Remote participant identifier, assigned by the call layer.
pub type PeerId = u64;

/// Most remote voices mixed at once (spec §7: groups of up to 100 members).
pub const MAX_PEERS: usize = 100;

const RING_MS: u32 = 200;
const LOOP_SLEEP: Duration = Duration::from_millis(5);
const STATUS_EVERY: Duration = Duration::from_millis(50);
const REOPEN_EVERY: Duration = Duration::from_secs(1);

/// An open input stream: mono f32 samples at `sample_rate` arrive in `samples`.
pub struct InputStream {
    pub samples: Consumer<f32>,
    pub sample_rate: u32,
    pub device_name: String,
    pub health: Arc<StreamHealth>,
    _guard: Box<dyn Any>,
}

impl InputStream {
    /// `guard` keeps the device open; dropping the stream drops it.
    pub fn new(
        samples: Consumer<f32>,
        sample_rate: u32,
        device_name: String,
        health: Arc<StreamHealth>,
        guard: impl Any,
    ) -> Self {
        Self {
            samples,
            sample_rate,
            device_name,
            health,
            _guard: Box::new(guard),
        }
    }
}

/// An open output stream: mono f32 samples pushed into `samples` are played.
pub struct OutputStream {
    pub samples: Producer<f32>,
    pub sample_rate: u32,
    pub device_name: String,
    pub health: Arc<StreamHealth>,
    _guard: Box<dyn Any>,
}

impl OutputStream {
    pub fn new(
        samples: Producer<f32>,
        sample_rate: u32,
        device_name: String,
        health: Arc<StreamHealth>,
        guard: impl Any,
    ) -> Self {
        Self {
            samples,
            sample_rate,
            device_name,
            health,
            _guard: Box::new(guard),
        }
    }
}

/// Opens device streams. Called only on the engine thread, so the returned streams
/// (cpal streams are not `Send` on every platform) never cross threads.
pub trait AudioIo: Send + 'static {
    /// `device` is a [`crate::device::DeviceInfo::id`]; `None` = system default.
    fn open_input(&mut self, device: Option<&str>, ring_ms: u32)
    -> Result<InputStream, AudioError>;
    fn open_output(
        &mut self,
        device: Option<&str>,
        ring_ms: u32,
    ) -> Result<OutputStream, AudioError>;
}

/// Real devices through cpal.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpalIo;

impl AudioIo for CpalIo {
    fn open_input(
        &mut self,
        device: Option<&str>,
        ring_ms: u32,
    ) -> Result<InputStream, AudioError> {
        let (capture, ring) = open_capture(device, ring_ms)?;
        Ok(InputStream::new(
            ring,
            capture.sample_rate,
            capture.device_name.clone(),
            capture.health.clone(),
            capture,
        ))
    }

    fn open_output(
        &mut self,
        device: Option<&str>,
        ring_ms: u32,
    ) -> Result<OutputStream, AudioError> {
        let (playback, ring) = open_playback(device, ring_ms)?;
        Ok(OutputStream::new(
            ring,
            playback.sample_rate,
            playback.device_name.clone(),
            playback.health.clone(),
            playback,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Selected input device id (`None` = system default).
    pub input: Option<String>,
    pub output: Option<String>,
    pub encoder: EncoderConfig,
    pub jitter: JitterConfig,
    /// Audio kept queued for the speaker; also the silence written when it opens.
    pub prefill_ms: u32,
    /// Capacity of the outgoing and incoming packet channels (frames).
    pub channel_frames: usize,
    pub muted: bool,
    /// Echo cancellation / noise suppression on the microphone path.
    pub dsp: DspConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            encoder: EncoderConfig::default(),
            jitter: JitterConfig::default(),
            prefill_ms: 40,
            // 1 s of audio: a transport stall longer than that drops frames.
            channel_frames: 50,
            muted: false,
            dsp: DspConfig::default(),
        }
    }
}

/// One encoded 20 ms microphone frame for the transport.
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingFrame {
    pub seq: u16,
    /// RTP timestamp, 48 kHz units.
    pub timestamp: u32,
    pub payload: Vec<u8>,
    pub peak: f32,
}

/// One received Opus packet from a remote peer.
#[derive(Debug, Clone)]
pub struct IncomingPacket {
    pub peer: PeerId,
    pub seq: u16,
    pub payload: Vec<u8>,
    /// When the packet arrived from the network (jitter estimate input).
    pub arrival: Instant,
}

/// Transport-side handle for feeding received packets into the engine. Never blocks:
/// a full queue drops the packet (the jitter buffer conceals it) and counts the drop.
#[derive(Clone)]
pub struct PacketSink {
    tx: SyncSender<IncomingPacket>,
    dropped: Arc<AtomicU64>,
}

impl PacketSink {
    /// Returns `false` when the packet was dropped (queue full or engine stopped).
    pub fn push(&self, packet: IncomingPacket) -> bool {
        match self.tx.try_send(packet) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

/// Network side of a running engine.
pub struct VoiceLink {
    /// Encoded microphone frames, in order. Bounded; the engine drops when it is full.
    pub outgoing: Receiver<OutgoingFrame>,
    pub incoming: PacketSink,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveDevice {
    pub name: String,
    pub sample_rate: u32,
    /// `true` when the selected device could not be opened and the default is used.
    pub fallback: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerStatus {
    pub peer: PeerId,
    pub volume: f32,
    pub stats: ReceiverStats,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineStatus {
    pub running: bool,
    pub muted: bool,
    pub input: Option<ActiveDevice>,
    pub output: Option<ActiveDevice>,
    /// Peak of the last encoded microphone frame (0 while muted).
    pub mic_peak: f32,
    pub frames_sent: u64,
    /// Encoded frames dropped because the outgoing channel was full.
    pub frames_dropped: u64,
    /// Received packets dropped (incoming queue full or peer limit reached).
    pub packets_dropped: u64,
    /// Samples lost in device rings, summed over every stream opened so far.
    pub input_xruns: u64,
    pub output_xruns: u64,
    /// Glitches reported by the audio backend (both directions, all streams so far).
    pub backend_xruns: u64,
    /// Streams reopened after a failure or a device switch.
    pub reopens: u64,
    /// 20 ms output ticks rendered.
    pub ticks: u64,
    pub last_error: Option<String>,
    pub peers: Vec<PeerStatus>,
    /// Requested processing; it only runs when `dsp_available` (build with `apm`).
    pub dsp: DspConfig,
    pub dsp_available: bool,
    pub dsp_stats: DspStats,
}

enum Command {
    SetMuted(bool),
    SetInput(Option<String>),
    SetOutput(Option<String>),
    SetPeerVolume(PeerId, f32),
    RemovePeer(PeerId),
    SetEncoder(EncoderConfig),
    SetDsp(DspConfig),
    Stop,
}

/// Handle to the engine thread. Dropping it stops the thread and releases the devices.
pub struct AudioEngine {
    commands: SyncSender<Command>,
    status: Arc<Mutex<EngineStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Starts the worker and returns once it has tried to open both devices (the result
    /// is in [`AudioEngine::status`]; a missing device is not an error, it is retried).
    pub fn start(
        io: impl AudioIo,
        config: EngineConfig,
    ) -> Result<(AudioEngine, VoiceLink), AudioError> {
        let (cmd_tx, cmd_rx) = sync_channel(64);
        let (out_tx, out_rx) = sync_channel(config.channel_frames.max(1));
        let (in_tx, in_rx) = sync_channel(config.channel_frames.max(1) * 4);
        let dropped = Arc::new(AtomicU64::new(0));
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let sender = VoiceSender::new(CODEC_RATE, config.encoder)?;
        let out_resampler = MonoResampler::new(CODEC_RATE, CODEC_RATE)?;
        let dsp = VoiceDsp::new(config.dsp)?;
        let worker_status = status.clone();
        let worker_dropped = dropped.clone();
        let (ready_tx, ready_rx) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("tp-audio".into())
            .spawn(move || {
                let worker = Worker {
                    io: Box::new(io),
                    sender,
                    dsp,
                    receivers: HashMap::new(),
                    volumes: HashMap::new(),
                    input: None,
                    output: None,
                    out_resampler,
                    commands: cmd_rx,
                    outgoing: out_tx,
                    incoming: in_rx,
                    incoming_dropped: worker_dropped,
                    status: worker_status,
                    st: EngineStatus {
                        running: true,
                        muted: config.muted,
                        dsp: config.dsp,
                        dsp_available: DSP_AVAILABLE,
                        ..EngineStatus::default()
                    },
                    input_retry: None,
                    output_retry: None,
                    past_input_xruns: 0,
                    past_output_xruns: 0,
                    past_backend_xruns: 0,
                    wall_tick: None,
                    epoch: Instant::now(),
                    mix: vec![0.0; FRAME_SAMPLES],
                    peer_pcm: Vec::with_capacity(FRAME_SAMPLES),
                    device_pcm: Vec::with_capacity(FRAME_SAMPLES * 4),
                    mic_buf: Vec::with_capacity(4096),
                    config,
                };
                worker.run(ready_tx)
            })
            .map_err(|e| AudioError::Engine(format!("spawn: {e}")))?;
        // The worker signals after its first open attempt; a closed channel means it
        // panicked, which the status (running = false) and `stop` will show.
        let _ = ready_rx.recv();
        Ok((
            AudioEngine {
                commands: cmd_tx,
                status,
                thread: Some(thread),
            },
            VoiceLink {
                outgoing: out_rx,
                incoming: PacketSink { tx: in_tx, dropped },
            },
        ))
    }

    fn send(&self, cmd: Command) -> Result<(), AudioError> {
        self.commands.try_send(cmd).map_err(|e| match e {
            TrySendError::Full(_) => AudioError::Engine("command queue full".into()),
            TrySendError::Disconnected(_) => AudioError::Engine("engine stopped".into()),
        })
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), AudioError> {
        self.send(Command::SetMuted(muted))
    }

    /// Switches the microphone (`None` = system default) without interrupting the
    /// outgoing stream's sequence numbers.
    pub fn set_input_device(&self, id: Option<String>) -> Result<(), AudioError> {
        self.send(Command::SetInput(id))
    }

    pub fn set_output_device(&self, id: Option<String>) -> Result<(), AudioError> {
        self.send(Command::SetOutput(id))
    }

    /// Local playback gain for one peer, 0.0–4.0 (does not change what they send).
    pub fn set_peer_volume(&self, peer: PeerId, volume: f32) -> Result<(), AudioError> {
        self.send(Command::SetPeerVolume(peer, volume))
    }

    /// Drops the peer's jitter buffer and decoder (participant left).
    pub fn remove_peer(&self, peer: PeerId) -> Result<(), AudioError> {
        self.send(Command::RemovePeer(peer))
    }

    pub fn set_encoder(&self, config: EncoderConfig) -> Result<(), AudioError> {
        self.send(Command::SetEncoder(config))
    }

    /// Turns noise suppression / echo cancellation / auto gain on or off live.
    pub fn set_dsp(&self, config: DspConfig) -> Result<(), AudioError> {
        self.send(Command::SetDsp(config))
    }

    pub fn status(&self) -> EngineStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Stops the worker, waits for it and releases both devices.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            // A full queue still gets the stop: the worker drains it every 5 ms.
            let _ = self.commands.send(Command::Stop);
            let _ = thread.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    io: Box<dyn AudioIo>,
    config: EngineConfig,
    sender: VoiceSender,
    dsp: VoiceDsp,
    receivers: HashMap<PeerId, VoiceReceiver>,
    /// Volumes survive receiver removal by the transport and apply to new receivers.
    volumes: HashMap<PeerId, f32>,
    input: Option<InputStream>,
    output: Option<OutputStream>,
    out_resampler: MonoResampler,
    commands: Receiver<Command>,
    outgoing: SyncSender<OutgoingFrame>,
    incoming: Receiver<IncomingPacket>,
    incoming_dropped: Arc<AtomicU64>,
    status: Arc<Mutex<EngineStatus>>,
    st: EngineStatus,
    /// Next reopen attempt for a missing stream.
    input_retry: Option<Instant>,
    output_retry: Option<Instant>,
    past_input_xruns: u64,
    past_output_xruns: u64,
    past_backend_xruns: u64,
    /// Wall-clock playout while no output device is open.
    wall_tick: Option<Instant>,
    epoch: Instant,
    mix: Vec<f32>,
    peer_pcm: Vec<f32>,
    device_pcm: Vec<f32>,
    mic_buf: Vec<f32>,
}

impl Worker {
    fn run(mut self, ready: SyncSender<()>) {
        self.sender.set_muted(self.config.muted);
        self.dsp.set_muted(self.config.muted);
        self.open_input();
        self.open_output();
        self.publish();
        let _ = ready.send(());
        let mut last_publish = Instant::now();
        loop {
            if !self.handle_commands() {
                break;
            }
            self.check_devices();
            if let Err(e) = self.capture() {
                self.st.last_error = Some(e.to_string());
            }
            self.receive();
            if let Err(e) = self.render() {
                self.st.last_error = Some(e.to_string());
            }
            if last_publish.elapsed() >= STATUS_EVERY {
                self.publish();
                last_publish = Instant::now();
            }
            std::thread::sleep(LOOP_SLEEP);
        }
        // Release the devices before reporting that the engine has stopped.
        self.drop_input();
        self.drop_output();
        self.st.running = false;
        self.publish();
    }

    /// Returns `false` on stop (or when the handle is gone).
    fn handle_commands(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Ok(Command::SetMuted(m)) => {
                    self.sender.set_muted(m);
                    self.dsp.set_muted(m);
                    self.st.muted = m;
                    if m {
                        self.st.mic_peak = 0.0;
                    }
                }
                Ok(Command::SetInput(id)) => {
                    self.config.input = id;
                    self.drop_input();
                    self.st.reopens += 1;
                    self.open_input();
                }
                Ok(Command::SetOutput(id)) => {
                    self.config.output = id;
                    self.drop_output();
                    self.st.reopens += 1;
                    self.open_output();
                }
                Ok(Command::SetPeerVolume(peer, v)) => {
                    let v = v.clamp(0.0, 4.0);
                    self.volumes.insert(peer, v);
                    if let Some(rx) = self.receivers.get_mut(&peer) {
                        rx.set_volume(v);
                    }
                }
                Ok(Command::RemovePeer(peer)) => {
                    // Packets queued before the command would otherwise bring it back.
                    self.receive();
                    self.receivers.remove(&peer);
                    self.volumes.remove(&peer);
                }
                Ok(Command::SetDsp(cfg)) => {
                    self.dsp.set_config(cfg);
                    self.st.dsp = cfg;
                }
                Ok(Command::SetEncoder(cfg)) => {
                    if let Err(e) = self.sender.encoder().apply(cfg) {
                        self.st.last_error = Some(e.to_string());
                    }
                }
            }
        }
    }

    /// Opens the selected input, falling back to the default. On failure schedules a retry.
    fn open_input(&mut self) {
        let selected = self.config.input.clone();
        let mut result = self.io.open_input(selected.as_deref(), RING_MS);
        let mut fallback = false;
        if result.is_err() && selected.is_some() {
            fallback = true;
            result = self.io.open_input(None, RING_MS);
        }
        match result.and_then(|s| {
            if s.sample_rate != self.sender.input_rate() {
                self.sender.set_input_rate(s.sample_rate)?;
            }
            Ok(s)
        }) {
            Ok(s) => {
                self.st.input = Some(ActiveDevice {
                    name: s.device_name.clone(),
                    sample_rate: s.sample_rate,
                    fallback,
                });
                if fallback {
                    self.st.last_error = Some(format!(
                        "input device {:?} unavailable, using default",
                        selected.unwrap_or_default()
                    ));
                }
                self.input = Some(s);
                self.input_retry = None;
            }
            Err(e) => {
                self.st.input = None;
                self.st.last_error = Some(format!("input: {e}"));
                self.input_retry = Some(Instant::now() + REOPEN_EVERY);
            }
        }
    }

    fn open_output(&mut self) {
        let selected = self.config.output.clone();
        let mut result = self.io.open_output(selected.as_deref(), RING_MS);
        let mut fallback = false;
        if result.is_err() && selected.is_some() {
            fallback = true;
            result = self.io.open_output(None, RING_MS);
        }
        match result.and_then(|s| {
            self.out_resampler = MonoResampler::new(CODEC_RATE, s.sample_rate)?;
            Ok(s)
        }) {
            Ok(mut s) => {
                // Prefill so the first decoded frame does not start with an underrun.
                let silence = (s.sample_rate * self.config.prefill_ms / 1000) as usize;
                for _ in 0..silence.min(s.samples.slots()) {
                    let _ = s.samples.push(0.0);
                }
                self.st.output = Some(ActiveDevice {
                    name: s.device_name.clone(),
                    sample_rate: s.sample_rate,
                    fallback,
                });
                if fallback {
                    self.st.last_error = Some(format!(
                        "output device {:?} unavailable, using default",
                        selected.unwrap_or_default()
                    ));
                }
                self.output = Some(s);
                self.output_retry = None;
                self.wall_tick = None;
            }
            Err(e) => {
                self.st.output = None;
                self.st.last_error = Some(format!("output: {e}"));
                self.output_retry = Some(Instant::now() + REOPEN_EVERY);
            }
        }
    }

    fn drop_input(&mut self) {
        if let Some(s) = self.input.take() {
            self.past_input_xruns += s.health.xruns.load(Ordering::Relaxed);
            self.past_backend_xruns += s.health.backend_xruns.load(Ordering::Relaxed);
        }
        self.st.input = None;
    }

    fn drop_output(&mut self) {
        if let Some(s) = self.output.take() {
            self.past_output_xruns += s.health.xruns.load(Ordering::Relaxed);
            self.past_backend_xruns += s.health.backend_xruns.load(Ordering::Relaxed);
        }
        self.st.output = None;
    }

    fn check_devices(&mut self) {
        let now = Instant::now();
        if self.input.as_ref().is_some_and(|s| s.health.failed()) {
            self.drop_input();
            self.st.reopens += 1;
            self.open_input();
        } else if self.input.is_none() && self.input_retry.is_some_and(|t| now >= t) {
            self.open_input();
        }
        if self.output.as_ref().is_some_and(|s| s.health.failed()) {
            self.drop_output();
            self.st.reopens += 1;
            self.open_output();
        } else if self.output.is_none() && self.output_retry.is_some_and(|t| now >= t) {
            self.open_output();
        }
    }

    fn capture(&mut self) -> Result<(), AudioError> {
        let Some(input) = self.input.as_mut() else {
            return Ok(());
        };
        self.mic_buf.clear();
        let n = input.samples.slots();
        if let Ok(chunk) = input.samples.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            self.mic_buf.extend_from_slice(a);
            self.mic_buf.extend_from_slice(b);
            chunk.commit_all();
        }
        let outgoing = &self.outgoing;
        let st = &mut self.st;
        let dsp = &mut self.dsp;
        self.sender.push_samples_with(
            &self.mic_buf,
            |f| dsp.process_capture(f),
            |f| {
                st.mic_peak = f.peak;
                let frame = OutgoingFrame {
                    seq: f.seq,
                    timestamp: f.timestamp,
                    payload: f.payload.to_vec(),
                    peak: f.peak,
                };
                // A disconnected receiver means no transport is attached: drop silently.
                match outgoing.try_send(frame) {
                    Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                    Err(TrySendError::Full(_)) => st.frames_dropped += 1,
                }
            },
        )?;
        self.st.frames_sent = self.sender.frames_sent();
        Ok(())
    }

    fn receive(&mut self) {
        while let Ok(p) = self.incoming.try_recv() {
            if !self.receivers.contains_key(&p.peer) {
                if self.receivers.len() >= MAX_PEERS {
                    self.incoming_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                match VoiceReceiver::new(CODEC_RATE, self.config.jitter) {
                    Ok(mut rx) => {
                        rx.set_volume(self.volumes.get(&p.peer).copied().unwrap_or(1.0));
                        self.receivers.insert(p.peer, rx);
                    }
                    Err(e) => {
                        self.st.last_error = Some(e.to_string());
                        continue;
                    }
                }
            }
            let arrival_ms = p.arrival.saturating_duration_since(self.epoch).as_millis() as u64;
            if let Some(rx) = self.receivers.get_mut(&p.peer) {
                rx.push_packet(p.seq, p.payload, arrival_ms);
            }
        }
    }

    /// Mixes every peer for one 20 ms tick at 48 kHz into `self.mix`. `to_speaker`: the mix
    /// is played, so it is also the echo canceller's far-end reference.
    fn mix_tick(&mut self, to_speaker: bool) -> Result<(), AudioError> {
        self.mix.fill(0.0);
        for rx in self.receivers.values_mut() {
            self.peer_pcm.clear();
            rx.pull_frame(&mut self.peer_pcm)?;
            for (m, s) in self.mix.iter_mut().zip(&self.peer_pcm) {
                *m += s;
            }
        }
        for m in &mut self.mix {
            *m = m.clamp(-1.0, 1.0);
        }
        if to_speaker {
            self.dsp.analyze_render(&self.mix)?;
        }
        self.st.ticks += 1;
        Ok(())
    }

    fn render(&mut self) -> Result<(), AudioError> {
        if self.output.is_none() {
            // No speaker: keep receivers draining on the wall clock, discard the audio.
            let now = Instant::now();
            let tick = Duration::from_millis(u64::from(FRAME_MS));
            let mut next = *self.wall_tick.get_or_insert(now);
            while next <= now {
                next += tick;
                self.wall_tick = Some(next);
                self.mix_tick(false)?;
            }
            return Ok(());
        }
        loop {
            let Some(out) = self.output.as_ref() else {
                return Ok(());
            };
            let queued = out.samples.buffer().capacity() - out.samples.slots();
            let target = (out.sample_rate * self.config.prefill_ms / 1000) as usize;
            if queued >= target {
                return Ok(());
            }
            self.mix_tick(true)?;
            self.device_pcm.clear();
            self.out_resampler
                .process(&self.mix, &mut self.device_pcm)?;
            let Some(out) = self.output.as_mut() else {
                return Ok(());
            };
            let n = self.device_pcm.len().min(out.samples.slots());
            if let Ok(mut chunk) = out.samples.write_chunk(n) {
                let (a, b) = chunk.as_mut_slices();
                for (dst, &s) in a.iter_mut().chain(b.iter_mut()).zip(&self.device_pcm) {
                    *dst = s;
                }
                chunk.commit_all();
            }
            if n == 0 {
                return Ok(());
            }
        }
    }

    fn publish(&mut self) {
        self.st.dsp_stats = self.dsp.stats();
        self.st.packets_dropped = self.incoming_dropped.load(Ordering::Relaxed);
        self.st.input_xruns = self.past_input_xruns
            + self
                .input
                .as_ref()
                .map_or(0, |s| s.health.xruns.load(Ordering::Relaxed));
        self.st.output_xruns = self.past_output_xruns
            + self
                .output
                .as_ref()
                .map_or(0, |s| s.health.xruns.load(Ordering::Relaxed));
        let backend = |h: Option<&Arc<StreamHealth>>| {
            h.map_or(0, |h| h.backend_xruns.load(Ordering::Relaxed))
        };
        self.st.backend_xruns = self.past_backend_xruns
            + backend(self.input.as_ref().map(|s| &s.health))
            + backend(self.output.as_ref().map(|s| &s.health));
        self.st.peers = self
            .receivers
            .iter()
            .map(|(&peer, rx)| PeerStatus {
                peer,
                volume: self.volumes.get(&peer).copied().unwrap_or(1.0),
                stats: rx.stats(),
            })
            .collect();
        self.st.peers.sort_by_key(|p| p.peer);
        if let Ok(mut s) = self.status.lock() {
            s.clone_from(&self.st);
        }
    }
}
