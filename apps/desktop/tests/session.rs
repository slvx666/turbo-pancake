//! Two desktop sessions (the code the GUI runs, without the window) talk through a real
//! `app-server` with its SFU: WebSocket signaling, media over real UDP to the server or
//! tunneled through the WebSocket (ICE + DTLS-SRTP either way), simulated audio devices.
//! Covers (spec S1 acceptance, headless part): join → call with 5 % loss, mute, leave
//! releases devices and threads and the other member sees it at once, rejoin; a VPN that
//! carries no UDP (every UDP datagram dropped) → automatic fallback to the WebSocket; a
//! server without UDP → WebSocket at once; a signaling outage ends media and reconnects;
//! three members each hear both others as separate streams (S2, ADR 0008).
//!
//! The tests run one at a time ([`serial`]): each client is a real-time audio pipeline
//! paced by the wall clock, and several debug builds of them in parallel starve each
//! other (seen as concealed frames), which is not what these tests measure.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use turbo_pancake_app_server::Config;
use turbo_pancake_desktop::session::{
    Command, MediaTransport, Member, ServerStatus, Session, SessionConfig, Snapshot, TransportPref,
};
use turbo_pancake_media_audio::DspConfig;
use turbo_pancake_media_audio::sim::{SimDevice, SimIo, SimState};

struct Client {
    session: Option<Session>,
    updates: Receiver<Snapshot>,
    last: Snapshot,
    state: Arc<SimState>,
}

struct Opts {
    mic_on: bool,
    send_loss: f32,
    transport: TransportPref,
    block_udp: bool,
    voice_gate: Option<f32>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mic_on: false,
            send_loss: 0.0,
            transport: TransportPref::Auto,
            block_udp: false,
            voice_gate: None,
        }
    }
}

impl Client {
    fn start(server: SocketAddr, name: &str, o: Opts) -> Self {
        let devices = || {
            (
                vec![SimDevice::new("mic", 48_000)],
                vec![SimDevice::new("spk", 48_000)],
            )
        };
        let (ins, outs) = devices();
        let io = SimIo::new(ins, outs);
        let state = io.state();
        state.muted_inputs.store(!o.mic_on, Ordering::Relaxed);
        let mut cfg = SessionConfig::new(format!("ws://{server}/v1/signal"), name);
        // The simulated mic is a stationary tone, which noise suppression removes.
        cfg.engine.dsp = DspConfig {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        };
        cfg.send_loss = o.send_loss;
        cfg.transport = o.transport;
        cfg.block_udp = o.block_udp;
        cfg.engine.voice_gate = o.voice_gate;
        cfg.udp_attempt_timeout = Duration::from_secs(2);
        cfg.status_interval = Duration::from_millis(50);
        // Each engine start gets the same simulated devices (shared state).
        let shared = state.clone();
        let mut first = Some(io);
        let (tx, updates) = channel();
        let session = Session::start(
            cfg,
            move || {
                first.take().unwrap_or_else(|| {
                    let (ins, outs) = devices();
                    SimIo::with_state(ins, outs, shared.clone())
                })
            },
            move |s: &Snapshot| {
                let _ = tx.send(s.clone());
            },
        )
        .unwrap();
        Self {
            session: Some(session),
            updates,
            last: Snapshot::default(),
            state,
        }
    }

    fn send(&self, cmd: Command) {
        assert!(self.session.as_ref().unwrap().send(cmd));
    }

    fn wait(&mut self, what: &str, secs: u64, done: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(&self.last) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out: {what}\n{:#?}", self.last);
            if let Ok(s) = self.updates.recv_timeout(left) {
                self.last = s;
            }
        }
    }

    fn played(&self) -> u64 {
        played(&self.last)
    }

    fn transport(&self) -> Option<MediaTransport> {
        self.last.media.as_ref().map(|m| m.transport)
    }

    fn released(&self) -> bool {
        self.state.open_inputs.load(Ordering::Relaxed) == 0
            && self.state.open_outputs.load(Ordering::Relaxed) == 0
            && self.state.threads.load(Ordering::Relaxed) == 0
    }

    fn quit(&mut self) {
        self.session.take().unwrap().stop();
    }
}

