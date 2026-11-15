//! Our own screen share: the pipeline, its system audio, what the server is told
//! about it, adaptation to the uplink and the restart after a failure.

use super::*;

/// How often a share that stopped on its own is started again: a few attempts with a
/// growing delay inside a time window, so a pipeline that cannot run (monitor gone, no
/// encoder) ends with its error instead of restarting forever.
#[derive(Debug, Default)]
pub(super) struct RestartBudget {
    recent: Vec<Instant>,
}

impl RestartBudget {
    const ATTEMPTS: usize = 3;
    const WINDOW: Duration = Duration::from_secs(60);

    /// The delay before the next attempt, `None` when the budget is spent.
    pub(super) fn next(&mut self, now: Instant) -> Option<Duration> {
        self.recent
            .retain(|t| now.saturating_duration_since(*t) < Self::WINDOW);
        if self.recent.len() >= Self::ATTEMPTS {
            return None;
        }
        self.recent.push(now);
        Some(Duration::from_millis(500) * self.recent.len() as u32)
    }
}

impl Core {
    /// Starts (or replaces) our share; announced to the room with the next chance.
    pub(super) fn start_share(&mut self, config: ShareConfig) {
        if self.share.is_some() {
            // Switching monitor or quality: a new pipeline, the old one stops first.
            if let Some(old) = self.share.take() {
                old.share.stop();
            }
        }
        let route = self.share_route.clone();
        let deliver = Box::new(move |f: turbo_pancake_media_video::EncodedFrame| {
            let route = route.lock().unwrap();
            let Some(tx) = route.as_ref() else {
                return false;
            };
            tx.try_send(VideoOut {
                timestamp: rtp_time(f.timestamp),
                data: f.data,
            })
            .is_ok()
        });
        let preview_route = self.share_preview_route.clone();
        let preview = Box::new(move |f: turbo_pancake_media_video::EncodedFrame| {
            let route = preview_route.lock().unwrap();
            let Some(tx) = route.as_ref() else {
                return false;
            };
            tx.try_send(VideoOut {
                timestamp: rtp_time(f.timestamp),
                data: f.data,
            })
            .is_ok()
        });
        match ScreenShare::start_layers(config, deliver, Some(preview)) {
            Ok(share) => {
                if let Some(m) = &self.media {
                    let _ = m.driver.set_video_pacing(pacing_bps(config.bitrate_kbps));
                }
                self.share = Some(Sharing {
                    share,
                    config,
                    adapter: None,
                    last_adapt: Instant::now(),
                    pacing_drops: self
                        .media
                        .as_ref()
                        .map_or(0, |m| m.driver.stats().video_frames_dropped_pacing),
                    announced: (0, 0, 0, 0),
                    nacks: (0, 0),
                });
                self.snap.last_error = None;
                self.sync_system_audio();
                self.announce_share();
            }
            Err(VideoError::Unsupported) => {
                self.snap.last_error = Some("показ экрана на этой ОС пока не реализован".into());
            }
            Err(e) => self.snap.last_error = Some(format!("показ экрана: {e}")),
        }
    }

    pub(super) fn stop_share(&mut self) {
        // Whoever stops the share (the user, leaving the room) also ends its retries.
        self.share_retry = None;
        if let Some(s) = self.share.take() {
            s.share.stop();
            self.sync_system_audio();
            self.announce_share();
        }
    }

    /// Runs the system audio capture exactly while we share and it is wanted.
    pub(super) fn sync_system_audio(&mut self) {
        let want = self.share.is_some() && self.cfg.share_audio;
        if !want {
            if let Some(c) = self.system_audio.take() {
                c.stop();
            }
            return;
        }
        if self.system_audio.is_some() {
            return;
        }
        let route = self.share_audio_route.clone();
        let started = SystemAudioCapture::start(move |f| {
            if let Some(tx) = route.lock().unwrap().as_ref() {
                // Full: the call thread is behind; drop like a lost packet.
                let _ = tx.try_send(AudioOut {
                    seq: f.seq,
                    timestamp: f.timestamp,
                    payload: f.payload,
                });
            }
        });
        match started {
            Ok(c) => self.system_audio = Some(c),
            // The share goes on without sound; the UI says why (spec §5.1: no mixed
            // capture with our own sound as a fallback).
            Err(e) => self.snap.last_error = Some(format!("звук компьютера: {e}")),
        }
    }

