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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        })),
        ..published
    };
    let bytes = encode(&unpublished);
    assert_eq!(&bytes[..4], &[0x08, 0x01, 0xAA, 0x01]);
    assert_eq!(decode_server(&bytes).unwrap(), unpublished);
}

#[test]
fn screen_share_field_numbers_are_pinned() {
    // TrackPublished for a screen: 4 = kind (SCREEN = 1), 5 = rtx_ssrc.
    let screen = v1::ServerEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        event_id: 0,
        body: Some(S::TrackPublished(v1::TrackPublished {
            room_id: "r".into(),
            peer_id: "p".into(),
            ssrc: 5,
            kind: v1::TrackKind::Screen as i32,
            rtx_ssrc: 6,
        })),
    };
    let bytes = encode(&screen);
    assert_eq!(
        &bytes[..5],
        &[0x08, 0x01, 0xA2, 0x01, 0x0C],
        "ServerEnvelope field 20, 12 bytes"
    );
    assert_eq!(
        &bytes[5..],
        &[
            0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x18, 0x05, 0x20, 0x01, 0x28, 0x06
        ]
    );
    assert_eq!(decode_server(&bytes).unwrap(), screen);
    // An S2 TrackPublished (no field 4) still means audio.
    let old = [0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x18, 0x05];
    let t = v1::TrackPublished::decode(&old[..]).unwrap();
    assert_eq!(t.kind(), v1::TrackKind::Audio);

    // ClientEnvelope 17 = ScreenShare { 1:"r", 2: true, 3: 1920, 4: 1080, 5: 60 }.
    let share = turbo_pancake_protocol::client(
        0,
        C::ScreenShare(v1::ScreenShare {
            room_id: "r".into(),
            active: true,
            width: 1920,
            height: 1080,
            fps: 60,
            ..Default::default()
        }),
    );
    let bytes = encode(&share);
    assert_eq!(
        bytes,
        vec![
            0x08, 0x01, 0x8A, 0x01, 0x0D, 0x0A, 0x01, b'r', 0x10, 0x01, 0x18, 0x80, 0x0F, 0x20,
            0xB8, 0x08, 0x28, 0x3C
        ]
    );
    assert_eq!(decode_client(&bytes).unwrap(), share);
    // 18 = SubscribeScreen { 1:"r", 2:"p" }.
    let sub = turbo_pancake_protocol::client(
        0,
        C::SubscribeScreen(v1::SubscribeScreen {
            room_id: "r".into(),
            peer_id: "p".into(),
            ..Default::default()
        }),
    );
    assert_eq!(&encode(&sub)[..4], &[0x08, 0x01, 0x92, 0x01]);
    // ServerEnvelope 22 = ScreenShareState.
    let state = v1::ServerEnvelope {
        body: Some(S::ScreenShareState(v1::ScreenShareState {
            room_id: "r".into(),
            peer_id: "p".into(),
            active: true,
            width: 1280,
            height: 720,
            fps: 30,
            ..Default::default()
        })),
        ..screen
    };
    let bytes = encode(&state);
    assert_eq!(&bytes[..3], &[0x08, 0x01, 0xB2]);
    assert_eq!(decode_server(&bytes).unwrap(), state);
}

#[test]
fn screen_share_validation() {
    use turbo_pancake_protocol::{validate_screen_share, validate_subscribe_screen};
    let ok = v1::ScreenShare {
        room_id: "r1".into(),
        active: true,
        width: 2560,
        height: 1440,
        fps: 120,
        bitrate_kbps: 30_000,
        preview: true,
    };
    assert!(validate_screen_share(&ok).is_ok());
    let stop = v1::ScreenShare {
        room_id: "r1".into(),
        ..Default::default()
    };
    assert!(
        validate_screen_share(&stop).is_ok(),
        "stopping needs no size"
    );
    for bad in [
        v1::ScreenShare {
            width: 0,
            ..ok.clone()
        },
        v1::ScreenShare {
            height: 5000,
            ..ok.clone()
        },
        v1::ScreenShare {
            fps: 0,
            ..ok.clone()
        },
        v1::ScreenShare {
            fps: 1000,
            ..ok.clone()
        },
        v1::ScreenShare {
            room_id: "bad id".into(),
            ..ok.clone()
        },
        v1::ScreenShare {
            bitrate_kbps: 200_000,
            ..ok.clone()
        },
    ] {
        assert!(validate_screen_share(&bad).is_err(), "{bad:?}");
    }
    let sub = v1::SubscribeScreen {
        room_id: "r1".into(),
        peer_id: String::new(),
        layer: 0,
    };
    assert!(
        validate_subscribe_screen(&sub).is_ok(),
        "empty = watch none"
    );
    let long = v1::SubscribeScreen {
        peer_id: "p".repeat(65),
        ..sub
    };
    assert!(validate_subscribe_screen(&long).is_err());
    let bad_layer = v1::SubscribeScreen {
        room_id: "r1".into(),
        peer_id: "p".into(),
        layer: 7,
    };
    assert!(validate_subscribe_screen(&bad_layer).is_err());
}