fn played(s: &Snapshot) -> u64 {
    s.engine
        .as_ref()
        .and_then(|e| e.peers.first())
        .map_or(0, |p| p.stats.jitter.played)
}

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts an app-server (with its SFU) on its own runtime thread; dropping the sender
/// stops it.
fn server(
    addr: Option<SocketAddr>,
    config: Config,
) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr_tx, addr_rx) = channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let bind = addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
            let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            turbo_pancake_app_server::serve(listener, config, async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });
        // Dropping the runtime aborts the remaining connection tasks.
    });
    (addr_rx.recv().unwrap(), stop_tx)
}

/// B waits in `room`, A joins; both end up in a call.
fn pair(addr: SocketAddr, room: &str, a: Opts, b: Opts) -> (Client, Client) {
    let mut a = Client::start(addr, "Алиса", a);
    let mut b = Client::start(addr, "Боб", b);
    b.send(Command::Join(room.into()));
    b.wait("b joined", 10, |s| s.joined && s.engine.is_some());
    a.send(Command::Join(room.into()));
    a.wait("a in call", 20, Snapshot::in_call);
    b.wait("b in call", 20, Snapshot::in_call);
    (a, b)
}

fn talking(o: Opts) -> Opts {
    Opts { mic_on: true, ..o }
}

