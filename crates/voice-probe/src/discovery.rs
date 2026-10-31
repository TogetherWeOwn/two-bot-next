//! Discord-shaped UDP IP discovery codec.
//!
//! The probe sends a 74-byte request (`type = 1`, length 70, SSRC, 66 zero
//! bytes); the echo replies with a 74-byte response (`type = 2`, length 70,
//! SSRC echo, 64-byte null-padded observed-source-IP string, observed-source
//! port). The observed source proves the egress mapping, not just that a
//! reply arrived.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// Wire length of a discovery request or response.
pub const DISCOVERY_LEN: usize = 74;

/// Value of the length field inside discovery packets (bytes after it).
const DISCOVERY_BODY_LEN: u16 = 70;

/// Request packet type.
const TYPE_REQUEST: u16 = 1;

/// Response packet type.
const TYPE_RESPONSE: u16 = 2;

/// Length of the null-padded IP string field in a response.
const ADDR_FIELD_LEN: usize = 64;

/// Byte offset of the IP string field in a response.
const ADDR_FIELD_START: usize = 8;

/// Byte offset of the port field in a response.
const PORT_FIELD_START: usize = 72;

/// Rejection reasons for a malformed discovery response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    /// Packet length is not exactly [`DISCOVERY_LEN`]; carries the length seen.
    BadLength(usize),
    /// Type field is not 2; carries the type seen.
    BadType(u16),
    /// Length field is not 70; carries the value seen.
    BadLengthField(u16),
    /// The echoed SSRC is not the one the probe sent.
    SsrcMismatch { expected: u32, got: u32 },
    /// The address field is not a null-terminated IP string.
    BadAddress,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength(len) => {
                write!(f, "discovery packet must be 74 bytes, got {len}")
            }
            Self::BadType(got) => {
                write!(f, "discovery response type must be 2, got {got}")
            }
            Self::BadLengthField(got) => {
                write!(f, "discovery length field must be 70, got {got}")
            }
            Self::SsrcMismatch { expected, got } => {
                write!(f, "discovery ssrc mismatch: expected {expected}, got {got}")
            }
            Self::BadAddress => write!(
                f,
                "discovery address field is not a null-terminated IP string"
            ),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// A decoded discovery response: the echoed SSRC and the probe's source
/// address as observed by the echo server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryResponse {
    /// SSRC echoed back from the request.
    pub ssrc: u32,
    /// Probe source address as seen by the echo server.
    pub source: SocketAddr,
}

