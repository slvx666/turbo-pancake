//! Screen share through the SFU over real UDP (S3 step 4): a member shares H.264 access
//! units at 60 fps, another subscribes and reassembles them. Only subscribers get the
//! video; losses are repaired by NACK/RTX on both legs; a viewer's keyframe request
//! reaches the sharer.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use turbo_pancake_media_sfu::{
    Layer, LayerChoice, LayerReason, Link, PeerSink, Sfu, SfuConfig, ShareInfo, Subscription,
    Track, TrackKind,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role, VideoIo,
};
use turbo_pancake_media_transport::video::{AssembledFrame, FrameAssembler, VideoIn, VideoOut};

enum ToClient {
    Answer(String),
    Track(String, Track, bool),
    Screen(String, Option<ShareInfo>),
    Subscription(Subscription),
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
    fn subscription(&mut self, sub: &Subscription) {
        let _ = self.events.send(ToClient::Subscription(sub.clone()));
    }
}

struct Member {
    driver: CallDriver,
    _audio: SyncSender<AudioOut>,
    video: SyncSender<VideoOut>,
    preview: SyncSender<VideoOut>,
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
    let (preview, preview_out) = sync_channel::<VideoOut>(8);
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
        preview: preview_out,
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
        preview,
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
                TrackKind::Audio | TrackKind::ScreenPreview => {}
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
        ..Default::default()
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
        ..Default::default()
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
            fps: 60,
            ..Default::default()
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

/// Every track `viewer` was told about for `peer`, once all four kinds are known.
fn tracks_of(viewer: &Member, peer: &str) -> std::collections::HashMap<&'static str, Track> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = std::collections::HashMap::new();
    while out.len() < 4 {
        let left = deadline.saturating_duration_since(Instant::now());
        if let ToClient::Track(p, t, true) = viewer.sfu_events.recv_timeout(left).unwrap()
            && p == peer
        {
            let k = match t.kind {
                TrackKind::Audio => "audio",
                TrackKind::Screen => "screen",
                TrackKind::ScreenAudio => "screen_audio",
                TrackKind::ScreenPreview => "preview",
            };
            out.insert(k, t);
        }
    }
    out
}

