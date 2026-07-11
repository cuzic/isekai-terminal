//! Default [`quicmux::MuxClientConfig`] values for isekai's own ATTACH v2
//! wire protocol, and the `noq`-backed [`quicmux::AnyMuxFactory`] the CLI
//! (`isekai-ssh`/`isekai-pipe`) uses.
//!
//! Connection establishment, cert-pinning, and backend selection themselves
//! now live in `quicmux` (this module used to own `SystemQuicEndpointFactory`/
//! `SystemQuicEndpoint`/`SystemQuicConnection`/`SystemByteStream` and
//! `PinnedCertVerifier`/`client_config_for` directly — all moved to
//! `quicmux::{noq_backend, cert}` once it became clear the same logic was
//! needed, byte-for-byte, by a second backend (`qmux_relay.rs`) and by
//! `isekai-terminal-core`'s own Android adapter). What's left here is purely
//! product policy: which ALPN/exporter-label/timeout/stream-limit values
//! isekai's `noq`-backed connections use, and a thin constructor that builds
//! the `quicmux::AnyMuxFactory` from them.

use std::time::Duration;

use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use quicmux::{AnyMuxFactory, MuxClientConfig};

/// The connection is declared dead after this much silence. Must be short
/// enough that a dead connection is detected before isekai-helper's
/// parked-session TTL expires (`ISEKAI_PIPE_DESIGN.md`).
const CLIENT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// PING interval to keep NAT UDP mappings alive. Kept at 1/3 of the idle
/// timeout so a handful of lost PINGs can be tolerated.
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// This crate's own ATTACH v2 wire protocol never opens more than one
/// bidirectional data stream plus one control stream per connection, and
/// never accepts a peer-initiated unidirectional stream — see
/// `resume::open_control_stream`/`relay::connect_and_handshake`'s own stream
/// usage for confirmation. Kept as a named constant (not inlined into
/// [`isekai_mux_config`]) purely so a reader diffing this file against a
/// future change can tell a deliberate policy value apart from an
/// accidental one.
const MAX_CONCURRENT_BIDI_STREAMS: u32 = 1;
const MAX_CONCURRENT_UNI_STREAMS: u32 = 0;

/// Builds the [`quicmux::MuxClientConfig`] every isekai-owned connection
/// (relay, STUN P2P, resume, multipath) uses: this crate's own ALPN/
/// exporter-label, plus the idle-timeout/keepalive/stream-limit tuning
/// above.
///
/// `multipath`: whether to advertise `noq`'s multipath extension
/// (ignored entirely by non-`noq` backends, see
/// [`quicmux::MuxClientConfig::multipath`]'s docs) — required on *both*
/// sides of a connection before `noq::Connection::open_path` (or
/// `noq::Endpoint::rebind`'s own connection-migration validation) will do
/// anything but fail/hang. `isekai-ssh`/`isekai-pipe`'s own connections pass
/// `true` unconditionally: any connection they make might later go through
/// [`quicmux::AnyMuxEndpoint::rebinder`] (`isekai-pipe`'s
/// `--experimental-network-rebind`), and `isekai-pipe serve`'s own
/// server-side transport config already negotiates this unconditionally
/// too, for the identical reason — a client that doesn't negotiate it back
/// was the one actual asymmetry, not a deliberate opt-in.
/// `isekai-terminal-core`'s Android adapter is the one caller that still
/// passes `false` — see that file's own docs for why.
pub fn isekai_mux_config(multipath: bool) -> MuxClientConfig {
    MuxClientConfig {
        alpn: ALPN.to_vec(),
        exporter_label: EXPORTER_LABEL.to_vec(),
        max_idle_timeout: CLIENT_MAX_IDLE_TIMEOUT,
        keep_alive_interval: CLIENT_KEEP_ALIVE_INTERVAL,
        max_concurrent_bidi_streams: MAX_CONCURRENT_BIDI_STREAMS,
        max_concurrent_uni_streams: MAX_CONCURRENT_UNI_STREAMS,
        multipath,
    }
}

/// The CLI's `noq`-backed [`quicmux::AnyMuxFactory`] — binds/wraps plain
/// `tokio::net::UdpSocket`s (no fault injection, no Android-specific type;
/// see this crate's own module-level "no Android/UniFFI types" boundary).
/// Passes `multipath: true` to [`isekai_mux_config`] — see that function's
/// docs for why every CLI connection negotiates this unconditionally.
pub fn system_quic_factory() -> AnyMuxFactory {
    AnyMuxFactory::noq(isekai_mux_config(true))
}
