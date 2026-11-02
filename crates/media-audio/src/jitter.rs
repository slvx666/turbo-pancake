//! Adaptive jitter buffer for 20 ms Opus frames.
//!
//! * Packets are keyed by the 16-bit RTP sequence number, extended to 64 bits.
//! * The playout side calls [`JitterBuffer::pop`] once per 20 ms tick and gets the next
//!   frame, the FEC source for a missing frame, or a concealment request. Speech
//!   continuity wins over latency: a gap never stalls playout (spec §4).
//! * The target delay follows the observed spread of packet transit times over a sliding
//!   window (≈ p95 − min, plus a safety margin), so it grows quickly under jitter and
//!   shrinks again on a calm network. Excess delay is removed by dropping frames one at a
//!   time, which also absorbs sender/receiver clock drift.
//! * Transit times use the RTP timestamp, not the sequence number: a sender with voice
//!   activation stops sending in pauses and continues the sequence numbers afterwards, so
//!   only the timestamp shows the gap. A packet whose timestamp jumped ahead of its
//!   sequence number starts a new talkspurt: it plays from its start after the target
//!   delay instead of being discarded as late.

use std::collections::VecDeque;

use crate::{FRAME_MS, FRAME_SAMPLES};

/// RTP timestamp units per frame.
const TS_FRAME: i64 = FRAME_SAMPLES as i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    pub min_delay_ms: u32,
    pub max_delay_ms: u32,
    /// Delay used until enough packets were seen to estimate jitter.
    pub initial_delay_ms: u32,
    /// Extra delay on top of the measured spread.
    pub margin_ms: u32,
    /// Number of recent packets used for the spread estimate.
    pub window: usize,
    /// Slots in the buffer (frames); packets further ahead reset the buffer.
    pub capacity: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            min_delay_ms: 20,
            max_delay_ms: 400,
            initial_delay_ms: 60,
            margin_ms: 10,
            window: 100,
            capacity: 64,
        }
    }
}

/// What to render for the current 20 ms tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Playout {
    /// Still filling up before the first frame; render silence.
    Buffering,
    /// The packet for this tick.
    Packet(Vec<u8>),
    /// This tick's packet is missing but the next one is here: decode it with FEC.
    Fec(Vec<u8>),
    /// Nothing usable: run packet loss concealment.
    Lost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    pub fec: u64,
    pub concealed: u64,
    /// Ticks where nothing at all was buffered ahead (network stall).
    pub underruns: u64,
    pub late: u64,
    pub duplicates: u64,
    /// Frames dropped to bring the delay back to the target.
    pub dropped: u64,
    pub resets: u64,
    /// Talkspurts started after a pause of the sender (voice activation).
    pub talkspurts: u64,
    pub target_delay_ms: u32,
}

struct Slot {
    seq: u64,
    payload: Vec<u8>,
}