/// The next subscription update `m` gets (other events are skipped).
fn next_subscription(m: &Member, within: Duration) -> Option<Subscription> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match m.sfu_events.recv_timeout(left) {
            Ok(ToClient::Subscription(s)) => return Some(s),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

const HIGH_LEN: usize = 3000;
const LOW_LEN: usize = 600;

/// What one viewer assembled from its main stream and from previews.
#[derive(Default)]
struct Seen {
    main: FrameAssembler,
    main_frames: Vec<AssembledFrame>,
    preview: FrameAssembler,
    preview_frames: Vec<AssembledFrame>,
}

type ViewerSlot<'a> = (&'a Member, u32, u32, &'a mut Seen);

/// A shares both layers (high 30 fps, low every third frame) for `dur`, answering
/// keyframe requests per layer; every viewer assembles its main stream (`main` SSRC) and
/// the preview stream (`preview` SSRC).
fn pump(a: &Member, viewers: &mut [ViewerSlot<'_>], n0: &mut u32, dur: Duration) {
    let end = Instant::now() + dur;
    let (mut key_high, mut key_low) = (true, true);
    let mut next = Instant::now();
    while Instant::now() < end {
        while let Ok(ev) = a.calls.try_recv() {
            match ev {
                CallEvent::KeyframeRequested => key_high = true,
                CallEvent::PreviewKeyframeRequested => key_low = true,
                _ => {}
            }
        }
        if Instant::now() >= next {
            let n = *n0;
            *n0 += 1;
            let ts = n * 3000;
            let kh = key_high || n.is_multiple_of(60);
            key_high = false;
            let _ = a.video.try_send(VideoOut {
                timestamp: ts,
                data: access_unit(kh, if kh { 20_000 } else { HIGH_LEN }, n as u8),
            });
            if n.is_multiple_of(3) {
                let kl = key_low || n.is_multiple_of(60);
                key_low = false;
                let _ = a.preview.try_send(VideoOut {
                    timestamp: ts,
                    data: access_unit(kl, if kl { 2_000 } else { LOW_LEN }, n as u8),
                });
            }
            next += Duration::from_millis(33);
        }
        for (m, main, preview, seen) in viewers.iter_mut() {
            let packets: Vec<VideoIn> = std::mem::take(&mut *m.received.lock().unwrap());
            for p in packets {
                if p.preview && p.ssrc == *preview {
                    seen.preview_frames.extend(seen.preview.push(p));
                } else if !p.preview && p.ssrc == *main {
                    seen.main_frames.extend(seen.main.push(p));
                }
            }
            let now = Instant::now();
            seen.main_frames.extend(seen.main.poll(now));
            seen.preview_frames.extend(seen.preview.poll(now));
            if seen.main.take_keyframe_request() {
                m.driver.request_keyframe(*main).ok();
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Main-stream P-frames by layer (sizes tell them apart; keyframes are skipped).
fn layers(frames: &[AssembledFrame]) -> (usize, usize) {
    let high = frames
        .iter()
        .filter(|f| !f.keyframe && f.data.len() > HIGH_LEN)
        .count();
    let low = frames
        .iter()
        .filter(|f| !f.keyframe && f.data.len() < HIGH_LEN && f.data.len() > LOW_LEN)
        .count();
    (high, low)
}

#[test]
fn layers_switch_previews_flow_and_the_budget_admits() {
    turbo_pancake_media_transport::init_crypto();
    // Room for one 4 Mbit/s high stream.
    let sfu = Sfu::start(SfuConfig {
        udp_bind: Some("127.0.0.1:0".parse().unwrap()),
        max_high_egress_kbps: Some(5000),
    })
    .unwrap();
    let a = member(&sfu, "a", 0.0);
    let b = member(&sfu, "b", 0.0);
    let c = member(&sfu, "c", 0.0);
    let d = member(&sfu, "d", 0.0);
    let mut ids = Vec::new();
    for m in [&b, &c, &d] {
        let t = tracks_of(m, "a");
        let (s, p) = (t["screen"], t["preview"]);
        m.driver.expect_video_source(s.ssrc, s.rtx).unwrap();
        m.driver.expect_preview_source(p.ssrc, p.rtx).unwrap();
        ids.push((s.ssrc, p.ssrc));
    }
    let info = ShareInfo {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 4000,
        preview: true,
    };
    assert!(sfu.share("a", Some(info)));
    assert!(sfu.subscribe_layer("b", Some("a"), LayerChoice::Auto));
    let sb = next_subscription(&b, Duration::from_secs(3)).expect("b told");
    assert_eq!((sb.layer, sb.reason), (Layer::High, LayerReason::Requested));
    assert!(sfu.subscribe_layer("c", Some("a"), LayerChoice::Auto));
    let sc = next_subscription(&c, Duration::from_secs(3)).expect("c told");
    assert_eq!(
        (sc.layer, sc.reason),
        (Layer::Low, LayerReason::Admission),
        "the budget holds one high stream"
    );
    assert!(sfu.previews("d", vec!["a".into()]));

    let (mut seen_b, mut seen_c, mut seen_d) = (Seen::default(), Seen::default(), Seen::default());
    let mut n = 0;
    {
        let mut v = [
            (&b, ids[0].0, ids[0].1, &mut seen_b),
            (&c, ids[1].0, ids[1].1, &mut seen_c),
            (&d, ids[2].0, ids[2].1, &mut seen_d),
        ];
        pump(&a, &mut v, &mut n, Duration::from_millis(2500));
    }
    let (bh, bl) = layers(&seen_b.main_frames);
    let (ch, cl) = layers(&seen_c.main_frames);
    println!(
        "TURBO_PANCAKE_LAYERS phase1 b high {bh} low {bl}; c high {ch} low {cl}; d previews {}",
        seen_d.preview_frames.len()
    );
    assert!(bh >= 50 && bl == 0, "b gets the high layer: {bh}/{bl}");
    assert!(cl >= 15 && ch == 0, "c gets the low layer: {ch}/{cl}");
    assert!(seen_d.preview_frames.len() >= 15, "d gets the preview");
    assert!(
        seen_d.main_frames.is_empty(),
        "no main stream for a preview-only member"
    );
    assert!(
        seen_b.preview_frames.is_empty() && seen_c.preview_frames.is_empty(),
        "no previews without a preview subscription"
    );

    // B asks for the low layer: the budget frees and C (auto) moves up.
    assert!(sfu.subscribe_layer("b", Some("a"), LayerChoice::Low));
    let sb = next_subscription(&b, Duration::from_secs(3)).expect("b told");
    assert_eq!((sb.layer, sb.reason), (Layer::Low, LayerReason::Requested));
    let sc = next_subscription(&c, Duration::from_secs(3)).expect("c told");
    assert_eq!((sc.layer, sc.reason), (Layer::High, LayerReason::Requested));
    let (b_before, c_before) = (seen_b.main_frames.len(), seen_c.main_frames.len());
    let (b_dropped, c_dropped) = (
        seen_b.main.stats.frames_dropped,
        seen_c.main.stats.frames_dropped,
    );
    {
        let mut v = [
            (&b, ids[0].0, ids[0].1, &mut seen_b),
            (&c, ids[1].0, ids[1].1, &mut seen_c),
        ];
        pump(&a, &mut v, &mut n, Duration::from_millis(2500));
    }
    let (bh2, bl2) = layers(&seen_b.main_frames[b_before..]);
    let (ch2, cl2) = layers(&seen_c.main_frames[c_before..]);
    let (bd, cd) = (
        seen_b.main.stats.frames_dropped - b_dropped,
        seen_c.main.stats.frames_dropped - c_dropped,
    );
    println!(
        "TURBO_PANCAKE_LAYERS phase2 b high {bh2} low {bl2}; c high {ch2} low {cl2}; dropped b {bd} c {cd}; SFU {:?}",
        sfu.stats()
    );
    // At most the frames before the switch keyframe are of the old layer.
    assert!(bl2 >= 15 && bh2 <= 15, "b switched down: {bh2}/{bl2}");
    assert!(ch2 >= 40 && cl2 <= 10, "c switched up: {ch2}/{cl2}");
    assert!(
        bd <= 1 && cd <= 1,
        "a switch continues the stream (no loss, no wait for a keyframe)"
    );
    let st = sfu.stats();
    assert!(st.layer_switches >= 2, "{st:?}");
    assert_eq!((st.viewers_high, st.viewers_low), (1, 1));

    // C's downlink loses 30 %: auto moves it to the low layer (receiver reports).
    c.driver.set_test_loss(0.0, 0.3).unwrap();
    let start = Instant::now();
    let mut got = None;
    {
        let mut v = [(&c, ids[1].0, ids[1].1, &mut seen_c)];
        while start.elapsed() < Duration::from_secs(8) && got.is_none() {
            pump(&a, &mut v, &mut n, Duration::from_millis(300));
            while let Ok(ev) = v[0].0.sfu_events.try_recv() {
                if let ToClient::Subscription(s) = ev {
                    got = Some(s);
                }
            }
        }
    }
    let s = got.expect("c moved by its network");
    println!(
        "TURBO_PANCAKE_LAYERS network: {:?} after {:.1} s",
        s,
        start.elapsed().as_secs_f64()
    );
    assert_eq!((s.layer, s.reason), (Layer::Low, LayerReason::Network));
}
