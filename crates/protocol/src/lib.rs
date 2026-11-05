//! turbo-pancake wire protocol.
//!
//! The canonical schema lives in `proto/turbo_pancake/v1/*.proto`; Rust types are
//! generated at build time. This crate adds version constants, limits shared by
//! client and server, framing helpers and input validation.

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/turbo_pancake.v1.rs"));
}

use prost::Message;

/// Protocol version spoken by this build (`turbo_pancake.v1`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard upper bound for one encoded envelope (one WebSocket frame).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

pub const MAX_ROOM_ID_LEN: usize = 64;
pub const MAX_PEER_ID_LEN: usize = 64;
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;
pub const MAX_CLIENT_VERSION_LEN: usize = 128;
/// SDP blobs are the largest legitimate payload; keep well under the frame limit.
pub const MAX_SDP_BYTES: usize = 32 * 1024;
pub const MAX_ICE_CANDIDATE_BYTES: usize = 1024;
/// Largest media datagram carried in `MediaDatagram` (DTLS/SRTP stay below the MTU).
pub const MAX_DATAGRAM_BYTES: usize = 2048;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("frame of {0} bytes exceeds limit of {MAX_FRAME_BYTES}")]
    TooLarge(usize),
    #[error("malformed protobuf: {0}")]
    Malformed(String),
    #[error("unsupported protocol version {0}, expected {PROTOCOL_VERSION}")]
    UnsupportedVersion(u32),
    #[error("envelope has no body")]
    EmptyBody,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("room id must be 1..={MAX_ROOM_ID_LEN} chars of [A-Za-z0-9_-]")]
    RoomId,
    #[error("display name must be 1..={MAX_DISPLAY_NAME_CHARS} printable characters")]
    DisplayName,
    #[error("client version too long")]
    ClientVersion,
    #[error("signal payload missing")]
    MissingPayload,
    #[error("SDP missing type or too large")]
    Sdp,
    #[error("ICE candidate too large")]
    IceCandidate,
    #[error("peer id missing or too long")]
    PeerId,
    #[error("media offer needs a transport and an SDP of at most {MAX_SDP_BYTES} bytes")]
    MediaOffer,
    #[error("media datagram empty or larger than {MAX_DATAGRAM_BYTES} bytes")]
    Datagram,
    #[error("screen share needs 1..={MAX_SHARE_DIMENSION} px sides and 1..={MAX_SHARE_FPS} fps")]
    ScreenShare,
    #[error("at most {MAX_PREVIEWS} distinct preview subscriptions with valid peer ids")]
    Previews,
    #[error("unknown screen layer")]
    Layer,
    #[error(
        "username must be {MIN_USERNAME_LEN}..={MAX_USERNAME_LEN} chars of [a-z0-9_], start with a letter and not be reserved"
    )]
    Username,
    #[error("password must be {MIN_PASSWORD_CHARS}..={MAX_PASSWORD_CHARS} characters")]
    Password,
    #[error("message must be 1..={MAX_MESSAGE_CHARS} characters and not blank")]
    MessageBody,
    #[error("device name must be at most {MAX_DEVICE_NAME_CHARS} printable characters")]
    DeviceName,
    #[error("client nonce must be 1..={MAX_NONCE_LEN} chars of [A-Za-z0-9-]")]
    Nonce,
    #[error("malformed id")]
    Id,
    #[error("session token too long")]
    Token,
    #[error("group title must be 1..={MAX_GROUP_TITLE_CHARS} printable characters, not blank")]
    GroupTitle,
}

// ---- Messenger limits (S5, ADR 0013) ----

pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 32;
pub const MIN_PASSWORD_CHARS: usize = 8;
pub const MAX_PASSWORD_CHARS: usize = 128;
pub const MAX_MESSAGE_CHARS: usize = 4000;
pub const MAX_DEVICE_NAME_CHARS: usize = 64;
pub const MAX_NONCE_LEN: usize = 64;
pub const MAX_TOKEN_LEN: usize = 128;
pub const MAX_GROUP_MEMBERS: usize = 100;
pub const MAX_GROUP_TITLE_CHARS: usize = 100;
/// Largest HTTP API request body.
pub const MAX_API_BODY_BYTES: usize = 64 * 1024;
/// Page sizes of `/messages` and `/changes`.
pub const MAX_HISTORY_PAGE: u32 = 100;
pub const MAX_CHANGES_PAGE: u32 = 500;
/// Content type of API bodies.
pub const API_CONTENT_TYPE: &str = "application/x-protobuf";