#[test]
fn two_sessions_call_through_the_server() {
    let _serial = serial();
    let (addr, _server) = server(None, Config::default());
    // A talks (440 Hz tone) and loses 5 % of what it sends; B is silent.
    let (mut a, mut b) = pair(
        addr,
        "s1-room",
        talking(Opts {
            send_loss: 0.05,
            ..Opts::default()
        }),
        Opts::default(),
    );
    assert_eq!(b.last.member_names(), vec!["Алиса".to_string()]);
    assert_eq!(a.last.member_names(), vec!["Боб".to_string()]);
    assert_eq!(a.transport(), Some(MediaTransport::Udp));
    assert_eq!(b.transport(), Some(MediaTransport::Udp));

    // 4 s of speech at B, relayed by the server.
    b.wait("4 s played at b", 20, |s| played(s) >= 200);
    let rx = b.last.engine.as_ref().unwrap().peers[0].stats;
    a.wait("a sent audio", 5, |s| {
        s.call.as_ref().is_some_and(|c| c.packets_sent >= 200)
    });
    eprintln!("b receiver: {rx:?}\na media: {:?}", a.last.call);
    assert_eq!(rx.decode_errors, 0);
    assert!(rx.jitter.late <= 3, "{rx:?}");
    assert!(rx.concealed_ratio() < 0.03, "{rx:?}");
    assert!(
        b.state.recent_peak() > 0.2,
        "b speaker {}",
        b.state.recent_peak()
    );
    assert!(
        a.state.recent_peak() < 0.01,
        "a speaker {}",
        a.state.recent_peak()
    );
    assert!(a.last.call.as_ref().unwrap().impaired_drops > 0);

    // Mute at A: B hears silence (encoded silence keeps the stream going).
    a.send(Command::SetMuted(true));
    let t = Instant::now();
    while b.state.recent_peak() > 0.01 {
        assert!(t.elapsed() < Duration::from_secs(5), "mute not heard");
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = b.played();
    b.wait("stream continues while muted", 10, |s| {
        played(s) >= before + 25
    });
    a.send(Command::SetMuted(false));

    // A leaves: devices and threads released; B is told at once and stays connected.
    a.send(Command::Leave);
    let t = Instant::now();
    a.wait("a left", 10, |s| s.room.is_none() && s.engine.is_none());
    b.wait("b sees a leave", 10, |s| s.joined && s.members.is_empty());
    let noticed = t.elapsed();
    assert!(a.released(), "a still holds devices");
    assert!(
        noticed < Duration::from_secs(2),
        "b noticed after {noticed:?}"
    );

    // A comes back: a call again, audio reaches B.
    a.send(Command::Join("s1-room".into()));
    a.wait("a in call again", 20, Snapshot::in_call);
    b.wait("b in call again", 20, Snapshot::in_call);
    let before = b.played();
    b.wait("audio after rejoin", 10, |s| played(s) >= before + 50);
    assert!(b.state.recent_peak() > 0.2);

    // Quit both: everything released.
    a.quit();
    b.wait("b alone", 10, |s| s.members.is_empty());
    b.quit();
    assert!(a.released() && b.released());
}

#[test]
fn vpn_without_udp_falls_back_to_the_websocket() {
    let _serial = serial();
    let (addr, _server) = server(None, Config::default());
    // Every UDP datagram of both clients is lost: only the tunnel can work.
    let blocked = || Opts {
        block_udp: true,
        ..Opts::default()
    };
    let t = Instant::now();
    let (mut a, mut b) = pair(addr, "vpn", talking(blocked()), blocked());
    eprintln!("fallback connected after {:?}", t.elapsed());
    assert_eq!(a.transport(), Some(MediaTransport::Tunnel));
    assert_eq!(b.transport(), Some(MediaTransport::Tunnel));
    assert!(a.last.media_sessions >= 2, "UDP tried first");
    b.wait("3 s played at b over the tunnel", 20, |s| played(s) >= 150);
    let rx = b.last.engine.as_ref().unwrap().peers[0].stats;
    eprintln!("b receiver over websocket: {rx:?}");
    assert_eq!(rx.decode_errors, 0);
    assert!(rx.concealed_ratio() < 0.03, "{rx:?}");
    assert!(b.state.recent_peak() > 0.2);
    a.quit();
    b.quit();
    assert!(a.released() && b.released());
}

#[test]
fn server_without_udp_uses_the_websocket_at_once() {
    let _serial = serial();
    let config = Config {
        media_udp: false,
        ..Config::default()
    };
    let (addr, _server) = server(None, config);
    let t = Instant::now();
    let (mut a, mut b) = pair(addr, "no-udp", talking(Opts::default()), Opts::default());
    // The UDP offer is rejected right away, no UDP timeout to wait for.
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(a.transport(), Some(MediaTransport::Tunnel));
    b.wait("audio over the websocket", 20, |s| played(s) >= 100);
    assert!(b.state.recent_peak() > 0.2);
    a.quit();
    b.quit();
}

#[test]
fn signaling_outage_ends_media_and_reconnects() {
    let _serial = serial();
    let (addr, server_stop) = server(None, Config::default());
    let (mut a, mut b) = pair(addr, "outage", talking(Opts::default()), Opts::default());

    // Server goes away: both see it, media ends, the room is still wanted.
    drop(server_stop);
    a.wait("a offline", 10, |s| {
        matches!(s.server, ServerStatus::Disconnected(_)) && s.media.is_none()
    });
    b.wait("b offline", 10, |s| {
        matches!(s.server, ServerStatus::Disconnected(_))
    });
    assert_eq!(a.last.room.as_deref(), Some("outage"));
    assert!(
        a.last.engine.is_some(),
        "engine kept while the room is wanted"
    );

    // Server back on the same port: both reconnect, rejoin and talk again.
    let (_addr, _server) = server(Some(addr), Config::default());
    a.wait("a back in call", 30, |s| {
        s.server == ServerStatus::Connected && s.in_call()
    });
    b.wait("b back in call", 30, Snapshot::in_call);
    let before = b.played();
    b.wait("audio after reconnect", 10, |s| played(s) >= before + 50);
    // The stream of the session before the outage is gone, not a silent ghost peer.
    let peers = &b.last.engine.as_ref().unwrap().peers;
    assert_eq!(peers.len(), 1, "{peers:?}");
    a.quit();
    b.quit();
    assert!(a.released() && b.released());
}

/// Remote streams at this client that played at least `frames` frames of sound.
fn heard(s: &Snapshot, frames: u64) -> usize {
    s.engine.as_ref().map_or(0, |e| {
        e.peers
            .iter()
            .filter(|p| p.stats.jitter.played >= frames && p.stats.last_peak > 0.1)
            .count()
    })
}

#[test]
fn three_members_each_hear_both_others() {
    let _serial = serial();
    let (addr, _server) = server(None, Config::default());
    let names = ["Алиса", "Боб", "Вера"];
    let mut c: Vec<Client> = names
        .iter()
        .map(|n| Client::start(addr, n, talking(Opts::default())))
        .collect();
    for x in &mut c {
        x.send(Command::Join("trio".into()));
        x.wait("joined", 10, |s| s.joined && s.engine.is_some());
    }
    // Every member: two separate remote streams (one per other member), both audible.
    for x in &mut c {
        x.wait("both others heard", 20, |s| {
            s.members.len() == 2 && heard(s, 100) == 2
        });
        let e = x.last.engine.as_ref().unwrap();
        assert_eq!(e.peers.len(), 2, "{:?}", e.peers);
        for p in &e.peers {
            assert_eq!(p.stats.decode_errors, 0);
            assert!(p.stats.concealed_ratio() < 0.03, "{:?}", p.stats);
        }
    }

    // Вера leaves: the others keep hearing each other, her stream is gone at once.
    let mut v = c.pop().unwrap();
    v.send(Command::Leave);
    v.wait("left", 10, |s| s.engine.is_none());
    assert!(v.released());
    for x in &mut c {
        x.wait("one stream left", 5, |s| {
            s.members.len() == 1 && s.engine.as_ref().is_some_and(|e| e.peers.len() == 1)
        });
        let before = x.played();
        x.wait("still hears the other", 10, |s| played(s) >= before + 50);
    }

    // She comes back with a new session: everyone hears both others again.
    v.send(Command::Join("trio".into()));
    c.push(v);
    for x in &mut c {
        x.wait("both others heard again", 20, |s| {
            s.members.len() == 2
                && s.engine.as_ref().is_some_and(|e| e.peers.len() == 2)
                && heard(s, 50) == 2
        });
    }
    for mut x in c {
        x.quit();
        assert!(x.released());
    }
}

fn speaking(s: &Snapshot, name: &str) -> bool {
    s.members.iter().any(|m| m.name == name && m.speaking())
}

fn member<'a>(s: &'a Snapshot, name: &str) -> &'a Member {
    s.members.iter().find(|m| m.name == name).unwrap()
}

