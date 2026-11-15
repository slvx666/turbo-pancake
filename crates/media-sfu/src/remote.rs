//! The SFU in a process of its own (spec §3: an isolated media service).
//!
//! The server keeps the same interface as the in-process [`Sfu`]: [`RemoteSfu`] starts a
//! worker process, sends it the commands over its standard input and gets the members'
//! events back over its standard output; [`run_worker`] is the other end and runs the
//! real [`Sfu`] there. Media over UDP never crosses the pipe (the worker owns the
//! socket); only tunnel datagrams and control messages do.
//!
//! What this buys: a fault in the media code — a panic (release builds abort), a hang, a
//! crash inside a codec-level dependency — ends the worker, not the server with its
//! chats and accounts. The supervisor notices, tells every affected member's sink
//! ([`PeerSink::closed`]) and starts a new worker; members whose calls dropped
//! reconnect their media as after any network failure.
//!
//! Wire format: frames of `u32` length (little endian) + one tag byte + fields, all
//! integers little endian, strings and byte blocks length-prefixed. Both ends are this
//! file in the same build, so the format carries no version.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::IpAddr;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{
    Layer, LayerChoice, LayerReason, Link, PeerId, PeerSink, Sfu, SfuConfig, SfuStats, ShareInfo,
    Subscription, Track, TrackKind,
};

/// Largest frame accepted (an SDP or a datagram is far smaller): a corrupt length must
/// not allocate without bound.
const MAX_FRAME: usize = 1 << 20;
/// Frames waiting for the pipe; beyond it they are dropped like lost packets.
const QUEUE: usize = 8192;

// ---- encoding ----

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn tag(tag: u8) -> Self {
        Out(vec![tag])
    }
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.0.extend_from_slice(v);
        self
    }
    fn str(self, v: &str) -> Self {
        self.bytes(v.as_bytes())
    }
    fn opt_str(self, v: Option<&str>) -> Self {
        match v {
            Some(s) => self.u8(1).str(s),
            None => self.u8(0),
        }
    }
}

struct In<'a> {
    buf: &'a [u8],
}

/// A frame that does not decode: the other end is not this build, or the pipe is broken.
#[derive(Debug)]
struct Malformed;

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.buf.len() < n {
            return Err(Malformed);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, Malformed> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Result<String, Malformed> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| Malformed)
    }
    fn opt_str(&mut self) -> Result<Option<String>, Malformed> {
        Ok(match self.u8()? {
            0 => None,
            _ => Some(self.str()?),
        })
    }
}

fn write_frame(w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    w.write_all(&(frame.len() as u32).to_le_bytes())?;
    w.write_all(frame)
}

/// The next frame, `None` at a clean end of the stream.
fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
    }
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame)?;
    Ok(Some(frame))
}

// Server → worker.
const C_JOIN: u8 = 1;
const C_LEAVE: u8 = 2;
const C_DATAGRAM: u8 = 3;
const C_SHARE: u8 = 4;
const C_SUBSCRIBE: u8 = 5;
const C_PREVIEWS: u8 = 6;
// Worker → server.
const E_READY: u8 = 64;
const E_ANSWER: u8 = 65;
const E_DATAGRAM: u8 = 66;
const E_TRACK: u8 = 67;
const E_SCREEN: u8 = 68;
const E_SUBSCRIPTION: u8 = 69;
const E_LISTENERS_LOSS: u8 = 70;
const E_STATS: u8 = 71;

fn share_out(o: Out, share: Option<ShareInfo>) -> Out {
    match share {
        None => o.u8(0),
        Some(s) => o
            .u8(1)
            .u32(s.width)
            .u32(s.height)
            .u32(s.fps)
            .u32(s.bitrate_kbps)
            .u8(u8::from(s.preview)),
    }
}

fn share_in(i: &mut In<'_>) -> Result<Option<ShareInfo>, Malformed> {
    Ok(match i.u8()? {
        0 => None,
        _ => Some(ShareInfo {
            width: i.u32()?,
            height: i.u32()?,
            fps: i.u32()?,
            bitrate_kbps: i.u32()?,
            preview: i.u8()? != 0,
        }),
    })
}

