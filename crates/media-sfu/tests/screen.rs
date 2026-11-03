//! Screen share through the SFU over real UDP (S3 step 4): a member shares H.264 access
//! units at 60 fps, another subscribes and reassembles them. Only subscribers get the
//! video; losses are repaired by NACK/RTX on both legs; a viewer's keyframe request
//! reaches the sharer.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use turbo_pancake_media_sfu::{Link, PeerSink, Sfu, SfuConfig, ShareInfo, Track, TrackKind};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role, VideoIo,
};
use turbo_pancake_media_transport::video::{AssembledFrame, FrameAssembler, VideoIn, VideoOut};

enum ToClient {
    Answer(String),
    Track(String, Track, bool),
    Screen(String, Option<ShareInfo>),
}

struct Sink {
    events: SyncSender<ToClient>,
}

impl PeerSink for Sink {
    fn answer(&mut self, result: Result<String, String>) {
        let _ = self.events.send(ToClient::Answer(result.expect("answer")));
    }
    fn datagram(&mut self, _data: Vec<u8>) {
        unreachable!("UDP members")
    }
    fn track(&mut self, peer: &str, track: Track, published: bool) {
        let _ = self
            .events
            .send(ToClient::Track(peer.to_owned(), track, published));
    }
    fn screen(&mut self, peer: &str, share: Option<ShareInfo>) {
        let _ = self.events.send(ToClient::Screen(peer.to_owned(), share));
    }
}

struct Member {
    driver: CallDriver,
    _audio: SyncSender<AudioOut>,
    video: SyncSender<VideoOut>,
    screen_audio: SyncSender<AudioOut>,
    /// SSRCs of received audio packets (voice or share audio).
    audio_in: Arc<Mutex<Vec<u32>>>,
    received: Arc<Mutex<Vec<VideoIn>>>,
    sfu_events: Receiver<ToClient>,
    calls: Receiver<CallEvent>,
}

fn member(sfu: &Sfu, name: &str, send_loss: f32) -> Member {
    let (events_tx, sfu_events) = sync_channel::<ToClient>(256);
    let (audio, outgoing) = sync_channel::<AudioOut>(16);
    let (video, video_out) = sync_channel::<VideoOut>(8);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink_rx = received.clone();
    let mut config = CallConfig::new(Role::Offerer, "127.0.0.1:0".parse().unwrap());
    config.send_loss = send_loss;
    let (screen_audio, audio_out) = sync_channel::<AudioOut>(16);
    let audio_in: Arc<Mutex<Vec<u32>>> = Arc::default();
    let heard = audio_in.clone();
    let io = VideoIo {
        audio_out,
        outgoing: video_out,
        on_video: Box::new(move |p| sink_rx.lock().unwrap().push(p)),
    };
    let (driver, calls) = CallDriver::start_media(
        config,
        MediaLink::Udp,
        outgoing,
        move |p| heard.lock().unwrap().push(p.ssrc),
        Some(io),
    )
    .unwrap();
    let CallEvent::Signal(LocalSignal::Offer(offer)) =
        calls.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("expected an offer")
    };
    let link = Link::Udp {
        candidate_ips: vec!["127.0.0.1".parse().unwrap()],
    };
    let sink = Sink { events: events_tx };
    assert!(sfu.join(name, "screen", offer, link, Box::new(sink)));
    let ToClient::Answer(answer) = sfu_events.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected the answer first")
    };
    driver.remote_answer(answer).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match calls.recv_timeout(left).expect("call did not connect") {
            CallEvent::State(CallState::Connected) => break,
            CallEvent::State(CallState::Failed(e)) => panic!("call failed: {e}"),
            _ => {}
        }
    }
    Member {
        driver,
        _audio: audio,
        video,
        screen_audio,
        audio_in,
        received,
        sfu_events,
        calls,
    }
}

const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

/// A fake access unit: (SPS + PPS + IDR | P slice) whose slice bytes all equal `fill`.
fn access_unit(key: bool, len: usize, fill: u8) -> Vec<u8> {
    let mut out = Vec::new();
    if key {
        for nal in [SPS, PPS] {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }]);
    out.extend(std::iter::repeat_n(fill, len));
    out
}

