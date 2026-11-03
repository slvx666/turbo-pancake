//! Screen share video over RTP (S3): H.264 packetization (RFC 6184, packetization mode 1,
//! str0m's packetizer) on the sending side and frame assembly on the receiving side.
//!
//! The call runs in RTP mode, so the receiver sees packets as they arrive, including late
//! retransmissions (RTX answers to NACKs). [`FrameAssembler`] collects them into complete
//! access units: a frame is handed out only when every packet from its first to its marker
//! packet is there. A frame that stays incomplete longer than [`FrameAssembler::GIVE_UP`]
//! is dropped and so is everything after it until the next keyframe (H.264 P-frames depend
//! on their predecessors); the owner then asks the sender for a keyframe.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use str0m::format::CodecExtra;
use str0m::unversioned::{Depacketizer, H264Depacketizer, H264Packetizer, Packetizer};

/// RTP clock rate of video.
pub const VIDEO_CLOCK: u32 = 90_000;

/// Largest RTP payload we produce: the tunnel adds its own framing and VPNs shrink the path
/// MTU, so stay well below 1500 bytes.
pub const VIDEO_MTU: usize = 1100;

/// One encoded access unit (Annex B) to send.
#[derive(Debug, Clone)]
pub struct VideoOut {
    /// RTP timestamp, 90 kHz.
    pub timestamp: u32,
    pub data: Vec<u8>,
}

/// One received video RTP packet.
#[derive(Debug, Clone)]
pub struct VideoIn {
    pub ssrc: u32,
    /// Extended sequence number.
    pub seq: u64,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
    pub arrival: Instant,
}

/// Splits access units into RTP payloads; the last one of a frame carries the marker.
#[derive(Debug, Default)]
pub struct VideoPacketizer {
    inner: H264Packetizer,
}

impl VideoPacketizer {
    pub fn packetize(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        self.inner.packetize(VIDEO_MTU, frame).unwrap_or_default()
    }
}

/// A complete access unit, Annex B.
#[derive(Debug, Clone)]
pub struct AssembledFrame {
    pub timestamp: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Arrival of the frame's last packet.
    pub arrival: Instant,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames: u64,
    /// Frames given up as incomplete, or dropped while waiting for a keyframe.
    pub frames_dropped: u64,
    pub packets: u64,
    /// Duplicate or too-old packets.
    pub packets_ignored: u64,
}

struct Packet {
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    arrival: Instant,
}