pub struct JitterBuffer {
    config: JitterConfig,
    slots: Vec<Option<Slot>>,
    /// Highest extended sequence received.
    highest: Option<u64>,
    /// Next extended sequence to play; `None` while (re)buffering.
    next: Option<u64>,
    /// Highest extended RTP timestamp received.
    ts_highest: Option<i64>,
    /// Timestamp - sequence * frame of the current talkspurt; grows with every pause.
    ts_offset: Option<i64>,
    transit: VecDeque<i64>,
    scratch: Vec<i64>,
    target_frames: u64,
    empty_ticks: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(config: JitterConfig) -> Self {
        let capacity = config.capacity.max(8);
        let mut jb = Self {
            config,
            slots: (0..capacity).map(|_| None).collect(),
            highest: None,
            next: None,
            ts_highest: None,
            ts_offset: None,
            transit: VecDeque::with_capacity(config.window),
            scratch: Vec::with_capacity(config.window),
            target_frames: 0,
            empty_ticks: 0,
            stats: JitterStats::default(),
        };
        jb.target_frames = jb.frames_for(config.initial_delay_ms);
        jb.stats.target_delay_ms = jb.target_delay_ms();
        jb
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn target_delay_ms(&self) -> u32 {
        self.target_frames as u32 * FRAME_MS
    }

    /// Frames buffered ahead of the playout point (including holes).
    pub fn depth(&self) -> u64 {
        match (self.highest, self.next) {
            (Some(h), Some(n)) if h >= n => h - n + 1,
            (Some(h), None) => self.lowest().map_or(0, |l| h - l + 1),
            _ => 0,
        }
    }

    fn frames_for(&self, ms: u32) -> u64 {
        let ms = ms.clamp(self.config.min_delay_ms, self.config.max_delay_ms);
        u64::from(ms.div_ceil(FRAME_MS)).max(1)
    }

    fn extend(&self, seq: u16) -> u64 {
        match self.highest {
            None => u64::from(seq) + (1 << 16), // headroom so early reordering stays positive
            Some(h) => {
                let delta = seq.wrapping_sub(h as u16) as i16;
                h.wrapping_add_signed(i64::from(delta))
            }
        }
    }

    fn extend_ts(&self, ts: u32) -> i64 {
        match self.ts_highest {
            None => i64::from(ts) + (1 << 32),
            Some(h) => h + i64::from(ts.wrapping_sub(h as u32) as i32),
        }
    }

    fn lowest(&self) -> Option<u64> {
        self.slots.iter().flatten().map(|s| s.seq).min()
    }

    fn clear(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.next = None;
        self.highest = None;
        self.ts_highest = None;
        self.ts_offset = None;
        self.transit.clear();
        self.stats.resets += 1;
    }

    /// Inserts a packet of a sender that never pauses (timestamp = sequence * frame).
    pub fn insert(&mut self, seq: u16, payload: Vec<u8>, arrival_ms: u64) {
        let timestamp = (self.extend(seq) as u32).wrapping_mul(FRAME_SAMPLES as u32);
        self.insert_at(seq, timestamp, payload, arrival_ms);
    }

    /// Inserts a received packet with its RTP timestamp. `arrival_ms` is the local
    /// receive time on a monotonic clock with any origin.
    pub fn insert_at(&mut self, seq: u16, timestamp: u32, payload: Vec<u8>, arrival_ms: u64) {
        self.stats.received += 1;
        let mut ext = self.extend(seq);
        if let Some(offset) = self.ts_offset
            && self.extend_ts(timestamp) - ext as i64 * TS_FRAME > offset + TS_FRAME / 2
        {
            // The sender paused. Everything before `next` has been played (or skipped),
            // so if playout already ran past this packet while concealing the pause,
            // rebuffer from it instead of discarding the talkspurt as late.
            self.stats.talkspurts += 1;
            if self.next.is_some_and(|n| ext < n) {
                self.next = None;
                self.empty_ticks = 0;
            }
        }
        let cap = self.slots.len() as u64;
        if let Some(next) = self.next {
            if ext < next {
                self.stats.late += 1;
                return;
            }
            if ext >= next + cap {
                // Far ahead of playout (long stall or sender restart): start over.
                self.clear();
                ext = self.extend(seq);
            }
        } else if let Some(low) = self.lowest()
            && (ext >= low + cap || ext + cap <= low)
        {
            self.clear();
            ext = self.extend(seq);
        }
        let idx = (ext % cap) as usize;
        if self.slots[idx].as_ref().is_some_and(|s| s.seq == ext) {
            self.stats.duplicates += 1;
            return;
        }
        self.slots[idx] = Some(Slot { seq: ext, payload });
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        let ts = self.extend_ts(timestamp);
        self.ts_highest = Some(self.ts_highest.map_or(ts, |h| h.max(ts)));
        let offset = ts - ext as i64 * TS_FRAME;
        self.ts_offset = Some(self.ts_offset.map_or(offset, |o| o.max(offset)));
        self.update_target(ts, arrival_ms);
    }

    /// `ts`: extended RTP timestamp (media time).
    fn update_target(&mut self, ts: i64, arrival_ms: u64) {
        let transit = arrival_ms as i64 - ts * i64::from(FRAME_MS) / TS_FRAME;
        if self.transit.len() == self.config.window {
            self.transit.pop_front();
        }
        self.transit.push_back(transit);
        if self.transit.len() < 10 {
            return;
        }
        self.scratch.clear();
        self.scratch.extend(self.transit.iter().copied());
        self.scratch.sort_unstable();
        let min = self.scratch[0];
        let p95 = self.scratch[(self.scratch.len() - 1) * 95 / 100];
        let spread = (p95 - min).max(0) as u32;
        let wanted = self.frames_for(spread + self.config.margin_ms + FRAME_MS);
        // Grow at once, shrink one frame at a time.
        if wanted > self.target_frames {
            self.target_frames = wanted;
        } else if wanted < self.target_frames {
            self.target_frames -= 1;
        }
        self.stats.target_delay_ms = self.target_delay_ms();
    }

    fn take(&mut self, seq: u64) -> Option<Vec<u8>> {
        let idx = (seq % self.slots.len() as u64) as usize;
        match &self.slots[idx] {
            Some(s) if s.seq == seq => self.slots[idx].take().map(|s| s.payload),
            _ => None,
        }
    }

    fn peek(&self, seq: u64) -> Option<&[u8]> {
        let idx = (seq % self.slots.len() as u64) as usize;
        match &self.slots[idx] {
            Some(s) if s.seq == seq => Some(&s.payload),
            _ => None,
        }
    }

    /// Returns what to play for the current 20 ms tick.
    pub fn pop(&mut self) -> Playout {
        let next = match self.next {
            Some(n) => n,
            None => {
                let Some(low) = self.lowest() else {
                    return Playout::Buffering;
                };
                if self.depth() < self.target_frames {
                    return Playout::Buffering;
                }
                self.next = Some(low);
                low
            }
        };

        let highest = self.highest.unwrap_or(next);
        if highest < next {
            // Nothing buffered ahead: conceal and keep the clock running; after 200 ms
            // of silence rebuffer so playback resumes with the target delay.
            self.stats.underruns += 1;
            self.stats.concealed += 1;
            self.empty_ticks += 1;
            self.next = Some(next + 1);
            if self.empty_ticks >= 10 {
                self.next = None;
                self.empty_ticks = 0;
            }
            return Playout::Lost;
        }
        self.empty_ticks = 0;

        let mut next = next;
        // Too much delay (burst after a stall, or drift): drop one frame per tick.
        if highest - next + 1 > self.target_frames + 2 {
            self.take(next);
            self.stats.dropped += 1;
            next += 1;
        }
        self.next = Some(next + 1);
        if let Some(p) = self.take(next) {
            self.stats.played += 1;
            Playout::Packet(p)
        } else if let Some(p) = self.peek(next + 1).map(<[u8]>::to_vec) {
            self.stats.fec += 1;
            Playout::Fec(p)
        } else {
            self.stats.concealed += 1;
            Playout::Lost
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u16) -> Vec<u8> {
        seq.to_be_bytes().to_vec()
    }

    fn seq_of(p: &Playout) -> Option<u16> {
        match p {
            Playout::Packet(v) => Some(u16::from_be_bytes([v[0], v[1]])),
            _ => None,
        }
    }

    #[test]
    fn plays_in_order_after_initial_delay() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.pop(), Playout::Buffering);
        jb.insert(10, pkt(10), 0);
        jb.insert(11, pkt(11), 20);
        assert_eq!(jb.pop(), Playout::Buffering); // 60 ms target, 40 ms buffered
        jb.insert(12, pkt(12), 40);
        let mut played = Vec::new();
        for i in 13..40u16 {
            played.push(seq_of(&jb.pop()).unwrap());
            jb.insert(i, pkt(i), u64::from(i - 10) * 20);
        }
        assert_eq!(played, (10..37).collect::<Vec<_>>());
    }

