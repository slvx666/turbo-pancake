//! A member whose packets reach the SFU in bursts loses none of them.
//!
//! str0m in RTP mode keeps one received packet until `poll_output`: the SFU must poll
//! after every datagram it hands in. Found on the macOS CI runner, where clients send in
//! bursts (≈ 35 % of RTP was overwritten before forwarding). Over the tunnel the burst is
//! deterministic: all datagrams of one client loop iteration sit in the SFU's command
//! queue and are handed in one after another.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use turbo_pancake_media_sfu::{Link, PeerSink, Sfu, SfuConfig, ShareInfo, Track, TrackKind};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role,
};

enum ToClient {
    Answer(String),
    Track(u32),
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
}

struct Member {
    driver: CallDriver,
    audio: SyncSender<AudioOut>,
    received: Arc<AtomicU64>,
}

/// Connects one tunnel member to the SFU and waits until its session is up.
fn member(sfu: &Arc<Sfu>, name: &str) -> (Member, Receiver<ToClient>, Receiver<CallEvent>) {
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
    let (driver, call_events) = CallDriver::start_with(config, link, outgoing, move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let CallEvent::Signal(LocalSignal::Offer(offer)) =
        call_events.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("expected an offer")
    };
    let sink = Sink {
        events: events_tx,
        datagrams: to_client,
    };
    assert!(sfu.join(name, "burst", offer, Link::Tunnel, Box::new(sink)));
    let ToClient::Answer(answer) = events.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected the answer first")
    };
    driver.remote_answer(answer).unwrap();
    (
        Member {
            driver,
            audio,
            received,
        },
        events,
        call_events,
    )
}

fn wait_connected(events: &Receiver<CallEvent>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left).expect("call did not connect") {
            CallEvent::State(CallState::Connected) => return,
            CallEvent::State(CallState::Failed(e)) => panic!("call failed: {e}"),
            _ => {}
        }
    }
}

#[test]
fn bursts_of_packets_are_all_forwarded() {
    turbo_pancake_media_transport::init_crypto();
    let sfu = Arc::new(Sfu::start(SfuConfig::default()).unwrap());
    let (a, _a_events, a_calls) = member(&sfu, "a");
    let (b, b_events, b_calls) = member(&sfu, "b");
    wait_connected(&a_calls);
    wait_connected(&b_calls);
    // B learns A's stream (sent when B joined, after A).
    let ssrc = match b_events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ToClient::Track(s) => s,
        ToClient::Answer(_) => panic!("second answer"),
    };
    b.driver.expect_source(ssrc).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // 20 bursts of 5 frames, one burst every 100 ms (real time stays 20 ms per frame).
    const BURSTS: u16 = 20;
    const PER_BURST: u16 = 5;
    let mut sent = 0u64;
    for burst in 0..BURSTS {
        for i in 0..PER_BURST {
            let seq = burst * PER_BURST + i;
            a.audio
                .try_send(AudioOut {
                    seq,
                    timestamp: u32::from(seq) * 960,
                    payload: vec![0xF8; 80],
                })
                .unwrap();
            sent += 1;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.received.load(Ordering::Relaxed) < sent && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let got = b.received.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(600)); // stats are published every 500 ms
    let st = sfu.stats();
    assert_eq!(got, sent, "B received {got} of {sent}; sfu {st:?}");
    assert_eq!(st.rtp_in, sent, "{st:?}");
    a.driver.stop();
    b.driver.stop();
    sfu.stop();
}
