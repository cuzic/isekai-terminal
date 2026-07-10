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
//! **Precondition this module cannot itself enforce**: both candidates
//! passed to [`race_direct_and_relay`] must resolve to the *same* underlying
//! `isekai-pipe serve` instance (the same `AttachArbiter`). If they don't,
//! both could be independently accepted (two different arbiters each
//! thinking they alone hold the winning attach), which defeats the fencing
//! guarantee `#18` exists to provide. Callers assembling `DirectRelayRaceTargets`
//! must ensure this — e.g. from a single `ConnectionIntent`/`HandshakeJson`
//! that advertises both a `stun_observed_addr` and a `relay_public_addr` for
//! the same helper process.

use std::future::Future;
use std::time::Duration;

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::session_id::SessionId;

use crate::attempt::AttemptFailure;
use crate::relay::{connect_and_handshake, random_session_id};
use crate::stun_p2p::{connect_stun_p2p_with_round, StunP2pTarget};
use crate::telemetry::CandidateIdentity;
use crate::traits::{ByteStream, QuicEndpointFactory};
use crate::types::{BindSpec, RemoteSpec};
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
    pub stream: Box<dyn ByteStream>,
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
/// Whichever future is still in flight when the other one wins is simply
/// dropped (cancelled in place) — no `CANCEL_ATTACH` is sent (this crate's
/// candidates share one QUIC connection factory call per attempt, not a
/// background task, so there is nothing left running to explicitly tell the
/// server about; the loser's connection attempt just stops making progress
/// once dropped, and the server's own `AttachArbiter` will eventually see
/// nothing more arrive on that lease if it wasn't already accepted).
pub async fn race_direct_and_relay(
    factory: &dyn QuicEndpointFactory,
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
    let make_relay_fut = || relay_attempt(factory, &targets.relay, session_id, generation, relay_identity);

    match race_two(direct_fut, relay_delay, make_relay_fut).await {
        Ok((RaceSlot::First, stream)) => Ok(RaceOutcome { winner: RaceWinner::Direct, stream }),
        Ok((RaceSlot::Second, stream)) => Ok(RaceOutcome { winner: RaceWinner::Relay, stream }),
        Err((direct, relay)) => Err(RaceConnectError { direct, relay }),
    }
}

/// Which of [`race_two`]'s two futures produced the value it returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RaceSlot {
    First,
    Second,
}

/// The generic Happy-Eyeballs-style two-way race behind
/// [`race_direct_and_relay`] (see this module's docs for why a staggered
/// start rather than launching both at once): run `first` immediately; if it
/// hasn't finished (succeeded *or* failed) within `second_delay`, construct
/// `second` (via `make_second`, called at most once — lazily, so a `first`
/// that wins outright never causes `second`'s setup, e.g. a socket bind, to
/// happen at all) and take whichever of the two finishes successfully first,
/// falling back to whichever is still in flight if the other one already
/// failed. Only errors if both ultimately fail.
///
/// No I/O of its own — operates on any two futures sharing a success type
/// `T` and this crate's [`AttemptFailure`], so it can be driven by cheap fake
/// futures under `tokio::time::pause()` in tests instead of real STUN/relay
/// connection attempts (`connect_stun_p2p_with_round`/`relay_attempt` call
/// directly into `stun_p2p_with_round`/`relay_attempt`'s own real sockets, so
/// racing *those* directly in a unit test isn't possible — this is the seam
/// that lets the scheduling logic be tested without them, per the note left
/// in `#66`).
async fn race_two<T, F1, F2>(
    first: F1,
    second_delay: Duration,
    make_second: impl FnOnce() -> F2,
) -> Result<(RaceSlot, T), (AttemptFailure, AttemptFailure)>
where
    F1: Future<Output = Result<T, AttemptFailure>>,
    F2: Future<Output = Result<T, AttemptFailure>>,
{
    tokio::pin!(first);

    // Phase 1: give `first` a head start. If it finishes (either way) before
    // `second_delay` elapses, `second` never even starts.
    if let Ok(first_result) = tokio::time::timeout(second_delay, first.as_mut()).await {
        return match first_result {
            Ok(v) => Ok((RaceSlot::First, v)),
            Err(first_err) => match make_second().await {
                Ok(v) => Ok((RaceSlot::Second, v)),
                Err(second_err) => Err((first_err, second_err)),
            },
        };
    }

    // Phase 2: `first` is still running past the stagger window; `second`
    // joins the race. Whichever succeeds first wins; if the first to finish
    // failed, keep waiting on the other one rather than giving up
    // immediately (both must fail for this to be an error).
    let second = make_second();
    tokio::pin!(second);

    tokio::select! {
        first_result = first.as_mut() => {
            match first_result {
                Ok(v) => Ok((RaceSlot::First, v)),
                Err(first_err) => match second.as_mut().await {
                    Ok(v) => Ok((RaceSlot::Second, v)),
                    Err(second_err) => Err((first_err, second_err)),
                },
            }
        }
        second_result = second.as_mut() => {
            match second_result {
                Ok(v) => Ok((RaceSlot::Second, v)),
                Err(second_err) => match first.as_mut().await {
                    Ok(v) => Ok((RaceSlot::First, v)),
                    Err(first_err) => Err((first_err, second_err)),
                },
            }
        }
    }
}

