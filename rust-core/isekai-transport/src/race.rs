//! Minimal direct(STUN P2P)/relay two-path race (`#19`, ChatGPT
//! second-opinion consultation 2026-07-08 3rd round).
//!
//! Deliberately narrow scope, matching `ISEKAI_PIPE_DESIGN.md` task #19's own
//! description: exactly two candidates (one direct/STUN, one relay), a
//! **direct-first staggered start** rather than launching both
//! simultaneously (RFC 8305 "Happy Eyeballs"-style — launching both at once
//! tends to let the topologically-shorter relay path win essentially every
//! time, since it usually completes its QUIC handshake faster than a STUN
//! query + hole-punch + handshake, defeating the point of preferring direct
//! connectivity when it's available), and **no generation-retry loop** —
//! unlike `#25`'s sequential fallback connector, if both candidates fail
//! this returns an error immediately rather than advancing the generation
//! and trying again. Generalizing this to N candidates and/or wiring in
//! `#25`'s retry behavior is deferred to `#13b`'s evaluation, once real
//! telemetry says it's worth the complexity.
//!
//! The actual staggered-race mechanics (`tokio::select!` over two futures
//! with a delayed second start) don't depend on QUIC/mux types or any
//! isekai-specific type — that part moved to `quicmux::race_with_stagger`
//! once it became clear it was a pure async combinator. This module keeps
//! everything that *is* isekai-specific: the shared fencing identity
//! (`session_id`/`generation`, `#18`), `AttemptFailure` classification, and
//! `RaceWinner`/`RaceConnectError`.
//!
//! **Precondition this module cannot itself enforce**: both candidates
//! passed to [`race_direct_and_relay`] must resolve to the *same* underlying
//! `isekai-pipe serve` instance (the same `AttachArbiter`). If they don't,
//! both could be independently accepted (two different arbiters each
//! thinking they alone hold the winning attach), which defeats the fencing
//! guarantee `#18` exists to provide. Callers assembling `DirectRelayRaceTargets`
//! must ensure this — e.g. from a single `ConnectionIntent`/`HandshakeJson`
//! that advertises both a `stun_observed_addr` and a `relay_public_addr` for
//! the same helper process.

use std::time::Duration;

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::session_id::SessionId;
use quicmux::{AnyByteStream, AnyMuxFactory, RemoteSpec, Winner};

use crate::attempt::AttemptFailure;
use crate::relay::{connect_and_handshake, random_session_id};
use crate::stun_p2p::{connect_stun_p2p_with_round, StunP2pTarget};
use crate::telemetry::CandidateIdentity;
use crate::RelayTarget;

/// Default stagger before the relay candidate joins the race, if the direct
/// candidate hasn't already finished (succeeded or failed) by then. `250ms`
/// mirrors RFC 8305 Happy Eyeballs v2's own default connection-attempt delay
/// — a reasonable starting point, not a value validated against this
/// project's own telemetry yet (`#13a`/`#13b`'s job).
pub const DEFAULT_RELAY_DELAY: Duration = Duration::from_millis(250);

/// Both candidates `race_direct_and_relay` needs. See module docs for the
/// "same underlying helper" precondition this type cannot itself enforce.
#[derive(Debug, Clone)]
pub struct DirectRelayRaceTargets {
    pub stun_server: std::net::SocketAddr,
    pub direct: StunP2pTarget,
    pub relay: RelayTarget,
}

/// Which candidate actually won the race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaceWinner {
    Direct,
    Relay,
}

/// The race's result: the winning candidate's raw byte stream, ready for
/// pass-through exactly like a single-candidate `connect_via_relay`/
/// `connect_stun_p2p` call — plus which one won, for logging/telemetry.
pub struct RaceOutcome {
    pub winner: RaceWinner,
    pub stream: AnyByteStream,
}

/// Both candidates failed. `LostRace` (`ALREADY_ATTACHED`) is deliberately
/// *not* a case here — a candidate that loses the race server-side reports
/// success or a pre-attach/ambiguous/terminal failure to this module, never
/// makes it back to the caller as "the reason we lost" unless the *other*
/// candidate also failed (see module docs on why: only the losing local
/// future's failure matters once the other side has already won).
#[derive(Debug)]
pub struct RaceConnectError {
    pub direct: AttemptFailure,
    pub relay: AttemptFailure,
}