fn choice_out(c: LayerChoice) -> u8 {
    match c {
        LayerChoice::Auto => 0,
        LayerChoice::High => 1,
        LayerChoice::Low => 2,
    }
}

fn choice_in(v: u8) -> Result<LayerChoice, Malformed> {
    Ok(match v {
        0 => LayerChoice::Auto,
        1 => LayerChoice::High,
        2 => LayerChoice::Low,
        _ => return Err(Malformed),
    })
}

fn stats_out(s: &SfuStats) -> Vec<u8> {
    let fields = stats_fields(s);
    let mut o = Out::tag(E_STATS);
    for v in fields {
        o = o.u64(v);
    }
    o.0
}

fn stats_fields(s: &SfuStats) -> [u64; 22] {
    [
        s.peers as u64,
        s.tunnel_peers as u64,
        s.connected_peers as u64,
        s.rtp_in,
        s.rtp_out,
        s.video_in,
        s.video_out,
        s.preview_in,
        s.preview_out,
        s.keyframe_requests,
        s.layer_switches,
        s.viewers_high as u64,
        s.viewers_low as u64,
        s.preview_subscriptions as u64,
        s.high_egress_kbps,
        s.streams_out as u64,
        s.udp_in,
        s.udp_out,
        s.udp_send_errors,
        s.tunnel_in,
        s.tunnel_out,
        s.dropped_commands,
    ]
}

fn stats_in(i: &mut In<'_>) -> Result<SfuStats, Malformed> {
    let mut f = [0u64; 22];
    for v in &mut f {
        *v = i.u64()?;
    }
    Ok(SfuStats {
        peers: f[0] as usize,
        tunnel_peers: f[1] as usize,
        connected_peers: f[2] as usize,
        rtp_in: f[3],
        rtp_out: f[4],
        video_in: f[5],
        video_out: f[6],
        preview_in: f[7],
        preview_out: f[8],
        keyframe_requests: f[9],
        layer_switches: f[10],
        viewers_high: f[11] as usize,
        viewers_low: f[12] as usize,
        preview_subscriptions: f[13] as usize,
        high_egress_kbps: f[14],
        streams_out: f[15] as usize,
        udp_in: f[16],
        udp_out: f[17],
        udp_send_errors: f[18],
        tunnel_in: f[19],
        tunnel_out: f[20],
        dropped_commands: f[21],
    })
}

// ---- worker side ----

/// A member's sink inside the worker: every callback becomes a frame for the server.
struct PipeSink {
    peer: PeerId,
    frames: SyncSender<Vec<u8>>,
}

impl PipeSink {
    fn send(&self, o: Out) {
        // A full queue drops the frame: the SFU thread never waits for the pipe.
        let _ = self.frames.try_send(o.0);
    }
}

impl PeerSink for PipeSink {
    fn answer(&mut self, result: Result<String, String>) {
        let (ok, text) = match &result {
            Ok(sdp) => (1, sdp.as_str()),
            Err(e) => (0, e.as_str()),
        };
        self.send(Out::tag(E_ANSWER).str(&self.peer).u8(ok).str(text));
    }
    fn datagram(&mut self, data: Vec<u8>) {
        self.send(Out::tag(E_DATAGRAM).str(&self.peer).bytes(&data));
    }
    fn track(&mut self, peer: &str, track: Track, published: bool) {
        let kind = match track.kind {
            TrackKind::Audio => 0,
            TrackKind::Screen => 1,
            TrackKind::ScreenAudio => 2,
            TrackKind::ScreenPreview => 3,
        };
        self.send(
            Out::tag(E_TRACK)
                .str(&self.peer)
                .str(peer)
                .u8(kind)
                .u32(track.ssrc)
                .u8(u8::from(track.rtx.is_some()))
                .u32(track.rtx.unwrap_or(0))
                .u8(u8::from(published)),
        );
    }
    fn screen(&mut self, peer: &str, share: Option<ShareInfo>) {
        self.send(share_out(
            Out::tag(E_SCREEN).str(&self.peer).str(peer),
            share,
        ));
    }
    fn subscription(&mut self, sub: &Subscription) {
        self.send(
            Out::tag(E_SUBSCRIPTION)
                .str(&self.peer)
                .str(&sub.source)
                .u8(choice_out(sub.requested))
                .u8(match sub.layer {
                    Layer::High => 0,
                    Layer::Low => 1,
                })
                .u8(match sub.reason {
                    LayerReason::Requested => 0,
                    LayerReason::Network => 1,
                    LayerReason::Admission => 2,
                    LayerReason::SingleLayer => 3,
                }),
        );
    }
    fn listeners_loss(&mut self, loss: f32) {
        self.send(
            Out::tag(E_LISTENERS_LOSS)
                .str(&self.peer)
                .u32(loss.to_bits()),
        );
    }
}

