//! Wire-contract tests: these pin the binary encoding of v1 so that an
//! accidental field renumbering or type change fails CI instead of silently
//! breaking deployed clients.

use prost::Message;
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};
use turbo_pancake_protocol::{
    DecodeError, MAX_FRAME_BYTES, PROTOCOL_VERSION, ValidationError, client, decode_client,
    decode_server, encode, validate_signal,
};

#[test]
fn ping_encoding_is_pinned() {
    let env = client(
        7,
        C::Ping(v1::Ping {
            client_time_us: 300,
        }),
    );
    // field 1 (varint) = 1, field 2 (varint) = 7, field 14 (len) = { field 1 (varint) = 300 }
    assert_eq!(
        encode(&env),
        vec![0x08, 0x01, 0x10, 0x07, 0x72, 0x03, 0x08, 0xAC, 0x02]
    );
}

#[test]
fn hello_field_numbers_are_pinned() {
    let env = client(
        1,
        C::Hello(v1::Hello {
            client_version: "c".into(),
            display_name: "d".into(),
        }),
    );
    // field 10 (len) = { 1:"c", 2:"d" }
    assert_eq!(
        encode(&env),
        vec![
            0x08, 0x01, 0x10, 0x01, 0x52, 0x06, 0x0A, 0x01, b'c', 0x12, 0x01, b'd'
        ]
    );
}

#[test]
fn roundtrip_signal_relay() {
    let env = v1::ServerEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        event_id: 42,
        body: Some(S::Signal(v1::SignalRelay {
            room_id: "room".into(),
            from_peer_id: "p1".into(),
            payload: Some(v1::signal_relay::Payload::Description(
                v1::SessionDescription {
                    sdp_type: v1::SdpType::Offer as i32,
                    sdp: "v=0\r\n".into(),
                },
            )),
        })),
    };
    let decoded = decode_server(&encode(&env)).unwrap();
    assert_eq!(decoded, env);
}

#[test]
fn rejects_wrong_version() {
    let mut env = client(1, C::Ping(v1::Ping::default()));
    env.protocol_version = 2;
    assert_eq!(
        decode_client(&env.encode_to_vec()),
        Err(DecodeError::UnsupportedVersion(2))
    );
}

#[test]
fn rejects_missing_body_and_garbage_and_oversize() {
    let env = v1::ClientEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 1,
        body: None,
    };
    assert_eq!(
        decode_client(&env.encode_to_vec()),
        Err(DecodeError::EmptyBody)
    );
    assert!(matches!(
        decode_client(&[0xFF, 0xFF, 0xFF]),
        Err(DecodeError::Malformed(_))
    ));
    let big = vec![0u8; MAX_FRAME_BYTES + 1];
    assert_eq!(
        decode_client(&big),
        Err(DecodeError::TooLarge(MAX_FRAME_BYTES + 1))
    );
}

#[test]
fn unknown_fields_are_tolerated_for_forward_compat() {
    // A newer v1 client may add fields; append unknown field 99 (varint) = 1.
    let mut bytes = encode(&client(3, C::Ping(v1::Ping { client_time_us: 1 })));
    bytes.extend_from_slice(&[0x98, 0x06, 0x01]);
    let env = decode_client(&bytes).unwrap();
    assert_eq!(env.request_id, 3);
}

#[test]
fn signal_validation() {
    let ok = v1::Signal {
        room_id: "r".into(),
        to_peer_id: "p".into(),
        payload: Some(v1::signal::Payload::IceCandidate(v1::IceCandidate {
            candidate: "candidate:1 1 udp 1 127.0.0.1 9 typ host".into(),
            sdp_mid: "0".into(),
            sdp_mline_index: 0,
        })),
    };
    assert!(validate_signal(&ok).is_ok());

    let mut no_payload = ok.clone();
    no_payload.payload = None;
    assert_eq!(
        validate_signal(&no_payload),
        Err(ValidationError::MissingPayload)
    );

    let mut untyped_sdp = ok.clone();
    untyped_sdp.payload = Some(v1::signal::Payload::Description(v1::SessionDescription {
        sdp_type: 0,
        sdp: "v=0".into(),
    }));
    assert_eq!(validate_signal(&untyped_sdp), Err(ValidationError::Sdp));

    let mut no_peer = ok;
    no_peer.to_peer_id.clear();
    assert_eq!(validate_signal(&no_peer), Err(ValidationError::PeerId));
}

