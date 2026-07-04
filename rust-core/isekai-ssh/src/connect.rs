//! `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` "接続シーケンス", `connect`
//! side). Phase S-2 scope: resolve the target isekai-helper via the trust
//! store (`isekai-trust`, `~/.config/isekai-ssh/known_helpers.toml`) instead
//! of the dev-only bypass. `init`/`--via`-driven re-deployment (S-3) is still
//! out of scope.
//!
//! Phase S-6 adds `--mode <relay|stun>` (`ConnectArgs::mode`, default
//! `relay`): which of `isekai-transport`'s two HELLO/proof/ACK paths
//! (`connect_via_relay` vs `connect_stun_p2p`) actually reaches the target
//! isekai-helper. **The trust store schema itself is unchanged** — see
//! `resolve_stun_from_trust_store`'s docs for how its
//! `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` fields
//! are reinterpreted for STUN mode. `--mode stun` carries a known,
//! documented limitation (no recovery from NAT mapping loss mid-session,
//! `ISEKAI_SSH_DESIGN.md` "isekai-sshでのNAT越え方式の既定") that `run` warns
//! about on stderr every time it is used.
//!
//! Phase S-4c adds resume support to `--mode relay` (the default): instead of
//! the simple, non-resumable `relay_stdio` used for `--mode stun`,
//! `run_relay_resumable` opens a control stream, keeps a C2H replay buffer
//! (`crate::resume::C2hReplayBuffer`), and transparently reconnects
//! (`isekai_transport::reconnect_and_resume`) if the QUIC connection is lost
//! — `ssh`'s stdin/stdout pipes are never closed just because the
//! underlying QUIC connection died (`ISEKAI_SSH_DESIGN.md` "resume を
//! ProxyCommand の背後に隠す"). `--mode stun` is **not** resume-capable yet
//! (`isekai-transport`'s `resume` module is only wired up for `RelayTarget`
//! today) and keeps using the original one-shot `relay_stdio`.
//!
//! **stdout purity is the load-bearing invariant of this whole module**: this
//! process is invoked as `ssh`'s `ProxyCommand`, so anything written to
//! stdout other than bytes read from the QUIC stream corrupts the SSH
//! session from `ssh`'s point of view. All diagnostics go through the `log`
//! crate (routed to stderr by `main.rs`'s `env_logger` setup), and trust
//! store resolution failures are turned into plain `anyhow::Error`s that
//! `main.rs` prints to stderr directly — never to stdout.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(all(debug_assertions, feature = "dev-insecure"))]
use anyhow::bail;
use anyhow::{Context, Result};
use isekai_transport::{
    connect_stun_p2p, connect_via_relay_resumable, reconnect_and_resume, spawn_app_ack_tasks, AppAckCounters,
    BackoffPolicy, ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, C2hSentOffset, H2cClientDeliveredOffset,
    RelayTarget, StunP2pTarget, SystemQuicEndpointFactory,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::{ConnectArgs, ConnectMode};
use crate::resume::C2hReplayBuffer;

/// Upper bound on unconfirmed C2H bytes kept in memory
/// (`ISEKAI_SSH_DESIGN.md`'s C2H replay buffer). Matches isekai-helper's own
/// `DEFAULT_RESUME_BUFFER_SIZE` (`isekai-helper/src/main.rs`) so neither side
/// is the tighter bottleneck.
const C2H_REPLAY_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;

/// How long `run_relay_resumable` keeps attempting to resume after the QUIC
/// connection is lost before giving up and letting the process exit
/// (`ISEKAI_SSH_DESIGN.md`: "resume window...既定120秒", matching
/// isekai-helper's own `--resume-window` default). This is intentionally the
/// *minimal* give-up policy the task calls for ("一定回数/時間で諦めて
/// プロセスを終了する" — a more careful close-down sequence is S-4d's scope).
const RESUME_WINDOW: Duration = Duration::from_secs(120);

/// Reconnect backoff between resume attempts. Deliberately no jitter here
/// (`BackoffPolicy::base_delay`, not `delay_for_attempt`) — isekai-ssh is a
/// single CLI process reconnecting to one specific isekai-helper instance,
/// not a fleet of clients that could thunder against a shared server, so the
/// jitter's only purpose (avoiding a reconnect stampede) doesn't apply, and
/// skipping it avoids pulling in a `rand::Rng` just for this.
const RESUME_BACKOFF: BackoffPolicy =
    BackoffPolicy { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.0 };

/// How often `pump_c2h`'s backpressure wait re-checks whether the replay
/// buffer has room again, while stdin reads are paused
/// (`ISEKAI_SSH_DESIGN.md`: "読み取りを呼ばなければパイプが埋まって...という
/// 単純な仕組みで十分" — a plain poll loop is that "simple enough" mechanism).
const BACKPRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Marker error so `main.rs` can tell "host has no trust store entry" apart
/// from every other failure and map it to a dedicated exit code
/// (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-2 "exit codeの分類"). Carried as the
/// root cause of the `anyhow::Error` returned by `resolve_from_trust_store`;
/// `main.rs` finds it via `anyhow::Error::chain()`.
#[derive(Debug)]
pub struct TrustNotInitialized;

impl std::fmt::Display for TrustNotInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host is not registered in the isekai-ssh trust store")
    }
}

