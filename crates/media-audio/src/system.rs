//! System audio of a screen share (spec §5.1): everything the computer plays **except the
//! messenger itself**, so a viewer's voice played here never goes back to them.
//!
//! Windows 10 2004+ / 11: WASAPI process loopback (`VAD\Process_Loopback`) in mode
//! `EXCLUDE_TARGET_PROCESS_TREE` with our own process id — the OS mixes every other
//! process. The exclusion is exact by construction (it is how the OS builds the stream),
//! and `tests/system_audio.rs` checks it with a tone from this process and one from a
//! child process. No voice processing (NS/AEC/AGC) is applied (spec §4): music and game
//! audio go out as they are, Opus stereo 48 kHz, 20 ms frames, `Application::Audio`.
//!
//! macOS (ScreenCaptureKit `excludesCurrentProcessAudio`) and Linux (PipeWire routing)
//! report [`AudioError::Unsupported`] until implemented and verified on real hardware.

// The frame packer is used only by the Windows capture (and the tests) for now.
#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::AudioError;

/// One encoded 20 ms frame of system audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemAudioFrame {
    pub seq: u16,
    /// RTP timestamp, 48 kHz units; keeps running across silence (nothing is sent then).
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemAudioStats {
    /// Frames encoded and handed out.
    pub frames: u64,
    /// 20 ms periods in which nothing played (nothing sent).
    pub silent_frames: u64,
    /// Peak of the last second, 0–1.
    pub peak: f32,
    /// Exactly our process tree excluded by the OS.
    pub excludes_self: bool,
    pub error: Option<String>,
}

pub struct SystemAudioCapture {
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<SystemAudioStats>>,
    thread: Option<JoinHandle<()>>,
}

/// Opus bitrate of system audio (stereo music quality).
pub const SYSTEM_AUDIO_BITRATE: i32 = 96_000;