    /// Tells the server whether we share (needs the media session; repeated after every
    /// new media offer).
    pub(super) fn announce_share(&mut self) {
        let (Some(room), true) = (self.joined.clone(), self.media.is_some()) else {
            return;
        };
        let body = match &mut self.share {
            Some(s) => {
                let st = s.share.stats();
                // Before the first stats the configured values (fitted later).
                let (w, h, fps, kbps) = if st.width > 0 {
                    (st.width, st.height, st.fps, st.bitrate_kbps_target)
                } else {
                    (
                        s.config.width,
                        s.config.height,
                        s.config.fps,
                        s.config.bitrate_kbps,
                    )
                };
                s.announced = (w, h, fps, kbps);
                v1::ScreenShare {
                    room_id: room,
                    active: true,
                    width: w,
                    height: h,
                    fps,
                    bitrate_kbps: kbps,
                    preview: s.config.preview.is_some(),
                }
            }
            None => v1::ScreenShare {
                room_id: room,
                ..Default::default()
            },
        };
        self.push(C::ScreenShare(body));
    }

    /// Once a second while sharing: adapts the high layer to what the uplink reports.
    pub(super) fn adapt_share(&mut self) {
        let Some(stats) = self.media.as_ref().map(|m| m.driver.stats()) else {
            return;
        };
        let Some(s) = self.share.as_mut() else {
            return;
        };
        if !s.config.adaptive || s.last_adapt.elapsed() < Duration::from_secs(1) {
            return;
        }
        s.last_adapt = Instant::now();
        let st = s.share.stats();
        if st.width == 0 {
            return;
        }
        let adapter = s.adapter.get_or_insert_with(|| {
            Adapter::new(
                (st.source_width, st.source_height),
                Rung {
                    width: st.width,
                    height: st.height,
                    fps: st.fps,
                },
                s.config.bitrate_kbps,
            )
        });
        let drops = stats.video_frames_dropped_pacing;
        let dropped = drops > s.pacing_drops;
        s.pacing_drops = drops;
        // Receiver reports count repaired packets as received, so with retransmission
        // they show little of a lossy uplink: the share of packets the server asked to
        // repeat (NACKs) is the other loss signal.
        let (n0, p0) = s.nacks;
        let (n1, p1) = (stats.video_nacks_received, stats.video_packets_sent);
        s.nacks = (n1, p1);
        // Only over enough packets: at low rates one NACK per repair round is a lot.
        let nack_ratio =
            (p1 >= p0 + 100 && n1 >= n0 && p0 > 0).then(|| (n1 - n0) as f32 / (p1 - p0) as f32);
        let loss = match (stats.video_egress_loss, nack_ratio) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let Some(t) = adapter.update(Instant::now(), loss, dropped) else {
            return;
        };
        if (t.rung.width, t.rung.height, t.rung.fps) != (st.width, st.height, st.fps) {
            s.share.reconfigure(t.rung.width, t.rung.height, t.rung.fps);
        }
        s.share.set_bitrate(t.bitrate_kbps);
        if let Some(m) = &self.media {
            let _ = m.driver.set_video_pacing(pacing_bps(t.bitrate_kbps));
        }
    }

    /// Schedules another start of a share that stopped on its own, or gives up with
    /// the reason once the budget is spent.
    pub(super) fn retry_share(&mut self, config: ShareConfig, why: &str) {
        match self.share_restarts.next(Instant::now()) {
            Some(delay) => {
                self.share_retry = Some((Instant::now() + delay, config));
                self.snap.last_error = Some(format!("показ экрана прерван ({why}), перезапуск…"));
            }
            None => self.snap.last_error = Some(format!("показ экрана: {why}")),
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn a_few_attempts_per_window_with_growing_delay() {
        let mut b = RestartBudget::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(b.next(at(0)), Some(Duration::from_millis(500)));
        assert_eq!(b.next(at(2)), Some(Duration::from_millis(1000)));
        assert_eq!(b.next(at(4)), Some(Duration::from_millis(1500)));
        assert_eq!(b.next(at(6)), None, "budget spent");
        // The window moved on: attempts are available again.
        assert!(b.next(at(70)).is_some());
    }
}