impl std::error::Error for TrustNotInitialized {}

/// Which of `resolve_target`'s paths produced the target. Only used to pick
/// the right error message if the subsequent HELLO/proof/ACK fails — see
/// `run`'s use of it.
enum TargetSource {
    /// Debug + `dev-insecure`-feature builds only (`cli.rs`); does not exist
    /// in a release binary. Always carries a `RelayTarget` regardless of
    /// `--mode` — see `dev_insecure_target`'s docs.
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
        ResolvedTarget::Relay(relay_target) => run_relay_resumable(relay_target, &args.host, &source).await,
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
            relay_stdio(stream).await
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
// --mode", `dev_insecure_target`'s docs), so unlike
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
        if let Some(target) = dev_insecure_target(args)? {
            log::warn!(
                "isekai-ssh: using --dev-insecure-* bypass instead of the trust store for host '{}' \
                 (debug + dev-insecure build only; this path does not exist in a release binary; always \
                 relay-mode regardless of --mode, see dev_insecure_target's docs)",
                args.host
            );
            return Ok((ResolvedTarget::Relay(target), TargetSource::DevInsecureBypass));
        }
    }

    match args.mode {
        ConnectMode::Relay => {
            let target = resolve_relay_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Relay(target), TargetSource::TrustStore))
        }
        ConnectMode::Stun => {
            // clap's `required_if_eq("mode", "stun")` on `ConnectArgs::stun_server`
            // guarantees this is `Some` by the time argument parsing succeeds.
            let stun_server = args
                .stun_server
                .context("isekai-ssh: --stun-server is required with --mode stun (should be enforced by clap)")?;
            let target = resolve_stun_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Stun { stun_server, target }, TargetSource::TrustStore))
        }
    }
}

