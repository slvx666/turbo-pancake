//! Audio device I/O through cpal.
//!
//! The data callbacks run on the OS audio thread and only move samples between the
//! device buffer and an `rtrb` SPSC ring: no locks, no allocation, no logging. Channels
//! are down-mixed to mono on capture and duplicated on playback. Overflows/underruns and
//! fatal stream errors (unplugged device) are reported through atomics so the owner can
//! rebuild the stream on another device instead of crashing (spec §4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::AudioError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable identifier (`cpal::DeviceId` string) used to reopen the same device.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceList {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

fn info(d: &cpal::Device, default_id: Option<&str>) -> Option<DeviceInfo> {
    let id = d.id().ok()?.to_string();
    let name = d
        .description()
        .map(|desc| desc.name().to_owned())
        .unwrap_or_else(|_| id.clone());
    Some(DeviceInfo {
        is_default: default_id == Some(id.as_str()),
        id,
        name,
    })
}

/// Lists input and output devices of the default host.
pub fn list_devices() -> DeviceList {
    let host = cpal::default_host();
    let default_in = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());
    let default_out = host
        .default_output_device()
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());
    DeviceList {
        inputs: host
            .input_devices()
            .map(|it| it.filter_map(|d| info(&d, default_in.as_deref())).collect())
            .unwrap_or_default(),
        outputs: host
            .output_devices()
            .map(|it| {
                it.filter_map(|d| info(&d, default_out.as_deref()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn find_device(id: Option<&str>, input: bool) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    match id {
        None => if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }
        .ok_or_else(|| AudioError::NoDevice("default".into())),
        Some(id) => {
            let parsed: cpal::DeviceId = id
                .parse()
                .map_err(|_| AudioError::NoDevice(id.to_owned()))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| AudioError::NoDevice(id.to_owned()))
        }
    }
}

/// Counters shared between a device callback and its owner.
#[derive(Debug, Default)]
pub struct StreamHealth {
    /// Samples lost because the ring was full (capture) or missing (playback underrun).
    pub xruns: AtomicU64,
    pub callbacks: AtomicU64,
    /// Glitches reported by the backend itself (`ErrorKind::Xrun`, e.g. a WASAPI capture
    /// discontinuity). The stream keeps running.
    pub backend_xruns: AtomicU64,
    /// The OS refused real-time scheduling for the callback thread (still plays).
    pub realtime_denied: AtomicBool,
    /// Set when the stream reported an error that requires rebuilding it.
    pub failed: AtomicBool,
}

impl StreamHealth {
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

fn error_handler(health: Arc<StreamHealth>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e: cpal::Error| {
        use cpal::ErrorKind::*;
        match e.kind() {
            // Rerouted by the backend: the stream keeps working.
            DeviceChanged => {}
            // Non-fatal: treating these as failures made the engine reopen the WASAPI
            // microphone on every capture discontinuity (found on the dev machine).
            Xrun => {
                health.backend_xruns.fetch_add(1, Ordering::Relaxed);
            }
            RealtimeDenied => health.realtime_denied.store(true, Ordering::Relaxed),
            _ => health.failed.store(true, Ordering::Relaxed),
        }
    }
}

/// Microphone stream. Drop it to stop capturing and release the device.
pub struct Capture {
    _stream: cpal::Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub device_name: String,
    pub health: Arc<StreamHealth>,
}

/// Speaker stream. Drop it to stop playback and release the device.
pub struct Playback {
    _stream: cpal::Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub device_name: String,
    pub health: Arc<StreamHealth>,
}

/// Opens an input device (`None` = system default) and starts capturing mono f32
/// samples into the returned ring. `ring_ms` sizes the ring.
pub fn open_capture(
    id: Option<&str>,
    ring_ms: u32,
) -> Result<(Capture, Consumer<f32>), AudioError> {
    let device = find_device(id, true)?;
    let supported = device
        .default_input_config()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    let config = supported.config();
    let rate = config.sample_rate;
    let channels = config.channels;
    let (producer, consumer) = RingBuffer::new((rate * ring_ms / 1000).max(1024) as usize);
    let health = Arc::new(StreamHealth::default());
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_capture::<f32>(&device, config, producer, health.clone()),
        SampleFormat::I16 => build_capture::<i16>(&device, config, producer, health.clone()),
        SampleFormat::I32 => build_capture::<i32>(&device, config, producer, health.clone()),
        SampleFormat::U16 => build_capture::<u16>(&device, config, producer, health.clone()),
        other => Err(AudioError::Device(format!(
            "unsupported input format {other:?}"
        ))),
    }?;
    stream
        .play()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    Ok((
        Capture {
            _stream: stream,
            sample_rate: rate,
            channels,
            device_name: device_name(&device),
            health,
        },
        consumer,
    ))
}

fn build_capture<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: Producer<f32>,
    health: Arc<StreamHealth>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let scale = 1.0 / channels as f32;
    let cb_health = health.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                cb_health.callbacks.fetch_add(1, Ordering::Relaxed);
                let frames = data.len() / channels;
                let n = frames.min(producer.slots());
                if n < frames {
                    cb_health
                        .xruns
                        .fetch_add((frames - n) as u64, Ordering::Relaxed);
                }
                if let Ok(mut chunk) = producer.write_chunk(n) {
                    let (a, b) = chunk.as_mut_slices();
                    let mut frames_in = data.chunks_exact(channels);
                    for dst in a.iter_mut().chain(b.iter_mut()) {
                        let frame = frames_in.next().unwrap_or(&[]);
                        *dst = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() * scale;
                    }
                    chunk.commit_all();
                }
            },
            error_handler(health),
            None,
        )
        .map_err(|e| AudioError::Device(e.to_string()))
}

