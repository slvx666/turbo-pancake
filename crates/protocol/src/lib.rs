//! DsMini wire protocol.
//!
//! The canonical schema lives in `proto/dsmini/v1/*.proto`; Rust types are
//! generated at build time. This crate adds version constants, limits shared by
//! client and server, framing helpers and input validation.

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/dsmini.v1.rs"));
}

use prost::Message;

/// Protocol version spoken by this build (`dsmini.v1`).
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
}

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

pub fn validate_hello(h: &v1::Hello) -> Result<(), ValidationError> {
    if h.client_version.len() > MAX_CLIENT_VERSION_LEN {
        return Err(ValidationError::ClientVersion);
    }
    validate_display_name(&h.display_name)
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
}
