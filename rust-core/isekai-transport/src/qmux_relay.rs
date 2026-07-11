//! `qmux`-backed [`quicmux::AnyMuxFactory`] constructor, for reaching a
//! relay-assigned `isekai-helper` endpoint from a network that blocks
//! outbound UDP (`#qmux-leg1`). Opt-in via the `qmux-relay` feature (off by
//! default — CLAUDE.md's opportunistic/opt-in-by-default principle, and
//! `qmux`'s pre-1.0 API churn risk).
//!
//! The `qmux` backend implementation itself (TCP dial, manually-driven TLS
//! handshake, QMux session setup, cert pinning) lives in
//! `quicmux::qmux_backend` now — this module is just this crate's own
//! product-policy layer on top: which ALPN to use, and a thin constructor
//! that builds the `quicmux::AnyMuxFactory` from it. See that module's docs
//! for the backend-level caveats (unverified ALPN against the real relay,
//! no rebind support, `wrap_bound_socket` structurally unsupported since
//! QMux runs over TCP).

use isekai_protocol::hello::EXPORTER_LABEL;
use quicmux::{AnyMuxFactory, MuxClientConfig};

/// See this module's docs — unverified against the real relay. Distinct
/// from `system::isekai_mux_config`'s plain ALPN
/// (`isekai_protocol::hello::ALPN`, `"isekai-pipe/1"`): draft-ietf-quic-qmux
/// §8.1 requires a distinct ALPN per application-protocol mapping carried
/// over QMux, so this leg cannot reuse that token even though it speaks the
/// identical ATTACH-protocol bytes once the session is established.
pub const QMUX_ALPN: &[u8] = b"isekai-pipe/1+qmux01";

/// The `qmux`-backed [`quicmux::AnyMuxFactory`] for this relay leg. Every
/// [`quicmux::MuxClientConfig`] field besides `alpn`/`exporter_label` is
/// ignored by the `qmux` backend (see that type's own field docs), so the
/// idle-timeout/keepalive/stream-limit/multipath values here are copied from
/// `system::isekai_mux_config`'s equivalents purely for consistency, not
/// because this backend reads them.
pub fn qmux_relay_factory() -> AnyMuxFactory {
    AnyMuxFactory::qmux(MuxClientConfig {
        alpn: QMUX_ALPN.to_vec(),
        exporter_label: EXPORTER_LABEL.to_vec(),
        max_idle_timeout: std::time::Duration::from_secs(15),
        keep_alive_interval: std::time::Duration::from_secs(5),
        max_concurrent_bidi_streams: 1,
        max_concurrent_uni_streams: 0,
        multipath: false,
    })
}