/// Opens an output device (`None` = system default). Mono f32 samples pushed into the
/// returned ring are played on every channel; an empty ring plays silence.
pub fn open_playback(
    id: Option<&str>,
    ring_ms: u32,
) -> Result<(Playback, Producer<f32>), AudioError> {
    let device = find_device(id, false)?;
    let supported = device
        .default_output_config()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    let config = supported.config();
    let rate = config.sample_rate;
    let channels = config.channels;
    let (producer, consumer) = RingBuffer::new((rate * ring_ms / 1000).max(1024) as usize);
    let health = Arc::new(StreamHealth::default());
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_playback::<f32>(&device, config, consumer, health.clone()),
        SampleFormat::I16 => build_playback::<i16>(&device, config, consumer, health.clone()),
        SampleFormat::I32 => build_playback::<i32>(&device, config, consumer, health.clone()),
        SampleFormat::U16 => build_playback::<u16>(&device, config, consumer, health.clone()),
        other => Err(AudioError::Device(format!(
            "unsupported output format {other:?}"
        ))),
    }?;
    stream
        .play()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    Ok((
        Playback {
            _stream: stream,
            sample_rate: rate,
            channels,
            device_name: device_name(&device),
            health,
        },
        producer,
    ))
}

fn build_playback<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut consumer: Consumer<f32>,
    health: Arc<StreamHealth>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    let cb_health = health.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                cb_health.callbacks.fetch_add(1, Ordering::Relaxed);
                let frames = data.len() / channels;
                let n = frames.min(consumer.slots());
                let mut out = data.chunks_exact_mut(channels);
                if let Ok(chunk) = consumer.read_chunk(n) {
                    let (a, b) = chunk.as_slices();
                    for &s in a.iter().chain(b.iter()) {
                        if let Some(frame) = out.next() {
                            frame.fill(T::from_sample(s));
                        }
                    }
                    chunk.commit_all();
                }
                if n < frames {
                    cb_health
                        .xruns
                        .fetch_add((frames - n) as u64, Ordering::Relaxed);
                    for frame in out {
                        frame.fill(T::EQUILIBRIUM);
                    }
                }
            },
            error_handler(health),
            None,
        )
        .map_err(|e| AudioError::Device(e.to_string()))
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|desc| desc.name().to_owned())
        .unwrap_or_else(|_| "<unknown>".into())
}
