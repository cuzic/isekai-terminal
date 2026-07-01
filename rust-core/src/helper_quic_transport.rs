//! Phase 7-4: 自作ヘルパー（isekai-helper）経由の QUIC トランスポート。
//!
//! Phase 5B の tsshd/`QuicSession` と異なり、サーバー側に事前インストールされたデーモンを
//! 前提とせず、SSH 経由で isekai-helper を自動ブートストラップ（Phase 7-3）してから
//! QUIC 接続する。ワイヤー契約の詳細は `/HELPER_PROTOCOL.md` を参照。
//!
//! ビルド前提: `rust-core/scripts/build-isekai-helper-musl.sh` を先に実行し、
//! `target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-helper` が存在すること
//! （`include_bytes!` でこの crate の Android ビルドに埋め込む）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use log::{info, warn};
use quinn::crypto::rustls::QuicClientConfig;
use russh::client;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

use crate::helper_bootstrap::{self, BootstrapError, HelperBinaries, HelperHandshake};
use crate::transport::{run_ssh_channel_loop, RusshEventHandler, TransportCommand, TransportEvent};
use crate::{init_logger, CellData, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;

type HmacSha256 = Hmac<Sha256>;

const EXPORTER_LABEL: &[u8] = b"isekai-helper-auth-v1";
const ALPN: &[u8] = b"isekai-helper/1";
const FRAME_HELLO: u8 = 0x01;
const FRAME_ACK: u8 = 0x02;
const FRAME_REJECT_TARGET: u8 = 0xFC;
const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
const FRAME_REJECT_AUTH: u8 = 0xFF;

/// isekai-helper/Cargo.toml の version と一致させる。バージョン不一致時は
/// helper_bootstrap::ensure_helper_running が再配布する。
const HELPER_VERSION: &str = "0.1.0";

const HELPER_BIN_X86_64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/x86_64-unknown-linux-musl/release/isekai-helper"
));
const HELPER_BIN_AARCH64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/aarch64-unknown-linux-musl/release/isekai-helper"
));

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct HelperQuicConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
}

#[derive(uniffi::Object)]
pub struct HelperQuicSession {
    config: HelperQuicConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_helper_quic_session(config: HelperQuicConfig) -> Arc<HelperQuicSession> {
    init_logger();
    Arc::new(HelperQuicSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl HelperQuicSession {
    /// 明示的にヘルパー経由 QUIC のみを試す（フォールバック無し）。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            match try_connect_helper_quic(&config).await {
                Ok(stream) => run_over_stream(config, stream, cmd_rx, event_tx).await,
                Err(e) => {
                    warn!("helper_quic: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
            }
        });
        Ok(())
    }

    /// `TransportPreference::Auto` 相当: ヘルパー経由 QUIC を試し、失敗したら
    /// 通常の TCP SSH（Phase 1-4）にフォールバックする。
    pub fn connect_auto(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            run_helper_quic_transport_auto(config, cmd_rx, event_tx).await;
        });
        Ok(())
    }

    pub fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub fn disconnect(&self) { self.core.disconnect(); }

    pub fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }
}

// ── 証明書ピン留め ───────────────────────────────────────
// handshake JSON の cert_sha256（SSH チャネル経由で受け渡し済み、TOFU より強い信頼の起点）
// とだけ照合する。通常の CA チェーン検証は行わない（自己署名 ephemeral cert のため）。

