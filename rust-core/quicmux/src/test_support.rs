//! Test-only self-signed-cert + [`MuxServerConfig`] assembly, shared by this
//! crate's own `cfg(test)` code (`noq_backend`/`qmux_backend`/`resume`) and
//! by downstream crates' tests via the `test-support` cargo feature
//! (`isekai-transport`, `isekai-pipe`) — the same ~20-line
//! `rcgen::generate_simple_self_signed` → `CertificateDer` → hex SHA-256 →
//! `MuxServerConfig{..}` block used to be hand-copied in each of those
//! files.
//!
//! Gated behind `test-support` (not just `cfg(test)`) so downstream crates
//! can opt into it from their own dev-dependencies without this crate
//! pulling `rcgen` into ordinary (non-test) builds — see this crate's
//! `Cargo.toml` for how `cargo test -p quicmux` gets the feature active for
//! its own internal tests too (self dev-dependency).

use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

use crate::config::MuxServerConfig;

/// Generates a fresh self-signed certificate for `sni` and returns a
/// [`MuxServerConfig`] built from it, alongside the certificate's
/// lowercase-hex SHA-256 fingerprint for the client side to pin against.
///
/// `alpn`/`exporter_label` are left empty and every other tuning knob is set
/// to a generic default (15s idle timeout, 5s keepalive, 2 concurrent bidi
/// streams, no uni streams, multipath/datagrams off) — this crate has no
/// business guessing a caller's real protocol identifiers or stream-limit
/// policy (see [`crate::MuxClientConfig`]'s own docs on why it has no
/// built-in default), so callers that need particular values (e.g.
/// `isekai_protocol::hello::ALPN`/`EXPORTER_LABEL`, a larger
/// `max_concurrent_bidi_streams`) overwrite the relevant field(s) on the
/// returned config directly.
pub fn self_signed_server_config(sni: &str) -> (MuxServerConfig, String) {
    let cert = rcgen::generate_simple_self_signed(vec![sni.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert.der().clone());
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let cert_sha256_hex = {
        let mut hasher = Sha256::new();
        hasher.update(cert_der.as_ref());
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    let config = MuxServerConfig {
        alpn: Vec::new(),
        exporter_label: Vec::new(),
        max_idle_timeout: Duration::from_secs(15),
        keep_alive_interval: Duration::from_secs(5),
        max_concurrent_bidi_streams: 2,
        max_concurrent_uni_streams: 0,
        multipath: false,
        datagram_send_buffer_size: None,
        cert_chain: vec![cert_der],
        private_key: key_der,
    };
    (config, cert_sha256_hex)
}
