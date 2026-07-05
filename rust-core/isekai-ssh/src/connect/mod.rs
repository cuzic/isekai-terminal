//! `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` "接続シーケンス", `connect`
//! side). Phase S-2 scope: resolve the target isekai-helper via the trust
//! store (`isekai-trust`, `~/.config/isekai-ssh/known_helpers.toml`) instead
//! of the dev-only bypass. `init`/`--via`-driven re-deployment (S-3) is still
//! out of scope.
//!
//! Split across three submodules, each independently documenting its own
//! corner of the flow:
//! - `resolve`: turns `ConnectArgs` into a `ResolvedTarget`, either via the
//!   trust store or the dev-only bypass. No network I/O.
//! - `relay_pump`: `--mode relay`'s resumable connect+relay lifecycle
//!   (`run_relay_resumable`), including the C2H replay buffer and
//!   reconnect-on-disconnect loop.
//! - `stun_pump`: `--mode stun`'s simple, non-resumable one-shot relay
//!   (`relay_stdio`) — see that module's docs for why it stays a separate,
//!   simpler implementation rather than sharing `relay_pump`'s machinery.
//!
//! Phase S-6 adds `--mode <relay|stun>` (`ConnectArgs::mode`, default
//! `relay`): which of `isekai-transport`'s two HELLO/proof/ACK paths
//! (`connect_via_relay` vs `connect_stun_p2p`) actually reaches the target
//! isekai-helper. **The trust store schema itself is unchanged** — see
//! `resolve::resolve_stun_from_trust_store`'s docs for how its
//! `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` fields
//! are reinterpreted for STUN mode. `--mode stun` carries a known,
//! documented limitation (no recovery from NAT mapping loss mid-session,
//! `ISEKAI_SSH_DESIGN.md` "isekai-sshでのNAT越え方式の既定") that `run` warns
//! about on stderr every time it is used.
//!
//! Phase S-4c adds resume support to `--mode relay` (the default): instead of
//! the simple, non-resumable `relay_stdio` used for `--mode stun`,
//! `relay_pump::run_relay_resumable` opens a control stream, keeps a C2H
//! replay buffer (`crate::resume::C2hReplayBuffer`), and transparently
//! reconnects (`isekai_transport::reconnect_and_resume`) if the QUIC
//! connection is lost — `ssh`'s stdin/stdout pipes are never closed just
//! because the underlying QUIC connection died (`ISEKAI_SSH_DESIGN.md`
//! "resume を ProxyCommand の背後に隠す"). `--mode stun` is **not**
//! resume-capable yet (`isekai-transport`'s `resume` module is only wired up
//! for `RelayTarget` today) and keeps using the original one-shot
//! `relay_stdio`.
//!
//! Phase S-4d makes the resume window itself configurable
//! (`ConnectArgs::resume_window`, still defaulting to 120s to match
//! isekai-helper's own `--resume-window`) and makes
//! `relay_pump::run_relay_resumable`'s give-up path deliberate rather than
//! incidental: on top of the process simply exiting (which lets the OS close
//! stdio as a side effect), it now explicitly shuts down stdout and drops
//! stdin first, and always prints an stderr message (`eprintln!`, not
//! `log::warn!`, so it is visible regardless of `RUST_LOG` — matching the
//! `--mode stun` warning above) saying by how much the resume window was
//! exceeded.
//!
//! **stdout purity is the load-bearing invariant of this whole module**: this
//! process is invoked as `ssh`'s `ProxyCommand`, so anything written to
//! stdout other than bytes read from the QUIC stream corrupts the SSH
//! session from `ssh`'s point of view. All diagnostics go through the `log`
//! crate (routed to stderr by `main.rs`'s `env_logger` setup), and trust
//! store resolution failures are turned into plain `anyhow::Error`s that
//! `main.rs` prints to stderr directly — never to stdout.

mod relay_pump;
mod resolve;
mod stun_pump;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use isekai_transport::{connect_stun_p2p, RelayTarget, StunP2pTarget};

use crate::cli::{ConnectArgs, ConnectMode};

pub use resolve::TrustNotInitialized;

/// Which of `resolve_target`'s paths produced the target. Only used to pick
/// the right error message if the subsequent HELLO/proof/ACK fails — see
/// `run`'s use of it.
enum TargetSource {
    /// Debug + `dev-insecure`-feature builds only (`cli.rs`); does not exist
    /// in a release binary. Always carries a `RelayTarget` regardless of
    /// `--mode` — see `resolve::dev_insecure_target`'s docs.
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    DevInsecureBypass,
    TrustStore,
}

/// A resolved connection target, tagged by which `isekai-transport`
/// HELLO/proof/ACK path (`ConnectArgs::mode`) it must be handed to. Built by
/// `resolve_target`; `run` matches on this to call `connect_via_relay` or
/// `connect_stun_p2p`.
enum ResolvedTarget {
    Relay(RelayTarget),
    Stun { stun_server: SocketAddr, target: StunP2pTarget },
}

