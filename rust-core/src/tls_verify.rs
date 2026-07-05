//! `quic_transport::SkipServerVerification`と`helper_quic_transport::PinnedCertVerifier`は
//! `verify_server_cert`（証明書を無条件許可 vs sha256ピン留め照合）だけが異なり、
//! `rustls::client::danger::ServerCertVerifier`の残り3メソッドは
//! `Arc<rustls::crypto::CryptoProvider>`への単純な委譲で完全に同一実装だったため、
//! ここに共通化する。

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::{DigitallySignedStruct, SignatureScheme};

pub(crate) fn verify_tls12_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    provider: &CryptoProvider,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls12_signature(message, cert, dss, &provider.signature_verification_algorithms)
}

pub(crate) fn verify_tls13_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    provider: &CryptoProvider,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(message, cert, dss, &provider.signature_verification_algorithms)
}

pub(crate) fn supported_verify_schemes(provider: &CryptoProvider) -> Vec<SignatureScheme> {
    provider.signature_verification_algorithms.supported_schemes()
}
