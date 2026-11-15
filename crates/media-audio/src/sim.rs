//! Simulated audio devices for tests and headless runs (CI runners have no sound cards).
//!
//! Each open stream runs a thread that moves samples in real time at the device rate,
//! like a cpal callback would: inputs produce a 440 Hz tone (peak 0.3), outputs consume
//! and record what they played. [`SimState`] counts open streams and running device
//! threads so tests can prove that the engine releases everything, and lets them make a
//! device disappear or fail mid-stream.

use std::collections::{HashSet, VecDeque};
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rtrb::RingBuffer;

use crate::AudioError;
use crate::device::StreamHealth;
use crate::engine::{AudioIo, InputStream, OutputStream};

#[derive(Debug, Clone)]
pub struct SimDevice {
    pub id: String,
    pub rate: u32,
    /// Output device with two channels (takes a side signal next to the mid).
    pub stereo: bool,
}

impl SimDevice {
    pub fn new(id: &str, rate: u32) -> Self {
        Self {
            id: id.to_owned(),
            rate,
            stereo: false,
        }
    }

    /// A speaker with a left and a right channel.
    pub fn stereo(mut self) -> Self {
        self.stereo = true;
        self
    }
}

/// Shared view of the simulated devices.
#[derive(Debug, Default)]
pub struct SimState {
    pub open_inputs: AtomicUsize,
    pub open_outputs: AtomicUsize,
    /// Device threads currently running (inputs + outputs).
    pub threads: AtomicUsize,
    pub input_opens: AtomicU64,
    pub output_opens: AtomicU64,
    /// Samples taken from output rings by the simulated speakers.
    pub played: AtomicU64,
    /// Played samples with |s| > 0.01.
    pub played_loud: AtomicU64,
    /// Peak of the last 100 ms played (f32 bits).
    recent_peak: AtomicU32,
    /// Stereo speakers: peaks of the left and right channel in the last 100 ms.
    recent_left: AtomicU32,
    recent_right: AtomicU32,
    unavailable: Mutex<HashSet<String>>,
    healths: Mutex<Vec<(String, Arc<StreamHealth>)>>,
    pub muted_inputs: AtomicBool,
    /// Acoustic coupling: what the speakers play reaches the microphones (gain, queue).
    echo: Mutex<Option<(f32, VecDeque<f32>)>>,
}

impl SimState {
    /// Peaks of the left and right channel of a stereo speaker (last 100 ms).
    pub fn recent_stereo_peaks(&self) -> (f32, f32) {
        (
            f32::from_bits(self.recent_left.load(Ordering::Relaxed)),
            f32::from_bits(self.recent_right.load(Ordering::Relaxed)),
        )
    }

    pub fn recent_peak(&self) -> f32 {
        f32::from_bits(self.recent_peak.load(Ordering::Relaxed))
    }

    /// Makes opening `id` fail (as if unplugged) or succeed again.
    pub fn set_unavailable(&self, id: &str, unavailable: bool) {
        let mut set = self.unavailable.lock().unwrap();
        if unavailable {
            set.insert(id.to_owned());
        } else {
            set.remove(id);
        }
    }

    /// Couples speakers to microphones like a laptop without headphones: played samples
    /// come back at the microphone `delay_ms` later, scaled by `gain`. Assumes inputs and
    /// outputs run at the same rate. `None` removes the coupling.
    pub fn set_acoustic_echo(&self, echo: Option<(f32, u32, u32)>) {
        *self.echo.lock().unwrap() = echo.map(|(gain, delay_ms, rate)| {
            (
                gain,
                VecDeque::from(vec![0.0; (rate * delay_ms / 1000) as usize]),
            )
        });
    }

    /// Reports a fatal error on every open stream of `id` (device unplugged mid-call).
    pub fn fail_device(&self, id: &str) {
        for (dev, h) in self.healths.lock().unwrap().iter() {
            if dev == id {
                h.failed.store(true, Ordering::Relaxed);
            }
        }
    }
}

pub struct SimIo {
    inputs: Vec<SimDevice>,
    outputs: Vec<SimDevice>,
    state: Arc<SimState>,
}

impl SimIo {
    /// The first device of each list is the system default.
    pub fn new(inputs: Vec<SimDevice>, outputs: Vec<SimDevice>) -> Self {
        Self {
            inputs,
            outputs,
            state: Arc::new(SimState::default()),
        }
    }

    /// Same devices as another `SimIo` sharing `state` (an engine restarted later).
    pub fn with_state(
        inputs: Vec<SimDevice>,
        outputs: Vec<SimDevice>,
        state: Arc<SimState>,
    ) -> Self {
        Self {
            inputs,
            outputs,
            state,
        }
    }

    pub fn state(&self) -> Arc<SimState> {
        self.state.clone()
    }

    fn pick(&self, list: &[SimDevice], id: Option<&str>) -> Result<SimDevice, AudioError> {
        let dev = match id {
            None => list.first(),
            Some(id) => list.iter().find(|d| d.id == id),
        }
        .ok_or_else(|| AudioError::NoDevice(id.unwrap_or("default").to_owned()))?;
        if self.state.unavailable.lock().unwrap().contains(&dev.id) {
            return Err(AudioError::NoDevice(dev.id.clone()));
        }
        Ok(dev.clone())
    }
}

/// Stops the device thread when the stream is dropped.
struct SimGuard {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    state: Arc<SimState>,
    health: Arc<StreamHealth>,
    input: bool,
}

impl Drop for SimGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let counter = if self.input {
            &self.state.open_inputs
        } else {
            &self.state.open_outputs
        };
        counter.fetch_sub(1, Ordering::Relaxed);
        self.state
            .healths
            .lock()
            .unwrap()
            .retain(|(_, h)| !Arc::ptr_eq(h, &self.health));
    }
}