impl std::fmt::Display for RaceConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "both candidates failed: [direct: {}] [relay: {}]", self.direct, self.relay)
    }
}

impl std::error::Error for RaceConnectError {}

/// Races `targets.direct` against `targets.relay`, starting direct
/// immediately and relay only after `relay_delay` has elapsed without direct
/// finishing (success *or* failure) — see module docs for why simultaneous
/// launch would be self-defeating. Both candidates share one
/// `session_id`/`generation` (`ConnectionGeneration::INITIAL` — this
/// function never retries with a new generation; see module docs), so the
/// server's `AttachArbiter` sees them as one race within one round and lets
/// whichever it authenticates first win (`ALREADY_ATTACHED` for the loser),
/// exactly the guarantee `#18`'s fencing exists to provide.
///
/// The actual racing (staggered start, waiting on the other candidate if the
/// first to finish failed) is [`quicmux::race_with_stagger`] — this function
/// only supplies the two candidate futures and translates its
/// backend-agnostic `Winner::A`/`Winner::B` back into this module's own
/// `RaceWinner::Direct`/`RaceWinner::Relay`. Whichever future is still in
/// flight when the other one wins is simply dropped (cancelled in place) —
/// no `CANCEL_ATTACH` is sent (this crate's candidates share one QUIC
/// connection factory call per attempt, not a background task, so there is
/// nothing left running to explicitly tell the server about; the loser's
/// connection attempt just stops making progress once dropped, and the
/// server's own `AttachArbiter` will eventually see nothing more arrive on
/// that lease if it wasn't already accepted).
pub async fn race_direct_and_relay(
    factory: &AnyMuxFactory,
    targets: &DirectRelayRaceTargets,
    relay_delay: Duration,
) -> Result<RaceOutcome, RaceConnectError> {
    let session_id = random_session_id();
    let generation = ConnectionGeneration::INITIAL;

    let direct_identity = CandidateIdentity { kind: "stun-p2p", source: "race", provider: "race", id: "direct" };
    let direct_fut = async {
        connect_stun_p2p_with_round(factory, targets.stun_server, &targets.direct, session_id, generation, direct_identity)
            .await
            .map(|conn| conn.stream)
    };

    let relay_identity = CandidateIdentity { kind: "relay", source: "race", provider: "race", id: "relay" };
    let relay_fut = relay_attempt(factory, &targets.relay, session_id, generation, relay_identity);

    match quicmux::race_with_stagger(direct_fut, relay_fut, relay_delay).await {
        Ok((Winner::A, stream)) => Ok(RaceOutcome { winner: RaceWinner::Direct, stream }),
        Ok((Winner::B, stream)) => Ok(RaceOutcome { winner: RaceWinner::Relay, stream }),
        Err((direct, relay)) => Err(RaceConnectError { direct, relay }),
    }
}

async fn relay_attempt(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    session_id: SessionId,
    generation: ConnectionGeneration,
    identity: CandidateIdentity<'_>,
) -> Result<AnyByteStream, AttemptFailure> {
    let endpoint = factory
        .create_endpoint(quicmux::BindSpec::any_ipv4())
        .await
        .map_err(|source| AttemptFailure::RetryablePreAttach { source: crate::error::TransportError::Mux(source) })?;
    let remote =
        RemoteSpec { addr: target.helper_addr, server_name: target.server_name.clone(), cert_sha256_hex: target.cert_sha256_hex.clone() };
    // No resume support in the race path yet (module docs: minimal scope) —
    // `0` means "no preference".
    connect_and_handshake(&endpoint, remote, &target.session_secret, session_id, generation, 0, identity)
        .await
        .map(|(_conn, stream, _proof, _grace)| stream)
        .map_err(AttemptFailure::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_connect_error_display_includes_both_failures() {
        use crate::error::TransportError;
        let err = RaceConnectError {
            direct: AttemptFailure::RetryablePreAttach { source: TransportError::UnexpectedEof },
            relay: AttemptFailure::Terminal { source: TransportError::UnexpectedEof },
        };
        let rendered = err.to_string();
        assert!(rendered.contains("direct"));
        assert!(rendered.contains("relay"));
    }
}
