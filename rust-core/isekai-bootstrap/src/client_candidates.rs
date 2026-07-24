//! Client-side STUN self-observation for `BootstrapRequestV2`'s
//! `client_candidates` (`#20b`). Shared by `OpenSshBackend` (CLI,
//! `openssh.rs`) and `rust-core/src/helper_bootstrap.rs` (Android) so both
//! sides collect and package candidates identically (isekai-terminal-core/
//! isekai-ssh crate共有化 Phase 2c).

use std::net::SocketAddr;

use isekai_protocol::bootstrap_request::{BootstrapCandidateV2, BootstrapRequestV2, BOOTSTRAP_PROTOCOL_V2};
use isekai_protocol::session_id::{SessionId, SESSION_ID_LEN};
use isekai_protocol::BootstrapAttemptId;
use rand::RngCore;

/// How long a bootstrap-discovered client candidate is claimed valid for
/// (`BootstrapCandidateV2::valid_for_ms`) — comfortably longer than a single
/// bootstrap round trip (typically well under a second on loopback, at most a
/// few seconds over a real network) but far under
/// `isekai_protocol::bootstrap_request::MAX_CANDIDATE_VALID_FOR_MS` (5
/// minutes), which the receiver clamps to regardless.
pub const CLIENT_CANDIDATE_VALID_FOR_MS: u64 = 30_000;

/// Queries each of `stun_servers` for this side's own observed address — a
/// fresh, throwaway UDP socket per query (discovery only, never reused for
/// an actual QUIC dial later) — producing one [`BootstrapCandidateV2`] per
/// server that answered (`#20b`). A server that fails to respond
/// (unreachable, timed out, local bind failure) is logged and skipped rather
/// than failing the whole bootstrap attempt: the server-side punch loop
/// (`isekai-pipe serve`, `#20a-3`) simply gets fewer candidates to try, and
/// the existing `direct-by-bootstrap-host`/relay candidates are unaffected
/// either way.
pub async fn collect_client_stun_candidates(stun_servers: &[SocketAddr]) -> Vec<BootstrapCandidateV2> {
    let mut candidates = Vec::with_capacity(stun_servers.len());
    for &stun_server in stun_servers {
        let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            Ok(socket) => socket,
            Err(e) => {
                log::warn!(
                    "isekai-bootstrap: failed to bind a UDP socket to query STUN server {stun_server}, skipping: {e}"
                );
                continue;
            }
        };
        match isekai_stun::query_stun(&socket, stun_server).await {
            Ok(observed_addr) => candidates.push(BootstrapCandidateV2 {
                route: "stun-p2p".to_string(),
                endpoint: observed_addr.to_string(),
                valid_for_ms: CLIENT_CANDIDATE_VALID_FOR_MS,
            }),
            Err(e) => {
                log::warn!("isekai-bootstrap: STUN query to {stun_server} failed, skipping this candidate: {e}");
            }
        }
    }
    candidates
}