const TICK: Duration = Duration::from_millis(2);

/// Runs `step(samples_due)` every 2 ms with the number of samples the device clock
/// produced since the previous step.
fn device_thread(
    rate: u32,
    stop: Arc<AtomicBool>,
    state: Arc<SimState>,
    mut step: impl FnMut(usize) + Send + 'static,
) -> JoinHandle<()> {
    state.threads.fetch_add(1, Ordering::Relaxed);
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut done = 0u64;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            let due = (start.elapsed().as_secs_f64() * f64::from(rate)) as u64;
            step((due - done) as usize);
            done = due;
        }
        state.threads.fetch_sub(1, Ordering::Relaxed);
    })
}

impl AudioIo for SimIo {
    fn open_input(
        &mut self,
        device: Option<&str>,
        ring_ms: u32,
    ) -> Result<InputStream, AudioError> {
        let dev = self.pick(&self.inputs, device)?;
        let (mut producer, consumer) = RingBuffer::new((dev.rate * ring_ms / 1000) as usize);
        let health = Arc::new(StreamHealth::default());
        let stop = Arc::new(AtomicBool::new(false));
        let st = self.state.clone();
        let h = health.clone();
        let rate = dev.rate;
        let mut n = 0u64;
        let thread = device_thread(rate, stop.clone(), st.clone(), move |due| {
            h.callbacks.fetch_add(1, Ordering::Relaxed);
            let silent = st.muted_inputs.load(Ordering::Relaxed);
            let mut echo = st.echo.lock().unwrap();
            for _ in 0..due {
                let mut s = if silent {
                    0.0
                } else {
                    0.3 * (TAU * 440.0 * (n % u64::from(rate)) as f32 / rate as f32).sin()
                };
                if let Some((gain, queue)) = echo.as_mut() {
                    s += *gain * queue.pop_front().unwrap_or(0.0);
                }
                n += 1;
                if producer.push(s).is_err() {
                    h.xruns.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        self.register(&dev, &health, true);
        Ok(InputStream::new(
            consumer,
            dev.rate,
            dev.id.clone(),
            health.clone(),
            SimGuard {
                stop,
                thread: Some(thread),
                state: self.state.clone(),
                health,
                input: true,
            },
        ))
    }

    fn open_output(
        &mut self,
        device: Option<&str>,
        ring_ms: u32,
    ) -> Result<OutputStream, AudioError> {
        let dev = self.pick(&self.outputs, device)?;
        let (producer, mut consumer) = RingBuffer::new((dev.rate * ring_ms / 1000) as usize);
        let (side_producer, mut side) = if dev.stereo {
            let (p, c) = RingBuffer::<f32>::new((dev.rate * ring_ms / 1000) as usize);
            (Some(p), Some(c))
        } else {
            (None, None)
        };
        let health = Arc::new(StreamHealth::default());
        let stop = Arc::new(AtomicBool::new(false));
        let st = self.state.clone();
        let h = health.clone();
        let window = (dev.rate / 10) as usize;
        let (mut in_window, mut peak) = (0usize, 0.0f32);
        let (mut left, mut right) = (0.0f32, 0.0f32);
        let thread = device_thread(dev.rate, stop.clone(), st.clone(), move |due| {
            h.callbacks.fetch_add(1, Ordering::Relaxed);
            let mut loud = 0;
            let mut echo = st.echo.lock().unwrap();
            for _ in 0..due {
                let s: f32 = match consumer.pop() {
                    Ok(s) => {
                        let s: f32 = s;
                        // One side sample per mid sample, like a real stereo device.
                        if let Some(side) = side.as_mut() {
                            let d: f32 = side.pop().unwrap_or(0.0);
                            left = left.max((s + d).abs());
                            right = right.max((s - d).abs());
                        }
                        s
                    }
                    Err(_) => {
                        h.xruns.fetch_add(1, Ordering::Relaxed);
                        0.0
                    }
                };
                if s.abs() > 0.01 {
                    loud += 1;
                }
                if let Some((_, queue)) = echo.as_mut() {
                    // Bounded: a stalled microphone must not grow the queue forever.
                    if queue.len() < 48_000 {
                        queue.push_back(s);
                    }
                }
                peak = peak.max(s.abs());
                in_window += 1;
                if in_window >= window {
                    st.recent_peak.store(peak.to_bits(), Ordering::Relaxed);
                    st.recent_left.store(left.to_bits(), Ordering::Relaxed);
                    st.recent_right.store(right.to_bits(), Ordering::Relaxed);
                    in_window = 0;
                    peak = 0.0;
                    (left, right) = (0.0, 0.0);
                }
            }
            st.played.fetch_add(due as u64, Ordering::Relaxed);
            st.played_loud.fetch_add(loud, Ordering::Relaxed);
        });
        self.register(&dev, &health, false);
        Ok::<_, AudioError>(OutputStream::new(
            producer,
            dev.rate,
            dev.id.clone(),
            health.clone(),
            SimGuard {
                stop,
                thread: Some(thread),
                state: self.state.clone(),
                health,
                input: false,
            },
        ))
        .map(|o: OutputStream| o.with_side(side_producer))
    }
}

impl SimIo {
    fn register(&self, dev: &SimDevice, health: &Arc<StreamHealth>, input: bool) {
        let (open, opens) = if input {
            (&self.state.open_inputs, &self.state.input_opens)
        } else {
            (&self.state.open_outputs, &self.state.output_opens)
        };
        open.fetch_add(1, Ordering::Relaxed);
        opens.fetch_add(1, Ordering::Relaxed);
        self.state
            .healths
            .lock()
            .unwrap()
            .push((dev.id.clone(), health.clone()));
    }
}
