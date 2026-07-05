//! Minimal STUN (RFC 5389/8489) client: just enough to send a Binding
//! Request and parse the Binding Success Response's XOR-MAPPED-ADDRESS
//! (falling back to the legacy, non-XOR'd MAPPED-ADDRESS if a server sends
//! only that). This is deliberately not a general STUN/TURN/ICE
//! implementation — isekai-terminal/isekai-helper only need "what does the
//! outside world see as my address on this socket", nothing else.
//!
//! Shared by both `isekai-terminal-core` (isekai-terminal, the client role) and
//! `isekai-helper` (the agent role) so the wire-level STUN handling can't
//! drift between the two sides of the hole-punch handshake.
//!
//! Crucially, callers must perform this query on the *same* UDP socket that
//! will later be used for the actual QUIC listen/connect: the whole point is
//! to learn the NAT mapping for that specific socket so it can be shared
//! out-of-band with a peer for hole punching.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rand::Rng;
use tokio::net::UdpSocket;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const MAPPED_ADDRESS: u16 = 0x0001;
const HEADER_LEN: usize = 20;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StunError {
    #[error("STUN response shorter than the 20-byte header")]
    TooShort,
    #[error("STUN response is not a Binding Success Response (type {0:#06x})")]
    NotBindingSuccess(u16),
    #[error("STUN response magic cookie mismatch")]
    BadMagicCookie,
    #[error("STUN response transaction id does not match the request")]
    TransactionIdMismatch,
    #[error("STUN response has no (XOR_)MAPPED_ADDRESS attribute")]
    NoMappedAddress,
    #[error("STUN (XOR_)MAPPED_ADDRESS attribute has an unknown/unsupported address family")]
    UnknownFamily,
    #[error("STUN (XOR_)MAPPED_ADDRESS attribute is truncated")]
    TruncatedAddress,
    #[error("io error: {0}")]
    Io(String),
    #[error("timed out waiting for STUN response from {0} after {1} attempts")]
    Timeout(SocketAddr, u32),
}

/// Builds a Binding Request with a fresh random transaction id.
/// Returns `(transaction_id, message_bytes)`.
fn build_binding_request() -> ([u8; 12], Vec<u8>) {
    let mut transaction_id = [0u8; 12];
    rand::thread_rng().fill(&mut transaction_id);

    let mut msg = Vec::with_capacity(HEADER_LEN);
    msg.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // message length: no attributes
    msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg.extend_from_slice(&transaction_id);
    (transaction_id, msg)
}

/// Parses a Binding Success Response, verifying it matches
/// `expected_transaction_id`, and extracts the mapped address.
fn parse_binding_response(
    data: &[u8],
    expected_transaction_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if data.len() < HEADER_LEN {
        return Err(StunError::TooShort);
    }
    let message_type = u16::from_be_bytes([data[0], data[1]]);
    if message_type != BINDING_SUCCESS_RESPONSE {
        return Err(StunError::NotBindingSuccess(message_type));
    }
    let message_length = u16::from_be_bytes([data[2], data[3]]) as usize;
    let magic_cookie = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if magic_cookie != MAGIC_COOKIE {
        return Err(StunError::BadMagicCookie);
    }
    let transaction_id = &data[8..20];
    if transaction_id != expected_transaction_id {
        return Err(StunError::TransactionIdMismatch);
    }

    let body_end = (HEADER_LEN + message_length).min(data.len());
    let mut attrs = &data[HEADER_LEN..body_end];
    let mut fallback_mapped_address: Option<SocketAddr> = None;

    while attrs.len() >= 4 {
        let attr_type = u16::from_be_bytes([attrs[0], attrs[1]]);
        let attr_len = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let padded_len = attr_len.div_ceil(4) * 4;
        if attrs.len() < 4 + attr_len {
            break; // truncated attribute; nothing more usable follows
        }
        let value = &attrs[4..4 + attr_len];

        match attr_type {
            XOR_MAPPED_ADDRESS => {
                return decode_xor_mapped_address(value, expected_transaction_id);
            }
            MAPPED_ADDRESS if fallback_mapped_address.is_none() => {
                fallback_mapped_address = decode_mapped_address(value).ok();
            }
            _ => {}
        }

        if attrs.len() < 4 + padded_len {
            break;
        }
        attrs = &attrs[4 + padded_len..];
    }

    fallback_mapped_address.ok_or(StunError::NoMappedAddress)
}

