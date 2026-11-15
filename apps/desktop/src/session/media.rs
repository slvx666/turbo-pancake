//! The media session: the audio engine, the call (transport) with its events, the
//! members' streams and the voice encoder's adaptation.

use super::*;

/// Audio-class scheduling for a thread that carries voice (`media_audio::rt`).
fn realtime_thread() {
    let _ = turbo_pancake_media_audio::rt::boost_current_thread();
}

impl Core {
    /// Accepts a member's stream on the media session and gives it their volume.
    pub(super) fn add_source(&mut self, peer: &str, ssrc: u32) {
        if let Some(m) = &self.media {
            let _ = m.driver.expect_source(ssrc);
        }
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().insert(ssrc);
            let _ = a
                .engine
                .set_peer_volume(PeerId::from(ssrc), self.gain(peer));
        }
    }

    /// Stops accepting a stream and drops its jitter buffer.
    pub(super) fn drop_source(&mut self, ssrc: u32) {
        if let Some(m) = &self.media {
            let _ = m.driver.forget_source(ssrc);
        }
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().remove(&ssrc);
            let _ = a.engine.remove_peer(PeerId::from(ssrc));
        }
    }

    pub(super) fn call_event(&mut self, generation: u64, ev: CallEvent) {
        let Some(media) = self.media.as_mut().filter(|m| m.generation == generation) else {
            return; // from a media session that already ended
        };
        self.dirty = true;
        match ev {
            CallEvent::Signal(LocalSignal::Offer(sdp)) => {
                let (Some(room), transport) = (self.joined.clone(), media.transport) else {
                    return;
                };
                let transport = match transport {
                    MediaTransport::Udp => v1::MediaTransport::Udp,
                    MediaTransport::Tunnel => v1::MediaTransport::Websocket,
                };
                let dialed_ip = self.dialed_ip.map(|ip| ip.to_string()).unwrap_or_default();
                let rid = self.push(C::MediaOffer(v1::MediaOffer {
                    room_id: room,
                    sdp,
                    transport: transport as i32,
                    dialed_ip,
                }));
                if let Some(m) = &mut self.media {
                    m.offer_rid = rid;
                }
                // The server processes these after the offer: a new session learns our
                // share and our subscription again.
                if self.share.is_some() {
                    self.announce_share();
                }
                if self.watching.is_some() {
                    self.subscribe();
                }
                // A new session receives no previews until asked again.
                self.previewed.clear();
                self.sync_previews();
            }
            // The SFU is ICE-lite with its candidates in the answer: nothing to trickle.
            CallEvent::Signal(_) => {}
            CallEvent::KeyframeRequested => self.keyframe_requested(),
            CallEvent::PreviewKeyframeRequested => {
                if let Some(s) = &self.share {
                    s.share.request_preview_keyframe();
                }
            }
            CallEvent::State(state) => {
                media.state = state.clone();
                if let CallState::Failed(reason) = state {
                    self.media_failed(format!("медиа: {reason}"));
                } else if state == CallState::Connected {
                    self.snap.last_error = None;
                }
            }
        }
    }

    /// The server asked for a keyframe of our share (a new viewer, a loss).
    pub(super) fn keyframe_requested(&mut self) {
        if let Some(s) = &self.share {
            s.share.request_keyframe();
        }
    }

    /// The media session failed: switch transport (Auto) and start over while in the room.
    pub(super) fn media_failed(&mut self, reason: String) {
        let Some(transport) = self.media.as_ref().map(|m| m.transport) else {
            return;
        };
        self.snap.last_error = Some(reason);
        self.end_media();
        if self.cfg.transport == TransportPref::Auto {
            self.next_transport = match transport {
                MediaTransport::Udp => MediaTransport::Tunnel,
                MediaTransport::Tunnel => MediaTransport::Udp,
            };
        }
        self.start_media();
    }

    pub(super) fn start_audio(&mut self) {
        match (self.start_engine)(self.cfg.engine.clone()) {
            Ok((engine, link)) => {
                // A new engine starts with the base encoder settings.
                self.voice_adapter = LossAdapter::new(self.cfg.engine.encoder);
                self.listeners_loss = 0.0;
                let route: Arc<Mutex<Option<SyncSender<AudioOut>>>> = Arc::default();
                let pump_route = route.clone();
                let outgoing = link.outgoing;
                // Ends when the engine stops (its sender is dropped).
                let pump = std::thread::Builder::new()
                    .name("tp-audio-pump".into())
                    .spawn(move || {
                        realtime_thread();
                        while let Ok(f) = outgoing.recv() {
                            if let Some(tx) = pump_route.lock().unwrap().as_ref() {
                                // A full queue drops: the call thread is behind.
                                let _ = tx.try_send(AudioOut {
                                    seq: f.seq,
                                    timestamp: f.timestamp,
                                    payload: f.payload,
                                });
                            }
                        }
                    });
                match pump {
                    Ok(pump) => {
                        self.audio = Some(Audio {
                            engine,
                            sink: link.incoming,
                            accepted: Arc::default(),
                            route,
                            pump,
                        });
                    }
                    Err(e) => {
                        engine.stop();
                        self.snap.last_error = Some(format!("аудио: {e}"));
                    }
                }
            }
            Err(e) => self.snap.last_error = Some(format!("аудио: {e}")),
        }
    }

    pub(super) fn stop_audio(&mut self) {
        self.end_media();
        if let Some(a) = self.audio.take() {
            a.engine.stop();
            let _ = a.pump.join();
        }
        self.dirty = true;
    }

    /// Starts the media session with the SFU over `next_transport` (in the joined room).
    pub(super) fn start_media(&mut self) {
        if self.media.is_some() || self.joined.is_none() || !self.online {
            return;
        }
        let Some(audio) = &self.audio else {
            self.snap.last_error = Some("нет аудиодвижка".into());
            return;
        };
        let transport = self.next_transport;
        let (tx, rx) = sync_channel::<AudioOut>(50);
        let sink = audio.sink.clone();
        let accepted = audio.accepted.clone();
        let mut cc = CallConfig::new(Role::Offerer, self.cfg.media_bind);
        cc.advertise = self.cfg.advertise;
        cc.thread_init = Some(realtime_thread);
        cc.send_loss = self.cfg.send_loss;
        cc.recv_loss = self.cfg.recv_loss;
        cc.stun_servers = self.cfg.stun_servers.clone();
        if transport == MediaTransport::Udp {
            if self.cfg.transport == TransportPref::Auto {
                cc.connect_timeout = self.cfg.udp_attempt_timeout;
            }
            if self.cfg.block_udp {
                cc.send_loss = 1.0;
            }
        }
        self.generation += 1;
        let generation = self.generation;
        let (link, tunnel_in) = match transport {
            MediaTransport::Udp => (MediaLink::Udp, None),
            MediaTransport::Tunnel => {
                // Audio + RTCP from the server, and bursts of a watched share's keyframes.
                let (in_tx, in_rx) = sync_channel::<Vec<u8>>(2048);
                let out = self.tunnel_tx.clone();
                let outbound = Box::new(move |d: Vec<u8>| {
                    let _ = out.send((generation, d));
                });
                (
                    MediaLink::Tunnel {
                        inbound: in_rx,
                        outbound,
                    },
                    Some(in_tx),
                )
            }
        };
        let (video_tx, video_rx) = sync_channel::<VideoOut>(16);
        let (preview_tx, preview_rx) = sync_channel::<VideoOut>(8);
        let (preview_ssrcs, preview_route) =
            (self.preview_ssrcs.clone(), self.preview_route.clone());
        let (share_audio_tx, share_audio_rx) = sync_channel::<AudioOut>(50);
        let (watched, viewer) = (self.watched_ssrc.clone(), self.viewer_route.clone());
        let video = VideoIo {
            audio_out: share_audio_rx,
            outgoing: video_rx,
            preview: preview_rx,
            on_video: Box::new(move |p: VideoIn| {
                if p.preview {
                    if preview_ssrcs.lock().unwrap().contains(&p.ssrc)
                        && let Some(tx) = preview_route.lock().unwrap().as_ref()
                    {
                        // Full: the decoder is behind; drop like a lost packet.
                        let _ = tx.try_send(p);
                    }
                    return;
                }
                if p.ssrc != watched.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(tx) = viewer.lock().unwrap().as_ref() {
                    // Full: the viewer is behind; drop like a lost packet.
                    let _ = tx.try_send(p);
                }
            }),
        };
        let on_audio = move |p: turbo_pancake_media_transport::call::AudioIn| {
            if !accepted.lock().unwrap().contains(&p.ssrc) {
                return;
            }
            sink.push(IncomingPacket {
                peer: PeerId::from(p.ssrc),
                seq: p.seq,
                timestamp: p.timestamp,
                payload: p.payload,
                arrival: p.arrival,
            });
        };
        let started = CallDriver::start_media(cc, link, rx, on_audio, Some(video));
        let (driver, events) = match started {
            Ok(d) => d,
            Err(e) => {
                self.snap.last_error = Some(format!("медиа: {e}"));
                return;
            }
        };
        let to_session = self.call_tx.clone();
        // Ends when the call thread exits (its event sender is dropped).
        let bridge = std::thread::Builder::new()
            .name("tp-call-events".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    if to_session.send((generation, ev)).is_err() {
                        break;
                    }
                }
            });
        let bridge = match bridge {
            Ok(b) => b,
            Err(e) => {
                driver.stop();
                self.snap.last_error = Some(format!("медиа: {e}"));
                return;
            }
        };
        *audio.route.lock().unwrap() = Some(tx);
        *self.share_route.lock().unwrap() = Some(video_tx);
        *self.share_preview_route.lock().unwrap() = Some(preview_tx);
        *self.share_audio_route.lock().unwrap() = Some(share_audio_tx);
        self.media = Some(Media {
            driver,
            generation,
            transport,
            state: CallState::Negotiating,
            offer_rid: 0,
            tunnel_in,
            bridge,
        });
        self.snap.media_sessions += 1;
        self.dirty = true;
        // Streams the server announced earlier stay valid until it says otherwise.
        let known: Vec<(String, u32)> = self.tracks.iter().map(|(p, s)| (p.clone(), *s)).collect();
        for (peer, ssrc) in known {
            self.add_source(&peer, ssrc);
        }
        if let Some(m) = &self.media {
            for (ssrc, rtx) in self.screen_tracks.values() {
                let _ = m.driver.expect_video_source(*ssrc, *rtx);
            }
            for ssrc in self.screen_audio_tracks.values() {
                let _ = m.driver.expect_screen_audio_source(*ssrc);
            }
            for (ssrc, rtx) in self.preview_tracks.values() {
                let _ = m.driver.expect_preview_source(*ssrc, *rtx);
            }
            if let Some(s) = &self.share {
                let kbps = s
                    .adapter
                    .as_ref()
                    .map_or(s.config.bitrate_kbps, |a| a.target().bitrate_kbps);
                let _ = m.driver.set_video_pacing(pacing_bps(kbps));
            }
        }
        self.sync_watch();
    }

    pub(super) fn end_media(&mut self) {
        let Some(media) = self.media.take() else {
            return;
        };
        if let Some(a) = &self.audio {
            *a.route.lock().unwrap() = None;
        }
        *self.share_route.lock().unwrap() = None;
        *self.share_preview_route.lock().unwrap() = None;
        *self.share_audio_route.lock().unwrap() = None;
        self.layer = None;
        if let (Some(s), Some(a)) = (self.watched_audio.take(), &self.audio) {
            let _ = a.engine.remove_peer(PeerId::from(s));
        }
        *self.keyframes.lock().unwrap() = None;
        // The next session's streams start over.
        self.watched_ssrc.store(0, Ordering::Relaxed);
        if let Some(v) = &self.viewer {
            v.reset();
        }
        media.driver.stop();
        // The call thread has exited, so the bridge's receiver is closed; it never
        // blocks on the (unbounded) session channel.
        let _ = media.bridge.join();
        // Jitter buffers start over with the next session.
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().clear();
            for ssrc in self.tracks.values() {
                let _ = a.engine.remove_peer(PeerId::from(*ssrc));
            }
        }
        self.dirty = true;
    }

    /// Once a second: the voice encoder's loss resilience follows the path (spec §4):
    /// the worse of our uplink (the server's receiver reports) and the worst downlink
    /// among the listeners (`VoiceFeedback` from the server).
    pub(super) fn adapt_voice(&mut self) {
        if self.voice_adapt_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.voice_adapt_at = Instant::now();
        let (Some(a), Some(m)) = (&self.audio, &self.media) else {
            return;
        };
        let st = m.driver.stats();
        let loss = st.egress_loss.unwrap_or(0.0).max(self.listeners_loss);
        if let Some(cfg) = self.voice_adapter.update(loss) {
            let _ = a.engine.set_encoder(cfg);
        }
    }
}