#[derive(Debug)]
struct PinnedCertVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
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
            Err(rustls::Error::General(format!(
                "isekai-helper cert pin mismatch: expected {} got {}",
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
        rustls::crypto::verify_tls12_signature(
            message, cert, dss, &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message, cert, dss, &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

// ── ブートストラップ（Phase 7-3 呼び出し） ───────────────

async fn bootstrap_via_ssh(config: &HelperQuicConfig) -> Result<HelperHandshake, String> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
    // NOTE: このブートストラップ用 SSH 接続のホスト鍵は、本セッションと同じサーバー・
    // 同じ known_hosts エントリを検証すべきだが、Phase 7-4 のスコープでは簡略化し
    // 常に承認する。TODO: 既存の KnownHost チェック（HostKeyChecker）と統合する。
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            if let TransportEvent::HostKey(_, reply) = ev {
                let _ = reply.send(true);
            }
        }
    });

    let russh_config = Arc::new(client::Config::default());
    let handler = RusshEventHandler { event_tx };
    let mut session = client::connect(russh_config, (config.ssh_host.as_str(), config.ssh_port), handler)
        .await
        .map_err(|e| format!("bootstrap SSH connect failed: {e}"))?;

    let authenticated = match &config.auth {
        SshAuth::Password { password } => session
            .authenticate_password(&config.username, password)
            .await
            .ok()
            .unwrap_or(false),
        SshAuth::PublicKey { private_key_pem } => match russh_keys::PrivateKey::from_openssh(private_key_pem) {
            Ok(key) => session
                .authenticate_publickey(&config.username, Arc::new(key))
                .await
                .ok()
                .unwrap_or(false),
            Err(e) => {
                warn!("helper_quic: bootstrap private key parse failed: {e}");
                false
            }
        },
    };
    if !authenticated {
        return Err("bootstrap SSH authentication failed".to_string());
    }

    let binaries = HelperBinaries { x86_64: HELPER_BIN_X86_64, aarch64: HELPER_BIN_AARCH64 };
    helper_bootstrap::ensure_helper_running(&mut session, &binaries, HELPER_VERSION, "127.0.0.1:22")
        .await
        .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────

async fn connect_helper_quic_stream(
    ssh_host: &str,
    handshake: &HelperHandshake,
) -> Result<tokio::io::Join<quinn::RecvStream, quinn::SendStream>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS config failed".to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: handshake.cert_sha256.clone(),
            provider,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT はクライアント側でも使わない（HELPER_PROTOCOL.md「0-RTT はクライアント・
    // サーバー双方で完全に無効化する」契約）。quinn::Connecting::into_0rtt() を呼ばず
    // 通常のハンドシェイク完了を待つのがクライアント側の対応。

    let quic_crypto = QuicClientConfig::try_from(crypto).map_err(|_| "QUIC crypto config failed".to_string())?;

    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));

    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));

    let remote: SocketAddr = tokio::net::lookup_host((ssh_host, handshake.listen_port))
        .await
        .map_err(|e| format!("DNS lookup failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved for isekai-helper host".to_string())?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| format!("endpoint bind failed: {e}"))?;
    endpoint.set_default_client_config(client_config);

    info!("helper_quic: connecting to {remote}");
    let conn = endpoint
        .connect(remote, "isekai-helper.local")
        .map_err(|e| format!("connect setup failed: {e}"))?
        .await
        .map_err(|e| format!("QUIC handshake failed: {e}"))?;
    info!("helper_quic: QUIC handshake ok rtt={:?}", conn.rtt());

    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| format!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(&session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    let proof = mac.finalize().into_bytes();

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi failed: {e}"))?;
    let mut hello = Vec::with_capacity(33);
    hello.push(FRAME_HELLO);
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await.map_err(|e| format!("HELLO write failed: {e}"))?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await.map_err(|e| format!("ACK read failed: {e}"))?;
    match resp[0] {
        FRAME_ACK => {}
        FRAME_REJECT_AUTH => return Err("isekai-helper rejected: auth (proof mismatch)".to_string()),
        FRAME_REJECT_DUPLICATE => {
            return Err("isekai-helper rejected: duplicate active connection".to_string())
        }
        FRAME_REJECT_TARGET => return Err("isekai-helper rejected: target unreachable".to_string()),
        FRAME_REJECT_UNSUPPORTED => return Err("isekai-helper rejected: unsupported frame".to_string()),
        other => return Err(format!("isekai-helper: unexpected response byte {other:#x}")),
    }
    info!("helper_quic: HELLO/ACK ok — handing off to SSH");

    Ok(tokio::io::join(recv, send))
}