/// Looks `host` up in the trust store, failing closed — via the
/// `TrustNotInitialized` marker error — if `host` (normalized) has no entry;
/// `main.rs` maps that to a dedicated exit code and callers never attempt any
/// network I/O in that case. Shared by both `--mode`s: which fields of the
/// returned `HelperTrust` mean what differs (see
/// `resolve_relay_from_trust_store`/`resolve_stun_from_trust_store`), but the
/// lookup itself — and its trust store schema — does not.
fn load_trust_entry(host: &str) -> Result<(String, isekai_trust::schema::HelperTrust)> {
    let key = isekai_trust::normalize_host_port(host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{host}'"))?;

    let store_path = isekai_trust::default_trust_store_path()
        .context("isekai-ssh: could not determine the trust store path (is $HOME set?)")?;
    let store = isekai_trust::load_trust_store(&store_path)
        .with_context(|| format!("isekai-ssh: failed to load trust store at {}", store_path.display()))?;

    let Some(entry) = store.get(&key) else {
        return Err(anyhow::Error::new(TrustNotInitialized).context(format!(
            "isekai-ssh: '{host}' is not a trusted host yet (looked up as '{key}' in {}).\n\
             Run:\n  isekai-ssh init {host}\n\
             once to deploy isekai-helper there and register trust.",
            store_path.display(),
        )));
    };

    Ok((key, entry.clone()))
}

/// Builds a `RelayTarget` (`--mode relay`, the default) from `host`'s trust
/// store entry (`ISEKAI_SSH_DESIGN.md` "trust store のファイル形式"):
/// `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` are used
/// at face value, exactly as their names say.
fn resolve_relay_from_trust_store(host: &str) -> Result<RelayTarget> {
    let (key, entry) = load_trust_entry(host)?;

    let helper_addr: SocketAddr = entry.cached_relay_addr.parse().with_context(|| {
        format!(
            "isekai-ssh: trust store entry for '{key}' has an invalid cached_relay_addr '{}'",
            entry.cached_relay_addr
        )
    })?;
    let session_secret =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &entry.cached_session_secret)
            .with_context(|| format!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"))?;

    Ok(RelayTarget {
        helper_addr,
        // isekai-helper ignores the SNI it's presented (see
        // `isekai_transport::RelayTarget::server_name`'s docs); the trust
        // store doesn't need to record one.
        server_name: "isekai-helper".to_string(),
        cert_sha256_hex: entry.cached_cert_sha256.clone(),
        session_secret,
    })
}

/// Builds a `StunP2pTarget` (`--mode stun`) from `host`'s trust store entry.
///
/// **The trust store schema is not changed for STUN mode** (deliberate,
/// `ISEKAI_SSH_DESIGN.md` S-6 task scope): a `HelperTrust` entry always means
/// "however this isekai-helper instance is reachable, here is the
/// information needed to do so", and for both `--mode`s that boils down to
/// the same three pieces of data — an address to dial, and the cert/session
/// credentials the HELLO/proof/ACK handshake needs. `--mode relay`
/// (`resolve_relay_from_trust_store`) reads them as the relay-assigned
/// address; `--mode stun` reads the *exact same fields* as the peer's own
/// STUN-observed address instead (`HandshakeJson::stun_observed_addr`, as
/// captured by `isekai-ssh init`/a re-deployment at `--stun-server` query
/// time) plus the same cert/session credentials:
///
/// - `cached_relay_addr` -> `StunP2pTarget::peer_addr`
/// - `cached_cert_sha256` -> `StunP2pTarget::cert_sha256_hex`
/// - `cached_session_secret` -> `StunP2pTarget::session_secret`
///
/// Renaming these fields to something mode-agnostic was considered and
/// rejected for this phase: the task's explicit scope is "trust storeのスキーマは
/// 今回変更しない", so field names stay as `isekai-trust::schema::HelperTrust`
/// already defines them; this function (and its relay-mode sibling) is the
/// one place the reinterpretation is spelled out.
fn resolve_stun_from_trust_store(host: &str) -> Result<StunP2pTarget> {
    let (key, entry) = load_trust_entry(host)?;

    let peer_addr: SocketAddr = entry.cached_relay_addr.parse().with_context(|| {
        format!(
            "isekai-ssh: trust store entry for '{key}' has an invalid cached_relay_addr \
             (read as the peer's STUN-observed address for --mode stun) '{}'",
            entry.cached_relay_addr
        )
    })?;
    let session_secret =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &entry.cached_session_secret)
            .with_context(|| format!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"))?;

    Ok(StunP2pTarget {
        peer_addr,
        // isekai-helper ignores the SNI it's presented (see
        // `isekai_transport::RelayTarget::server_name`'s docs, shared by
        // `StunP2pTarget`); the trust store doesn't need to record one.
        server_name: "isekai-helper".to_string(),
        cert_sha256_hex: entry.cached_cert_sha256.clone(),
        session_secret,
    })
}

/// DEV/TEST ONLY (see `cli.rs::DevInsecureArgs`): always builds a
/// `RelayTarget`, regardless of `ConnectArgs::mode`. This bypass predates
/// `--mode`/STUN support (S-1, before the trust store existed) and exists
/// only to unblock a debug-build end-to-end test against a fixed
/// relay-assigned endpoint; wiring it up to also short-circuit STUN mode
/// would just be more untested surface for a flag that must never ship in a
/// release binary in the first place (`main.rs`'s `compile_error!` guard).
#[cfg(all(debug_assertions, feature = "dev-insecure"))]
fn dev_insecure_target(args: &ConnectArgs) -> Result<Option<RelayTarget>> {
    use std::net::SocketAddr;

    let d = &args.dev_insecure;
    let any_set = d.dev_insecure_target.is_some()
        || d.dev_insecure_cert_sha256.is_some()
        || d.dev_insecure_session_secret.is_some();

    let (Some(target_addr), Some(cert_sha256_hex), Some(session_secret_b64)) = (
        d.dev_insecure_target.as_deref(),
        d.dev_insecure_cert_sha256.as_deref(),
        d.dev_insecure_session_secret.as_deref(),
    ) else {
        if any_set {
            bail!(
                "isekai-ssh: --dev-insecure-target, --dev-insecure-cert-sha256, and \
                 --dev-insecure-session-secret must all be given together"
            );
        }
        return Ok(None);
    };

    let helper_addr: SocketAddr =
        target_addr.parse().with_context(|| format!("isekai-ssh: invalid --dev-insecure-target '{target_addr}'"))?;
    let session_secret = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, session_secret_b64)
        .context("isekai-ssh: --dev-insecure-session-secret must be valid base64")?;

    Ok(Some(RelayTarget {
        helper_addr,
        server_name: d.dev_insecure_server_name.clone(),
        cert_sha256_hex: cert_sha256_hex.to_string(),
        session_secret,
    }))
}