pub struct FrameAssembler {
    packets: BTreeMap<u64, Packet>,
    /// Sequence number of the first packet of the next frame to hand out.
    next: Option<u64>,
    /// After a loss (and at the start) only a keyframe can be decoded.
    need_keyframe: bool,
    /// Set when frames were dropped; cleared by [`FrameAssembler::take_keyframe_request`].
    want_keyframe: bool,
    depacketizer: H264Depacketizer,
    pub stats: AssemblerStats,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    /// How long a frame may wait for missing packets (retransmissions take about one RTT).
    pub const GIVE_UP: Duration = Duration::from_millis(250);
    /// Bound on buffered packets (a 1440p keyframe is a few hundred).
    const MAX_PACKETS: usize = 4096;

    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            need_keyframe: true,
            want_keyframe: false,
            depacketizer: H264Depacketizer::default(),
            stats: AssemblerStats::default(),
        }
    }

    /// Forgets everything (a new stream, e.g. after switching the watched share).
    pub fn reset(&mut self) {
        let stats = std::mem::take(&mut self.stats);
        *self = Self::new();
        self.stats = stats;
    }

    /// True once after frames were dropped: the owner should ask for a keyframe.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Adds a packet and returns every frame it completed, in order.
    pub fn push(&mut self, p: VideoIn) -> Vec<AssembledFrame> {
        self.stats.packets += 1;
        if self.next.is_some_and(|n| p.seq < n) || self.packets.contains_key(&p.seq) {
            self.stats.packets_ignored += 1;
            return self.poll(p.arrival);
        }
        if self.packets.len() >= Self::MAX_PACKETS {
            self.drop_all();
        }
        self.packets.insert(
            p.seq,
            Packet {
                timestamp: p.timestamp,
                marker: p.marker,
                payload: p.payload,
                arrival: p.arrival,
            },
        );
        self.poll(p.arrival)
    }

    /// Hands out completed frames and gives up on stale incomplete ones. Call it also
    /// without new packets (e.g. every few ms) so a stalled frame times out.
    pub fn poll(&mut self, now: Instant) -> Vec<AssembledFrame> {
        let mut out = Vec::new();
        loop {
            if self.next.is_none() && !self.find_start() {
                return out;
            }
            match self.complete_frame() {
                Some(seqs) => {
                    if let Some(frame) = self.take_frame(&seqs) {
                        out.push(frame);
                    }
                }
                None => {
                    // Incomplete: wait for retransmissions, unless it waited too long.
                    let oldest = self.packets.values().map(|p| p.arrival).min();
                    if oldest.is_some_and(|t| now.saturating_duration_since(t) > Self::GIVE_UP) {
                        self.give_up_frame();
                        continue;
                    }
                    return out;
                }
            }
        }
    }

    /// Without a known next frame (at the start, after a loss) only a keyframe can be
    /// decoded: picks the first buffered packet that starts one and drops what is before.
    fn find_start(&mut self) -> bool {
        let start = self
            .packets
            .iter()
            .find(|(_, p)| starts_keyframe(&p.payload))
            .map(|(s, _)| *s);
        let stale: Vec<u64> = match start {
            Some(s) => self.packets.range(..s).map(|(k, _)| *k).collect(),
            // No keyframe start yet: keep only the newest packets, the start may be among
            // the ones still to come (or be retransmitted).
            None => Vec::new(),
        };
        for k in stale {
            self.packets.remove(&k);
            self.stats.packets_ignored += 1;
        }
        self.next = start;
        start.is_some()
    }

    /// Sequence numbers of the next frame if all of its packets are here.
    fn complete_frame(&self) -> Option<Vec<u64>> {
        let first = self.next?;
        let ts = self.packets.get(&first)?.timestamp;
        let mut seqs = Vec::new();
        let mut s = first;
        loop {
            let p = self.packets.get(&s)?;
            if p.timestamp != ts {
                // The next frame started without a marker on this one: treat as complete.
                return Some(seqs);
            }
            seqs.push(s);
            if p.marker {
                return Some(seqs);
            }
            s += 1;
        }
    }

    fn take_frame(&mut self, seqs: &[u64]) -> Option<AssembledFrame> {
        let last = *seqs.last()?;
        self.next = Some(last + 1);
        let mut data = Vec::new();
        let mut extra = CodecExtra::None;
        let mut timestamp = 0;
        let mut arrival = Instant::now();
        let mut ok = true;
        self.depacketizer = H264Depacketizer::default();
        for s in seqs {
            let Some(p) = self.packets.remove(s) else {
                continue;
            };
            timestamp = p.timestamp;
            arrival = p.arrival;
            ok &= self
                .depacketizer
                .depacketize(&p.payload, &mut data, &mut extra)
                .is_ok();
        }
        let keyframe = is_keyframe(&data);
        if !ok || data.is_empty() || (self.need_keyframe && !keyframe) {
            self.stats.frames_dropped += 1;
            if !ok {
                self.lost();
            }
            return None;
        }
        self.need_keyframe = false;
        self.stats.frames += 1;
        Some(AssembledFrame {
            timestamp,
            data,
            keyframe,
            arrival,
        })
    }

    /// Drops the stalled frame and everything up to the next keyframe start.
    fn give_up_frame(&mut self) {
        let Some(first) = self.next.take() else {
            return;
        };
        // The stalled frame's own packets (a keyframe start among them must not be found
        // again).
        let doomed: Vec<u64> = self.packets.range(..=first).map(|(s, _)| *s).collect();
        for s in doomed {
            self.packets.remove(&s);
        }
        self.stats.frames_dropped += 1;
        self.lost();
        if !self.find_start() {
            // Nothing decodable buffered: whatever is here predates the next keyframe.
            self.stats.packets_ignored += self.packets.len() as u64;
            self.packets.clear();
        }
    }

    fn drop_all(&mut self) {
        self.packets.clear();
        self.next = None;
        self.stats.frames_dropped += 1;
        self.lost();
    }

    fn lost(&mut self) {
        self.need_keyframe = true;
        self.want_keyframe = true;
    }
}

/// True if an RTP payload begins a keyframe: SPS (single or first in a STAP-A) or the
/// start of an IDR slice (single or FU-A start).
fn starts_keyframe(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    match first & 0x1F {
        5 | 7 => true,
        24 => payload.get(3).is_some_and(|b| matches!(b & 0x1F, 5 | 7)),
        28 => payload
            .get(1)
            .is_some_and(|b| b & 0x80 != 0 && b & 0x1F == 5),
        _ => false,
    }
}

/// True if the Annex B access unit contains an IDR slice.
pub fn is_keyframe(annex_b: &[u8]) -> bool {
    nal_types(annex_b).any(|t| t == 5)
}

