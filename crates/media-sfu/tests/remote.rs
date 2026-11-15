//! The SFU in a worker process (`remote`): voice crosses the process boundary, and when
//! the worker dies the server side survives — the members are told, a new worker starts
//! and new media sessions work.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use turbo_pancake_media_sfu::{Link, PeerSink, RemoteSfu, ShareInfo, Track, TrackKind};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role,
};

enum ToClient {
    Answer(String),
    Track(u32),
    Closed,
}

struct Sink {
    events: SyncSender<ToClient>,
    datagrams: SyncSender<Vec<u8>>,
}

impl PeerSink for Sink {
    fn answer(&mut self, result: Result<String, String>) {
        let _ = self.events.send(ToClient::Answer(result.expect("answer")));
    }
    fn datagram(&mut self, data: Vec<u8>) {
        let _ = self.datagrams.try_send(data);
    }
    fn track(&mut self, _peer: &str, track: Track, published: bool) {
        if published && track.kind == TrackKind::Audio {
            let _ = self.events.send(ToClient::Track(track.ssrc));
        }
    }
    fn screen(&mut self, _peer: &str, _share: Option<ShareInfo>) {}
    fn closed(&mut self) {
        let _ = self.events.send(ToClient::Closed);
    }
}

struct Member {
    driver: CallDriver,
    audio: SyncSender<AudioOut>,
    received: Arc<AtomicU64>,
    events: Receiver<ToClient>,
    calls: Receiver<CallEvent>,
}

fn worker() -> RemoteSfu {
    RemoteSfu::start(|| {
        Command::new(env!("CARGO_BIN_EXE_sfu-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
    .expect("media worker")
}

/// Connects one tunnel member through the worker and waits for the answer.
fn member(sfu: &Arc<RemoteSfu>, name: &str) -> Member {
    let (to_client, inbound) = sync_channel::<Vec<u8>>(4096);
    let (events_tx, events) = sync_channel::<ToClient>(64);
    let (audio, outgoing) = sync_channel::<AudioOut>(256);
    let received = Arc::new(AtomicU64::new(0));
    let counter = received.clone();
    let (s, peer) = (sfu.clone(), name.to_owned());
    let link = MediaLink::Tunnel {
        inbound,
        outbound: Box::new(move |d| {
            s.datagram(&peer, d);
        }),
    };
    let config = CallConfig::new(Role::Offerer, "127.0.0.1:0".parse().unwrap());
    let (driver, calls) = CallDriver::start_with(config, link, outgoing, move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let CallEvent::Signal(LocalSignal::Offer(offer)) =
        calls.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("expected an offer")
    };
    let sink = Sink {
        events: events_tx,
        datagrams: to_client,
    };
    assert!(sfu.join(name, "room", offer, Link::Tunnel, Box::new(sink)));
    let ToClient::Answer(answer) = events.recv_timeout(Duration::from_secs(10)).unwrap() else {
        panic!("expected the answer first")
    };
    driver.remote_answer(answer).unwrap();
    Member {
        driver,
        audio,
        received,
        events,
        calls,
    }
}

fn wait_connected(m: &Member) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match m.calls.recv_timeout(left).expect("call did not connect") {
            CallEvent::State(CallState::Connected) => return,
            CallEvent::State(CallState::Failed(e)) => panic!("call failed: {e}"),
            _ => {}
        }
    }
}

/// `a` sends 50 voice frames; returns how many `b` received.
fn talk(a: &Member, b: &Member) -> u64 {
    let ssrc = match b.events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ToClient::Track(s) => s,
        ToClient::Answer(_) => panic!("second answer"),
        ToClient::Closed => panic!("closed"),
    };
    b.driver.expect_source(ssrc).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let before = b.received.load(Ordering::Relaxed);
    for seq in 0..50u16 {
        a.audio
            .try_send(AudioOut {
                seq,
                timestamp: u32::from(seq) * 960,
                payload: vec![0xF8; 80],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.received.load(Ordering::Relaxed) - before < 50 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    b.received.load(Ordering::Relaxed) - before
}

#[test]
fn voice_crosses_the_process_boundary_and_a_dead_worker_is_replaced() {
    turbo_pancake_media_transport::init_crypto();
    let sfu = Arc::new(worker());
    assert_eq!(sfu.worker_starts(), 1);
    let a = member(&sfu, "a");
    let b = member(&sfu, "b");
    wait_connected(&a);
    wait_connected(&b);
    assert_eq!(talk(&a, &b), 50, "every frame forwarded by the worker");
    // Statistics come back over the pipe (published every 500 ms).
    std::thread::sleep(Duration::from_millis(700));
    let st = sfu.stats();
    assert_eq!((st.peers, st.connected_peers), (2, 2), "{st:?}");
    assert!(st.rtp_in >= 50, "{st:?}");

    // The media service dies. This process — the server's side — goes on.
    sfu.kill_worker();
    for m in [&a, &b] {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match m.events.recv_timeout(left).expect("member was not told") {
                ToClient::Closed => break,
                _ => continue,
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while sfu.worker_starts() < 2 || sfu.stats().peers != 0 {
        assert!(Instant::now() < deadline, "no new worker");
        std::thread::sleep(Duration::from_millis(50));
    }
    a.driver.stop();
    b.driver.stop();

    // New media sessions work on the new worker (a join may race its start-up).
    let deadline = Instant::now() + Duration::from_secs(15);
    let (c, d) = loop {
        std::thread::sleep(Duration::from_millis(200));
        let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (member(&sfu, "c"), member(&sfu, "d"))
        }));
        match ready {
            Ok(pair) => break pair,
            Err(_) => assert!(Instant::now() < deadline, "the new worker takes no members"),
        }
    };
    wait_connected(&c);
    wait_connected(&d);
    assert_eq!(talk(&c, &d), 50, "every frame forwarded by the new worker");
    c.driver.stop();
    d.driver.stop();
    sfu.stop();
}