/// Builds a fresh `BootstrapRequestV2` with randomly-generated
/// `session_id`/`bootstrap_attempt_id` and real client candidates collected
/// from `stun_servers` (`#20b`, [`collect_client_stun_candidates`]). See
/// `isekai_protocol::bootstrap_request`'s module docs for why the
/// session/attempt identifiers are generated here, independent of any ATTACH
/// v2 fencing identity a later QUIC connection attempt will use.
pub async fn fresh_bootstrap_request_v2(stun_servers: &[SocketAddr]) -> BootstrapRequestV2 {
    let mut session_id_bytes = [0u8; SESSION_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut session_id_bytes);
    let mut attempt_id_bytes = [0u8; isekai_protocol::bootstrap_request::BOOTSTRAP_ATTEMPT_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut attempt_id_bytes);

    BootstrapRequestV2 {
        v: BOOTSTRAP_PROTOCOL_V2,
        session_id: SessionId::from_bytes(session_id_bytes).to_hex(),
        bootstrap_attempt_id: BootstrapAttemptId::from_bytes(attempt_id_bytes).to_hex(),
        client_candidates: collect_client_stun_candidates(stun_servers).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same minimal mock STUN server (RFC 5389 Binding Request/Response) used
    /// throughout this workspace (`isekai-pipe/tests/serve_e2e.rs`,
    /// `isekai-transport/tests/stun_p2p_e2e.rs`) — duplicated per this
    /// crate's own convention rather than shared.
    async fn spawn_mock_stun_server() -> SocketAddr {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = server.recv_from(&mut buf).await else { break };
                if n < 20 {
                    continue;
                }
                let transaction_id = &buf[8..20];
                let SocketAddr::V4(from_v4) = from else { continue };

                let magic_cookie: u32 = 0x2112_A442;
                let xport = from_v4.port() ^ ((magic_cookie >> 16) as u16);
                let xaddr = u32::from(*from_v4.ip()) ^ magic_cookie;

                let mut resp = Vec::with_capacity(32);
                resp.extend_from_slice(&0x0101u16.to_be_bytes());
                resp.extend_from_slice(&12u16.to_be_bytes());
                resp.extend_from_slice(&magic_cookie.to_be_bytes());
                resp.extend_from_slice(transaction_id);
                resp.extend_from_slice(&0x0020u16.to_be_bytes());
                resp.extend_from_slice(&8u16.to_be_bytes());
                resp.push(0);
                resp.push(0x01);
                resp.extend_from_slice(&xport.to_be_bytes());
                resp.extend_from_slice(&xaddr.to_be_bytes());

                let _ = server.send_to(&resp, from).await;
            }
        });
        addr
    }

    /// Binds a UDP socket that never answers, to simulate an unreachable STUN
    /// server. The caller must keep the returned socket alive for as long as
    /// the "dead" address needs to stay dead: dropping it immediately frees
    /// the ephemeral port, which the OS can then hand to the very next
    /// `bind("127.0.0.1:0")` call in the same test (observed on real Windows
    /// CI, `#33`) — silently turning the "dead" address into a live one.
    async fn dead_stun_server() -> (SocketAddr, tokio::net::UdpSocket) {
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        (addr, probe)
    }

    #[tokio::test]
    async fn collect_client_stun_candidates_yields_one_per_responding_server() {
        let stun_a = spawn_mock_stun_server().await;
        let stun_b = spawn_mock_stun_server().await;

        let candidates = collect_client_stun_candidates(&[stun_a, stun_b]).await;

        assert_eq!(candidates.len(), 2);
        for candidate in &candidates {
            assert_eq!(candidate.route, "stun-p2p");
            assert_eq!(candidate.valid_for_ms, CLIENT_CANDIDATE_VALID_FOR_MS);
            let addr: SocketAddr = candidate.endpoint.parse().expect("endpoint should be a valid socket address");
            assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
        }
    }

    #[tokio::test]
    async fn collect_client_stun_candidates_skips_unreachable_servers_without_failing() {
        let (dead, _dead_socket) = dead_stun_server().await;
        let real = spawn_mock_stun_server().await;

        let candidates = collect_client_stun_candidates(&[dead, real]).await;

        assert_eq!(candidates.len(), 1, "the dead server should be skipped, not fail the whole collection");
    }

    #[tokio::test]
    async fn collect_client_stun_candidates_is_empty_for_no_servers() {
        assert!(collect_client_stun_candidates(&[]).await.is_empty());
    }

    #[tokio::test]
    async fn fresh_bootstrap_request_v2_carries_the_collected_candidates() {
        let real = spawn_mock_stun_server().await;
        let request = fresh_bootstrap_request_v2(&[real]).await;

        assert_eq!(request.v, BOOTSTRAP_PROTOCOL_V2);
        assert!(request.session_id().is_ok());
        assert!(request.bootstrap_attempt_id().is_ok());
        assert_eq!(request.client_candidates.len(), 1);
    }
}