/// The screen tracks `viewer` was told about for `peer`: (video ssrc, rtx, audio ssrc).
fn screen_track_of(viewer: &Member, peer: &str) -> (u32, Option<u32>, u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut video, mut audio) = (None, None);
    loop {
        if let (Some((s, r)), Some(a)) = (video, audio) {
            return (s, r, a);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if let ToClient::Track(p, t, true) = viewer.sfu_events.recv_timeout(left).unwrap()
            && p == peer
        {
            match t.kind {
                TrackKind::Screen => video = Some((t.ssrc, t.rtx)),
                TrackKind::ScreenAudio => audio = Some(t.ssrc),
                TrackKind::Audio => {}
            }
        }
    }
}

struct Run {
    frames: Vec<AssembledFrame>,
    sent_at: Vec<Instant>,
    keyframe_requests: u64,
}

/// A sends `count` frames at 60 fps (keyframes at 0 and every 60, or when asked); B
/// assembles what arrives.
fn share(a: &Member, b: &Member, ssrc: u32, count: usize) -> Run {
    let mut assembler = FrameAssembler::new();
    let mut frames = Vec::new();
    let mut sent_at = Vec::new();
    let mut keyframe_due = true;
    let mut keyframe_requests = 0;
    let start = Instant::now();
    let drain = |assembler: &mut FrameAssembler, frames: &mut Vec<AssembledFrame>| {
        let packets: Vec<VideoIn> = std::mem::take(&mut *b.received.lock().unwrap());
        for p in packets {
            frames.extend(assembler.push(p));
        }
        frames.extend(assembler.poll(Instant::now()));
        if assembler.take_keyframe_request() {
            b.driver.request_keyframe(ssrc).ok();
        }
    };
    for n in 0..count {
        while let Ok(ev) = a.calls.try_recv() {
            if ev == CallEvent::KeyframeRequested {
                keyframe_due = true;
                keyframe_requests += 1;
            }
        }
        let key = keyframe_due || n % 60 == 0;
        keyframe_due = false;
        let len = if key { 150_000 } else { 8_000 };
        a.video
            .send(VideoOut {
                timestamp: n as u32 * 1500,
                data: access_unit(key, len, n as u8),
            })
            .unwrap();
        sent_at.push(Instant::now());
        let due = start + Duration::from_micros(16_667 * (n as u64 + 1));
        while Instant::now() < due {
            drain(&mut assembler, &mut frames);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        drain(&mut assembler, &mut frames);
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("assembler {:?}", assembler.stats);
    Run {
        frames,
        sent_at,
        keyframe_requests,
    }
}

fn check_frames(run: &Run, min_share: f64) -> f64 {
    let mut last = None;
    let mut latency = Vec::new();
    for f in &run.frames {
        let n = (f.timestamp / 1500) as usize;
        assert!(last.is_none_or(|l| n > l), "frames in order");
        last = Some(n);
        let slice = &f.data[f.data.len() - 10..];
        assert!(slice.iter().all(|&b| b == n as u8), "frame {n} intact");
        latency.push(
            f.arrival
                .saturating_duration_since(run.sent_at[n])
                .as_secs_f64()
                * 1000.0,
        );
    }
    let share = run.frames.len() as f64 / run.sent_at.len() as f64;
    latency.sort_by(f64::total_cmp);
    let p = |q: f64| latency[((latency.len() - 1) as f64 * q) as usize];
    println!(
        "TURBO_PANCAKE_SCREEN delivered {}/{} ({:.1} %), send→assembled p50 {:.1} / p95 {:.1} / max {:.1} ms, keyframe requests {}",
        run.frames.len(),
        run.sent_at.len(),
        share * 100.0,
        p(0.5),
        p(0.95),
        p(1.0),
        run.keyframe_requests
    );
    assert!(share >= min_share, "delivered {:.1} %", share * 100.0);
    share
}

fn sfu() -> Sfu {
    turbo_pancake_media_transport::init_crypto();
    Sfu::start(SfuConfig {
        udp_bind: Some("127.0.0.1:0".parse().unwrap()),
    })
    .unwrap()
}

#[test]
fn a_subscriber_receives_the_share_and_others_do_not() {
    let sfu = sfu();
    let a = member(&sfu, "a", 0.0);
    let b = member(&sfu, "b", 0.0);
    let c = member(&sfu, "c", 0.0);
    let (ssrc, rtx, audio_ssrc) = screen_track_of(&b, "a");
    b.driver.expect_video_source(ssrc, rtx).unwrap();
    b.driver.expect_screen_audio_source(audio_ssrc).unwrap();
    let (c_ssrc, c_rtx, c_audio) = screen_track_of(&c, "a");
    c.driver.expect_video_source(c_ssrc, c_rtx).unwrap();
    c.driver.expect_screen_audio_source(c_audio).unwrap();

    let info = ShareInfo {
        width: 1920,
        height: 1080,
        fps: 60,
    };
    assert!(sfu.share("a", Some(info)));
    assert!(sfu.subscribe("b", Some("a")));
    // B and C both hear about the share.
    for m in [&b, &c] {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let ToClient::Screen(p, s) = m.sfu_events.recv_timeout(left).unwrap() {
                assert_eq!((p.as_str(), s), ("a", Some(info)));
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    // The share's system audio: 25 frames of 20 ms.
    for seq in 0..25u16 {
        a.screen_audio
            .send(AudioOut {
                seq,
                timestamp: u32::from(seq) * 960,
                payload: vec![0xFC; 60],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let run = share(&a, &b, ssrc, 120);
    check_frames(&run, 1.0);
    assert!(
        c.received.lock().unwrap().is_empty(),
        "no video for a member that did not subscribe"
    );
    let b_audio = b
        .audio_in
        .lock()
        .unwrap()
        .iter()
        .filter(|s| **s == audio_ssrc)
        .count();
    assert!(
        b_audio >= 24,
        "share audio reached the subscriber: {b_audio}/25"
    );
    assert!(
        !c.audio_in.lock().unwrap().contains(&c_audio),
        "no share audio for a member that did not subscribe"
    );

    // A viewer's keyframe request reaches the sharer.
    while a.calls.try_recv().is_ok() {}
    b.driver.request_keyframe(ssrc).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut asked = false;
    while Instant::now() < deadline && !asked {
        asked = matches!(
            a.calls.recv_timeout(Duration::from_millis(100)),
            Ok(CallEvent::KeyframeRequested)
        );
    }
    assert!(asked, "PLI from the viewer reached the sharer");

    // Stopping the share tells the others and stops the forwarding.
    assert!(sfu.share("a", None));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let ToClient::Screen(p, None) = c.sfu_events.recv_timeout(left).unwrap() {
            assert_eq!(p, "a");
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    b.received.lock().unwrap().clear();
    a.video
        .send(VideoOut {
            timestamp: 999_000,
            data: access_unit(true, 1000, 1),
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.received.lock().unwrap().is_empty(), "nothing after stop");
    let stats = sfu.stats();
    println!("SFU {stats:?}");
    // The subscribe came before any video (nothing to ask yet); the PLI is counted.
    assert!(stats.keyframe_requests >= 1, "PLI passed on");
}

#[test]
fn losses_are_repaired_by_retransmission() {
    let sfu = sfu();
    // 3 % of the sharer's and of the viewer's datagrams are lost (RTP, RTCP, NACKs).
    let a = member(&sfu, "a", 0.03);
    let b = member(&sfu, "b", 0.03);
    let (ssrc, rtx, _) = screen_track_of(&b, "a");
    b.driver.expect_video_source(ssrc, rtx).unwrap();
    assert!(sfu.share(
        "a",
        Some(ShareInfo {
            width: 1920,
            height: 1080,
            fps: 60
        })
    ));
    assert!(sfu.subscribe("b", Some("a")));
    std::thread::sleep(Duration::from_millis(200));
    let run = share(&a, &b, ssrc, 180);
    // Every 150 KB keyframe is ≈ 140 packets: without repair most would lose a packet,
    // and every frame after it would be dropped (37 % delivered before pacing, since
    // str0m only repairs within 100 sequence numbers of the newest packet).
    println!("sharer {:?}", a.driver.stats());
    println!("viewer {:?}", b.driver.stats());
    println!("SFU {:?}", sfu.stats());
    check_frames(&run, 0.85);
}