/// Names nobody may register (service names and names that look like staff).
const RESERVED_USERNAMES: &[&str] = &[
    "admin",
    "administrator",
    "root",
    "system",
    "support",
    "help",
    "moderator",
    "mod",
    "staff",
    "official",
    "security",
    "server",
    "service",
    "bot",
    "null",
    "undefined",
    "anonymous",
    "everyone",
    "here",
    "me",
    "turbo_pancake",
    "turbopancake",
    "api",
    "www",
];

/// Case-insensitive key of a username (the unique, normalised form, ADR 0013).
pub fn username_key(username: &str) -> String {
    username.to_ascii_lowercase()
}

/// ASCII only (no look-alike letters from other scripts), starts with a letter, no
/// reserved names, no trailing or doubled underscore.
pub fn validate_username(username: &str) -> Result<(), ValidationError> {
    let key = username_key(username);
    let ok = (MIN_USERNAME_LEN..=MAX_USERNAME_LEN).contains(&key.len())
        && key.as_bytes()[0].is_ascii_lowercase()
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !key.ends_with('_')
        && !key.contains("__")
        && !RESERVED_USERNAMES.contains(&key.as_str());
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Username)
    }
}

pub fn validate_password(password: &str) -> Result<(), ValidationError> {
    let n = password.chars().count();
    if (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&n) {
        Ok(())
    } else {
        Err(ValidationError::Password)
    }
}

/// Text message: any Unicode (emoji included), not blank, no NUL, ≤ 4000 characters.
pub fn validate_message_body(body: &str) -> Result<(), ValidationError> {
    let n = body.chars().count();
    let ok = n > 0 && n <= MAX_MESSAGE_CHARS && !body.trim().is_empty() && !body.contains('\0');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::MessageBody)
    }
}

pub fn validate_device_name(name: &str) -> Result<(), ValidationError> {
    if name.chars().count() <= MAX_DEVICE_NAME_CHARS && !name.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(ValidationError::DeviceName)
    }
}

pub fn validate_group_title(title: &str) -> Result<(), ValidationError> {
    if !title.trim().is_empty()
        && title.chars().count() <= MAX_GROUP_TITLE_CHARS
        && !title.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(ValidationError::GroupTitle)
    }
}

pub fn validate_nonce(nonce: &str) -> Result<(), ValidationError> {
    let ok = !nonce.is_empty()
        && nonce.len() <= MAX_NONCE_LEN
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Nonce)
    }
}

/// A UUID in its canonical hyphenated form (8-4-4-4-12 hex digits).
pub fn validate_id(id: &str) -> Result<(), ValidationError> {
    let b = id.as_bytes();
    let ok = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    if ok { Ok(()) } else { Err(ValidationError::Id) }
}

/// Most previews one member may subscribe to (spec §5.1: a grid of up to 50 shares).
pub const MAX_PREVIEWS: usize = 50;
/// Largest high-layer bitrate a share may announce (1440p120 needs ≈ 20–40 Mbit/s).
pub const MAX_SHARE_BITRATE_KBPS: u32 = 100_000;

/// Largest side of a shared picture (spec §5.1: up to 2560×1440; room for 4K monitors
/// scaled down by the sender).
pub const MAX_SHARE_DIMENSION: u32 = 4096;
pub const MAX_SHARE_FPS: u32 = 240;

/// Decodes and version-checks a client frame.
pub fn decode_client(bytes: &[u8]) -> Result<v1::ClientEnvelope, DecodeError> {
    decode_checked(bytes, |e: &v1::ClientEnvelope| {
        (e.protocol_version, e.body.is_some())
    })
}