/// `--mode relay`'s connect+relay lifecycle (`ISEKAI_SSH_DESIGN.md` Phase
/// S-4c): establishes a resumable session, then drives `run_data_pump` in a
/// loop. As long as `run_data_pump` reports a real disconnect (not a clean
/// EOF), this function keeps trying `reconnect_and_resume` — bounded by
/// `RESUME_WINDOW` — before finally giving up. `ssh`'s stdin/stdout are never
/// explicitly touched here beyond normal reads/writes; giving up simply
/// returns `Ok(())`, letting the process exit and its stdio fds close as a
/// side effect of that (`ISEKAI_SSH_DESIGN.md`'s minimal give-up policy for
/// this phase — see `RESUME_WINDOW`'s docs; a more deliberate close-down
/// sequence is S-4d's scope).
async fn run_relay_resumable(target: RelayTarget, host: &str, source: &TargetSource) -> Result<()> {
    log::info!("isekai-ssh: connecting to isekai-helper at {} (--mode relay)", target.helper_addr);
    let factory = SystemQuicEndpointFactory;
    let established = connect_via_relay_resumable(&factory, &target)
        .await
        .with_context(|| relay_hello_failure_message(host, source))?;
    log::info!(
        "isekai-ssh: HELLO/ACK + control stream established (session_id={}) — relaying stdin/stdout <-> QUIC",
        established.session_id
    );

    let session_id = established.session_id;
    // The `connection` handles returned alongside each data/control stream
    // are not needed past this point: every concrete `ByteStream` keeps its
    // own connection alive internally (proven by `isekai-transport`'s own
    // `relay_e2e.rs`, which drops its connection handle immediately and
    // still successfully uses the resulting stream) — isekai-ssh only ever
    // needs the streams themselves.
    drop(established.connection);

    let counters = Arc::new(AppAckCounters::new());
    let app_ack_tasks = spawn_app_ack_tasks(established.control_stream, counters.clone());
    let replay = Arc::new(Mutex::new(C2hReplayBuffer::new(C2H_REPLAY_BUFFER_CAPACITY)));

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut data_stream = established.data_stream;
    let mut disconnected_since: Option<Instant> = None;
    let mut attempt: u32 = 0;

    loop {
        let (quic_read, quic_write) = data_stream.split();
        let outcome =
            run_data_pump(&mut stdin, &mut stdout, quic_read, quic_write, &replay, &counters).await;
        app_ack_tasks.abort();

        match outcome {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("isekai-ssh: data stream ended with an error, attempting to resume: {e:#}");
            }
        }

        let deadline = *disconnected_since.get_or_insert_with(Instant::now) + RESUME_WINDOW;
        let new_stream = loop {
            let now = Instant::now();
            if now >= deadline {
                log::warn!(
                    "isekai-ssh: resume window ({RESUME_WINDOW:?}) exceeded for session_id={session_id}, \
                     giving up and letting the process exit"
                );
                return Ok(());
            }
            let delay = RESUME_BACKOFF.base_delay(attempt).min(deadline - now);
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(delay).await;

            let client_sent_offset = C2hSentOffset::new(replay.lock().unwrap().end_offset());
            let client_delivered_offset = H2cClientDeliveredOffset::new(counters.h2c_client_delivered_offset());
            match reconnect_and_resume(&factory, &target, session_id, client_sent_offset, client_delivered_offset)
                .await
            {
                Ok(mut resumed) => {
                    drop(resumed.connection);
                    let to_replay =
                        { replay.lock().unwrap().replay_from(resumed.helper_committed_offset.get()) };
                    if let Some(bytes) = to_replay {
                        if !bytes.is_empty() {
                            if let Err(e) = resumed.data_stream.write_all(&bytes).await {
                                log::warn!("isekai-ssh: failed to replay unconfirmed C2H bytes after resume: {e}");
                                continue;
                            }
                        }
                    }
                    replay.lock().unwrap().advance_start(resumed.helper_committed_offset.get());
                    log::info!(
                        "isekai-ssh: resume succeeded (session_id={session_id}, \
                         helper_committed_offset={})",
                        resumed.helper_committed_offset
                    );
                    break resumed.data_stream;
                }
                Err(e) => {
                    log::warn!("isekai-ssh: resume attempt {attempt} failed: {e:#}, retrying");
                }
            }
        };

        // A fresh data stream is resumed, but per `isekai_transport::resume`'s
        // module docs, the control stream is deliberately *not* reopened
        // after a resume (mirrors `isekai_link_relay_transport.rs::reattach_fn`'s
        // reference behavior) — `app_ack_tasks` above was already aborted;
        // there is nothing to restart it with. `counters.h2c_client_delivered_offset()`
        // still gets included directly in any subsequent `RESUME` frame, so
        // no progress-reporting information is lost, only the
        // opportunistic mid-connection buffer trimming via `APP_ACK`.
        data_stream = new_stream;
        disconnected_since = None;
        attempt = 0;
    }
}

