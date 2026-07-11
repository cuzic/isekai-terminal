//! [`PinnedCertVerifier`]: verifies a peer's leaf certificate against a
//! pinned SHA-256 fingerprint instead of a CA chain. Shared by every backend
//! this crate supports — a peer that presents an ephemeral self-signed
//! certificate has no CA chain to validate in the first place; the
//! fingerprint itself, delivered out-of-band by the caller, is the trust
//! root.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

/// Set right before [`PinnedCertVerifier::verify_server_cert`] returns
/// `Err` on a mismatch, since `ServerCertVerifier`'s return type is fixed to
/// `Result<_, rustls::Error>` and can't carry a typed [`crate::MuxError`]
/// directly. A backend's own connect path checks this slot after a
/// handshake failure to recover the structured
/// `MuxError::CertPinMismatch { expected, got }` instead of treating every
/// handshake failure as an opaque [`crate::MuxError::Handshake`].
pub type CertMismatchSlot = Arc<Mutex<Option<(String, String)>>>;

/// Verifies the peer's leaf certificate against a pinned SHA-256 fingerprint
/// instead of a CA chain. `pub` (not crate-private) because both the `noq`
/// backend (via `rustls::ClientConfig`) and the `qmux` backend (via a
/// manually-driven `tokio_rustls::TlsConnector`, since `qmux::Session::
/// connect` needs a raw `TlsStream` rather than a pre-built client config)
/// need to plug this same verifier into their own TLS setup — duplicating
/// a security-sensitive verifier between them would be strictly worse than
/// sharing one.
#[derive(Debug)]
pub struct PinnedCertVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
    mismatch: CertMismatchSlot,
}

impl PinnedCertVerifier {
    pub fn new(expected_sha256_hex: String, provider: Arc<rustls::crypto::CryptoProvider>, mismatch: CertMismatchSlot) -> Self {
        Self { expected_sha256_hex, provider, mismatch }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got == self.expected_sha256_hex {
            Ok(ServerCertVerified::assertion())
        } else {
            *self.mismatch.lock().unwrap() = Some((self.expected_sha256_hex.clone(), got.clone()));
            Err(rustls::Error::General(format!(
                "certificate pin mismatch: expected {} got {}",
                self.expected_sha256_hex, got
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    const FAKE_CERT_BYTES: &[u8] = b"quicmux cert.rs test fixture, not a real cert";

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn verifier_with(expected_sha256_hex: &str) -> (PinnedCertVerifier, CertMismatchSlot) {
        let mismatch = Arc::new(Mutex::new(None));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        (PinnedCertVerifier::new(expected_sha256_hex.to_string(), provider, mismatch.clone()), mismatch)
    }

    fn verify(verifier: &PinnedCertVerifier, cert_bytes: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
        let cert = CertificateDer::from(cert_bytes.to_vec());
        let server_name = ServerName::try_from("quicmux-test.invalid").unwrap();
        verifier.verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now())
    }

    #[test]
    fn verify_server_cert_succeeds_when_pin_matches() {
        let expected = sha256_hex(FAKE_CERT_BYTES);
        let (verifier, mismatch) = verifier_with(&expected);

        let result = verify(&verifier, FAKE_CERT_BYTES);

        assert!(result.is_ok());
        assert_eq!(*mismatch.lock().unwrap(), None);
    }

    #[test]
    fn verify_server_cert_rejects_and_records_expected_and_got_on_mismatch() {
        let expected = "0".repeat(64);
        let got = sha256_hex(FAKE_CERT_BYTES);
        let (verifier, mismatch) = verifier_with(&expected);

        let result = verify(&verifier, FAKE_CERT_BYTES);

        assert!(result.is_err());
        assert_eq!(*mismatch.lock().unwrap(), Some((expected, got)));
    }

    #[test]
    fn mismatch_slot_stays_empty_until_a_verify_call_fails() {
        let expected = sha256_hex(FAKE_CERT_BYTES);
        let (_verifier, mismatch) = verifier_with(&expected);

        assert_eq!(*mismatch.lock().unwrap(), None);
    }
}