#[test]
fn speaking_indicators_member_volume_and_voice_activation() {
    let _serial = serial();
    let (addr, _server) = server(None, Config::default());
    // Алиса talks all the time, Боб talks with voice activation, Вера only listens.
    let mut a = Client::start(addr, "Алиса", talking(Opts::default()));
    let gated = Opts {
        voice_gate: Some(-40.0),
        ..Opts::default()
    };
    let mut b = Client::start(addr, "Боб", talking(gated));
    let mut v = Client::start(addr, "Вера", Opts::default());
    for x in [&mut a, &mut b, &mut v] {
        x.send(Command::Join("vad".into()));
        x.wait("joined", 10, |s| s.joined && s.engine.is_some());
    }
    v.wait("Вера sees both speak", 20, |s| {
        s.members.len() == 2 && speaking(s, "Алиса") && speaking(s, "Боб")
    });
    a.wait("Алиса sees Боб speak, Вера silent", 20, |s| {
        s.members.len() == 2
            && speaking(s, "Боб")
            && member(s, "Вера").stream.is_some()
            && !speaking(s, "Вера")
    });
    // Own indicators.
    assert!(a.last.engine.as_ref().unwrap().speaking);
    assert!(!v.last.engine.as_ref().unwrap().speaking);

    // Боб falls silent: voice activation stops his stream, the others see him quiet.
    b.state.muted_inputs.store(true, Ordering::Relaxed);
    b.wait("Боб gated", 10, |s| {
        s.engine
            .as_ref()
            .is_some_and(|e| !e.speaking && e.frames_gated > 25)
    });
    let e = b.last.engine.clone().unwrap();
    b.wait("Боб keeps gating", 10, |s| {
        s.engine
            .as_ref()
            .is_some_and(|x| x.frames_gated >= e.frames_gated + 25)
    });
    assert_eq!(
        b.last.engine.as_ref().unwrap().frames_sent,
        e.frames_sent,
        "nothing sent while silent"
    );
    v.wait("Боб quiet at Вера", 10, |s| {
        !speaking(s, "Боб") && speaking(s, "Алиса")
    });

    // He talks again: his next talkspurt plays at once, nothing discarded as late.
    let before = member(&v.last, "Боб")
        .stream
        .as_ref()
        .unwrap()
        .stats
        .jitter
        .played;
    b.state.muted_inputs.store(false, Ordering::Relaxed);
    v.wait("Боб speaks again", 10, |s| {
        speaking(s, "Боб")
            && member(s, "Боб")
                .stream
                .as_ref()
                .is_some_and(|p| p.stats.jitter.played >= before + 50)
    });
    let j = member(&v.last, "Боб").stream.as_ref().unwrap().stats.jitter;
    assert!(j.talkspurts >= 1, "{j:?}");
    assert!(j.late <= 5, "{j:?}");

    // Вера turns Алиса down to zero: silent for her, still shown speaking; Боб unchanged.
    let alice = member(&v.last, "Алиса").peer_id.clone();
    v.send(Command::SetMemberVolume(alice, 0.0));
    v.wait("Алиса muted locally", 10, |s| {
        let m = member(s, "Алиса");
        let p = m.stream.as_ref();
        m.volume == 0.0
            && m.speaking()
            && p.is_some_and(|p| p.volume == 0.0 && p.stats.last_peak < 0.01)
            && member(s, "Боб")
                .stream
                .as_ref()
                .is_some_and(|p| p.volume == 1.0 && p.stats.last_peak > 0.1)
    });
    // The call volume scales every member on top of their own volume.
    v.send(Command::SetRemoteVolume(0.5));
    v.wait("call volume", 10, |s| {
        member(s, "Боб")
            .stream
            .as_ref()
            .is_some_and(|p| p.volume == 0.5)
            && member(s, "Алиса")
                .stream
                .as_ref()
                .is_some_and(|p| p.volume == 0.0)
    });

    for mut x in [a, b, v] {
        x.quit();
        assert!(x.released());
    }
}

