//! Two desktop sessions (the code the GUI runs, without the window) talk through a real
//! `app-server` over WebSocket signaling and real UDP media (ICE + DTLS-SRTP), with
//! simulated audio devices and 5 % loss on one direction (spec S1 acceptance, headless
//! part). Covers: join → call, continuity under loss, mute, leave releases devices and
//! threads, the remaining member notices at once (PeerLeft), rejoin calls again, and
//! a signaling outage ends the call and reconnects.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use turbo_pancake_desktop::session::{Command, ServerStatus, Session, SessionConfig, Snapshot};
use turbo_pancake_media_audio::DspConfig;
use turbo_pancake_media_audio::sim::{SimDevice, SimIo, SimState};
use turbo_pancake_media_transport::call::CallState;

struct Client {
    session: Option<Session>,
    updates: Receiver<Snapshot>,
    last: Snapshot,
    state: Arc<SimState>,
}

impl Client {
    fn start(server: SocketAddr, name: &str, mic_on: bool, send_loss: f32) -> Self {
        let io = SimIo::new(
            vec![SimDevice::new("mic", 48_000)],
            vec![SimDevice::new("spk", 48_000)],
        );
        let state = io.state();
        state.muted_inputs.store(!mic_on, Ordering::Relaxed);
        let mut cfg = SessionConfig::new(format!("ws://{server}/v1/signal"), name);
        // The simulated mic is a stationary tone, which noise suppression removes.
        cfg.engine.dsp = DspConfig {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        };
        cfg.send_loss = send_loss;
        cfg.status_interval = Duration::from_millis(50);
        // Each engine start gets the same simulated devices (shared state).
        let devices = (
            vec![SimDevice::new("mic", 48_000)],
            vec![SimDevice::new("spk", 48_000)],
        );
        let shared = state.clone();
        let mut first = Some(io);
        let (tx, updates) = channel();
        let session = Session::start(
            cfg,
            move || {
                first.take().unwrap_or_else(|| {
                    SimIo::with_state(devices.0.clone(), devices.1.clone(), shared.clone())
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

    fn released(&self) -> bool {
        self.state.open_inputs.load(Ordering::Relaxed) == 0
            && self.state.open_outputs.load(Ordering::Relaxed) == 0
            && self.state.threads.load(Ordering::Relaxed) == 0
    }
}

fn played(s: &Snapshot) -> u64 {
    s.engine
        .as_ref()
        .and_then(|e| e.peers.first())
        .map_or(0, |p| p.stats.jitter.played)
}

fn in_call(s: &Snapshot) -> bool {
    s.remote
        .as_ref()
        .is_some_and(|r| r.state == CallState::Connected)
}

/// Starts an app-server on its own runtime thread; dropping the sender stops it.
fn server(addr: Option<SocketAddr>) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
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
            turbo_pancake_app_server::serve(listener, Default::default(), async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });
        // Dropping the runtime aborts the remaining connection tasks.
    });
    (addr_rx.recv().unwrap(), stop_tx)
}

#[test]
fn two_sessions_call_through_the_server() {
    let (addr, _server) = server(None);
    // A talks (440 Hz tone) and loses 5 % of what it sends; B is silent.
    let mut a = Client::start(addr, "Алиса", true, 0.05);
    let mut b = Client::start(addr, "Боб", false, 0.0);
    a.wait("a online", 10, |s| s.server == ServerStatus::Connected);
    b.wait("b online", 10, |s| s.server == ServerStatus::Connected);

    // B waits in the room, A joins and calls.
    b.send(Command::Join("s1-room".into()));
    b.wait("b joined", 10, |s| s.joined && s.engine.is_some());
    a.send(Command::Join("s1-room".into()));
    a.wait("a in call", 15, in_call);
    b.wait("b in call", 15, in_call);
    assert_eq!(b.last.remote.as_ref().unwrap().name, "Алиса");
    assert_eq!(a.last.remote.as_ref().unwrap().name, "Боб");

    // 4 s of speech at B.
    b.wait("4 s played at b", 20, |s| played(s) >= 200);
    let rx = b.last.engine.as_ref().unwrap().peers[0].stats;
    // A's last snapshot is from when it connected; take a current one.
    a.wait("a sent audio", 5, |s| {
        s.call.as_ref().is_some_and(|c| c.packets_sent >= 200)
    });
    eprintln!("b receiver: {rx:?}\na call: {:?}", a.last.call);
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

    // A leaves: devices and threads released; B is told at once, not after 3 s silence.
    a.send(Command::Leave);
    let t = Instant::now();
    a.wait("a left", 10, |s| s.room.is_none() && s.engine.is_none());
    b.wait("b sees a leave", 10, |s| s.joined && s.remote.is_none());
    let noticed = t.elapsed();
    assert!(a.released(), "a still holds devices");
    assert!(
        noticed < Duration::from_secs(2),
        "b noticed after {noticed:?}"
    );

    // A comes back: a new call, B answers again (engine stayed up at B).
    a.send(Command::Join("s1-room".into()));
    a.wait("a in call again", 15, in_call);
    b.wait("b in call again", 15, |s| {
        in_call(s) && s.calls_started == 2
    });
    let before = b.played();
    b.wait("audio after rejoin", 10, |s| played(s) >= before + 50);
    assert!(b.state.recent_peak() > 0.2);

    // Quit both: everything released.
    a.session.take().unwrap().stop();
    b.wait("b alone", 10, |s| s.remote.is_none());
    b.session.take().unwrap().stop();
    assert!(a.released() && b.released());
}

#[test]
fn signaling_outage_ends_the_call_and_reconnects() {
    let (addr, server_stop) = server(None);
    let mut a = Client::start(addr, "a", true, 0.0);
    let mut b = Client::start(addr, "b", false, 0.0);
    b.send(Command::Join("outage".into()));
    b.wait("b joined", 10, |s| s.joined);
    a.send(Command::Join("outage".into()));
    a.wait("a in call", 15, in_call);
    b.wait("b in call", 15, in_call);

    // Server goes away: both see it, the call ends, the room is still wanted.
    drop(server_stop);
    a.wait("a offline", 10, |s| {
        matches!(s.server, ServerStatus::Disconnected(_)) && s.remote.is_none()
    });
    b.wait("b offline", 10, |s| {
        matches!(s.server, ServerStatus::Disconnected(_))
    });
    assert_eq!(a.last.room.as_deref(), Some("outage"));
    assert!(
        a.last.engine.is_some(),
        "engine kept while the room is wanted"
    );

    // Server back on the same port: both reconnect, rejoin and call again.
    let (_addr, _server) = server(Some(addr));
    a.wait("a back in call", 30, |s| {
        s.server == ServerStatus::Connected && in_call(s)
    });
    b.wait("b back in call", 30, in_call);
    a.session.take().unwrap().stop();
    b.session.take().unwrap().stop();
    assert!(a.released() && b.released());
}