fn worker_command(sfu: &Sfu, frames: &SyncSender<Vec<u8>>, frame: &[u8]) -> Result<(), Malformed> {
    let mut i = In { buf: frame };
    match i.u8()? {
        C_JOIN => {
            let (peer, room, offer) = (i.str()?, i.str()?, i.str()?);
            let link = match i.u8()? {
                0 => Link::Tunnel,
                _ => {
                    let n = i.u32()? as usize;
                    let mut ips = Vec::with_capacity(n.min(16));
                    for _ in 0..n {
                        ips.push(i.str()?.parse::<IpAddr>().map_err(|_| Malformed)?);
                    }
                    Link::Udp { candidate_ips: ips }
                }
            };
            let sink = PipeSink {
                peer: peer.clone(),
                frames: frames.clone(),
            };
            sfu.join(&peer, &room, offer, link, Box::new(sink));
        }
        C_LEAVE => sfu.leave(&i.str()?),
        C_DATAGRAM => {
            let peer = i.str()?;
            sfu.datagram(&peer, i.bytes()?.to_vec());
        }
        C_SHARE => {
            let peer = i.str()?;
            sfu.share(&peer, share_in(&mut i)?);
        }
        C_SUBSCRIBE => {
            let peer = i.str()?;
            let source = i.opt_str()?;
            sfu.subscribe_layer(&peer, source.as_deref(), choice_in(i.u8()?)?);
        }
        C_PREVIEWS => {
            let peer = i.str()?;
            let n = i.u32()? as usize;
            let mut sources = Vec::with_capacity(n.min(256));
            for _ in 0..n {
                sources.push(i.str()?);
            }
            sfu.previews(&peer, sources);
        }
        _ => return Err(Malformed),
    }
    Ok(())
}