/// Drives both pump directions concurrently in a single task (not two
/// separate `tokio::spawn`ed tasks, unlike `relay_stdio`) so `stdin`/`stdout`
/// can be borrowed across reconnects instead of needing to be recreated (or
/// made `'static`) every time `run_relay_resumable` loops.
///
/// Matches `relay_stdio`'s "clean EOF on one side does not abort the other"
/// rule for a clean `Ok(())` (both sides must finish before this returns
/// `Ok(())`), but diverges for errors: any error on *either* side immediately
/// ends the whole pump — returning that `Err` (the other side's future is
/// simply dropped, canceling it) — rather than waiting for the survivor to
/// also finish. Once the underlying QUIC connection is gone, there is
/// nothing for the survivor to usefully keep doing. `run_relay_resumable`
/// treats `Ok(())` as "clean shutdown, no resume" and any `Err` as
/// "disconnected, attempt to resume" — it deliberately does not try to
/// distinguish "the QUIC connection died" from "a local stdio error
/// occurred" (`ISEKAI_SSH_DESIGN.md`'s minimal S-4c scope): a local-only
/// error (e.g. `ssh` itself dying, closing our stdout) will simply keep
/// failing every resume attempt's own subsequent pump too, and eventually
/// hit `run_relay_resumable`'s give-up path regardless.
async fn run_data_pump(
    stdin: &mut (impl AsyncRead + Unpin),
    stdout: &mut (impl AsyncWrite + Unpin),
    quic_read: Box<dyn ByteStreamReadHalf>,
    quic_write: Box<dyn ByteStreamWriteHalf>,
    replay: &Arc<Mutex<C2hReplayBuffer>>,
    counters: &Arc<AppAckCounters>,
) -> Result<()> {
    let c2h_fut = pump_c2h(stdin, quic_write, replay.clone(), counters.clone());
    let h2c_fut = pump_h2c(quic_read, stdout, counters.clone());
    tokio::pin!(c2h_fut);
    tokio::pin!(h2c_fut);

    let mut c2h_done = false;
    let mut h2c_done = false;
    loop {
        tokio::select! {
            res = &mut c2h_fut, if !c2h_done => {
                res.context("isekai-ssh: C2H (stdin -> isekai-helper) pump failed")?;
                c2h_done = true;
            }
            res = &mut h2c_fut, if !h2c_done => {
                res.context("isekai-ssh: H2C (isekai-helper -> stdout) pump failed")?;
                h2c_done = true;
            }
        }
        if c2h_done && h2c_done {
            return Ok(());
        }
    }
}