/// Decodes and version-checks a server frame.
pub fn decode_server(bytes: &[u8]) -> Result<v1::ServerEnvelope, DecodeError> {
    decode_checked(bytes, |e: &v1::ServerEnvelope| {
        (e.protocol_version, e.body.is_some())
    })
}

fn decode_checked<M: Message + Default>(
    bytes: &[u8],
    meta: impl Fn(&M) -> (u32, bool),
) -> Result<M, DecodeError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(DecodeError::TooLarge(bytes.len()));
    }
    let msg = M::decode(bytes).map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let (version, has_body) = meta(&msg);
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    if !has_body {
        return Err(DecodeError::EmptyBody);
    }
    Ok(msg)
}

/// Builds a client envelope with the current protocol version.
pub fn client(request_id: u64, body: v1::client_envelope::Body) -> v1::ClientEnvelope {
    v1::ClientEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id,
        body: Some(body),
    }
}

pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_to_vec()
}

pub fn validate_room_id(id: &str) -> Result<(), ValidationError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ROOM_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::RoomId)
    }
}

pub fn validate_display_name(name: &str) -> Result<(), ValidationError> {
    let trimmed = name.trim();
    let count = trimmed.chars().count();
    let ok = count > 0
        && count <= MAX_DISPLAY_NAME_CHARS
        && trimmed.len() == name.len()
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(ValidationError::DisplayName)
    }
}

/// With a token the display name comes from the account (field 2 may be empty).
pub fn validate_hello(h: &v1::Hello) -> Result<(), ValidationError> {
    if h.client_version.len() > MAX_CLIENT_VERSION_LEN {
        return Err(ValidationError::ClientVersion);
    }
    if h.auth_token.len() > MAX_TOKEN_LEN {
        return Err(ValidationError::Token);
    }
    if h.auth_token.is_empty() {
        validate_display_name(&h.display_name)
    } else {
        Ok(())
    }
}

pub fn validate_signal(s: &v1::Signal) -> Result<(), ValidationError> {
    use v1::signal::Payload;
    validate_room_id(&s.room_id)?;
    if s.to_peer_id.is_empty() || s.to_peer_id.len() > MAX_PEER_ID_LEN {
        return Err(ValidationError::PeerId);
    }
    match &s.payload {
        None => Err(ValidationError::MissingPayload),
        Some(Payload::Description(d)) => {
            if d.sdp_type() == v1::SdpType::Unspecified || d.sdp.len() > MAX_SDP_BYTES {
                Err(ValidationError::Sdp)
            } else {
                Ok(())
            }
        }
        Some(Payload::IceCandidate(c)) => {
            if c.candidate.len() > MAX_ICE_CANDIDATE_BYTES || c.sdp_mid.len() > 64 {
                Err(ValidationError::IceCandidate)
            } else {
                Ok(())
            }
        }
    }
}

pub fn validate_media_offer(m: &v1::MediaOffer) -> Result<(), ValidationError> {
    validate_room_id(&m.room_id)?;
    let ok = m.transport() != v1::MediaTransport::Unspecified
        && !m.sdp.is_empty()
        && m.sdp.len() <= MAX_SDP_BYTES
        && m.dialed_ip.len() <= 64;
    if ok {
        Ok(())
    } else {
        Err(ValidationError::MediaOffer)
    }
}

pub fn validate_datagram(d: &v1::MediaDatagram) -> Result<(), ValidationError> {
    if d.data.is_empty() || d.data.len() > MAX_DATAGRAM_BYTES {
        Err(ValidationError::Datagram)
    } else {
        Ok(())
    }
}

pub fn validate_screen_share(m: &v1::ScreenShare) -> Result<(), ValidationError> {
    validate_room_id(&m.room_id)?;
    let side = 1..=MAX_SHARE_DIMENSION;
    let ok = !m.active
        || (side.contains(&m.width)
            && side.contains(&m.height)
            && (1..=MAX_SHARE_FPS).contains(&m.fps)
            && m.bitrate_kbps <= MAX_SHARE_BITRATE_KBPS);
    if ok {
        Ok(())
    } else {
        Err(ValidationError::ScreenShare)
    }
}