impl SystemAudioCapture {
    /// Starts capturing; `on_frame` runs on the capture thread and must not block.
    pub fn start(
        on_frame: impl FnMut(SystemAudioFrame) + Send + 'static,
    ) -> Result<Self, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(SystemAudioStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (s, st) = (stop.clone(), stats.clone());
        let thread = std::thread::Builder::new()
            .name("tp-system-audio".into())
            .spawn(move || imp::run(s, st, Box::new(on_frame), ready_tx))
            .map_err(|e| AudioError::Device(format!("system audio thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                stats,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(AudioError::Device("system audio thread exited".into()))
            }
        }
    }

    pub fn stats(&self) -> SystemAudioStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

type OnFrame = Box<dyn FnMut(SystemAudioFrame) + Send>;
type Ready = std::sync::mpsc::Sender<Result<(), AudioError>>;

/// Packs interleaved stereo samples into 20 ms Opus frames with RTP numbering.
struct Packer {
    encoder: opus::Encoder,
    pending: Vec<f32>,
    seq: u16,
    timestamp: u32,
    out: Vec<u8>,
    peak: f32,
}

const FRAME: usize = crate::FRAME_SAMPLES; // per channel

impl Packer {
    fn new() -> Result<Self, AudioError> {
        let mut encoder = opus::Encoder::new(
            crate::CODEC_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        encoder.set_bitrate(opus::Bitrate::Bits(SYSTEM_AUDIO_BITRATE))?;
        encoder.set_signal(opus::Signal::Music)?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(FRAME * 4),
            seq: 0,
            timestamp: 0,
            out: vec![0; 4000],
            peak: 0.0,
        })
    }

    /// Adds samples; returns the frames completed.
    fn push(&mut self, samples: &[f32]) -> Result<Vec<SystemAudioFrame>, AudioError> {
        self.pending.extend_from_slice(samples);
        let mut frames = Vec::new();
        while self.pending.len() >= FRAME * 2 {
            let chunk: Vec<f32> = self.pending.drain(..FRAME * 2).collect();
            self.peak = chunk.iter().fold(self.peak, |p, s| p.max(s.abs()));
            let n = self.encoder.encode_float(&chunk, &mut self.out)?;
            frames.push(SystemAudioFrame {
                seq: self.seq,
                timestamp: self.timestamp,
                payload: self.out[..n].to_vec(),
            });
            self.seq = self.seq.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(FRAME as u32);
        }
        Ok(frames)
    }

    /// Nothing played for `frames` × 20 ms: the timestamp runs on, nothing is sent.
    fn skip(&mut self, frames: u32) {
        self.pending.clear();
        self.timestamp = self.timestamp.wrapping_add(frames * FRAME as u32);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0_0_0};
    use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx};
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcessId, WaitForSingleObject,
    };
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

    use super::{OnFrame, Packer, Ready, SystemAudioStats};
    use crate::AudioError;

    fn os(context: &str, e: windows::core::Error) -> AudioError {
        AudioError::Device(format!("{context}: {}", e.message()))
    }

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Completion(Mutex<Option<Sender<()>>>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
        fn ActivateCompleted(
            &self,
            _op: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            if let Some(tx) = self.0.lock().ok().and_then(|mut t| t.take()) {
                let _ = tx.send(());
            }
            Ok(())
        }
    }

    /// Activates a process-loopback audio client that excludes our process tree.
    fn activate() -> Result<IAudioClient, AudioError> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    // SAFETY: plain query of our own id.
                    TargetProcessId: unsafe { GetCurrentProcessId() },
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // The PROPVARIANT only borrows `params`: never cleared (ManuallyDrop), so the OS
        // does not try to free our stack memory.
        let mut pv = ManuallyDrop::new(PROPVARIANT::default());
        // SAFETY: writing the VT_BLOB arm of a default-initialised PROPVARIANT.
        unsafe {
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous = PROPVARIANT_0_0_0 {
                blob: BLOB {
                    cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                    pBlobData: &params as *const _ as *mut u8,
                },
            };
        }
        let (tx, rx) = channel();
        let handler: IActivateAudioInterfaceCompletionHandler =
            Completion(Mutex::new(Some(tx))).into();
        // SAFETY: `pv` and `params` outlive the call; the handler is reference counted.
        let op = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*pv as *const PROPVARIANT),
                &handler,
            )
        }
        .map_err(|e| os("process loopback (needs Windows 10 2004 or newer)", e))?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| AudioError::Device("process loopback activation timed out".into()))?;
        let mut result = HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        // SAFETY: out-params owned here; the operation completed.
        unsafe { op.GetActivateResult(&mut result, &mut unknown) }
            .map_err(|e| os("activation result", e))?;
        result.ok().map_err(|e| os("process loopback", e))?;
        unknown
            .ok_or_else(|| AudioError::Device("no audio client".into()))?
            .cast()
            .map_err(|e| os("IAudioClient", e))
    }

    pub(super) fn run(
        stop: Arc<AtomicBool>,
        stats: Arc<Mutex<SystemAudioStats>>,
        mut on_frame: OnFrame,
        ready: Ready,
    ) {
        // SAFETY: COM for this thread; MTA is what the audio client expects here.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let setup = (|| -> Result<_, AudioError> {
            let client = activate()?;
            // Process loopback has no mix format of its own: we ask for 48 kHz stereo
            // float and the OS converts.
            let format = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: 2,
                nSamplesPerSec: crate::CODEC_RATE,
                nAvgBytesPerSec: crate::CODEC_RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            // SAFETY: COM calls on a live client with a format owned here.
            unsafe {
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK
                            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                        200_000, // 20 ms buffer, 100 ns units
                        0,
                        &format,
                        None,
                    )
                    .map_err(|e| os("IAudioClient::Initialize", e))?;
                let event = CreateEventW(None, false, false, None).map_err(|e| os("event", e))?;
                client
                    .SetEventHandle(event)
                    .map_err(|e| os("SetEventHandle", e))?;
                let capture: IAudioCaptureClient = client
                    .GetService()
                    .map_err(|e| os("IAudioCaptureClient", e))?;
                client.Start().map_err(|e| os("Start", e))?;
                Ok((client, capture, event))
            }
        })();
        let (client, capture, event) = match setup.and_then(|s| Ok((s, Packer::new()?))) {
            Ok((s, p)) => {
                let _ = ready.send(Ok(()));
                (s, p)
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        }
        .0;
        let mut packer = Packer::new().expect("checked above");
        let mut st = SystemAudioStats {
            excludes_self: true,
            ..Default::default()
        };
        let mut last_data = Instant::now();
        let mut last_publish = Instant::now();
        let mut buf: Vec<f32> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            // SAFETY: waiting on our own event handle.
            let woke = unsafe { WaitForSingleObject(event, 50) } == WAIT_OBJECT_0;
            let result = (|| -> Result<(), AudioError> {
                loop {
                    // SAFETY: the capture client is live; the buffer is released below.
                    let size = unsafe { capture.GetNextPacketSize() }
                        .map_err(|e| os("GetNextPacketSize", e))?;
                    if size == 0 {
                        return Ok(());
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    // SAFETY: GetBuffer hands out `frames` stereo float frames until
                    // ReleaseBuffer.
                    unsafe {
                        capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                            .map_err(|e| os("GetBuffer", e))?;
                        buf.clear();
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            buf.resize(frames as usize * 2, 0.0);
                        } else {
                            buf.extend_from_slice(std::slice::from_raw_parts(
                                data as *const f32,
                                frames as usize * 2,
                            ));
                        }
                        let _ = capture.ReleaseBuffer(frames);
                    }
                    last_data = Instant::now();
                    for f in packer.push(&buf)? {
                        st.frames += 1;
                        on_frame(f);
                    }
                }
            })();
            if let Err(e) = result {
                st.error = Some(e.to_string());
                break;
            }
            // Nothing plays: the OS delivers no packets. Keep the timestamp on time.
            let idle = last_data.elapsed();
            if !woke && idle >= Duration::from_millis(40) {
                let frames = (idle.as_millis() / 20) as u32;
                packer.skip(frames);
                st.silent_frames += u64::from(frames);
                last_data = Instant::now();
            }
            if last_publish.elapsed() >= Duration::from_secs(1) {
                st.peak = packer.peak;
                packer.peak = 0.0;
                last_publish = Instant::now();
                if let Ok(mut s) = stats.lock() {
                    s.clone_from(&st);
                }
            }
        }
        // SAFETY: stopping the live client and closing our event.
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(event);
        }
        if let Ok(mut s) = stats.lock() {
            s.clone_from(&st);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::{OnFrame, Ready, SystemAudioStats};
    use crate::AudioError;

    pub(super) fn run(
        _stop: Arc<AtomicBool>,
        _stats: Arc<Mutex<SystemAudioStats>>,
        _on_frame: OnFrame,
        ready: Ready,
    ) {
        let _ = ready.send(Err(AudioError::Unsupported(
            "system audio without the messenger's own sound is not implemented on this OS yet",
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packer_frames_and_timestamps() {
        let mut p = Packer::new().unwrap();
        // 1.5 frames, then 0.5: two frames total once complete.
        let tone: Vec<f32> = (0..FRAME * 3)
            .map(|i| (i as f32 * 0.05).sin() * 0.3)
            .collect();
        assert!(p.push(&tone).unwrap().len() == 1);
        let more = p.push(&tone[..FRAME]).unwrap();
        assert_eq!(more.len(), 1);
        assert_eq!((more[0].seq, more[0].timestamp), (1, FRAME as u32));
        p.skip(10);
        let next = p.push(&vec![0.0; FRAME * 2]).unwrap();
        assert_eq!(next[0].seq, 2, "seq continues across silence");
        assert_eq!(next[0].timestamp, FRAME as u32 * 12, "timestamp runs on");
        // A stereo packet decodes with the mono voice decoder (libopus downmixes).
        let mut d = crate::codec::VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; FRAME];
        assert_eq!(d.decode(&more[0].payload, &mut out).unwrap(), FRAME);
    }
}