/// NAL unit types of an Annex B byte stream.
pub fn nal_types(annex_b: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < annex_b.len() {
            if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
                let t = annex_b[i + 3] & 0x1F;
                i += 3;
                return Some(t);
            }
            i += 1;
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

    /// A fake access unit: SPS + PPS + IDR or a P slice, `len` bytes of slice data.
    fn frame(key: bool, len: usize, fill: u8) -> Vec<u8> {
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

    /// Packetizes frames into `VideoIn`s with consecutive sequence numbers.
    fn packets(frames: &[Vec<u8>]) -> Vec<VideoIn> {
        let mut p = VideoPacketizer::default();
        let mut seq = 1000;
        let mut out = Vec::new();
        let t0 = Instant::now();
        for (n, f) in frames.iter().enumerate() {
            let parts = p.packetize(f);
            let count = parts.len();
            for (i, payload) in parts.into_iter().enumerate() {
                out.push(VideoIn {
                    ssrc: 1,
                    seq,
                    timestamp: n as u32 * 1500,
                    marker: i + 1 == count,
                    payload,
                    arrival: t0,
                });
                seq += 1;
            }
        }
        out
    }

    fn slice_bytes(data: &[u8]) -> usize {
        // Payload bytes after the last start code + NAL header.
        let pos = data.windows(4).rposition(|w| w == [0, 0, 0, 1]).unwrap();
        data.len() - pos - 5
    }

    #[test]
    fn frames_round_trip_in_order() {
        let frames = vec![
            frame(true, 5000, 1),
            frame(false, 300, 2),
            frame(false, 2500, 3),
        ];
        let mut a = FrameAssembler::new();
        let mut got = Vec::new();
        for p in packets(&frames) {
            got.extend(a.push(p));
        }
        assert_eq!(got.len(), 3);
        assert!(got[0].keyframe && !got[1].keyframe);
        assert_eq!(slice_bytes(&got[0].data), 5000);
        assert_eq!(slice_bytes(&got[2].data), 2500);
        assert!(nal_types(&got[0].data).eq([7, 8, 5]));
        assert_eq!(a.stats.frames_dropped, 0);
    }

    #[test]
    fn late_retransmission_completes_the_frame() {
        let frames = vec![frame(true, 4000, 1), frame(false, 2000, 2)];
        let mut ps = packets(&frames);
        let late = ps.remove(2);
        let mut a = FrameAssembler::new();
        let mut got = Vec::new();
        for p in ps {
            got.extend(a.push(p));
        }
        assert!(got.is_empty(), "keyframe waits for its missing packet");
        got.extend(a.push(late));
        assert_eq!(got.len(), 2);
        assert!(!a.take_keyframe_request());
    }

    #[test]
    fn a_lost_packet_drops_until_the_next_keyframe() {
        let frames = vec![
            frame(true, 3000, 1),
            frame(false, 3000, 2),
            frame(false, 300, 3),
            frame(true, 3000, 4),
            frame(false, 300, 5),
        ];
        let mut ps = packets(&frames);
        // Lose a packet in the middle of frame 1 for good.
        let lost_seq = ps
            .iter()
            .find(|p| p.timestamp == 1500 && !p.marker)
            .map(|p| p.seq)
            .unwrap();
        ps.retain(|p| p.seq != lost_seq);
        let mut a = FrameAssembler::new();
        let mut got = Vec::new();
        for p in ps {
            got.extend(a.push(p));
        }
        let later = Instant::now() + FrameAssembler::GIVE_UP * 2;
        got.extend(a.poll(later));
        assert!(a.take_keyframe_request(), "loss asks for a keyframe");
        let ts: Vec<u32> = got.iter().map(|f| f.timestamp).collect();
        assert_eq!(ts, vec![0, 4500, 6000], "frames 1 and 2 dropped");
        assert!(got[1].keyframe);
    }

    #[test]
    fn losing_the_first_packet_of_a_frame_keeps_later_keyframes() {
        let frames = vec![
            frame(true, 3000, 1),
            frame(false, 3000, 2),
            frame(true, 3000, 3),
        ];
        let mut ps = packets(&frames);
        let first_of_p = ps.iter().find(|p| p.timestamp == 1500).unwrap().seq;
        ps.retain(|p| p.seq != first_of_p);
        let mut a = FrameAssembler::new();
        let mut got = Vec::new();
        for p in ps {
            got.extend(a.push(p));
        }
        got.extend(a.poll(Instant::now() + FrameAssembler::GIVE_UP * 2));
        let ts: Vec<u32> = got.iter().map(|f| f.timestamp).collect();
        assert_eq!(ts, vec![0, 3000]);
        assert!(a.take_keyframe_request());
    }

    #[test]
    fn starts_only_at_a_keyframe() {
        let frames = vec![
            frame(false, 300, 1),
            frame(false, 300, 2),
            frame(true, 3000, 3),
        ];
        let mut a = FrameAssembler::new();
        let mut got = Vec::new();
        for p in packets(&frames) {
            got.extend(a.push(p));
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].keyframe);
    }
}