/// C2H direction with backpressure and replay-buffer tee
/// (`ISEKAI_SSH_DESIGN.md` Phase S-4c task 2). Before every stdin read,
/// first syncs `replay`'s confirmed-prefix marker from
/// `counters.c2h_helper_committed_offset()` — the value
/// `isekai_transport::spawn_app_ack_tasks`'s receive loop keeps up to date
/// from isekai-helper's `APP_ACK`s — then waits for `replay` to have room
/// (`C2hReplayBuffer::is_full`/`remaining_capacity`), deliberately *not*
/// reading from stdin while the buffer is full, which is what actually
/// backpressures the parent `ssh` process (`ISEKAI_SSH_DESIGN.md`:
/// "読み取りを呼ばなければパイプが埋まってssh側の書き込みがブロックされる").
/// Without this sync step, `replay` would only ever get trimmed right after
/// an actual resume (`run_relay_resumable`'s `advance_start` call there) and
/// would hit its capacity — stalling stdin forever — on any sufficiently
/// long-lived, uninterrupted session; syncing continuously here is what
/// makes `APP_ACK`'s "trim opportunistically while still connected" purpose
/// actually take effect (`HELPER_PROTOCOL.md` §7.4).
///
/// Every byte written to the data stream is also appended to `replay` so a
/// future resume can replay it if isekai-helper didn't confirm committing it
/// before the disconnect.
async fn pump_c2h(
    stdin: &mut (impl AsyncRead + Unpin),
    mut quic_write: Box<dyn ByteStreamWriteHalf>,
    replay: Arc<Mutex<C2hReplayBuffer>>,
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        loop {
            let mut r = replay.lock().unwrap();
            r.advance_start(counters.c2h_helper_committed_offset());
            if !r.is_full() {
                break;
            }
            drop(r);
            tokio::time::sleep(BACKPRESSURE_POLL_INTERVAL).await;
        }
        let read_len = buf.len().min(replay.lock().unwrap().remaining_capacity());
        let n = stdin.read(&mut buf[..read_len]).await.context("isekai-ssh: reading from stdin failed")?;
        if n == 0 {
            let _ = quic_write.shutdown().await;
            return Ok(());
        }
        quic_write.write_all(&buf[..n]).await.context("isekai-ssh: writing to isekai-helper failed")?;
        replay.lock().unwrap().append(&buf[..n]);
    }
}

