//! Encodes/decodes the `ip_version(1B) || addr(4B or 16B) || port(2B BE)`
//! layout that `axum-masque-rs` uses both inside the `COMPRESSION_ASSIGN`
//! (0x11) capsule payload and, for uncompressed (`ip_version == 0` at
//! registration) datagrams, as a per-datagram prefix.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Encodes `addr` as `ip_version(1) || ip(4 or 16) || port(2, big-endian)`.
pub fn encode_addr(addr: SocketAddr) -> Vec<u8> {
    let mut buf = Vec::with_capacity(19);
    match addr.ip() {
        IpAddr::V4(ip) => {
            buf.push(4);
            buf.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            buf.push(6);
            buf.extend_from_slice(&ip.octets());
        }
    }
    buf.extend_from_slice(&addr.port().to_be_bytes());
    buf
}

/// Decodes an `ip_version(1) || ip(4 or 16) || port(2, big-endian)` prefix
/// from the front of `data`, returning the address and the remaining slice.
///
/// Returns `None` if `data` is too short or the ip_version byte is neither
/// `4` nor `6`.
pub fn decode_addr(data: &[u8]) -> Option<(SocketAddr, &[u8])> {
    let (&ip_version, rest) = data.split_first()?;
    match ip_version {
        4 => {
            if rest.len() < 6 {
                return None;
            }
            let ip = Ipv4Addr::from(<[u8; 4]>::try_from(&rest[..4]).unwrap());
            let port = u16::from_be_bytes(<[u8; 2]>::try_from(&rest[4..6]).unwrap());
            Some((SocketAddr::new(IpAddr::V4(ip), port), &rest[6..]))
        }
        6 => {
            if rest.len() < 18 {
                return None;
            }
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&rest[..16]).unwrap());
            let port = u16::from_be_bytes(<[u8; 2]>::try_from(&rest[16..18]).unwrap());
            Some((SocketAddr::new(IpAddr::V6(ip), port), &rest[18..]))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ipv4() {
        let addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let encoded = encode_addr(addr);
        assert_eq!(encoded.len(), 7); // 1 (version) + 4 (ip) + 2 (port)
        let (decoded, rest) = decode_addr(&encoded).unwrap();
        assert_eq!(decoded, addr);
        assert!(rest.is_empty());
    }

    #[test]
    fn round_trips_ipv6() {
        let addr: SocketAddr = "[::1]:4433".parse().unwrap();
        let encoded = encode_addr(addr);
        assert_eq!(encoded.len(), 19); // 1 (version) + 16 (ip) + 2 (port)
        let (decoded, rest) = decode_addr(&encoded).unwrap();
        assert_eq!(decoded, addr);
        assert!(rest.is_empty());
    }

    #[test]
    fn decode_rejects_truncated_and_unknown_version() {
        assert!(decode_addr(&[]).is_none());
        assert!(decode_addr(&[4, 1, 2, 3]).is_none()); // truncated ipv4
        assert!(decode_addr(&[6, 1, 2, 3]).is_none()); // truncated ipv6
        assert!(decode_addr(&[5, 1, 2, 3, 4, 5, 6]).is_none()); // unknown version
    }

    #[test]
    fn decode_leaves_trailing_payload_intact() {
        let addr: SocketAddr = "192.168.1.1:9000".parse().unwrap();
        let mut encoded = encode_addr(addr);
        encoded.extend_from_slice(b"payload");
        let (decoded, rest) = decode_addr(&encoded).unwrap();
        assert_eq!(decoded, addr);
        assert_eq!(rest, b"payload");
    }
}