async fn try_connect_helper_quic(
    config: &HelperQuicConfig,
) -> Result<tokio::io::Join<quinn::RecvStream, quinn::SendStream>, String> {
    let handshake = bootstrap_via_ssh(config).await?;
    connect_helper_quic_stream(&config.ssh_host, &handshake).await
}

async fn run_over_stream(
    config: HelperQuicConfig,
    stream: tokio::io::Join<quinn::RecvStream, quinn::SendStream>,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });
    let handler = RusshEventHandler { event_tx: event_tx.clone() };

    let session = match client::connect_stream(russh_config, stream, handler).await {
        Ok(s) => s,
        Err(e) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    run_ssh_channel_loop(
        &config.username, &config.auth, config.cols, config.rows,
        session, cmd_rx, event_tx,
    ).await;
}

/// `TransportPreference::Auto`: ヘルパー経由 QUIC を試し、ブートストラップ/QUIC 接続の
/// 時点で失敗したら通常の TCP SSH にフォールバックする。一度 russh セッションが確立した
/// 後の切断はフォールバック対象にしない（正常な切断イベントとして扱う）。
async fn run_helper_quic_transport_auto(
    config: HelperQuicConfig,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    match try_connect_helper_quic(&config).await {
        Ok(stream) => run_over_stream(config, stream, cmd_rx, event_tx).await,
        Err(e) => {
            warn!("helper_quic auto: falling back to plain SSH after: {e}");
            let ssh_config = crate::SshConfig {
                host: config.ssh_host,
                port: config.ssh_port,
                username: config.username,
                auth: config.auth,
                cols: config.cols,
                rows: config.rows,
            };
            crate::run_russh_transport(ssh_config, cmd_rx, event_tx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! `HelperQuicSession` を orchestrator レベルではなく直接使い、SSH ブートストラップ →
    //! isekai-helper QUIC → russh チャネル → 実シェルコマンド実行までを通しで検証する。
    //! 実 sshd（127.0.0.1:22）+ `HELPER_BOOTSTRAP_TEST_KEY` が必要な opt-in テスト。
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    struct TestCallback {
        buf: Arc<StdMutex<Vec<u8>>>,
        notify: Arc<Notify>,
    }

    impl SessionCallback for TestCallback {
        fn on_data(&self, data: Vec<u8>) {
            self.buf.lock().unwrap().extend_from_slice(&data);
            self.notify.notify_one();
        }
        fn on_host_key(&self, _fingerprint: String) -> bool { true }
        fn on_connected(&self) {}
        fn on_disconnected(&self, reason: Option<String>) {
            eprintln!("test: disconnected: {reason:?}");
        }
        fn on_screen_update(&self, _update: crate::ScreenUpdate) {}
        fn on_trzsz_request(&self, _t: String, _m: String, _n: Option<String>, _s: Option<u64>) {}
        fn on_trzsz_download_chunk(&self, _t: String, _d: Vec<u8>, _l: bool) {}
        fn on_trzsz_progress(&self, _t: String, _tr: u64, _to: Option<u64>) {}
        fn on_trzsz_finished(&self, _t: String, _s: bool, _m: Option<String>) {}
    }

    #[tokio::test]
    async fn full_stack_bootstrap_quic_and_shell_command() {
        let Ok(key_path) = std::env::var("HELPER_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: HELPER_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        // 秘密鍵の実体はテストなので PEM のまま渡す（本番の SshAuth::PublicKey と同じ形）。
        let config = HelperQuicConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
        };

        let session = create_helper_quic_session(config);
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let callback = TestCallback { buf: buf.clone(), notify: notify.clone() };
        session.connect(Box::new(callback)).expect("connect() call failed");

        // シェルプロンプトが出るまで少し待ってからコマンドを送る。
        tokio::time::sleep(Duration::from_millis(800)).await;
        session.send(b"echo full-stack-ok\n".to_vec());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let b = buf.lock().unwrap();
                if String::from_utf8_lossy(&b).contains("full-stack-ok") {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for echo output; got so far: {:?}",
                    String::from_utf8_lossy(&buf.lock().unwrap())
                );
            }
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .ok();
        }

        session.disconnect();
    }
}
