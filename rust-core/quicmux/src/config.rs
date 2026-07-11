//! [`MuxClientConfig`]: the client-side connection tuning `quicmux` needs
//! but must not hardcode itself — ALPN, the exporter label, idle timeout,
//! keepalive interval, and stream-count limits are all product policy
//! belonging to whichever application protocol runs over the connection,
//! not to this transport-abstraction crate. Every caller supplies its own
//! values; `quicmux` has no built-in default (a caller with no opinion on
//! e.g. idle timeout should still have to say so explicitly rather than
//! inherit a value this crate happened to pick).

use std::time::Duration;

/// Client-side connection tuning, supplied by the caller at endpoint/factory
/// construction time and applied to every connection dialed through it.
///
/// Not every field applies to every backend (see each field's docs for which
/// backend(s) it's load-bearing for) — a backend that has no use for a given
/// field simply ignores it, rather than this type growing backend-specific
/// variants. This keeps the config shape backend-agnostic even though the
/// backends themselves aren't equally general.
#[derive(Debug, Clone)]
pub struct MuxClientConfig {
    /// ALPN protocol identifier presented during the TLS handshake. Every
    /// backend this crate supports negotiates ALPN as part of its handshake
    /// (`noq`: QUIC's ALPN extension; `qmux`: draft-ietf-quic-qmux §8.1's
    /// own rule that each application-protocol mapping needs a distinct
    /// ALPN when carried over QMux), so this field is load-bearing for both.
    pub alpn: Vec<u8>,
    /// The `label` passed to `export_keying_material` — needed up front (not
    /// just at call time) because the `qmux` backend can only capture an
    /// exporter value once, immediately after its handshake completes and
    /// before handing the underlying TLS stream off to the QMux session
    /// (`qmux::Session::connect` takes ownership of it, so there is no way
    /// to retrieve a live handle to the TLS connection afterward). The `noq`
    /// backend ignores this field entirely — `noq::Connection::
    /// export_keying_material` can be called with any label at any time
    /// after the handshake, so it has no need to know the label in advance.
    pub exporter_label: Vec<u8>,
    /// The connection is declared dead after this much silence.
    pub max_idle_timeout: Duration,
    /// PING interval to keep the connection (and, for UDP-based backends,
    /// any NAT mapping) alive.
    pub keep_alive_interval: Duration,
    /// Maximum number of concurrent bidirectional streams the peer may open
    /// on this connection.
    pub max_concurrent_bidi_streams: u32,
    /// Maximum number of concurrent unidirectional streams the peer may open
    /// on this connection.
    pub max_concurrent_uni_streams: u32,
    /// Whether to advertise the `noq` multipath extension
    /// (`TransportConfig::max_concurrent_multipath_paths`) — required on
    /// *both* sides of a connection before `noq::Connection::open_path` (or
    /// `noq::Endpoint::rebind`'s own connection-migration validation) will
    /// do anything but fail/hang. Ignored entirely by the `qmux` backend,
    /// which has no path/multipath concept (it runs over one TCP
    /// connection).
    pub multipath: bool,
}

/// Server-side connection tuning, supplied by the caller at listener
/// construction time and applied to every connection this listener accepts.
///
/// Mirrors [`MuxClientConfig`]'s shape deliberately (same field names/types
/// for the tuning knobs both sides need) — a caller that dials itself with
/// [`MuxClientConfig`] and listens with this type is expected to keep the
/// two in sync (e.g. `alpn`/`max_concurrent_bidi_streams` must agree for the
/// handshake/stream limits to make sense), but `quicmux` itself never
/// enforces that; it is product policy, not a transport-abstraction concern.
/// Not `Clone`: `rustls::pki_types::PrivateKeyDer` deliberately isn't
/// (it holds key material and avoids casual copies) — it exposes
/// `clone_key()` for the rare caller that actually needs a duplicate, which
/// [`crate::noq_backend::noq_server_config`] uses internally since it only
/// borrows this config, not owns it.
pub struct MuxServerConfig {
    /// See [`MuxClientConfig::alpn`] — must match what a client dialing this
    /// listener presents, or the handshake fails.
    pub alpn: Vec<u8>,
    /// See [`MuxClientConfig::exporter_label`].
    pub exporter_label: Vec<u8>,
    /// See [`MuxClientConfig::max_idle_timeout`].
    pub max_idle_timeout: std::time::Duration,
    /// See [`MuxClientConfig::keep_alive_interval`].
    pub keep_alive_interval: std::time::Duration,
    /// See [`MuxClientConfig::max_concurrent_bidi_streams`].
    pub max_concurrent_bidi_streams: u32,
    /// See [`MuxClientConfig::max_concurrent_uni_streams`].
    pub max_concurrent_uni_streams: u32,
    /// See [`MuxClientConfig::multipath`].
    pub multipath: bool,
    /// The server's certificate chain (leaf first), presented during the TLS
    /// handshake. Every backend this crate supports authenticates the
    /// *client* by pinned SHA-256 fingerprint rather than a CA chain (see
    /// [`crate::PinnedCertVerifier`]), so this is typically a single
    /// ephemeral self-signed certificate generated fresh per process — not a
    /// CA-issued chain — but `quicmux` itself has no opinion on that; it is
    /// the caller's policy.
    pub cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// The private key matching `cert_chain`'s leaf certificate.
    pub private_key: rustls::pki_types::PrivateKeyDer<'static>,
}
