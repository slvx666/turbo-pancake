//! Events from the server (room, members, tracks, shares, calls).

use super::*;

impl Core {
    pub(super) fn add_member(&mut self, peer: v1::Peer) {
        self.member_order += 1;
        self.members
            .insert(peer.peer_id, (self.member_order, peer.display_name));
    }

    pub(super) fn server_event(&mut self, env: v1::ServerEnvelope) {
        let Some(body) = env.body else { return };
        if !matches!(body, S::MediaDatagram(_) | S::Pong(_)) {
            self.dirty = true;
        }
        match body {
            S::RoomJoined(j) => {
                if self.want_room.as_deref() != Some(j.room_id.as_str()) {
                    // Left (or switched) while the join was in flight.
                    self.push(C::LeaveRoom(v1::LeaveRoom { room_id: j.room_id }));
                    return;
                }
                crate::alerts::with(|a| a.call_joined(&j.room_id));
                self.joined = Some(j.room_id);
                self.members.clear();
                for p in j.peers {
                    self.add_member(p);
                }
                self.start_media();
            }
            S::PeerJoined(p) => {
                if self.joined.as_deref() != Some(p.room_id.as_str()) {
                    return;
                }
                if let Some(peer) = p.peer {
                    if !self.members.contains_key(&peer.peer_id) {
                        crate::alerts::with(|a| a.peer_joined());
                    }
                    self.add_member(peer);
                }
            }
            S::PeerLeft(l) => {
                // Its stream ends with TrackUnpublished.
                if self.members.remove(&l.peer_id).is_some() {
                    crate::alerts::with(|a| a.peer_left());
                }
                self.member_volumes.remove(&l.peer_id);
                self.shares.remove(&l.peer_id);
                if self.watching.as_deref() == Some(l.peer_id.as_str()) {
                    self.unwatch();
                }
                self.sync_previews();
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::ScreenPreview => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                let rtx = (t.rtx_ssrc != 0).then_some(t.rtx_ssrc);
                if let Some((old, _)) = self.preview_tracks.insert(t.peer_id.clone(), (t.ssrc, rtx))
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_preview_source(t.ssrc, rtx);
                }
                self.sync_previews();
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::ScreenPreview => {
                if self.preview_tracks.get(&t.peer_id).map(|(s, _)| *s) == Some(t.ssrc) {
                    self.preview_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    self.sync_previews();
                }
            }
            S::VoiceFeedback(f) => {
                if self.joined.as_deref() == Some(f.room_id.as_str()) {
                    self.listeners_loss = f.listeners_loss_percent.min(100) as f32 / 100.0;
                }
            }
            S::ScreenSubscription(sub) => {
                if self.watching.as_deref() == Some(sub.peer_id.as_str()) {
                    self.layer = Some(LayerState {
                        high: sub.layer() == v1::ScreenLayer::High,
                        reason: sub.reason(),
                    });
                }
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::Screen => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                let rtx = (t.rtx_ssrc != 0).then_some(t.rtx_ssrc);
                if let Some((old, _)) = self.screen_tracks.insert(t.peer_id.clone(), (t.ssrc, rtx))
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_video_source(t.ssrc, rtx);
                }
                if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                    self.sync_watch();
                }
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::ScreenAudio => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                if let Some(old) = self.screen_audio_tracks.insert(t.peer_id.clone(), t.ssrc)
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_screen_audio_source(t.ssrc);
                }
                if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                    self.sync_watch();
                }
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::ScreenAudio => {
                if self.screen_audio_tracks.get(&t.peer_id) == Some(&t.ssrc) {
                    self.screen_audio_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                        self.sync_watch();
                    }
                }
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::Screen => {
                if self.screen_tracks.get(&t.peer_id).map(|(s, _)| *s) == Some(t.ssrc) {
                    self.screen_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                        self.sync_watch();
                    }
                }
            }
            S::ScreenShareState(s) => {
                if self.joined.as_deref() != Some(s.room_id.as_str()) {
                    return;
                }
                if s.active {
                    self.shares.insert(
                        s.peer_id,
                        ShareState {
                            width: s.width,
                            height: s.height,
                            fps: s.fps,
                            bitrate_kbps: s.bitrate_kbps,
                            preview: s.preview,
                        },
                    );
                } else {
                    self.shares.remove(&s.peer_id);
                    if self.watching.as_deref() == Some(s.peer_id.as_str()) {
                        self.unwatch();
                    }
                }
                self.sync_previews();
            }
            S::TrackPublished(t) => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                if let Some(old) = self.tracks.insert(t.peer_id.clone(), t.ssrc)
                    && old != t.ssrc
                {
                    self.drop_source(old);
                }
                self.add_source(&t.peer_id, t.ssrc);
            }
            S::TrackUnpublished(t) => {
                if self.tracks.get(&t.peer_id) == Some(&t.ssrc) {
                    self.tracks.remove(&t.peer_id);
                    self.drop_source(t.ssrc);
                }
            }
            S::MediaAnswer(a) => {
                if let Some(m) = &self.media
                    && env.request_id == m.offer_rid
                    && let Err(e) = m.driver.remote_answer(a.sdp)
                {
                    self.snap.last_error = Some(e.to_string());
                }
            }
            S::MediaDatagram(d) => {
                if let Some(tx) = self.media.as_ref().and_then(|m| m.tunnel_in.as_ref()) {
                    // Full: the call thread is behind; drop like a lost packet.
                    let _ = tx.try_send(d.data);
                }
            }
            S::Pong(p) => {
                let now = self.epoch.elapsed().as_micros() as u64;
                self.snap.signaling_rtt_ms =
                    Some(now.saturating_sub(p.client_time_us) as f64 / 1000.0);
            }
            S::Error(e) => {
                let msg = format!(
                    "сервер: {} ({:?})",
                    e.message,
                    v1::ErrorCode::try_from(e.code).unwrap_or(v1::ErrorCode::Unspecified)
                );
                if env.request_id != 0 && env.request_id == self.join_rid {
                    if let Some(p) = self.authorization.take()
                        && let Some(c) = p.call
                    {
                        self.pending_leaves.push((
                            c.call_id,
                            p.participation_id,
                            self.auth_token.clone(),
                        ));
                    }
                    self.snap.last_error = Some(msg);
                    self.want_room = None;
                    self.joined = None;
                    self.stop_audio();
                } else if self
                    .media
                    .as_ref()
                    .is_some_and(|m| env.request_id != 0 && m.offer_rid == env.request_id)
                {
                    // Offer rejected (e.g. no UDP on the server): try the other way.
                    self.media_failed(msg);
                } else {
                    self.snap.last_error = Some(msg);
                }
            }
            // Our own leave or switch is already done locally (`joined` was cleared
            // first). A RoomLeft for the room we are in means the server ended our
            // membership: the call is over, do not rejoin on our own.
            S::RoomLeft(r) if self.joined.as_deref() == Some(r.room_id.as_str()) => {
                self.leave_room();
                self.want_room = None;
                self.stop_audio();
                self.snap.last_error = Some(if r.room_id.starts_with("call_") {
                    "Звонок завершён сервером".into()
                } else {
                    format!("звонок в «{}» завершён сервером", r.room_id)
                });
            }
            // Welcome, relayed signals; messenger events go to the messenger's own
            // connection (crates/messenger-client).
            _ => {}
        }
    }
}