#[test]
fn joining_another_room_closes_the_previous_call() {
    let _serial = serial();
    let (addr, _server) = server(None, Config::default());
    let mut a = Client::start(addr, "Алиса", talking(Opts::default()));
    let mut b = Client::start(addr, "Боб", talking(Opts::default()));
    let mut c = Client::start(addr, "Вера", talking(Opts::default()));
    a.send(Command::Join("first".into()));
    b.send(Command::Join("first".into()));
    c.send(Command::Join("second".into()));
    for x in [&mut a, &mut b] {
        x.wait("in the first call", 20, |s| {
            s.in_call() && heard(s, 50) == 1
        });
    }
    c.wait("alone in the second room", 20, |s| {
        s.joined && s.members.is_empty() && s.media.is_some()
    });

    // Алиса takes the other call: the first one ends for her and for Боб at once.
    a.send(Command::Join("second".into()));
    b.wait("Боб alone, her stream gone", 5, |s| {
        s.joined && s.members.is_empty() && s.engine.as_ref().is_some_and(|e| e.peers.is_empty())
    });
    for x in [&mut a, &mut c] {
        x.wait("in the second call", 20, |s| {
            s.room.as_deref() == Some("second")
                && s.joined
                && s.member_names().len() == 1
                && s.engine.as_ref().is_some_and(|e| e.peers.len() == 1)
                && heard(s, 50) == 1
        });
    }
    assert_eq!(a.last.room.as_deref(), Some("second"));
    assert_eq!(a.last.member_names(), vec!["Вера".to_string()]);
    assert_eq!(c.last.member_names(), vec!["Алиса".to_string()]);

    // Joining the room she is already in changes nothing.
    let sessions = a.last.media_sessions;
    a.send(Command::Join("second".into()));
    let before = a.played();
    a.wait("still hears Вера", 10, |s| played(s) >= before + 50);
    assert_eq!(a.last.media_sessions, sessions);
    assert_eq!(c.last.member_names().len(), 1);

    for mut x in [a, b, c] {
        x.quit();
        assert!(x.released());
    }
}

#[test]
fn group_call_reconnects_after_a_server_outage() {
    let _serial = serial();
    let (addr, server_stop) = server(None, Config::default());
    let mut c: Vec<Client> = ["Алиса", "Боб", "Вера"]
        .iter()
        .map(|n| Client::start(addr, n, talking(Opts::default())))
        .collect();
    for x in &mut c {
        x.send(Command::Join("regroup".into()));
    }
    for x in &mut c {
        x.wait("everyone heard", 20, |s| heard(s, 50) == 2);
    }

    drop(server_stop);
    for x in &mut c {
        x.wait("offline", 10, |s| {
            matches!(s.server, ServerStatus::Disconnected(_)) && s.media.is_none()
        });
    }

    // Server back: all three rejoin and hear both others again, as fresh streams only.
    let (_addr, _server) = server(Some(addr), Config::default());
    for x in &mut c {
        x.wait("everyone heard again", 30, |s| {
            s.server == ServerStatus::Connected
                && s.members.len() == 2
                && s.engine.as_ref().is_some_and(|e| e.peers.len() == 2)
                && heard(s, 50) == 2
        });
    }
    for mut x in c {
        x.quit();
        assert!(x.released());
    }
}
