//! Watching other members' shares: the watched (main) stream, its audio, the layer
//! subscription and the previews.

use super::*;

impl Core {
    pub(super) fn watch(&mut self, peer: Option<String>) {
        let Some(peer) = peer else {
            self.unwatch();
            self.subscribe();
            self.sync_previews();
            return;
        };
        if self.viewer.is_none() {
            match Viewer::start(
                self.picture.clone(),
                self.keyframes.clone(),
                self.present.clone(),
            ) {
                Ok(v) => {
                    *self.viewer_route.lock().unwrap() = Some(v.sender());
                    self.viewer = Some(v);
                }
                Err(e) => {
                    self.snap.last_error = Some(format!("просмотр: {e}"));
                    return;
                }
            }
        }
        if self.watching.as_ref() != Some(&peer) {
            self.layer = None;
        }
        self.watching = Some(peer);
        self.sync_watch();
        self.subscribe();
        self.sync_previews();
    }

    /// Stops watching: the viewer thread ends and the picture goes away.
    pub(super) fn unwatch(&mut self) {
        self.watching = None;
        self.layer = None;
        if let Some(old) = self.watched_audio.take()
            && let Some(a) = &self.audio
        {
            a.accepted.lock().unwrap().remove(&old);
            let _ = a.engine.remove_peer(PeerId::from(old));
        }
        self.watched_ssrc.store(0, Ordering::Relaxed);
        *self.keyframes.lock().unwrap() = None;
        *self.viewer_route.lock().unwrap() = None;
        if let Some(v) = self.viewer.take() {
            v.stop();
        }
        *self.picture.lock().unwrap() = None;
    }

    /// Points the call thread's video filter and the viewer at the watched share's stream.
    pub(super) fn sync_watch(&mut self) {
        let track = self
            .watching
            .as_ref()
            .and_then(|p| self.screen_tracks.get(p).copied());
        let ssrc = track.map_or(0, |(s, _)| s);
        if self.watched_ssrc.swap(ssrc, Ordering::Relaxed) != ssrc
            && let Some(v) = &self.viewer
        {
            v.reset();
        }
        *self.keyframes.lock().unwrap() = match (&self.media, track) {
            (Some(m), Some((s, _))) => Some(m.driver.keyframe_requester(s)),
            _ => None,
        };
        // The watched share's sound plays as its own engine stream.
        let audio = self
            .watching
            .as_ref()
            .and_then(|p| self.screen_audio_tracks.get(p).copied());
        if self.watched_audio != audio {
            if let Some(old) = self.watched_audio.take()
                && let Some(a) = &self.audio
            {
                a.accepted.lock().unwrap().remove(&old);
                let _ = a.engine.remove_peer(PeerId::from(old));
            }
            if let (Some(ssrc), Some(a)) = (audio, &self.audio) {
                // System audio is sent in stereo (voice streams are mono).
                let _ = a.engine.set_peer_stereo(PeerId::from(ssrc), true);
                a.accepted.lock().unwrap().insert(ssrc);
                let _ = a
                    .engine
                    .set_peer_volume(PeerId::from(ssrc), self.remote_volume);
            }
            self.watched_audio = audio;
        }
    }

    /// Tells the server whose share we watch (needs the media session).
    pub(super) fn subscribe(&mut self) {
        let (Some(room), true) = (self.joined.clone(), self.media.is_some()) else {
            return;
        };
        let peer_id = self.watching.clone().unwrap_or_default();
        let layer = match self.watch_layer {
            WatchLayer::Auto => v1::ScreenLayer::Auto,
            WatchLayer::High => v1::ScreenLayer::High,
            WatchLayer::Low => v1::ScreenLayer::Low,
        };
        self.push(C::SubscribeScreen(v1::SubscribeScreen {
            room_id: room,
            peer_id,
            layer: layer as i32,
        }));
    }

    /// The previews we want: the visible page of other members' shares (member order,
    /// the watched one left out: it is shown large). Tells the server and the decoder
    /// when it changed.
    pub(super) fn sync_previews(&mut self) {
        let mut order: Vec<(u64, String)> = self
            .shares
            .iter()
            .filter(|(p, st)| st.preview && self.watching.as_deref() != Some(p.as_str()))
            .filter_map(|(p, _)| self.members.get(p).map(|(o, _)| (*o, p.clone())))
            .collect();
        order.sort();
        let want: Vec<String> = if self.previews_visible {
            order
                .into_iter()
                .map(|(_, p)| p)
                .skip(self.preview_first)
                .take(self.cfg.max_previews)
                .collect()
        } else {
            Vec::new()
        };
        // The decoder follows what we receive (streams the server announced).
        let streams: Vec<(u32, Option<KeyframeRequester>)> = want
            .iter()
            .filter_map(|p| self.preview_tracks.get(p))
            .map(|(ssrc, _)| {
                (
                    *ssrc,
                    self.media
                        .as_ref()
                        .map(|m| m.driver.keyframe_requester(*ssrc)),
                )
            })
            .collect();
        *self.preview_ssrcs.lock().unwrap() = streams.iter().map(|(s, _)| *s).collect();
        if streams.is_empty() {
            *self.preview_route.lock().unwrap() = None;
            if let Some(d) = self.preview_decoder.take() {
                d.stop();
            }
            self.preview_slots.lock().unwrap().clear();
        } else {
            if self.preview_decoder.is_none() {
                match PreviewDecoder::start(self.preview_slots.clone()) {
                    Ok(d) => {
                        *self.preview_route.lock().unwrap() = Some(d.sender());
                        self.preview_decoder = Some(d);
                    }
                    Err(e) => self.snap.last_error = Some(format!("превью: {e}")),
                }
            }
            if let Some(d) = &self.preview_decoder {
                d.set_streams(streams);
            }
        }
        if want != self.previewed
            && let (Some(room), true) = (self.joined.clone(), self.media.is_some())
        {
            self.previewed = want.clone();
            self.push(C::SubscribePreviews(v1::SubscribePreviews {
                room_id: room,
                peer_ids: want,
            }));
        }
    }
}