pub async fn run(args: ConnectArgs) -> Result<()> {
    if args.mode == ConnectMode::Stun {
        // Known, documented limitation (`ISEKAI_SSH_DESIGN.md`
        // "isekai-sshでのNAT越え方式の既定"): STUN+SSH rendezvous P2P has no
        // relay fallback once established, so a NAT mapping change mid-
        // session (Wi-Fi<->cellular tethering roaming, etc.) simply ends the
        // session — there is nothing to reconnect to. Deliberately
        // `eprintln!`, not `log::warn!`: this warning must be visible every
        // time `--mode stun` is used regardless of `RUST_LOG` (unlike this
        // module's other, log-level-gated diagnostics) — it goes to stderr
        // either way, so this module's stdout-purity invariant still holds.
        eprintln!(
            "isekai-ssh: --mode stun in use for '{host}' — this session cannot recover from NAT mapping \
             loss (e.g. Wi-Fi<->cellular tethering roaming): unlike the default --mode relay, there is no \
             relay fallback path once the QUIC connection to isekai-helper is lost this way. Use the \
             default --mode relay if session resilience matters more than avoiding the relay hop.",
            host = args.host,
        );
    }

    let (target, source) = resolve_target(&args)?;

    match target {
        ResolvedTarget::Relay(relay_target) => {
            let resume_window = Duration::from_secs(args.resume_window);
            relay_pump::run_relay_resumable(relay_target, &args.host, &source, resume_window).await
        }
        ResolvedTarget::Stun { stun_server, target } => {
            log::info!(
                "isekai-ssh: connecting to isekai-helper at {} via STUN+SSH rendezvous P2P (--mode stun, \
                 stun_server={stun_server})",
                target.peer_addr
            );
            let stream = connect_stun_p2p(stun_server, &target)
                .await
                .map(|conn| conn.stream)
                .with_context(|| stun_hello_failure_message(&args.host))?;

            log::info!("isekai-ssh: HELLO/ACK complete — relaying stdin/stdout <-> QUIC");
            stun_pump::relay_stdio(stream).await
        }
    }
}

fn relay_hello_failure_message(host: &str, source: &TargetSource) -> String {
    match source {
        // Cached trust-store credentials are only valid until isekai-helper
        // restarts (its session_secret changes on restart). A HELLO/proof
        // rejection here is exactly that signal; today this just fails
        // closed with actionable guidance, but it is also the intended
        // future trigger for the `--via`-driven automatic re-deployment
        // fallback described in `ISEKAI_SSH_DESIGN.md`'s "CLIコマンド構成" /
        // "オープンな課題" (not implemented yet — S-3).
        TargetSource::TrustStore => format!(
            "isekai-ssh: HELLO/proof/ACK with isekai-helper for '{host}' failed using cached trust-store \
             credentials (--mode relay). isekai-helper may have restarted since the last `isekai-ssh init` \
             (its session_secret changes on restart) — re-run `isekai-ssh init {host}` to refresh trust. \
             (Automatic re-deployment via --via on this failure is not implemented yet, \
             ISEKAI_SSH_DESIGN.md フェーズ分割案 S-3.)"
        ),
        #[cfg(all(debug_assertions, feature = "dev-insecure"))]
        TargetSource::DevInsecureBypass => {
            "isekai-ssh: failed to establish HELLO/proof/ACK with isekai-helper".to_string()
        }
    }
}

// Distinct wording from `relay_hello_failure_message`: a HELLO/proof failure
// here is much more likely to mean "simultaneous open never punched through"
// than "isekai-helper restarted", so point the user at the fallback that
// doesn't depend on hole punching at all. `--mode stun` always resolves via
// the trust store (dev-insecure bypass "is always relay-mode regardless of
// --mode", `resolve::dev_insecure_target`'s docs), so unlike
// `relay_hello_failure_message` this needs no `TargetSource` match.
fn stun_hello_failure_message(host: &str) -> String {
    format!(
        "isekai-ssh: HELLO/proof/ACK with isekai-helper for '{host}' failed over STUN+SSH rendezvous \
         P2P (--mode stun). NAT越えが不成立だった可能性がある — this can happen when hole punching \
         does not succeed (e.g. symmetric NAT on either side) or the trust store's cached STUN-observed \
         address for isekai-helper is stale. `--mode relay`への切り替えを検討してください: re-run with \
         `--mode relay` (the default), which does not depend on simultaneous open succeeding. If the \
         cached address itself is stale, re-run `isekai-ssh init {host}` to refresh trust."
    )
}

/// Resolves `args.host` (and, for `--mode stun`, `args.stun_server`) to a
/// `ResolvedTarget`, either via the dev-only bypass (debug +
/// `dev-insecure`-feature builds only, see `cli.rs`) or — the path every
/// other build takes — via the trust store
/// (`~/.config/isekai-ssh/known_helpers.toml`). No network I/O happens in
/// this function; an unregistered host fails closed here, before any QUIC
/// connection attempt or stdout write.
fn resolve_target(args: &ConnectArgs) -> Result<(ResolvedTarget, TargetSource)> {
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    {
        if let Some(target) = resolve::dev_insecure_target(args)? {
            log::warn!(
                "isekai-ssh: using --dev-insecure-* bypass instead of the trust store for host '{}' \
                 (debug + dev-insecure build only; this path does not exist in a release binary; always \
                 relay-mode regardless of --mode, see resolve::dev_insecure_target's docs)",
                args.host
            );
            return Ok((ResolvedTarget::Relay(target), TargetSource::DevInsecureBypass));
        }
    }

    match args.mode {
        ConnectMode::Relay => {
            let target = resolve::resolve_relay_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Relay(target), TargetSource::TrustStore))
        }
        ConnectMode::Stun => {
            // clap's `required_if_eq("mode", "stun")` on `ConnectArgs::stun_server`
            // guarantees this is `Some` by the time argument parsing succeeds.
            let stun_server = args
                .stun_server
                .context("isekai-ssh: --stun-server is required with --mode stun (should be enforced by clap)")?;
            let target = resolve::resolve_stun_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Stun { stun_server, target }, TargetSource::TrustStore))
        }
    }
}
