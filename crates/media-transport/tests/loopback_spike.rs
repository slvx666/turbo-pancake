//! S0 feasibility spike: two str0m endpoints over real loopback UDP sockets.
//!
//! The SDP offer/answer travels through the real `turbo_pancake.v1` protobuf
//! `SessionDescription` encoding (as the signaling server would relay it), then
//! ICE + DTLS + SRTP must come up and both an audio RTP stream and a data
//! channel message must arrive. The RTP payload is opaque test bytes, not
//! encoded speech: this proves transport only, not the audio pipeline (S1).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use prost::Message;
use turbo_pancake_media_transport::UdpPeer;
use turbo_pancake_media_transport::str0m::change::{SdpAnswer, SdpOffer};
use turbo_pancake_media_transport::str0m::format::Codec;
use turbo_pancake_media_transport::str0m::media::{Direction, MediaKind, MediaTime};
use turbo_pancake_media_transport::str0m::{Event, IceConnectionState};
use turbo_pancake_protocol::v1;

fn loopback() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

/// Simulates the signaling hop: SDP text -> protobuf bytes -> SDP text.
fn via_wire(sdp_type: v1::SdpType, sdp: String) -> String {
    let bytes = v1::SessionDescription {
        sdp_type: sdp_type as i32,
        sdp,
    }
    .encode_to_vec();
    assert!(bytes.len() < turbo_pancake_protocol::MAX_SDP_BYTES);
    v1::SessionDescription::decode(bytes.as_slice())
        .unwrap()
        .sdp
}

#[test]
fn ice_dtls_srtp_audio_and_datachannel_over_udp() {
    let mut a = UdpPeer::bind(loopback()).unwrap();
    let mut b = UdpPeer::bind(loopback()).unwrap();

    let mut change = a.rtc.sdp_api();
    let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let cid = change.add_channel("control".into());
    let (offer, pending) = change.apply().expect("offer");

    let offer_txt = via_wire(v1::SdpType::Offer, offer.to_sdp_string());
    let offer = SdpOffer::from_sdp_string(&offer_txt).expect("parse offer");
    let answer = b.rtc.sdp_api().accept_offer(offer).expect("accept offer");
    let answer_txt = via_wire(v1::SdpType::Answer, answer.to_sdp_string());
    let answer = SdpAnswer::from_sdp_string(&answer_txt).expect("parse answer");
    a.rtc
        .sdp_api()
        .accept_answer(pending, answer)
        .expect("accept answer");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let (mut a_conn, mut b_conn, mut chan_open) = (false, false, false);
    while !(a_conn && b_conn && chan_open) {
        assert!(Instant::now() < deadline, "handshake did not complete");
        for e in a.step(Duration::from_millis(2)).unwrap() {
            match e {
                Event::Connected => a_conn = true,
                Event::ChannelOpen(id, _) if id == cid => chan_open = true,
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    panic!("a: ice disconnected")
                }
                _ => {}
            }
        }
        for e in b.step(Duration::from_millis(2)).unwrap() {
            if let Event::Connected = e {
                b_conn = true;
            }
        }
    }
    let handshake = started.elapsed();
    println!("SPIKE handshake_ms={}", handshake.as_millis());

    let pt = a
        .rtc
        .codec_config()
        .find(|p| p.spec().codec == Codec::Opus)
        .expect("opus negotiated")
        .pt();

    a.rtc
        .channel(cid)
        .expect("channel")
        .write(false, "привет".as_bytes())
        .unwrap();

    // 100 packets at 20 ms cadence (2 s of "audio" timing).
    let payload = vec![0xAB_u8; 120];
    let mut sent = 0u32;
    let mut received = 0u32;
    let mut got_text = false;
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(10);
    while (received < 100 || !got_text) && Instant::now() < deadline {
        let due = (t0.elapsed().as_millis() / 20) as u32 + 1;
        while sent < due.min(100) {
            let now = Instant::now();
            let rtp_time = MediaTime::new(u64::from(sent) * 960, opus_clock());
            a.rtc
                .writer(mid)
                .expect("writer")
                .write(pt, now, rtp_time, payload.clone())
                .unwrap();
            sent += 1;
        }
        a.step(Duration::from_millis(1)).unwrap();
        for e in b.step(Duration::from_millis(1)).unwrap() {
            match e {
                Event::MediaData(d) => {
                    assert_eq!(d.mid, mid);
                    assert_eq!(&d.data[..], &payload[..]);
                    received += 1;
                }
                Event::ChannelData(d) => {
                    assert_eq!(d.data, "привет".as_bytes());
                    got_text = true;
                }
                _ => {}
            }
        }
    }
    println!(
        "SPIKE rtp_sent={sent} rtp_received={received} a_tx_bytes={} b_rx_bytes={}",
        a.bytes_sent, b.bytes_received
    );
    assert!(got_text, "data channel message not received");
    // Loopback is lossless; allow a couple for the jitter-free startup edge.
    assert!(received >= 98, "only {received}/100 RTP packets arrived");

    a.rtc.disconnect();
    assert!(!a.rtc.is_alive());
}

fn opus_clock() -> turbo_pancake_media_transport::str0m::media::Frequency {
    turbo_pancake_media_transport::str0m::media::Frequency::FORTY_EIGHT_KHZ
}