async fn relay_attempt(
    factory: &dyn QuicEndpointFactory,
    target: &RelayTarget,
    session_id: SessionId,
    generation: ConnectionGeneration,
    identity: CandidateIdentity<'_>,
) -> Result<Box<dyn ByteStream>, AttemptFailure> {
    let endpoint = factory
        .create_endpoint(BindSpec::any_ipv4())
        .await
        .map_err(|source| AttemptFailure::RetryablePreAttach { source })?;
    let remote =
        RemoteSpec { addr: target.helper_addr, server_name: target.server_name.clone(), cert_sha256_hex: target.cert_sha256_hex.clone() };
    // No resume support in the race path yet (module docs: minimal scope) —
    // `0` means "no preference".
    connect_and_handshake(endpoint.as_ref(), remote, &target.session_secret, session_id, generation, 0, identity)
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

#[cfg(test)]
mod race_two_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::TransportError;

    fn some_failure() -> AttemptFailure {
        AttemptFailure::Terminal { source: TransportError::UnexpectedEof }
    }

    async fn after(delay: Duration, result: Result<i32, AttemptFailure>) -> Result<i32, AttemptFailure> {
        tokio::time::sleep(delay).await;
        result
    }

    #[tokio::test(start_paused = true)]
    async fn first_wins_before_the_delay_and_second_never_starts() {
        let second_started = AtomicUsize::new(0);

        let outcome = race_two(
            after(Duration::from_millis(10), Ok(1)),
            Duration::from_millis(1000),
            || {
                second_started.fetch_add(1, Ordering::SeqCst);
                after(Duration::from_millis(10), Ok(2))
            },
        )
        .await;

        assert!(matches!(outcome, Ok((RaceSlot::First, 1))));
        assert_eq!(second_started.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn second_wins_after_first_fails_within_the_delay_window() {
        let outcome = race_two(
            after(Duration::from_millis(10), Err(some_failure())),
            Duration::from_millis(1000),
            || after(Duration::from_millis(10), Ok(2)),
        )
        .await;

        assert!(matches!(outcome, Ok((RaceSlot::Second, 2))));
    }

    #[tokio::test(start_paused = true)]
    async fn both_fail_within_the_delay_window() {
        let outcome = race_two(
            after(Duration::from_millis(10), Err(some_failure())),
            Duration::from_millis(1000),
            || after(Duration::from_millis(10), Err(some_failure())),
        )
        .await;

        assert!(matches!(outcome, Err((AttemptFailure::Terminal { .. }, AttemptFailure::Terminal { .. }))));
    }

    #[tokio::test(start_paused = true)]
    async fn second_wins_in_phase_two_while_first_is_still_in_flight() {
        let outcome = race_two(
            after(Duration::from_millis(500), Ok(1)),
            Duration::from_millis(50),
            || after(Duration::from_millis(10), Ok(2)),
        )
        .await;

        assert!(matches!(outcome, Ok((RaceSlot::Second, 2))));
    }

    #[tokio::test(start_paused = true)]
    async fn first_wins_late_in_phase_two_after_second_already_failed() {
        let outcome = race_two(
            after(Duration::from_millis(200), Ok(5)),
            Duration::from_millis(50),
            || after(Duration::from_millis(10), Err(some_failure())),
        )
        .await;

        assert!(matches!(outcome, Ok((RaceSlot::First, 5))));
    }

    #[tokio::test(start_paused = true)]
    async fn both_fail_in_phase_two() {
        let outcome = race_two(
            after(Duration::from_millis(100), Err(some_failure())),
            Duration::from_millis(50),
            || after(Duration::from_millis(10), Err(some_failure())),
        )
        .await;

        assert!(matches!(outcome, Err((AttemptFailure::Terminal { .. }, AttemptFailure::Terminal { .. }))));
    }
}