/// H2C direction. Every successful stdout write also advances
/// `counters`'s `h2c_client_delivered_offset` — the "pending ACK, held
/// locally while disconnected, included in the next `RESUME`" value
/// `ISEKAI_SSH_DESIGN.md`'s H2C-delivered-boundary note describes.
async fn pump_h2c(
    mut quic_read: Box<dyn ByteStreamReadHalf>,
    stdout: &mut (impl AsyncWrite + Unpin),
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read.read(&mut buf).await.context("isekai-ssh: reading from isekai-helper failed")?;
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n]).await.context("isekai-ssh: writing to stdout failed")?;
        stdout.flush().await.context("isekai-ssh: flushing stdout failed")?;
        counters.advance_h2c_client_delivered_offset(n as u64);
    }
}

/// Runs the two independent copy tasks (`ISEKAI_SSH_DESIGN.md`: not
/// `tokio::io::copy_bidirectional`, since stdin/stdout are two separate
/// handles, not one duplex object). A clean EOF on one side does **not**
/// abort the other: e.g. `ssh` closing its write end of our stdin (C2H EOF)
/// is routine well before the server has said everything it's going to say,
/// and prematurely cutting H2C off there would truncate output the user was
/// still supposed to see. Both directions are allowed to run to their own
/// completion; only a genuine error (or task panic) on either side aborts
/// the other and returns early, since at that point continuing the survivor
/// alone serves no purpose. Once both sides have finished, this process
/// exits and closes its stdout, which is how `ssh` (reading our stdout)
/// learns the pass-through has ended.
///
/// Used only by `--mode stun` (`run`, above) — `--mode relay`'s
/// `run_relay_resumable`/`run_data_pump` supersede this for the resumable
/// path (S-4c). Kept as-is for STUN mode, which has no resume support to
/// speak of (`isekai_transport::resume` is only wired up for `RelayTarget`
/// today).
async fn relay_stdio(stream: Box<dyn ByteStream>) -> Result<()> {
    let (quic_read, quic_write) = stream.split();

    let mut c2h = tokio::spawn(pump_stdin_to_quic(quic_write));
    let mut h2c = tokio::spawn(pump_quic_to_stdout(quic_read));
    let (mut c2h_done, mut h2c_done) = (false, false);

    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                if let Err(err) = join_result("isekai-ssh: stdin->QUIC relay task panicked", res) {
                    h2c.abort();
                    return Err(err);
                }
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                if let Err(err) = join_result("isekai-ssh: QUIC->stdout relay task panicked", res) {
                    c2h.abort();
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// Flattens a `tokio::spawn` result (`Result<Result<()>, JoinError>`) into a
/// single `Result<()>`, attaching `panic_ctx` if the task itself panicked
/// rather than returning an error.
fn join_result(panic_ctx: &str, res: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    res.map_err(|e| anyhow::Error::new(e).context(panic_ctx.to_string()))?
}

/// C2H direction: `ssh` (our stdin) -> isekai-helper (the QUIC stream's send
/// side). On stdin EOF, finishes (shuts down) the QUIC send side so
/// isekai-helper sees a clean half-close rather than a reset.
async fn pump_stdin_to_quic(mut quic_write: Box<dyn isekai_transport::ByteStreamWriteHalf>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stdin.read(&mut buf).await.context("isekai-ssh: reading from stdin failed")?;
        if n == 0 {
            break;
        }
        quic_write.write_all(&buf[..n]).await.context("isekai-ssh: writing to isekai-helper failed")?;
    }
    // Best-effort: isekai-helper is free to have already gone away.
    let _ = quic_write.shutdown().await;
    Ok(())
}

/// H2C direction: isekai-helper (the QUIC stream's receive side) -> `ssh`
/// (our stdout). Every successful chunk is flushed immediately — `ssh`
/// expects to see SSH protocol bytes promptly, not batched.
async fn pump_quic_to_stdout(mut quic_read: Box<dyn isekai_transport::ByteStreamReadHalf>) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read.read(&mut buf).await.context("isekai-ssh: reading from isekai-helper failed")?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n]).await.context("isekai-ssh: writing to stdout failed")?;
        stdout.flush().await.context("isekai-ssh: flushing stdout failed")?;
    }
    Ok(())
}