fn decode_xor_mapped_address(
    value: &[u8],
    transaction_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::TruncatedAddress);
    }
    let family = value[1];
    let xport = u16::from_be_bytes([value[2], value[3]]);
    let port = xport ^ ((MAGIC_COOKIE >> 16) as u16);

    match family {
        0x01 => {
            if value.len() < 8 {
                return Err(StunError::TruncatedAddress);
            }
            let xaddr = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
            let addr = xaddr ^ MAGIC_COOKIE;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port))
        }
        0x02 => {
            if value.len() < 20 {
                return Err(StunError::TruncatedAddress);
            }
            let mut cookie_and_tx = [0u8; 16];
            cookie_and_tx[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            cookie_and_tx[4..].copy_from_slice(transaction_id);
            let mut octets = [0u8; 16];
            for i in 0..16 {
                octets[i] = value[4 + i] ^ cookie_and_tx[i];
            }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => Err(StunError::UnknownFamily),
    }
}

fn decode_mapped_address(value: &[u8]) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::TruncatedAddress);
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]);
    match family {
        0x01 => {
            if value.len() < 8 {
                return Err(StunError::TruncatedAddress);
            }
            let octets = [value[4], value[5], value[6], value[7]];
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x02 => {
            if value.len() < 20 {
                return Err(StunError::TruncatedAddress);
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => Err(StunError::UnknownFamily),
    }
}

/// Queries `stun_server` over `socket` (which must already be the socket the
/// caller intends to reuse for QUIC afterward) and returns this socket's
/// address as observed by the STUN server. Retries a few times with a short
/// timeout each, since UDP queries can be silently dropped.
pub async fn query_stun(
    socket: &UdpSocket,
    stun_server: SocketAddr,
) -> Result<SocketAddr, StunError> {
    const ATTEMPTS: u32 = 3;
    const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(800);

    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        let (transaction_id, request) = build_binding_request();
        if let Err(e) = socket.send_to(&request, stun_server).await {
            last_err = Some(StunError::Io(e.to_string()));
            continue;
        }

        let mut buf = [0u8; 512];
        match tokio::time::timeout(PER_ATTEMPT_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from == stun_server => {
                match parse_binding_response(&buf[..n], &transaction_id) {
                    Ok(addr) => return Ok(addr),
                    Err(e) => last_err = Some(e),
                }
            }
            Ok(Ok(_)) => continue, // stray datagram from someone else; retry
            Ok(Err(e)) => last_err = Some(StunError::Io(e.to_string())),
            Err(_) => continue, // this attempt's timeout elapsed; retry
        }
    }
    Err(last_err.unwrap_or(StunError::Timeout(stun_server, ATTEMPTS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_binding_request_has_correct_header() {
        let (transaction_id, msg) = build_binding_request();
        assert_eq!(msg.len(), HEADER_LEN);
        assert_eq!(u16::from_be_bytes([msg[0], msg[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]), 0);
        assert_eq!(
            u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&msg[8..20], &transaction_id);
    }

    /// Hand-builds a Binding Success Response carrying an IPv4
    /// XOR-MAPPED-ADDRESS and checks it decodes to the expected address.
    #[test]
    fn parses_ipv4_xor_mapped_address() {
        let transaction_id = [1u8; 12];
        let addr: SocketAddr = "203.0.113.5:54321".parse().unwrap();
        let response = build_test_response_ipv4(&transaction_id, addr, XOR_MAPPED_ADDRESS);
        let decoded = parse_binding_response(&response, &transaction_id).unwrap();
        assert_eq!(decoded, addr);
    }

    #[test]
    fn parses_ipv6_xor_mapped_address() {
        let transaction_id = [2u8; 12];
        let addr: SocketAddr = "[2001:db8::1]:12345".parse().unwrap();
        let response = build_test_response_ipv6(&transaction_id, addr);
        let decoded = parse_binding_response(&response, &transaction_id).unwrap();
        assert_eq!(decoded, addr);
    }

    #[test]
    fn falls_back_to_legacy_mapped_address() {
        let transaction_id = [3u8; 12];
        let addr: SocketAddr = "198.51.100.9:9999".parse().unwrap();
        let response = build_test_response_ipv4(&transaction_id, addr, MAPPED_ADDRESS);
        let decoded = parse_binding_response(&response, &transaction_id).unwrap();
        assert_eq!(decoded, addr);
    }

    #[test]
    fn prefers_xor_mapped_address_when_both_present() {
        let transaction_id = [4u8; 12];
        let xor_addr: SocketAddr = "203.0.113.5:1111".parse().unwrap();
        let legacy_addr: SocketAddr = "203.0.113.6:2222".parse().unwrap();

        let mut response = stun_header(&transaction_id, 0);
        append_ipv4_attr(&mut response, MAPPED_ADDRESS, legacy_addr, None);
        append_ipv4_attr(&mut response, XOR_MAPPED_ADDRESS, xor_addr, Some(&transaction_id));
        set_message_length(&mut response);

        let decoded = parse_binding_response(&response, &transaction_id).unwrap();
        assert_eq!(decoded, xor_addr);
    }

    #[test]
    fn rejects_wrong_transaction_id() {
        let transaction_id = [5u8; 12];
        let other_transaction_id = [6u8; 12];
        let addr: SocketAddr = "203.0.113.5:1234".parse().unwrap();
        let response = build_test_response_ipv4(&transaction_id, addr, XOR_MAPPED_ADDRESS);
        assert_eq!(
            parse_binding_response(&response, &other_transaction_id),
            Err(StunError::TransactionIdMismatch)
        );
    }

    #[test]
    fn rejects_bad_magic_cookie() {
        let transaction_id = [7u8; 12];
        let addr: SocketAddr = "203.0.113.5:1234".parse().unwrap();
        let mut response = build_test_response_ipv4(&transaction_id, addr, XOR_MAPPED_ADDRESS);
        response[4] = 0xff; // corrupt magic cookie
        assert_eq!(
            parse_binding_response(&response, &transaction_id),
            Err(StunError::BadMagicCookie)
        );
    }

    #[test]
    fn rejects_non_success_message_type() {
        let transaction_id = [8u8; 12];
        let mut response = stun_header(&transaction_id, 0);
        response[0..2].copy_from_slice(&0x0111u16.to_be_bytes()); // Binding Error Response
        assert_eq!(
            parse_binding_response(&response, &transaction_id),
            Err(StunError::NotBindingSuccess(0x0111))
        );
    }

    #[test]
    fn rejects_too_short_response() {
        assert_eq!(parse_binding_response(&[0u8; 10], &[0u8; 12]), Err(StunError::TooShort));
    }

    #[test]
    fn rejects_response_with_no_mapped_address_attribute() {
        let transaction_id = [9u8; 12];
        let response = stun_header(&transaction_id, 0);
        assert_eq!(
            parse_binding_response(&response, &transaction_id),
            Err(StunError::NoMappedAddress)
        );
    }

    // ── test helpers: hand-build STUN messages byte-for-byte ──────────

    fn stun_header(transaction_id: &[u8; 12], message_length: u16) -> Vec<u8> {
        let mut msg = Vec::with_capacity(HEADER_LEN);
        msg.extend_from_slice(&BINDING_SUCCESS_RESPONSE.to_be_bytes());
        msg.extend_from_slice(&message_length.to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(transaction_id);
        msg
    }

    fn set_message_length(msg: &mut [u8]) {
        let len = (msg.len() - HEADER_LEN) as u16;
        msg[2..4].copy_from_slice(&len.to_be_bytes());
    }

    fn append_ipv4_attr(
        msg: &mut Vec<u8>,
        attr_type: u16,
        addr: SocketAddr,
        xor_transaction_id: Option<&[u8; 12]>,
    ) {
        let SocketAddr::V4(v4) = addr else { panic!("expected ipv4") };
        let (port, octets) = match xor_transaction_id {
            Some(_) => (
                v4.port() ^ ((MAGIC_COOKIE >> 16) as u16),
                u32::from(*v4.ip()) ^ MAGIC_COOKIE,
            ),
            None => (v4.port(), u32::from(*v4.ip())),
        };
        msg.extend_from_slice(&attr_type.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0); // reserved
        msg.push(0x01); // family: IPv4
        msg.extend_from_slice(&port.to_be_bytes());
        msg.extend_from_slice(&octets.to_be_bytes());
    }

    fn build_test_response_ipv4(
        transaction_id: &[u8; 12],
        addr: SocketAddr,
        attr_type: u16,
    ) -> Vec<u8> {
        let mut msg = stun_header(transaction_id, 0);
        let xor_tx = (attr_type == XOR_MAPPED_ADDRESS).then_some(transaction_id);
        append_ipv4_attr(&mut msg, attr_type, addr, xor_tx);
        set_message_length(&mut msg);
        msg
    }

    fn build_test_response_ipv6(transaction_id: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
        let SocketAddr::V6(v6) = addr else { panic!("expected ipv6") };
        let mut msg = stun_header(transaction_id, 0);

        let mut cookie_and_tx = [0u8; 16];
        cookie_and_tx[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        cookie_and_tx[4..].copy_from_slice(transaction_id);
        let mut xoctets = [0u8; 16];
        for (i, b) in v6.ip().octets().iter().enumerate() {
            xoctets[i] = b ^ cookie_and_tx[i];
        }
        let xport = v6.port() ^ ((MAGIC_COOKIE >> 16) as u16);

        msg.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&20u16.to_be_bytes());
        msg.push(0);
        msg.push(0x02); // family: IPv6
        msg.extend_from_slice(&xport.to_be_bytes());
        msg.extend_from_slice(&xoctets);
        set_message_length(&mut msg);
        msg
    }

    /// A minimal local STUN server: replies to every Binding Request with a
    /// Binding Success Response whose XOR-MAPPED-ADDRESS is the request's
    /// observed source address — exactly what a real STUN server does, and
    /// enough to exercise `query_stun()` end-to-end over real sockets rather
    /// than just unit-testing the encode/decode functions in isolation.
    async fn spawn_mock_stun_server() -> SocketAddr {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = server.recv_from(&mut buf).await else { break };
                if n < HEADER_LEN {
                    continue;
                }
                let transaction_id: [u8; 12] = buf[8..20].try_into().unwrap();
                let response = build_test_response_ipv4(&transaction_id, from, XOR_MAPPED_ADDRESS);
                let _ = server.send_to(&response, from).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn query_stun_returns_the_socket_observed_address_over_real_udp() {
        let stun_server = spawn_mock_stun_server().await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        let observed = query_stun(&client, stun_server).await.unwrap();
        assert_eq!(observed, client_addr);
    }

    #[tokio::test]
    async fn query_stun_times_out_when_server_is_unreachable() {
        // Nothing listens here: a bound-then-immediately-dropped socket's
        // port is very unlikely to have anything else answer on it.
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = probe.local_addr().unwrap();
        drop(probe);

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let result = query_stun(&client, dead_addr).await;
        assert!(result.is_err(), "querying an unreachable STUN server should fail, not hang forever");
    }
}