#[test]
fn layer_messages_are_pinned() {
    use turbo_pancake_protocol::{MAX_PREVIEWS, validate_subscribe_previews};
    // Enum values on the wire (S3 clients send no layer = AUTO, which gives them high).
    assert_eq!(v1::ScreenLayer::Auto as i32, 0);
    assert_eq!(v1::ScreenLayer::High as i32, 1);
    assert_eq!(v1::ScreenLayer::Low as i32, 2);
    assert_eq!(v1::TrackKind::ScreenPreview as i32, 3);
    assert_eq!(v1::LayerReason::Requested as i32, 1);
    assert_eq!(v1::LayerReason::Network as i32, 2);
    assert_eq!(v1::LayerReason::Admission as i32, 3);
    assert_eq!(v1::LayerReason::SingleLayer as i32, 4);
    // ClientEnvelope 18 = SubscribeScreen { 1:"r", 2:"p", 3: LOW }.
    let sub = turbo_pancake_protocol::client(
        0,
        C::SubscribeScreen(v1::SubscribeScreen {
            room_id: "r".into(),
            peer_id: "p".into(),
            layer: v1::ScreenLayer::Low as i32,
        }),
    );
    assert_eq!(
        encode(&sub),
        vec![
            0x08, 0x01, 0x92, 0x01, 0x08, 0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x18, 0x02
        ]
    );
    // ClientEnvelope 19 = SubscribePreviews { 1:"r", 2:["p","q"] }.
    let prev = turbo_pancake_protocol::client(
        0,
        C::SubscribePreviews(v1::SubscribePreviews {
            room_id: "r".into(),
            peer_ids: vec!["p".into(), "q".into()],
        }),
    );
    let bytes = encode(&prev);
    assert_eq!(
        bytes,
        vec![
            0x08, 0x01, 0x9A, 0x01, 0x09, 0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x12, 0x01, b'q'
        ]
    );
    assert_eq!(decode_client(&bytes).unwrap(), prev);
    // ServerEnvelope 23 = ScreenSubscription { 1:"r", 2:"p", 3: AUTO, 4: LOW, 5: NETWORK }.
    let upd = v1::ServerEnvelope {
        protocol_version: 1,
        request_id: 0,
        event_id: 9,
        body: Some(S::ScreenSubscription(v1::ScreenSubscription {
            room_id: "r".into(),
            peer_id: "p".into(),
            requested: 0,
            layer: 2,
            reason: 2,
        })),
    };
    let bytes = encode(&upd);
    assert_eq!(
        bytes,
        vec![
            0x08, 0x01, 0x18, 0x09, 0xBA, 0x01, 0x0A, 0x0A, 0x01, b'r', 0x12, 0x01, b'p', 0x20,
            0x02, 0x28, 0x02
        ]
    );
    assert_eq!(decode_server(&bytes).unwrap(), upd);
    // ScreenShare 6 = bitrate, 7 = preview; ScreenShareState 7 = bitrate, 8 = preview.
    let share = v1::ScreenShare {
        room_id: "r".into(),
        active: true,
        width: 2,
        height: 2,
        fps: 1,
        bitrate_kbps: 1,
        preview: true,
    };
    assert_eq!(&share.encode_to_vec()[11..], &[0x30, 0x01, 0x38, 0x01]);
    let state = v1::ScreenShareState {
        bitrate_kbps: 1,
        preview: true,
        ..Default::default()
    };
    assert_eq!(state.encode_to_vec(), vec![0x38, 0x01, 0x40, 0x01]);

    let ok = v1::SubscribePreviews {
        room_id: "r1".into(),
        peer_ids: (0..MAX_PREVIEWS).map(|i| format!("p{i}")).collect(),
    };
    assert!(validate_subscribe_previews(&ok).is_ok());
    let too_many = v1::SubscribePreviews {
        peer_ids: (0..=MAX_PREVIEWS).map(|i| format!("p{i}")).collect(),
        ..ok.clone()
    };
    assert!(validate_subscribe_previews(&too_many).is_err());
    let dup = v1::SubscribePreviews {
        peer_ids: vec!["p".into(), "p".into()],
        ..ok.clone()
    };
    assert!(validate_subscribe_previews(&dup).is_err());
    let empty_id = v1::SubscribePreviews {
        peer_ids: vec![String::new()],
        ..ok.clone()
    };
    assert!(validate_subscribe_previews(&empty_id).is_err());
    let none = v1::SubscribePreviews {
        peer_ids: vec![],
        ..ok
    };
    assert!(validate_subscribe_previews(&none).is_ok(), "empty = none");
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

#[test]
fn leave_reasons_are_pinned() {
    assert_eq!(v1::LeaveReason::Left as i32, 1);
    assert_eq!(v1::LeaveReason::Disconnected as i32, 2);
    assert_eq!(v1::LeaveReason::Timeout as i32, 3);
    assert_eq!(v1::LeaveReason::Switched as i32, 4);
    let left = v1::PeerLeft {
        room_id: "r".into(),
        peer_id: "p".into(),
        reason: 4,
    };
    assert_eq!(left.reason(), v1::LeaveReason::Switched);
}

// ---- Messenger (S5, ADR 0013) ----

#[test]
fn hello_token_and_welcome_user_are_pinned() {
    let hello = v1::Hello {
        client_version: String::new(),
        display_name: String::new(),
        auth_token: "t".into(),
    };
    // field 3 (len) = "t"
    assert_eq!(hello.encode_to_vec(), vec![0x1A, 0x01, b't']);
    let welcome = v1::Welcome {
        user: Some(v1::User {
            user_id: "u".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    // field 5 (len) = { 1:"u" }
    assert_eq!(welcome.encode_to_vec(), vec![0x2A, 0x03, 0x0A, 0x01, b'u']);
    assert_eq!(v1::ErrorCode::Unauthenticated as i32, 10);
}

#[test]
fn chat_message_field_numbers_are_pinned() {
    let m = v1::ChatMessage {
        author_display_name: String::new(),
        message_id: "m".into(),
        conversation_id: "c".into(),
        server_sequence: 3,
        change_seq: 4,
        author_id: "a".into(),
        client_nonce: "n".into(),
        body: "b".into(),
        reply_to_id: "r".into(),
        created_at_ms: 9,
        edited_at_ms: 10,
        deleted: true,
    };
    assert_eq!(
        m.encode_to_vec(),
        vec![
            0x0A, 1, b'm', 0x12, 1, b'c', 0x18, 3, 0x20, 4, 0x2A, 1, b'a', 0x32, 1, b'n', 0x3A, 1,
            b'b', 0x42, 1, b'r', 0x48, 9, 0x50, 10, 0x58, 1
        ]
    );
    let send = v1::SendMessageRequest {
        client_nonce: "n".into(),
        body: "b".into(),
        reply_to_id: "r".into(),
    };
    assert_eq!(
        send.encode_to_vec(),
        vec![0x0A, 1, b'n', 0x12, 1, b'b', 0x1A, 1, b'r']
    );
}

#[test]
fn messenger_events_are_pinned() {
    let ev = |body: S| v1::ServerEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        event_id: 1,
        body: Some(body),
    };
    let msg = || v1::MessageEvent {
        message: Some(v1::ChatMessage::default()),
    };
    // Tag byte of each oneof variant: (field << 3) | 2, two-byte varint for ≥ 16.
    let cases: [(S, u8); 7] = [
        (S::MessageCreated(msg()), 24),
        (S::MessageEdited(msg()), 25),
        (S::MessageDeleted(msg()), 26),
        (S::ReadUpdated(v1::Receipt::default()), 27),
        (S::PresenceChanged(v1::PresenceChanged::default()), 28),
        (
            S::ConversationUpdated(v1::ConversationUpdated::default()),
            29,
        ),
        (S::SessionRevoked(v1::SessionRevoked::default()), 30),
    ];
    for (body, field) in cases {
        let bytes = encode(&ev(body));
        let tag = (u32::from(field) << 3) | 2;
        assert_eq!(&bytes[4..6], &[(tag as u8) | 0x80, (tag >> 7) as u8]);
        assert!(decode_server(&bytes).is_ok());
    }
    let receipt = v1::Receipt {
        conversation_id: "c".into(),
        user_id: "u".into(),
        delivered_sequence: 2,
        read_sequence: 1,
    };
    assert_eq!(
        receipt.encode_to_vec(),
        vec![0x0A, 1, b'c', 0x12, 1, b'u', 0x18, 2, 0x20, 1]
    );
}

#[test]
fn api_codes_and_enums_are_pinned() {
    use v1::ApiErrorCode as E;
    let codes = [
        (E::BadRequest, 1),
        (E::Unauthenticated, 2),
        (E::Forbidden, 3),
        (E::NotFound, 4),
        (E::Conflict, 5),
        (E::PayloadTooLarge, 6),
        (E::RateLimited, 7),
        (E::Unavailable, 8),
        (E::Internal, 9),
    ];
    for (c, n) in codes {
        assert_eq!(c as i32, n);
    }
    assert_eq!(v1::ConversationKind::Direct as i32, 1);
    assert_eq!(v1::ConversationKind::Group as i32, 2);
    assert_eq!(v1::MemberRole::Creator as i32, 1);
    let auth = v1::AuthResponse {
        token: "t".into(),
        session_id: "s".into(),
        user: None,
        expires_at_ms: 1,
    };
    assert_eq!(
        auth.encode_to_vec(),
        vec![0x0A, 1, b't', 0x12, 1, b's', 0x20, 1]
    );
    let conv = v1::Conversation {
        blocked_by_me: true,
        can_send: true,
        ..Default::default()
    };
    assert_eq!(conv.encode_to_vec(), vec![0x40, 1, 0x48, 1]);
}

#[test]
fn group_contract_and_limits() {
    assert_eq!(
        v1::Member {
            membership_revision: 2,
            ..Default::default()
        }
        .encode_to_vec(),
        vec![40, 2]
    );
    let req = v1::CreateGroupRequest {
        title: "g".into(),
        usernames: vec!["a".into(), "b".into()],
    };
    assert_eq!(
        req.encode_to_vec(),
        vec![10, 1, b'g', 18, 1, b'a', 18, 1, b'b']
    );
    assert_eq!(
        v1::RenameGroupRequest { title: "g".into() }.encode_to_vec(),
        vec![10, 1, b'g']
    );
    assert_eq!(
        v1::AddGroupMemberRequest {
            username: "a".into()
        }
        .encode_to_vec(),
        vec![10, 1, b'a']
    );
    use turbo_pancake_protocol::{MAX_GROUP_MEMBERS, validate_group_title};
    assert_eq!(MAX_GROUP_MEMBERS, 100);
    for invalid in ["", "  ", "a\nb", "a\0b", &"я".repeat(101)] {
        assert!(validate_group_title(invalid).is_err());
    }
    assert!(validate_group_title(&"я".repeat(100)).is_ok());
    assert!(validate_group_title("Рабочая группа 👋").is_ok());
}

#[test]
fn conversation_call_contracts_are_additive_and_pinned() {
    assert_eq!(
        v1::ChatMessage {
            author_display_name: "a".into(),
            ..Default::default()
        }
        .encode_to_vec(),
        vec![98, 1, b'a']
    );
    let variants = [
        S::CallStarted(v1::ConversationCall::default()),
        S::CallParticipantJoined(v1::ConversationCall::default()),
        S::CallParticipantLeft(v1::ConversationCall::default()),
        S::CallEnded(v1::ConversationCall::default()),
    ];
    for (field, body) in (31u32..=34).zip(variants) {
        let bytes = v1::ServerEnvelope {
            body: Some(body),
            ..Default::default()
        }
        .encode_to_vec();
        let tag = (field << 3) | 2;
        assert_eq!(&bytes[..2], &[(tag as u8) | 128, (tag >> 7) as u8]);
    }
    assert_eq!(
        v1::JoinCallMedia {
            call_id: "c".into(),
            participation_id: "p".into()
        }
        .encode_to_vec(),
        vec![10, 1, b'c', 18, 1, b'p']
    );
    assert_eq!(
        v1::LeaveCallRequest {
            participation_id: "p".into()
        }
        .encode_to_vec(),
        vec![10, 1, b'p']
    );
    assert_eq!(
        v1::ConversationCall {
            revision: 7,
            state: 2,
            ..Default::default()
        }
        .encode_to_vec(),
        vec![24, 7, 32, 2]
    );
}