#[test]
fn media_field_numbers_are_pinned() {
    // ClientEnvelope field 16 (len) = MediaDatagram { 1: bytes [0xAB] }
    let env = client(0, C::MediaDatagram(v1::MediaDatagram { data: vec![0xAB] }));
    assert_eq!(
        encode(&env),
        vec![0x08, 0x01, 0x82, 0x01, 0x03, 0x0A, 0x01, 0xAB]
    );
    // ClientEnvelope field 15 (len) = MediaOffer { 1:"r", 2:"s", 3: UDP(1), 4:"i" }
    let env = client(
        0,
        C::MediaOffer(v1::MediaOffer {
            room_id: "r".into(),
            sdp: "s".into(),
            transport: v1::MediaTransport::Udp as i32,
            dialed_ip: "i".into(),
        }),
    );
    assert_eq!(
        encode(&env),
        vec![
            0x08, 0x01, 0x7A, 0x0B, 0x0A, 0x01, b'r', 0x12, 0x01, b's', 0x18, 0x01, 0x22, 0x01,
            b'i'
        ]
    );
    // ServerEnvelope field 18 = MediaAnswer, field 19 = MediaDatagram.
    let answer = v1::ServerEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        event_id: 0,
        body: Some(S::MediaAnswer(v1::MediaAnswer {
            room_id: String::new(),
            sdp: String::new(),
            transport: v1::MediaTransport::Websocket as i32,
        })),
    };
    assert_eq!(
        encode(&answer),
        vec![0x08, 0x01, 0x92, 0x01, 0x02, 0x18, 0x02]
    );
    let dg = v1::ServerEnvelope {
        body: Some(S::MediaDatagram(v1::MediaDatagram { data: vec![1, 2] })),
        ..answer
    };
    let bytes = encode(&dg);
    assert_eq!(
        bytes,
        vec![0x08, 0x01, 0x9A, 0x01, 0x04, 0x0A, 0x02, 0x01, 0x02]
    );
    assert_eq!(decode_server(&bytes).unwrap(), dg);
}

#[test]
fn track_field_numbers_are_pinned() {
    // ServerEnvelope field 20 (len) = TrackPublished { 1:"r", 2:"p", 3: 5 }
    let published = v1::ServerEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        event_id: 0,
        body: Some(S::TrackPublished(v1::TrackPublished {
            room_id: "r".into(),
            peer_id: "p".into(),
            ssrc: 5,
        })),
    };
    let bytes = encode(&published);
    assert_eq!(
        bytes,
        vec![
            0x08, 0x01, 0xA2, 0x01, 0x08, 0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x18, 0x05
        ]
    );
    assert_eq!(decode_server(&bytes).unwrap(), published);
    // Field 21 = TrackUnpublished, same layout.
    let unpublished = v1::ServerEnvelope {
        body: Some(S::TrackUnpublished(v1::TrackUnpublished {
            room_id: "r".into(),
            peer_id: "p".into(),
            ssrc: 0xFFFF_FFFF,
        })),
        ..published
    };
    let bytes = encode(&unpublished);
    assert_eq!(&bytes[..4], &[0x08, 0x01, 0xAA, 0x01]);
    assert_eq!(decode_server(&bytes).unwrap(), unpublished);
}

#[test]
fn media_validation() {
    use turbo_pancake_protocol::{MAX_DATAGRAM_BYTES, validate_datagram, validate_media_offer};
    let ok = v1::MediaOffer {
        room_id: "r1".into(),
        sdp: "v=0".into(),
        transport: v1::MediaTransport::Udp as i32,
        dialed_ip: "127.0.0.1".into(),
    };
    assert!(validate_media_offer(&ok).is_ok());
    let no_transport = v1::MediaOffer {
        transport: 0,
        ..ok.clone()
    };
    assert_eq!(
        validate_media_offer(&no_transport),
        Err(ValidationError::MediaOffer)
    );
    let empty_sdp = v1::MediaOffer {
        sdp: String::new(),
        ..ok.clone()
    };
    assert!(validate_media_offer(&empty_sdp).is_err());
    let bad_room = v1::MediaOffer {
        room_id: "a b".into(),
        ..ok
    };
    assert_eq!(
        validate_media_offer(&bad_room),
        Err(ValidationError::RoomId)
    );
    assert!(validate_datagram(&v1::MediaDatagram { data: vec![0; 10] }).is_ok());
    assert!(validate_datagram(&v1::MediaDatagram { data: vec![] }).is_err());
    assert!(
        validate_datagram(&v1::MediaDatagram {
            data: vec![0; MAX_DATAGRAM_BYTES + 1]
        })
        .is_err()
    );
}