    #[test]
    fn reorders_and_drops_duplicates_and_late() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for s in [2u16, 0, 1, 1, 3] {
            jb.insert(s, pkt(s), 0);
        }
        assert_eq!(jb.stats().duplicates, 1);
        let order: Vec<_> = (0..4).map(|_| seq_of(&jb.pop()).unwrap()).collect();
        assert_eq!(order, vec![0, 1, 2, 3]);
        jb.insert(1, pkt(1), 100);
        assert_eq!(jb.stats().late, 1);
    }

    #[test]
    fn missing_frame_uses_fec_then_plc() {
        let mut jb = JitterBuffer::new(JitterConfig {
            initial_delay_ms: 140,
            ..JitterConfig::default()
        });
        for s in [0u16, 2, 3, 6] {
            jb.insert(s, pkt(s), 0);
        }
        assert_eq!(seq_of(&jb.pop()), Some(0));
        assert_eq!(jb.pop(), Playout::Fec(pkt(2))); // 1 missing, 2 present
        assert_eq!(seq_of(&jb.pop()), Some(2));
        assert_eq!(seq_of(&jb.pop()), Some(3));
        assert_eq!(jb.pop(), Playout::Lost); // 4 missing, 5 missing
        assert_eq!(jb.pop(), Playout::Fec(pkt(6)));
        assert_eq!(seq_of(&jb.pop()), Some(6));
        let st = jb.stats();
        assert_eq!((st.played, st.fec, st.concealed), (4, 2, 1));
    }

    #[test]
    fn sequence_wraps_around() {
        let mut jb = JitterBuffer::new(JitterConfig {
            initial_delay_ms: 200,
            ..JitterConfig::default()
        });
        let seqs: Vec<u16> = (0..10).map(|i| 65_530u16.wrapping_add(i)).collect();
        for (i, s) in seqs.iter().enumerate() {
            jb.insert(*s, pkt(*s), i as u64 * 20);
        }
        let out: Vec<_> = (0..10).map(|_| seq_of(&jb.pop()).unwrap()).collect();
        assert_eq!(out, seqs);
    }

    #[test]
    fn target_grows_with_jitter_and_shrinks_when_calm() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut seq = 0u16;
        // 50 ms of jitter: every 4th packet is 50 ms late.
        for i in 0..200u64 {
            let extra = if i % 4 == 0 { 50 } else { 0 };
            jb.insert(seq, pkt(seq), i * 20 + extra);
            seq = seq.wrapping_add(1);
            jb.pop();
        }
        let jittery = jb.target_delay_ms();
        assert!(jittery >= 80, "target {jittery} ms under 50 ms jitter");
        for i in 200..600u64 {
            jb.insert(seq, pkt(seq), i * 20);
            seq = seq.wrapping_add(1);
            jb.pop();
        }
        let calm = jb.target_delay_ms();
        assert!(calm < jittery && calm <= 40, "target {calm} ms after calm");
    }

    #[test]
    fn stall_conceals_then_rebuffers_and_burst_is_drained() {
        let mut jb = JitterBuffer::new(JitterConfig {
            initial_delay_ms: 40,
            ..JitterConfig::default()
        });
        for s in 0..3u16 {
            jb.insert(s, pkt(s), u64::from(s) * 20);
        }
        for _ in 0..3 {
            assert!(seq_of(&jb.pop()).is_some());
        }
        // 300 ms stall: playout keeps running with concealment.
        for _ in 0..15 {
            let p = jb.pop();
            assert!(matches!(p, Playout::Lost | Playout::Buffering), "{p:?}");
        }
        assert!(jb.stats().underruns >= 10);
        // Burst of 15 packets at once (the stalled ones), then normal flow.
        for s in 3..18u16 {
            jb.insert(s, pkt(s), 360);
        }
        for (i, seq) in (18..78u16).enumerate() {
            jb.pop();
            jb.insert(seq, pkt(seq), 380 + i as u64 * 20);
        }
        assert!(jb.stats().dropped > 0);
        assert_eq!(jb.stats().talkspurts, 0);
        assert!(
            jb.depth() <= u64::from(jb.target_delay_ms() / FRAME_MS) + 2,
            "depth {} target {}",
            jb.depth(),
            jb.target_delay_ms()
        );
    }

    #[test]
    fn talkspurt_after_a_pause_plays_without_late_drops_or_extra_delay() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let ts = |frame: u32| frame.wrapping_mul(FRAME_SAMPLES as u32);
        // 1 s of speech (frames 0..50), then 1 s pause, then speech again: sequence
        // numbers continue at 50, timestamps at frame 100.
        let mut played = Vec::new();
        let mut t = 0u64;
        for f in 0..50u32 {
            jb.insert_at(f as u16, ts(f), pkt(f as u16), t);
            played.extend(seq_of(&jb.pop()));
            t += 20;
        }
        for _ in 0..50 {
            played.extend(seq_of(&jb.pop()));
            t += 20;
        }
        let delay_before = jb.target_delay_ms();
        for (i, f) in (100..150u32).enumerate() {
            let seq = 50 + i as u16;
            jb.insert_at(seq, ts(f), pkt(seq), t);
            played.extend(seq_of(&jb.pop()));
            t += 20;
        }
        for _ in 0..10 {
            played.extend(seq_of(&jb.pop()));
        }
        let s = jb.stats();
        assert_eq!(s.late, 0, "{s:?}");
        assert_eq!(s.talkspurts, 1, "{s:?}");
        assert_eq!(s.resets, 0, "{s:?}");
        assert_eq!(
            jb.target_delay_ms(),
            delay_before,
            "the pause is not jitter"
        );
        // Every frame of both talkspurts played once, in order.
        assert_eq!(played, (0..100u16).collect::<Vec<_>>());
    }

    #[test]
    fn short_pause_keeps_buffered_frames() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let ts = |frame: u32| frame * FRAME_SAMPLES as u32;
        // A 40 ms pause is shorter than the playout delay: frames of the first talkspurt
        // are still buffered when the second one arrives, and nothing is skipped.
        let mut played = Vec::new();
        for f in 0..5u32 {
            jb.insert_at(f as u16, ts(f), pkt(f as u16), u64::from(f) * 20);
            played.extend(seq_of(&jb.pop()));
        }
        for _ in 0..2 {
            played.extend(seq_of(&jb.pop()));
        }
        for (i, f) in (7..12u32).enumerate() {
            jb.insert_at(5 + i as u16, ts(f), pkt(5 + i as u16), u64::from(f) * 20);
            played.extend(seq_of(&jb.pop()));
        }
        for _ in 0..10 {
            played.extend(seq_of(&jb.pop()));
        }
        assert_eq!(played, (0..10u16).collect::<Vec<_>>());
        assert_eq!(jb.stats().talkspurts, 1);
    }
}