/// Encode a discovery request for `ssrc`.
#[must_use]
pub fn encode_request(ssrc: u32) -> [u8; DISCOVERY_LEN] {
    let mut packet = [0u8; DISCOVERY_LEN];
    packet[0..2].copy_from_slice(&TYPE_REQUEST.to_be_bytes());
    packet[2..4].copy_from_slice(&DISCOVERY_BODY_LEN.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

/// Encode a discovery response echoing `ssrc` and reporting `source` as the
/// observed sender. The IP renders as text and is null-padded to 64
/// bytes; the longest textual IP (IPv6, 45 chars) always leaves room for
/// the terminator.
#[must_use]
pub fn encode_response(ssrc: u32, source: SocketAddr) -> [u8; DISCOVERY_LEN] {
    let mut packet = [0u8; DISCOVERY_LEN];
    packet[0..2].copy_from_slice(&TYPE_RESPONSE.to_be_bytes());
    packet[2..4].copy_from_slice(&DISCOVERY_BODY_LEN.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    let text = source.ip().to_string();
    let bytes = text.as_bytes();
    let end = ADDR_FIELD_START + bytes.len();
    packet[ADDR_FIELD_START..end].copy_from_slice(bytes);
    packet[PORT_FIELD_START..].copy_from_slice(&source.port().to_be_bytes());
    packet
}

/// True when `buf` has the shape of a discovery request: exactly 74 bytes,
/// type 1, length field 70. The echo server uses this to tell discovery
/// requests (which get a shaped response) from opaque packets (which are
/// echoed back verbatim).
#[must_use]
pub fn is_discovery_request(buf: &[u8]) -> bool {
    if buf.len() != DISCOVERY_LEN {
        return false;
    }
    let packet_type = u16::from_be_bytes([buf[0], buf[1]]);
    let body_len = u16::from_be_bytes([buf[2], buf[3]]);
    packet_type == TYPE_REQUEST && body_len == DISCOVERY_BODY_LEN
}

/// Decode a discovery response, requiring `expected_ssrc`. Every shape
/// violation — size, type, length field, SSRC, address text — is an error:
/// a stray or truncated packet must never read as a successful discovery.
pub fn decode_response(
    buf: &[u8],
    expected_ssrc: u32,
) -> Result<DiscoveryResponse, DiscoveryError> {
    if buf.len() != DISCOVERY_LEN {
        return Err(DiscoveryError::BadLength(buf.len()));
    }
    let packet_type = u16::from_be_bytes([buf[0], buf[1]]);
    if packet_type != TYPE_RESPONSE {
        return Err(DiscoveryError::BadType(packet_type));
    }
    let body_len = u16::from_be_bytes([buf[2], buf[3]]);
    if body_len != DISCOVERY_BODY_LEN {
        return Err(DiscoveryError::BadLengthField(body_len));
    }
    let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if ssrc != expected_ssrc {
        return Err(DiscoveryError::SsrcMismatch {
            expected: expected_ssrc,
            got: ssrc,
        });
    }
    let field = &buf[ADDR_FIELD_START..ADDR_FIELD_START + ADDR_FIELD_LEN];
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(DiscoveryError::BadAddress)?;
    let text = std::str::from_utf8(&field[..end]).map_err(|_| DiscoveryError::BadAddress)?;
    let ip: IpAddr = text.parse().map_err(|_| DiscoveryError::BadAddress)?;
    let port = u16::from_be_bytes([buf[PORT_FIELD_START], buf[PORT_FIELD_START + 1]]);
    Ok(DiscoveryResponse {
        ssrc,
        source: SocketAddr::new(ip, port),
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn request_has_discord_shape() {
        let packet = encode_request(0x1122_3344);

        assert_eq!(packet.len(), DISCOVERY_LEN);
        assert_eq!(u16::from_be_bytes([packet[0], packet[1]]), 1);
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), 70);
        assert_eq!(
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            0x1122_3344
        );
        assert!(packet[8..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn request_is_recognised_and_response_is_not() {
        let request = encode_request(7);
        let response = encode_response(7, "203.0.113.7:52130".parse().unwrap());

        assert!(is_discovery_request(&request));
        assert!(!is_discovery_request(&response));
        assert!(!is_discovery_request(&[0u8; 16]));
        assert!(!is_discovery_request(&[0u8; 73]));
        assert!(!is_discovery_request(&[0u8; 75]));
    }

    #[test]
    fn response_round_trip() {
        let source: SocketAddr = "203.0.113.7:52130".parse().unwrap();
        let packet = encode_response(0xAABB_CCDD, source);

        assert_eq!(packet.len(), DISCOVERY_LEN);
        assert_eq!(u16::from_be_bytes([packet[0], packet[1]]), 2);
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), 70);

        let decoded = decode_response(&packet, 0xAABB_CCDD).unwrap();
        assert_eq!(decoded.ssrc, 0xAABB_CCDD);
        assert_eq!(decoded.source, source);
    }

    #[test]
    fn response_round_trip_ipv6() {
        let source: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let packet = encode_response(1, source);

        let decoded = decode_response(&packet, 1).unwrap();
        assert_eq!(decoded.source, source);
    }

    #[test]
    fn response_rejects_wrong_size() {
        assert!(matches!(
            decode_response(&[0u8; 73], 0),
            Err(DiscoveryError::BadLength(73))
        ));
        assert!(matches!(
            decode_response(&[0u8; 75], 0),
            Err(DiscoveryError::BadLength(75))
        ));
        assert!(matches!(
            decode_response(&[0u8; 16], 0),
            Err(DiscoveryError::BadLength(16))
        ));
    }

    #[test]
    fn response_rejects_bad_type_and_length() {
        let mut packet = encode_response(9, "192.0.2.1:8080".parse().unwrap());

        packet[0..2].copy_from_slice(&1u16.to_be_bytes());
        assert!(matches!(
            decode_response(&packet, 9),
            Err(DiscoveryError::BadType(1))
        ));

        packet[0..2].copy_from_slice(&2u16.to_be_bytes());
        packet[2..4].copy_from_slice(&69u16.to_be_bytes());
        assert!(matches!(
            decode_response(&packet, 9),
            Err(DiscoveryError::BadLengthField(69))
        ));
    }

    #[test]
    fn response_rejects_ssrc_mismatch() {
        let packet = encode_response(10, "192.0.2.1:8080".parse().unwrap());

        assert!(matches!(
            decode_response(&packet, 11),
            Err(DiscoveryError::SsrcMismatch {
                expected: 11,
                got: 10
            })
        ));
    }

    #[test]
    fn response_rejects_bad_address() {
        let mut packet = encode_response(12, "192.0.2.1:8080".parse().unwrap());

        // Non-IP text in the address field.
        packet[8..8 + 9].copy_from_slice(b"not-an-ip");
        assert!(matches!(
            decode_response(&packet, 12),
            Err(DiscoveryError::BadAddress)
        ));

        // No NUL terminator anywhere in the 64-byte field.
        let packet = encode_response(12, "192.0.2.1:8080".parse().unwrap());
        let mut packet = packet;
        for byte in packet.iter_mut().skip(8).take(64) {
            *byte = b'x';
        }
        assert!(matches!(
            decode_response(&packet, 12),
            Err(DiscoveryError::BadAddress)
        ));

        // Empty address.
        let mut packet = encode_response(12, "192.0.2.1:8080".parse().unwrap());
        packet[8] = 0;
        assert!(matches!(
            decode_response(&packet, 12),
            Err(DiscoveryError::BadAddress)
        ));

        // Non-UTF8 bytes before the terminator.
        let mut packet = encode_response(12, "192.0.2.1:8080".parse().unwrap());
        packet[8] = 0xFF;
        packet[9] = 0;
        assert!(matches!(
            decode_response(&packet, 12),
            Err(DiscoveryError::BadAddress)
        ));
    }

    #[test]
    fn error_display_names_the_fault() {
        assert_eq!(
            DiscoveryError::BadLength(3).to_string(),
            "discovery packet must be 74 bytes, got 3"
        );
        assert_eq!(
            DiscoveryError::BadType(1).to_string(),
            "discovery response type must be 2, got 1"
        );
        assert_eq!(
            DiscoveryError::BadLengthField(1).to_string(),
            "discovery length field must be 70, got 1"
        );
        assert_eq!(
            DiscoveryError::SsrcMismatch {
                expected: 1,
                got: 2
            }
            .to_string(),
            "discovery ssrc mismatch: expected 1, got 2"
        );
        assert_eq!(
            DiscoveryError::BadAddress.to_string(),
            "discovery address field is not a null-terminated IP string"
        );
    }
}