/// The worker process: runs the SFU with `config`, takes commands from `input` and
/// writes events to `output` until `input` ends (the server went away or stopped us).
pub fn run_worker(
    config: SfuConfig,
    input: impl Read,
    output: impl Write + Send + 'static,
) -> io::Result<()> {
    let sfu = Arc::new(Sfu::start(config)?);
    let (frames, queued) = sync_channel::<Vec<u8>>(QUEUE);
    let (recv, send) = sfu.udp_buffers().unwrap_or((0, 0));
    let ready = Out::tag(E_READY)
        .u32(u32::from(sfu.udp_port().unwrap_or(0)))
        .u64(recv as u64)
        .u64(send as u64);
    let _ = frames.try_send(ready.0);
    let writer = std::thread::Builder::new()
        .name("tp-sfu-pipe".into())
        .spawn(move || write_frames(queued, output))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stats = {
        let (sfu, frames, stop) = (sfu.clone(), frames.clone(), stop.clone());
        std::thread::Builder::new()
            .name("tp-sfu-stats".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let _ = frames.try_send(stats_out(&sfu.stats()));
                }
            })?
    };
    let mut input = BufReader::new(input);
    let result = loop {
        match read_frame(&mut input) {
            Ok(Some(frame)) => {
                if worker_command(&sfu, &frames, &frame).is_err() {
                    break Err(io::Error::new(io::ErrorKind::InvalidData, "command"));
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    stop.store(true, Ordering::Relaxed);
    sfu.stop();
    let _ = stats.join();
    drop(frames);
    drop(sfu);
    let _ = writer.join();
    result
}

/// Writes queued frames; flushes whenever the queue runs empty.
fn write_frames(queued: Receiver<Vec<u8>>, output: impl Write) {
    let mut out = BufWriter::new(output);
    while let Ok(frame) = queued.recv() {
        if write_frame(&mut out, &frame).is_err() {
            return;
        }
        while let Ok(more) = queued.try_recv() {
            if write_frame(&mut out, &more).is_err() {
                return;
            }
        }
        if out.flush().is_err() {
            return;
        }
    }
}

// ---- server side ----

struct Shared {
    sinks: Mutex<HashMap<PeerId, Box<dyn PeerSink>>>,
    stats: Mutex<SfuStats>,
    /// UDP port and buffer sizes of the current worker (0 = no UDP).
    udp: Mutex<(u16, usize, usize)>,
    /// Frames to the current worker; `None` while there is none.
    to_worker: Mutex<Option<SyncSender<Vec<u8>>>>,
    child: Mutex<Option<Child>>,
    /// Workers started so far (1 = never restarted).
    starts: AtomicU64,
    ready: AtomicU64,
    stop: AtomicBool,
    dropped: AtomicU64,
}

/// The SFU in a worker process, with the interface of [`Sfu`].
pub struct RemoteSfu {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteSfu {
    /// Starts the worker with `spawn` (a command whose standard input and output are
    /// pipes and which runs [`run_worker`]) and waits until it is ready. The worker is
    /// started again with `spawn` whenever it ends.
    pub fn start(
        spawn: impl FnMut() -> io::Result<Child> + Send + 'static,
    ) -> io::Result<RemoteSfu> {
        let shared = Arc::new(Shared {
            sinks: Mutex::default(),
            stats: Mutex::default(),
            udp: Mutex::new((0, 0, 0)),
            to_worker: Mutex::new(None),
            child: Mutex::new(None),
            starts: AtomicU64::new(0),
            ready: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        });
        let s = shared.clone();
        let supervisor = std::thread::Builder::new()
            .name("tp-sfu-supervisor".into())
            .spawn(move || supervise(s, spawn))?;
        let remote = RemoteSfu {
            shared,
            supervisor: Mutex::new(Some(supervisor)),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while remote.shared.ready.load(Ordering::Relaxed) == 0 {
            if Instant::now() >= deadline {
                remote.stop();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the media worker did not start",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(remote)
    }

    fn send(&self, o: Out) -> bool {
        let ok = self
            .shared
            .to_worker
            .lock()
            .ok()
            .and_then(|tx| tx.as_ref().map(|tx| tx.try_send(o.0).is_ok()))
            .unwrap_or(false);
        if !ok {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    pub fn udp_port(&self) -> Option<u16> {
        let port = self.shared.udp.lock().map(|u| u.0).unwrap_or(0);
        (port != 0).then_some(port)
    }

    pub fn udp_buffers(&self) -> Option<(usize, usize)> {
        let (port, recv, send) = self.shared.udp.lock().map(|u| *u).unwrap_or((0, 0, 0));
        (port != 0).then_some((recv, send))
    }

    /// Workers started so far: 1 until the first restart.
    pub fn worker_starts(&self) -> u64 {
        self.shared.starts.load(Ordering::Relaxed)
    }

    /// Ends the current worker process (the supervisor starts a new one): for tests and
    /// for an operator who wants the media service restarted without the server.
    pub fn kill_worker(&self) {
        if let Ok(mut child) = self.shared.child.lock()
            && let Some(c) = child.as_mut()
        {
            let _ = c.kill();
        }
    }

    pub fn join(
        &self,
        peer: &str,
        room: &str,
        offer: String,
        link: Link,
        sink: Box<dyn PeerSink>,
    ) -> bool {
        let mut o = Out::tag(C_JOIN).str(peer).str(room).str(&offer);
        o = match &link {
            Link::Tunnel => o.u8(0),
            Link::Udp { candidate_ips } => {
                let mut o = o.u8(1).u32(candidate_ips.len() as u32);
                for ip in candidate_ips {
                    o = o.str(&ip.to_string());
                }
                o
            }
        };
        // Registered first: the answer may arrive before `send` returns.
        if let Ok(mut sinks) = self.shared.sinks.lock() {
            sinks.insert(peer.to_owned(), sink);
        }
        let ok = self.send(o);
        if !ok && let Ok(mut sinks) = self.shared.sinks.lock() {
            sinks.remove(peer);
        }
        ok
    }

    pub fn leave(&self, peer: &str) {
        self.send(Out::tag(C_LEAVE).str(peer));
        if let Ok(mut sinks) = self.shared.sinks.lock() {
            sinks.remove(peer);
        }
    }

    pub fn share(&self, peer: &str, share: Option<ShareInfo>) -> bool {
        self.send(share_out(Out::tag(C_SHARE).str(peer), share))
    }

    pub fn subscribe(&self, peer: &str, source: Option<&str>) -> bool {
        self.subscribe_layer(peer, source, LayerChoice::Auto)
    }

    pub fn subscribe_layer(&self, peer: &str, source: Option<&str>, layer: LayerChoice) -> bool {
        self.send(
            Out::tag(C_SUBSCRIBE)
                .str(peer)
                .opt_str(source)
                .u8(choice_out(layer)),
        )
    }

    pub fn previews(&self, peer: &str, sources: Vec<PeerId>) -> bool {
        let mut o = Out::tag(C_PREVIEWS).str(peer).u32(sources.len() as u32);
        for s in &sources {
            o = o.str(s);
        }
        self.send(o)
    }

    pub fn datagram(&self, peer: &str, data: Vec<u8>) -> bool {
        self.send(Out::tag(C_DATAGRAM).str(peer).bytes(&data))
    }

    pub fn stats(&self) -> SfuStats {
        let mut s = self
            .shared
            .stats
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        s.dropped_commands += self.shared.dropped.load(Ordering::Relaxed);
        s
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Closing its input ends the worker; the kill covers one that hangs.
        if let Ok(mut tx) = self.shared.to_worker.lock() {
            *tx = None;
        }
        self.kill_worker();
        if let Some(t) = self.supervisor.lock().ok().and_then(|mut t| t.take()) {
            let _ = t.join();
        }
    }
}

impl Drop for RemoteSfu {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs workers one after another until stopped: start, pump its pipes, clean up.
fn supervise(shared: Arc<Shared>, mut spawn: impl FnMut() -> io::Result<Child>) {
    let mut backoff = Duration::from_millis(200);
    while !shared.stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if let Ok(mut child) = spawn() {
            shared.starts.fetch_add(1, Ordering::Relaxed);
            let pipes = (child.stdin.take(), child.stdout.take());
            if let Ok(mut c) = shared.child.lock() {
                *c = Some(child);
            }
            if let (Some(stdin), Some(stdout)) = pipes {
                pump(&shared, stdin, stdout);
            }
            // The worker is gone: no member has a media session any more.
            if let Ok(mut tx) = shared.to_worker.lock() {
                *tx = None;
            }
            let lost: Vec<Box<dyn PeerSink>> = shared
                .sinks
                .lock()
                .map(|mut s| s.drain().map(|(_, sink)| sink).collect())
                .unwrap_or_default();
            for mut sink in lost {
                sink.closed();
            }
            if let Ok(mut s) = shared.stats.lock() {
                *s = SfuStats::default();
            }
            if let Some(mut c) = shared.child.lock().ok().and_then(|mut c| c.take()) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        // A worker that ran for a while restarts at once; one that keeps dying backs
        // off (a broken installation must not spin).
        if started.elapsed() > Duration::from_secs(10) {
            backoff = Duration::from_millis(200);
        } else {
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
        let until = Instant::now() + backoff;
        while Instant::now() < until && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Moves frames both ways until the worker's output ends.
fn pump(shared: &Arc<Shared>, stdin: ChildStdin, stdout: ChildStdout) {
    let (tx, queued) = sync_channel::<Vec<u8>>(QUEUE);
    let writer = std::thread::Builder::new()
        .name("tp-sfu-pipe".into())
        .spawn(move || write_frames(queued, stdin));
    if let Ok(mut slot) = shared.to_worker.lock() {
        *slot = Some(tx);
    }
    let mut output = BufReader::new(stdout);
    while let Ok(Some(frame)) = read_frame(&mut output) {
        if worker_event(shared, &frame).is_err() {
            break;
        }
    }
    // Dropping the sender ends the writer, which closes the worker's input.
    if let Ok(mut slot) = shared.to_worker.lock() {
        *slot = None;
    }
    if let Ok(w) = writer {
        let _ = w.join();
    }
}

fn worker_event(shared: &Shared, frame: &[u8]) -> Result<(), Malformed> {
    let mut i = In { buf: frame };
    let tag = i.u8()?;
    match tag {
        E_READY => {
            let (port, recv, send) = (i.u32()?, i.u64()?, i.u64()?);
            if let Ok(mut u) = shared.udp.lock() {
                *u = (port as u16, recv as usize, send as usize);
            }
            shared.ready.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        E_STATS => {
            let s = stats_in(&mut i)?;
            if let Ok(mut st) = shared.stats.lock() {
                *st = s;
            }
            return Ok(());
        }
        _ => {}
    }
    let peer = i.str()?;
    let Ok(mut sinks) = shared.sinks.lock() else {
        return Ok(());
    };
    // Events for a member that already left are dropped.
    let Some(sink) = sinks.get_mut(&peer) else {
        return Ok(());
    };
    match tag {
        E_ANSWER => {
            let ok = i.u8()? != 0;
            let text = i.str()?;
            sink.answer(if ok { Ok(text) } else { Err(text) });
        }
        E_DATAGRAM => sink.datagram(i.bytes()?.to_vec()),
        E_TRACK => {
            let other = i.str()?;
            let kind = match i.u8()? {
                0 => TrackKind::Audio,
                1 => TrackKind::Screen,
                2 => TrackKind::ScreenAudio,
                3 => TrackKind::ScreenPreview,
                _ => return Err(Malformed),
            };
            let ssrc = i.u32()?;
            let has_rtx = i.u8()? != 0;
            let rtx = i.u32()?;
            let published = i.u8()? != 0;
            sink.track(
                &other,
                Track {
                    kind,
                    ssrc,
                    rtx: has_rtx.then_some(rtx),
                },
                published,
            );
        }
        E_SCREEN => {
            let other = i.str()?;
            sink.screen(&other, share_in(&mut i)?);
        }
        E_SUBSCRIPTION => {
            let source = i.str()?;
            let requested = choice_in(i.u8()?)?;
            let layer = match i.u8()? {
                0 => Layer::High,
                1 => Layer::Low,
                _ => return Err(Malformed),
            };
            let reason = match i.u8()? {
                0 => LayerReason::Requested,
                1 => LayerReason::Network,
                2 => LayerReason::Admission,
                3 => LayerReason::SingleLayer,
                _ => return Err(Malformed),
            };
            sink.subscription(&Subscription {
                source,
                requested,
                layer,
                reason,
            });
        }
        E_LISTENERS_LOSS => sink.listeners_loss(f32::from_bits(i.u32()?)),
        _ => return Err(Malformed),
    }
    Ok(())
}

/// The SFU the server talks to: in this process or in a worker.
pub enum MediaSfu {
    Local(Sfu),
    Remote(RemoteSfu),
}

macro_rules! both {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            MediaSfu::Local($s) => $e,
            MediaSfu::Remote($s) => $e,
        }
    };
}

impl MediaSfu {
    pub fn udp_port(&self) -> Option<u16> {
        both!(self, s => s.udp_port())
    }
    pub fn udp_buffers(&self) -> Option<(usize, usize)> {
        both!(self, s => s.udp_buffers())
    }
    pub fn join(
        &self,
        peer: &str,
        room: &str,
        offer: String,
        link: Link,
        sink: Box<dyn PeerSink>,
    ) -> bool {
        both!(self, s => s.join(peer, room, offer, link, sink))
    }
    pub fn leave(&self, peer: &str) {
        both!(self, s => s.leave(peer))
    }
    pub fn share(&self, peer: &str, share: Option<ShareInfo>) -> bool {
        both!(self, s => s.share(peer, share))
    }
    pub fn subscribe_layer(&self, peer: &str, source: Option<&str>, layer: LayerChoice) -> bool {
        both!(self, s => s.subscribe_layer(peer, source, layer))
    }
    pub fn previews(&self, peer: &str, sources: Vec<PeerId>) -> bool {
        both!(self, s => s.previews(peer, sources))
    }
    pub fn datagram(&self, peer: &str, data: Vec<u8>) -> bool {
        both!(self, s => s.datagram(peer, data))
    }
    pub fn stats(&self) -> SfuStats {
        both!(self, s => s.stats())
    }
    pub fn stop(&self) {
        both!(self, s => s.stop())
    }
    /// Workers started so far (`None` in-process): above 1 means the media service was
    /// restarted after a failure.
    pub fn worker_starts(&self) -> Option<u64> {
        match self {
            MediaSfu::Local(_) => None,
            MediaSfu::Remote(r) => Some(r.worker_starts()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_fields_round_trip() {
        let frame = Out::tag(9)
            .u8(7)
            .u32(0xDEAD_BEEF)
            .u64(u64::MAX - 1)
            .str("привет")
            .bytes(&[1, 2, 3])
            .opt_str(None)
            .opt_str(Some("x"))
            .0;
        let mut pipe = Vec::new();
        write_frame(&mut pipe, &frame).unwrap();
        write_frame(&mut pipe, &[1]).unwrap();
        let mut r = pipe.as_slice();
        let got = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(got, frame);
        let mut i = In { buf: &got };
        assert_eq!(i.u8().unwrap(), 9);
        assert_eq!(i.u8().unwrap(), 7);
        assert_eq!(i.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(i.u64().unwrap(), u64::MAX - 1);
        assert_eq!(i.str().unwrap(), "привет");
        assert_eq!(i.bytes().unwrap(), &[1, 2, 3]);
        assert_eq!(i.opt_str().unwrap(), None);
        assert_eq!(i.opt_str().unwrap().as_deref(), Some("x"));
        assert!(i.u8().is_err(), "nothing left");
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), vec![1]);
        assert!(read_frame(&mut r).unwrap().is_none(), "clean end");
    }

    #[test]
    fn broken_frames_are_refused_not_trusted() {
        // A length beyond the limit, an empty frame, a truncated body.
        let mut huge = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; 8]);
        assert!(read_frame(&mut huge.as_slice()).is_err());
        assert!(read_frame(&mut [0u8, 0, 0, 0].as_slice()).is_err());
        assert!(read_frame(&mut [5u8, 0, 0, 0, 1, 2].as_slice()).is_err());
        // A string that claims more bytes than the frame has.
        let mut i = In {
            buf: &[200, 0, 0, 0, b'a'],
        };
        assert!(i.str().is_err());
    }

    #[test]
    fn statistics_round_trip_field_by_field() {
        let mut s = SfuStats::default();
        let fields = stats_fields(&s).len() as u64;
        // Every field gets its own value: a swapped pair would show.
        s = stats_in(&mut In {
            buf: &(1..=fields).flat_map(u64::to_le_bytes).collect::<Vec<u8>>(),
        })
        .unwrap();
        assert_eq!(stats_fields(&s), core::array::from_fn(|i| i as u64 + 1));
        let frame = stats_out(&s);
        assert_eq!(frame[0], E_STATS);
        assert_eq!(stats_in(&mut In { buf: &frame[1..] }).unwrap(), s);
    }
}
