//! Minimal STUN Binding client (RFC 8489, no authentication): asks a STUN server which
//! address it sees our UDP socket as, so the call can offer a server-reflexive ICE
//! candidate and get through home NATs (UDP hole punching). TURN (relaying) is separate.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER: usize = 20;

/// Encodes a Binding request without attributes.
pub fn binding_request(transaction: [u8; 12]) -> [u8; HEADER] {
    let mut m = [0u8; HEADER];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message length 0 (no attributes).
    m[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    m[8..20].copy_from_slice(&transaction);
    m
}

/// Whether `msg` looks like a STUN message (first two bits zero, magic cookie).
pub fn is_stun(msg: &[u8]) -> bool {
    msg.len() >= HEADER && msg[0] & 0xC0 == 0 && msg[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// Mapped address of a Binding success response to `transaction`
/// (XOR-MAPPED-ADDRESS preferred, MAPPED-ADDRESS from old servers as fallback).
pub fn parse_binding_response(msg: &[u8], transaction: &[u8; 12]) -> Option<SocketAddr> {
    if !is_stun(msg) || u16::from_be_bytes([msg[0], msg[1]]) != BINDING_SUCCESS {
        return None;
    }
    if &msg[8..20] != transaction {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([msg[2], msg[3]]));
    let body = msg.get(HEADER..HEADER + len)?;
    let mut plain = None;
    let mut i = 0;
    while i + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = usize::from(u16::from_be_bytes([body[i + 2], body[i + 3]]));
        let value = body.get(i + 4..i + 4 + alen)?;
        match kind {
            ATTR_XOR_MAPPED_ADDRESS => return decode_address(value, Some(&msg[4..20])),
            ATTR_MAPPED_ADDRESS => plain = decode_address(value, None),
            _ => {}
        }
        // Attributes are padded to 4 bytes.
        i += 4 + alen.div_ceil(4) * 4;
    }
    plain
}

/// `xor` = magic cookie + transaction id (the XOR key of XOR-MAPPED-ADDRESS).
fn decode_address(v: &[u8], xor: Option<&[u8]>) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let key = |i: usize| xor.map_or(0, |k| k[i]);
    let port = u16::from_be_bytes([v[2] ^ key(0), v[3] ^ key(1)]);
    let ip = match v[1] {
        0x01 if v.len() >= 8 => {
            let mut a = [0u8; 4];
            for (i, b) in a.iter_mut().enumerate() {
                *b = v[4 + i] ^ key(i);
            }
            IpAddr::V4(Ipv4Addr::from(a))
        }
        0x02 if v.len() >= 20 => {
            let mut a = [0u8; 16];
            for (i, b) in a.iter_mut().enumerate() {
                *b = v[4 + i] ^ key(i);
            }
            IpAddr::V6(Ipv6Addr::from(a))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5769 §2.2 sample IPv4 response (XOR-MAPPED-ADDRESS part; integrity and
    /// fingerprint attributes are not checked by this client).
    #[test]
    fn rfc5769_ipv4_xor_mapped_address() {
        let tx = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut msg = vec![0x01, 0x01, 0x00, 0x14, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&tx);
        // SOFTWARE "test" (padded) then XOR-MAPPED-ADDRESS.
        msg.extend_from_slice(&[0x80, 0x22, 0x00, 0x04, b't', b'e', b's', b't']);
        msg.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        assert_eq!(
            parse_binding_response(&msg, &tx),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        // Wrong transaction or a request instead of a response: ignored.
        assert_eq!(parse_binding_response(&msg, &[0; 12]), None);
        assert_eq!(parse_binding_response(&binding_request(tx), &tx), None);
        assert!(is_stun(&binding_request(tx)));
        assert!(!is_stun(&[0x80; 20]));
    }

    #[test]
    fn plain_mapped_address_and_truncation() {
        let tx = [7; 12];
        let mut msg = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&tx);
        msg.extend_from_slice(&[0x00, 0x01, 0x00, 0x08, 0x00, 0x01, 0x1f, 0x90, 10, 0, 0, 1]);
        assert_eq!(
            parse_binding_response(&msg, &tx),
            Some("10.0.0.1:8080".parse().unwrap())
        );
        // Declared length beyond the datagram.
        msg[3] = 0x40;
        assert_eq!(parse_binding_response(&msg, &tx), None);
    }
}