pub fn validate_subscribe_screen(m: &v1::SubscribeScreen) -> Result<(), ValidationError> {
    validate_room_id(&m.room_id)?;
    if m.peer_id.len() > MAX_PEER_ID_LEN {
        return Err(ValidationError::PeerId);
    }
    if v1::ScreenLayer::try_from(m.layer).is_err() {
        return Err(ValidationError::Layer);
    }
    Ok(())
}

pub fn validate_subscribe_previews(m: &v1::SubscribePreviews) -> Result<(), ValidationError> {
    validate_room_id(&m.room_id)?;
    let mut seen = std::collections::HashSet::new();
    let ok = m.peer_ids.len() <= MAX_PREVIEWS
        && m.peer_ids
            .iter()
            .all(|p| !p.is_empty() && p.len() <= MAX_PEER_ID_LEN && seen.insert(p.as_str()));
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Previews)
    }
}

/// Converts the payload of an incoming `Signal` into its relayed form.
pub fn relay_payload(p: v1::signal::Payload) -> v1::signal_relay::Payload {
    match p {
        v1::signal::Payload::Description(d) => v1::signal_relay::Payload::Description(d),
        v1::signal::Payload::IceCandidate(c) => v1::signal_relay::Payload::IceCandidate(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_id_rules() {
        assert!(validate_room_id("abc-DEF_09").is_ok());
        assert!(validate_room_id("").is_err());
        assert!(validate_room_id("with space").is_err());
        assert!(validate_room_id("кириллица").is_err());
        assert!(validate_room_id(&"a".repeat(64)).is_ok());
        assert!(validate_room_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn display_name_rules() {
        assert!(validate_display_name("Алиса 🎧").is_ok());
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name("   ").is_err());
        assert!(validate_display_name(" lead").is_err());
        assert!(validate_display_name("bell\u{7}").is_err());
        assert!(validate_display_name(&"я".repeat(64)).is_ok());
        assert!(validate_display_name(&"я".repeat(65)).is_err());
    }

    #[test]
    fn username_rules() {
        let long_ok = "a".repeat(32);
        for ok in ["alice", "Alice_99", "bob", "a_b_c", &long_ok] {
            assert!(validate_username(ok).is_ok(), "{ok}");
        }
        let long_bad = "a".repeat(33);
        for bad in [
            "",
            "ab",
            "9lives",
            "_alice",
            "alice_",
            "al__ice",
            "al ice",
            "\u{430}lice", // Cyrillic а
            "alice!",
            "admin",
            "ADMIN",
            "root",
            "turbo_pancake",
            &long_bad,
        ] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
        assert_eq!(username_key("Alice_99"), "alice_99");
    }

    #[test]
    fn message_and_misc_rules() {
        assert!(validate_message_body("привет 👋").is_ok());
        assert!(validate_message_body(&"😀".repeat(4000)).is_ok());
        assert!(validate_message_body(&"😀".repeat(4001)).is_err());
        assert!(validate_message_body("").is_err());
        assert!(validate_message_body(" \n\t ").is_err());
        assert!(validate_message_body("a\0b").is_err());
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password("1234567").is_err());
        assert!(validate_password(&"x".repeat(129)).is_err());
        assert!(validate_id("3f2504e0-4f89-41d3-9a0c-0305e82c3301").is_ok());
        assert!(validate_id("3f2504e0-4f89-41d3-9a0c-0305e82c330").is_err());
        assert!(validate_id("3f2504e0x4f89-41d3-9a0c-0305e82c3301").is_err());
        assert!(validate_nonce("a1-b2").is_ok());
        assert!(validate_nonce("").is_err());
        assert!(validate_nonce("a b").is_err());
        assert!(validate_device_name("Windows PC").is_ok());
        assert!(validate_device_name("x\u{1}").is_err());
        let anon = v1::Hello {
            client_version: "c".into(),
            display_name: String::new(),
            auth_token: String::new(),
        };
        assert!(validate_hello(&anon).is_err());
        let authed = v1::Hello {
            auth_token: "t".into(),
            ..anon
        };
        assert!(validate_hello(&authed).is_ok());
    }
}
