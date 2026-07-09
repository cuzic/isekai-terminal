//! Phase 7-4: 自作ヘルパー（isekai-helper）経由の QUIC トランスポート。
//!
//! Phase 5B の tsshd/`QuicSession` と異なり、サーバー側に事前インストールされたデーモンを
//! 前提とせず、SSH 経由で isekai-helper を自動ブートストラップ（Phase 7-3）してから
//! QUIC 接続する。ワイヤー契約の詳細は `/HELPER_PROTOCOL.md` を参照。
//!
//! 重要: この transport は bootstrap 用 SSH 宛先（`ssh_host`）を、そのまま
//! QUIC dial 先の host 部分にも使う direct-by-bootstrap-host 経路である。
//! これは Tailscale、LAN、既知 direct host など、client から `ssh_host:listen_port`
//! へ UDP/QUIC で直接到達できる場合だけ成立する。ProxyJump で bootstrap できる
//! ことは、`ssh_host` へ QUIC dial できることを意味しない。NAT 越えや relay 前提の
//! 接続では `isekai_stun_p2p_transport.rs` / `isekai_link_relay_transport.rs` のように、
//! helper 起動後の観測 endpoint を使う経路を選ぶ。
//!
//! ビルド前提: `rust-core/scripts/build-isekai-pipe-musl.sh` を先に実行し、
//! `target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-pipe` が存在すること
//! （`include_bytes!` でこの crate の Android ビルドに埋め込む。リモートでは
//! `isekai-pipe serve ...` として起動する。旧 isekai-helper crate は
//! `archive/ISEKAI_PIPE_MIGRATION.md` P5 で isekai-pipe へ統合済み）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use log::{info, warn};
use noq::crypto::rustls::QuicClientConfig;
use russh::client;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_response, encode_attach_activate, encode_attach_hello,
    AttachActivate, AttachHello, AttachProof, AttachRejectReason, AttachResponse, AttemptId, ConnectionGeneration,
    ATTACH_READY_FRAME_LEN, ATTEMPT_ID_LEN, FRAME_ATTACH_READY, FRAME_REJECT_STALE_GENERATION,
    STALE_GENERATION_REJECT_FRAME_LEN,
};
use isekai_protocol::session_id::{SessionId, SESSION_ID_LEN};
use rand::RngCore;

use crate::helper_bootstrap::{self, BootstrapError, IsekaiPipeBinaries, IsekaiPipeHandshake, IsekaiPipeP2pMode};
use crate::resume_client::{self, ClientResumeState};
use crate::transport::{
    authenticate_session, connect_via_jump_or_direct, establish_ssh_handle_over_stream,
    run_ssh_channel_loop, zeroize_ssh_auth, PooledSshHandle, TransportEvent,
};
use crate::{init_logger, CellData, JumpConfig, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;

type HmacSha256 = Hmac<Sha256>;

/// C→S input replay buffer の既定上限（helper 側 `DEFAULT_RESUME_BUFFER_SIZE` と揃える）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;
/// control stream を開く/CONTROL_ACK を待つタイムアウト（helper 側 `HELLO_TIMEOUT` と揃える）。
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// QUIC connection が本当に死んだことを検知するまでの時間。実機検証（Phase 8-4b）で、
/// この値が未設定（quinn のデフォルト任せ）だと検知に 40 秒以上かかり、helper 側の
/// park セッション破棄（`isekai-helper::DEFAULT_PARKED_SESSION_TTL`）より遅くなって
/// reattach が必ず `REJECT_UNKNOWN_SESSION` になる、という致命的なタイミング不整合が
/// 見つかった。検知を速くしつつ、NAT の UDP マッピング（通常 30 秒）が切れて偽陽性の
/// タイムアウトが起きないよう `CLIENT_KEEP_ALIVE_INTERVAL` で PING を送り続ける。
const CLIENT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// NAT マッピング維持のための PING 間隔。`CLIENT_MAX_IDLE_TIMEOUT` の 1/3 以下にして、
/// 数回分の PING ロスを許容できるようにする。
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

// Phase 9-2 (multipath_transport.rs)・Phase 10 (isekai_stun_p2p_transport.rs /
// isekai_link_relay_transport.rs) もこのワイヤー契約・埋め込みバイナリを共有する
// ため pub(crate) re-export してある（ATTACH v2 の契約は接続経路が単一パスか
// multipath かによらず同一）。
// Phase S-0f: 値そのものの定義は isekai-protocol crate（pure crate、CLI版の
// isekai-ssh/isekai-transport とも共有）に一本化し、ここでは re-export するだけにする
// （ISEKAI_SSH_DESIGN.md「共有ロジックの crate 分割」参照）。ATTACH v2 のフレーム
// 定数・codec は各ファイルが直接 `isekai_protocol::attach` から import する（HELLO/ACK
// v1 のフレーム定数はサーバー側で撤去されたため re-export しない）。
pub(crate) use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};

/// `isekai-pipe/Cargo.toml` の version と一致させる（`isekai-pipe --version` の出力に
/// この文字列が部分一致することを`check_existing_version`が確認する）。バージョン
/// 不一致時は helper_bootstrap::ensure_helper_running が再配布する。
pub(crate) const ISEKAI_PIPE_VERSION: &str = "0.1.0";

pub(crate) const ISEKAI_PIPE_BIN_X86_64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/x86_64-unknown-linux-musl/release/isekai-pipe"
));
pub(crate) const ISEKAI_PIPE_BIN_AARCH64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/aarch64-unknown-linux-musl/release/isekai-pipe"
));

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct IsekaiPipeQuicConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ブートストラップ用SSH接続の踏み台(ProxyJump)。`SshConfig::jump`参照。
    pub jump: Option<JumpConfig>,
    /// isekai-helperのQUIC待受ポートを固定する(`None`ならこれまで通りOS任せの
    /// エフェメラルポート)。`direct_address`など外部到達アドレス経由で接続する場合、
    /// サーバー側ファイアウォールに事前にこのポートだけ許可しておける
    /// (Phase 7-5/9-2の実機検証で判明した既知課題への対応)。値の解決(ユーザー指定/
    /// 既定値/エフェメラル)はKotlin側で1回だけ行い、ここにはFFI境界を越える前に
    /// 確定した値だけを渡すこと。
    pub bind_port: Option<u16>,
}

#[derive(uniffi::Object)]
pub struct IsekaiPipeQuicSession {
    config: IsekaiPipeQuicConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_isekai_pipe_quic_session(config: IsekaiPipeQuicConfig) -> Arc<IsekaiPipeQuicSession> {
    init_logger();
    Arc::new(IsekaiPipeQuicSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl IsekaiPipeQuicSession {
    /// 明示的にヘルパー経由 QUIC のみを試す（フォールバック無し）。SSH接続プーリング
    /// (`archive/ISEKAI_SSH_DESIGN.md`参照)により、同一ホスト/ユーザー/鍵/ブートストラップ
    /// パラメータへ既にプールされたHandleがあれば、ブートストラップSSH・ヘルパー起動・
    /// QUICハンドシェイク・ネストしたSSH認証を丸ごとスキップして新しいSSHチャネルだけ開く。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let mut config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        // ブートストラップ用SSH(isekai-helperを起動するための踏み台接続)のホスト鍵検証を
        // 本セッションのcallback(Kotlin側のKnownHostRepositoryを参照する既存のTOFU/
        // 変更検知ロジック)にそのまま委譲する（`bootstrap_helper_via_ssh`参照）。
        let host_key_callback = self.core.callback();
        RUNTIME.spawn(async move {
            let (cols, rows) = (config.cols, config.rows);
            match acquire_pooled_handle(&mut config, host_key_callback, &event_tx).await {
                AcquireOutcome::Attached(pooled, pool_key) => {
                    run_ssh_channel_loop(&pooled, cols, rows, false, false, cmd_rx, event_tx).await;
                    if let Some(key) = pool_key {
                        crate::pool::release(&ISEKAI_PIPE_QUIC_POOL, key, ISEKAI_PIPE_QUIC_IDLE_GRACE);
                    }
                }
                AcquireOutcome::DialFailed(e) | AcquireOutcome::OtherFailed(e) => {
                    warn!("isekai_pipe_quic: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
            }
        });
        Ok(())
    }

    /// `TransportPreference::Auto` 相当: ヘルパー経由 QUIC を試し、失敗したら
    /// 通常の TCP SSH（Phase 1-4）にフォールバックする。プーリングのプールヒット時、
    /// および他タブの確立待ち(waiter)がその後失敗を観測した場合はフォールバックしない
    /// (自分自身がダイヤルを試みて失敗した場合のみフォールバックする、既存の挙動を維持)。
    pub fn connect_auto(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let mut config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        let host_key_callback = self.core.callback();
        RUNTIME.spawn(async move {
            let (cols, rows) = (config.cols, config.rows);
            match acquire_pooled_handle(&mut config, host_key_callback, &event_tx).await {
                AcquireOutcome::Attached(pooled, pool_key) => {
                    run_ssh_channel_loop(&pooled, cols, rows, false, false, cmd_rx, event_tx).await;
                    if let Some(key) = pool_key {
                        crate::pool::release(&ISEKAI_PIPE_QUIC_POOL, key, ISEKAI_PIPE_QUIC_IDLE_GRACE);
                    }
                }
                AcquireOutcome::DialFailed(e) => {
                    warn!("isekai_pipe_quic auto: falling back to plain SSH after: {e}");
                    let ssh_config = crate::SshConfig {
                        host: config.ssh_host,
                        port: config.ssh_port,
                        username: config.username,
                        auth: config.auth,
                        cols, rows,
                        // IsekaiPipeQuicConfig にはポートフォワード設定・agent forwarding 設定が無いため、
                        // フォールバック時はどちらも無効なプレーン SSH として接続する。
                        forwards: Vec::new(),
                        agent_forward: false,
                        // IsekaiPipeQuicConfig には踏み台(jump host)設定も無いため、フォールバック時は
                        // 対象ホストへ直接SSH接続する前提のまま(SshConfig::jump 参照)。
                        jump: None,
                        allow_non_loopback_forward_bind: false,
                    };
                    crate::run_russh_transport(ssh_config, cmd_rx, event_tx).await;
                }
                AcquireOutcome::OtherFailed(e) => {
                    warn!("isekai_pipe_quic auto: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
            }
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

    /// Phase 1C(#26): OSからネットワーク断を通知された時の対応(`SessionCore`が
    /// 判断、詳細は`session.rs`の`should_abort_on_network_lost`参照)。QUICは
    /// `is_quic=true`固定 — 接続済みならtransport自身のtransparent resumeを信頼し
    /// 何もしない。
    pub fn notify_network_lost(&self) {
        self.core.notify_network_lost(true);
    }
}

// SessionOrchestrator からのみ呼ばれる内部API(uniffi には直接は出さない)。
impl IsekaiPipeQuicSession {
    /// Phase 12: per-session theme。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── 証明書ピン留め ───────────────────────────────────────
// handshake JSON の cert_sha256（SSH チャネル経由で受け渡し済み、TOFU より強い信頼の起点）
// とだけ照合する。通常の CA チェーン検証は行わない（自己署名 ephemeral cert のため）。

#[derive(Debug)]
pub(crate) struct PinnedCertVerifier {
    pub(crate) expected_sha256_hex: String,
    pub(crate) provider: Arc<rustls::crypto::CryptoProvider>,
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

async fn bootstrap_via_ssh(
    config: &IsekaiPipeQuicConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<IsekaiPipeHandshake, String> {
    bootstrap_helper_via_ssh(
        &config.ssh_host, config.ssh_port, &config.username, &config.auth, &config.jump,
        config.bind_port, &IsekaiPipeP2pMode::None, host_key_callback,
    ).await
}

/// ブートストラップ用 SSH 接続（`bootstrap_helper_via_ssh` 等）が受け取る
/// `TransportEvent::HostKey` を、本セッションの `on_host_key`（Kotlin 側の
/// `KnownHostRepository` を参照する既存の TOFU/変更検知ロジック）へ転送する
/// イベントループを spawn する。このセッションで発生し得る唯一の HostKey イベントは
/// このブートストラップ SSH 由来であり(QUIC データプレーン自体はホスト鍵という概念を
/// 持たず cert_sha256 ピン留めのみ)、同一の判断ロジックを流用してよい。
///
/// `callback` が `None`(呼び出し元がセッションのcallbackを取得できなかった場合)は、
/// フェイルセーフとして常に拒否する — 以前のように無条件承認はしない。
pub(crate) fn spawn_bootstrap_host_key_forwarder(
    mut event_rx: tokio::sync::mpsc::Receiver<TransportEvent>,
    callback: Option<Arc<dyn SessionCallback>>,
) {
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            if let TransportEvent::HostKey(fp, reply) = ev {
                let accepted = match &callback {
                    Some(cb) => {
                        let cb = Arc::clone(cb);
                        tokio::task::spawn_blocking(move || cb.on_host_key(fp)).await.unwrap_or(false)
                    }
                    None => {
                        warn!("bootstrap host key check: no session callback available, rejecting for safety");
                        false
                    }
                };
                let _ = reply.send(accepted);
            }
        }
    });
}

/// SSH でログインし、isekai-helper をブートストラップして起動ハンドシェイクを得る。
/// `IsekaiPipeQuicConfig`（Phase 7/8、フォールバック無し/Auto）と `MultipathIsekaiPipeQuicConfig`
/// （Phase 9-2、パス冗長化）は候補アドレスの持ち方が異なるだけで、SSH ブートストラップ自体は
/// 共通なのでここに切り出してある。
///
/// `jump`: 設定されていれば `ssh_host:ssh_port` へ直接ではなく踏み台経由で接続する
/// （`SshConfig::jump`・`transport::connect_via_jump_or_direct` 参照）。対象ホストが
/// NAT配下で直接到達できない場合、初回のisekai-helper配布・起動にはこれが唯一の経路になる。
///
/// `bind_port`: `None` なら isekai-helper は既定通りエフェメラルポートで待ち受ける
/// （Tailscale経由のpath0はこれで十分、`ts-input`チェーンが素通しなため）。
/// `Some(port)` を渡すと、direct_host（外部到達アドレス）向けにファイアウォールで
/// 個別に許可した固定ポートで待ち受けさせる（Phase 9-4、`multipath_transport.rs`から使用）。
///
/// `p2p_mode`: Phase 10 の STUN+SSH rendezvous / relay 経由 P2P
/// (`isekai_stun_p2p_transport.rs`/`isekai_link_relay_transport.rs`)専用。他の呼び出し元は
/// 常に `&IsekaiPipeP2pMode::None` を渡す。
///
/// `host_key_callback`: このブートストラップ用 SSH 接続(踏み台込み)のホスト鍵を、
/// 本セッションと同じ known_hosts エントリ(Kotlin 側 `KnownHostRepository`)で検証する
/// ための callback。呼び出し元は `SessionCore::callback()` から取得したものをそのまま
/// 渡す（`spawn_bootstrap_host_key_forwarder` 参照）。
pub(crate) async fn bootstrap_helper_via_ssh(
    ssh_host: &str,
    ssh_port: u16,
    username: &str,
    auth: &SshAuth,
    jump: &Option<JumpConfig>,
    bind_port: Option<u16>,
    p2p_mode: &IsekaiPipeP2pMode,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<IsekaiPipeHandshake, String> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
    spawn_bootstrap_host_key_forwarder(event_rx, host_key_callback);

    let russh_config = Arc::new(client::Config::default());
    let mut established = connect_via_jump_or_direct(jump, russh_config, ssh_host, ssh_port, event_tx)
        .await
        .map_err(|e| format!("bootstrap SSH connect failed: {e}"))?;

    let (authenticated, _) = authenticate_session(&mut established.handle, username, auth).await;
    if !authenticated {
        return Err("bootstrap SSH authentication failed".to_string());
    }

    let binaries = IsekaiPipeBinaries { x86_64: ISEKAI_PIPE_BIN_X86_64, aarch64: ISEKAI_PIPE_BIN_AARCH64 };
    // このtransportにはSTUNサーバー設定が無いため常に空スライス
    // (client_candidatesは付かないが、`--bootstrap-request-file`封筒自体は
    // 一貫して送る。isekai-terminal-core/isekai-ssh crate共有化 Phase 2c)。
    helper_bootstrap::ensure_helper_running(
        &mut established.handle, &binaries, ISEKAI_PIPE_VERSION, "127.0.0.1:22", bind_port, p2p_mode, &[],
    )
        .await
        .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────

/// `cert_sha256_hex` にピン留めした QUIC connection を確立する。初回接続・
/// reattach（`RESUME`）のどちらからも呼ばれる共通処理。
async fn establish_quic_connection(remote: SocketAddr, cert_sha256_hex: &str) -> Result<noq::Connection, String> {
    // 実機検証 (Phase 7-5) 用: `debug_fault` 経由で有効化されない限り
    // `FaultyUdpSocket` は素通しで、通常利用時の挙動には影響しない。
    let socket = crate::faulty_udp_socket::bind_faulty_udp_socket(
        "0.0.0.0:0".parse().unwrap(),
        crate::debug_fault::shared_injector(),
    )
    .map_err(|e| format!("endpoint bind failed: {e}"))?;
    establish_quic_connection_with_socket(socket, remote, cert_sha256_hex).await
}

/// `establish_quic_connection` の中身のうち、ソケットを事前に用意して渡したい呼び出し元
/// （Phase 10: `isekai_stun_p2p_transport.rs` が STUN 問い合わせ・穴あけ probe 送信に
/// 使ったのと同一のソケットを、そのまま QUIC endpoint にも使い回したい）向けの下請け関数。
pub(crate) async fn establish_quic_connection_with_socket(
    socket: crate::faulty_udp_socket::FaultyUdpSocket,
    remote: SocketAddr,
    cert_sha256_hex: &str,
) -> Result<noq::Connection, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS config failed".to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
            provider,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT はクライアント側でも使わない（HELPER_PROTOCOL.md「0-RTT はクライアント・
    // サーバー双方で完全に無効化する」契約）。noq::Connecting::into_0rtt() を呼ばず
    // 通常のハンドシェイク完了を待つのがクライアント側の対応。

    let quic_crypto = QuicClientConfig::try_from(crypto).map_err(|_| "QUIC crypto config failed".to_string())?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.max_idle_timeout(Some(
        noq::IdleTimeout::try_from(CLIENT_MAX_IDLE_TIMEOUT).expect("valid idle timeout"),
    ));
    transport.keep_alive_interval(Some(CLIENT_KEEP_ALIVE_INTERVAL));

    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));

    let endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        None,
        Box::new(socket),
        Arc::new(noq::TokioRuntime),
    )
    .map_err(|e| format!("endpoint bind failed: {e}"))?;
    endpoint.set_default_client_config(client_config);

    info!("isekai_pipe_quic: connecting to {remote}");
    let conn = endpoint
        .connect(remote, "isekai-pipe.local")
        .map_err(|e| format!("connect setup failed: {e}"))?
        .await
        .map_err(|e| format!("QUIC handshake failed: {e}"))?;
    info!("isekai_pipe_quic: QUIC handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
    Ok(conn)
}

/// ブランドニューな論理セッション用のランダムな `SessionId`（ATTACH v2 では
/// クライアントが接続開始前に採番する、`#18-4`）。1 接続 = 1 セッションなので
/// 呼び出しごとに新規生成してよい（isekai-transport のような複数候補で 1 つの
/// session_id を共有する round の概念は Android 側には無い）。
pub(crate) fn random_session_id() -> SessionId {
    let mut bytes = [0u8; SESSION_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    SessionId::from_bytes(bytes)
}

pub(crate) fn random_attempt_id() -> AttemptId {
    let mut bytes = [0u8; ATTEMPT_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    AttemptId::from_bytes(bytes)
}

/// ATTACH v2 ハンドシェイク（`ATTACH_HELLO`/`AttachReadyV2`/`ATTACH_ACTIVATE`）を、
/// 既に確立済みの QUIC connection 上の新しい bi-directional stream で行う。成功時は
/// 以降そのまま SSH のパススルーに使えるデータ stream を返す。`isekai-transport::relay::
/// connect_and_handshake` の ATTACH 部分を Android 側の 3 経路（direct/relay/STUN）で
/// 共有するために切り出したもの。Android には generation を進める fencing/リトライ層は
/// 無いので常に `ConnectionGeneration::INITIAL` を使う。
pub(crate) async fn attach_handshake(
    conn: &noq::Connection,
    session_secret: &[u8],
) -> Result<(noq::SendStream, noq::RecvStream), String> {
    let session_id = random_session_id();
    let generation = ConnectionGeneration::INITIAL;
    let attempt_id = random_attempt_id();
    // No client-configurable resume-grace concept on Android yet — `0` means
    // "no preference, use the server's own default/max".
    let requested_resume_grace_secs = 0;
    let transcript = attach_hello_proof_transcript(&session_id, generation, &attempt_id, requested_resume_grace_secs);
    let attach_proof = AttachProof::new(compute_proof(conn, session_secret, &transcript)?);

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi failed: {e}"))?;
    let hello = AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs, proof: attach_proof };
    send.write_all(&encode_attach_hello(&hello))
        .await
        .map_err(|e| format!("ATTACH_HELLO write failed: {e}"))?;

    match read_attach_response(&mut recv).await? {
        AttachResponse::Ready { attach_token, .. } => {
            let activate = AttachActivate { session_id, generation, attempt_id, attach_token };
            send.write_all(&encode_attach_activate(&activate))
                .await
                .map_err(|e| format!("ATTACH_ACTIVATE write failed: {e}"))?;
            Ok((send, recv))
        }
        AttachResponse::Reject(reason) => Err(attach_reject_message(reason)),
    }
}

/// `AttachResponse` を wire から読む: まず 1 バイトの type、その値に応じて追加バイトを
/// 読む（`FRAME_ATTACH_READY` は `ATTACH_READY_FRAME_LEN - 1`、
/// `FRAME_REJECT_STALE_GENERATION` は `STALE_GENERATION_REJECT_FRAME_LEN - 1`、
/// その他の既知 reject byte は追加なし）。`decode_attach_response` の契約に合わせた
/// 二段読み。
pub(crate) async fn read_attach_response(recv: &mut noq::RecvStream) -> Result<AttachResponse, String> {
    let mut type_byte = [0u8; 1];
    recv.read_exact(&mut type_byte).await.map_err(|e| format!("ATTACH response read failed: {e}"))?;
    let mut full = vec![type_byte[0]];
    let extra_len = match type_byte[0] {
        FRAME_ATTACH_READY => ATTACH_READY_FRAME_LEN - 1,
        FRAME_REJECT_STALE_GENERATION => STALE_GENERATION_REJECT_FRAME_LEN - 1,
        _ => 0,
    };
    if extra_len > 0 {
        let mut rest = vec![0u8; extra_len];
        recv.read_exact(&mut rest).await.map_err(|e| format!("ATTACH response read failed: {e}"))?;
        full.extend_from_slice(&rest);
    }
    decode_attach_response(&full).map_err(|e| format!("isekai-helper: {e}"))
}

/// `AttachRejectReason` を人間可読なエラー文字列にする。v1 の
/// `RejectAuth`/`RejectTarget`/`RejectUnsupported`/`RejectDuplicate` 相当に加え、
/// ATTACH v2 で新設された fencing 系の理由も区別できるメッセージにする。
pub(crate) fn attach_reject_message(reason: AttachRejectReason) -> String {
    match reason {
        AttachRejectReason::Auth => "isekai-helper rejected: auth (proof mismatch)".to_string(),
        AttachRejectReason::Target => "isekai-helper rejected: target unreachable".to_string(),
        AttachRejectReason::Unsupported => "isekai-helper rejected: unsupported frame".to_string(),
        AttachRejectReason::AlreadyAttached => {
            "isekai-helper rejected: a different attempt already attached".to_string()
        }
        AttachRejectReason::BusyOtherSession => {
            "isekai-helper rejected: a different session is currently active".to_string()
        }
        AttachRejectReason::AttachAlreadyEstablished => {
            "isekai-helper rejected: session already established, should resume instead".to_string()
        }
        AttachRejectReason::StaleGeneration { current_generation } => {
            format!("isekai-helper rejected: stale generation (server is at generation {current_generation})")
        }
    }
}

/// `session_secret` と QUIC connection の exporter から proof を計算する
/// （ATTACH の `extra` には proof transcript、RESUME は session_id、CONTROL_HELLO は
/// 空を渡す）。
pub(crate) fn compute_proof(conn: &noq::Connection, session_secret: &[u8], extra: &[u8]) -> Result<[u8; 32], String> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| format!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    if !extra.is_empty() {
        mac.update(extra);
    }
    Ok(mac.finalize().into_bytes().into())
}

/// `RESUME`フレームを送り`RESUME_ACK`を待つ。`conn`は呼び出し元が経路(direct/relay/STUN)
/// ごとに異なる方法で確立済みの、resume先への新規QUIC connectionであること。
/// quic/relay/stunの3ファイルで重複していたRESUME送受信ロジックを集約したもの
/// (isekai-terminal-core/isekai-transport crate共有化 Phase 1a)。
pub(crate) async fn send_resume_and_await_ack(
    conn: &noq::Connection,
    session_secret: &[u8],
    session_id: resume_client::SessionId,
    client_sent_offset: u64,
    client_delivered_offset: u64,
) -> Result<resume_client::ReattachResult, String> {
    let resume_proof = compute_proof(conn, session_secret, &session_id)?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi (resume) failed: {e}"))?;
    let mut frame = vec![resume_client::RESUME];
    frame.extend_from_slice(&session_id);
    frame.extend_from_slice(&resume_proof);
    frame.extend_from_slice(&client_sent_offset.to_be_bytes());
    frame.extend_from_slice(&client_delivered_offset.to_be_bytes());
    send.write_all(&frame).await.map_err(|e| format!("RESUME write failed: {e}"))?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await.map_err(|e| format!("RESUME_ACK read failed: {e}"))?;
    if resp[0] != resume_client::RESUME_ACK {
        return Err(format!("isekai-helper rejected resume: {:#x}", resp[0]));
    }
    let mut rest = [0u8; 16];
    recv.read_exact(&mut rest).await.map_err(|e| format!("RESUME_ACK body read failed: {e}"))?;
    let helper_committed_offset = u64::from_be_bytes(rest[0..8].try_into().unwrap());
    Ok(resume_client::ReattachResult { send, recv, helper_committed_offset })
}

/// Resolve the explicit `direct-by-bootstrap-host` mode.
///
/// This compatibility path reuses the SSH bootstrap host as the QUIC dial host.
/// It is isolated here so ordinary candidate selection does not inherit the
/// false premise that bootstrap reachability implies UDP/QUIC reachability.
pub(crate) async fn resolve_direct_by_bootstrap_host(
    bootstrap_host: &str,
    handshake: &IsekaiPipeHandshake,
) -> Result<SocketAddr, String> {
    let port = handshake
        .direct_by_bootstrap_host_port()
        .ok_or_else(|| "handshake did not advertise a direct-by-bootstrap-host candidate".to_string())?;
    tokio::net::lookup_host((bootstrap_host, port))
        .await
        .map_err(|e| format!("DNS lookup failed for direct-by-bootstrap-host {bootstrap_host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address resolved for direct-by-bootstrap-host {bootstrap_host}:{port}"))
}

async fn connect_isekai_pipe_quic_stream(
    ssh_host: &str,
    handshake: &IsekaiPipeHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let remote = resolve_direct_by_bootstrap_host(ssh_host, handshake).await?;
    let cert_sha256_hex = handshake.cert_sha256().to_string();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let conn = establish_quic_connection(remote, &cert_sha256_hex).await?;

    // control stream の CONTROL_HELLO で使う plain proof（exporter のみ、ATTACH の
    // transcript 付き proof とは別物）。ATTACH ハンドシェイク自体は attach_handshake が
    // 独自に transcript 付き proof を計算する。
    let proof = compute_proof(&conn, &session_secret, b"")?;

    let (send, recv) = attach_handshake(&conn, &session_secret).await?;
    info!("isekai_pipe_quic: ATTACH ok — handing off to SSH");

    let resume_state = Arc::new(std::sync::Mutex::new(ClientResumeState::new(
        DEFAULT_RESUME_BUFFER_SIZE,
    )));

    // control stream の確立を待たずに即座に SSH セッションへ渡す。
    // noq の open_bi() は相手の stream credit が尽きていると（旧 helper 等）
    // 即座にエラーを返さず MAX_STREAMS を待って内部でブロックし得るため、
    // ここで await してしまうと旧 helper 相手に毎回 CONTROL_STREAM_TIMEOUT 分
    // シェル開始が遅延する（isekai-helper 側の e2e テストで実際に踏んだ
    // リグレッションと同種の問題）。control stream の確立は背後タスクとして
    // 並行に進める。
    {
        let conn = conn.clone();
        let proof = proof.to_vec();
        let resume_state = resume_state.clone();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(CONTROL_STREAM_TIMEOUT, open_control_stream(&conn, &proof)).await {
                Ok(Ok((csend, crecv, session_id))) => {
                    info!(
                        "isekai_pipe_quic: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    spawn_app_ack_tasks(csend, crecv, resume_state);
                }
                Ok(Err(e)) => {
                    info!("isekai_pipe_quic: control stream handshake failed ({e}), continuing without resume support");
                }
                Err(_) => {
                    info!("isekai_pipe_quic: control stream not accepted within timeout, continuing without resume support");
                }
            }
        });
    }

    // Phase 8-3: QUIC connection が失われても RESUME で reattach する
    // クロージャ。`remote`/`cert_sha256_hex`/`session_secret` を捕捉する。
    let reattach_fn: resume_client::ReattachFn = Arc::new(move |session_id, client_sent_offset, client_delivered_offset| {
        let cert_sha256_hex = cert_sha256_hex.clone();
        let session_secret = session_secret.clone();
        Box::pin(async move {
            let conn = establish_quic_connection(remote, &cert_sha256_hex).await?;
            let result = send_resume_and_await_ack(&conn, &session_secret, session_id, client_sent_offset, client_delivered_offset).await?;
            info!("isekai_pipe_quic: resume succeeded, helper_committed_offset={}", result.helper_committed_offset);
            Ok(result)
        })
    });

    Ok(resume_client::ReattachableStream::new(send, recv, resume_state, reattach_fn))
}

/// control stream を開き、`CONTROL_HELLO` を送って `CONTROL_ACK` を待つ。
/// data stream の HELLO と同じ `proof` を再利用する（同一 QUIC connection の
/// exporter から計算されるため同じ値になる、HELPER_PROTOCOL.md §7.3）。
pub(crate) async fn open_control_stream(
    conn: &noq::Connection,
    proof: &[u8],
) -> Result<(noq::SendStream, noq::RecvStream, resume_client::SessionId), String> {
    let (mut csend, mut crecv) = conn.open_bi().await.map_err(|e| format!("open_bi (control) failed: {e}"))?;
    let mut hello = Vec::with_capacity(33);
    hello.push(resume_client::CONTROL_HELLO);
    hello.extend_from_slice(proof);
    csend
        .write_all(&hello)
        .await
        .map_err(|e| format!("CONTROL_HELLO write failed: {e}"))?;

    let mut ack = [0u8; 17];
    crecv
        .read_exact(&mut ack)
        .await
        .map_err(|e| format!("CONTROL_ACK read failed: {e}"))?;
    if ack[0] != resume_client::CONTROL_ACK {
        return Err(format!("unexpected control response byte {:#x}", ack[0]));
    }
    let mut session_id = [0u8; 16];
    session_id.copy_from_slice(&ack[1..17]);
    Ok((csend, crecv, session_id))
}

/// APP_ACK の送受信を行う背後タスクを spawn する。data stream が閉じた後も
/// これらのタスクは自然に終了しない（control stream 側の read/write が
/// エラーになった時点でループを抜ける、ベストエフォート設計）。
pub(crate) fn spawn_app_ack_tasks(
    mut csend: noq::SendStream,
    mut crecv: noq::RecvStream,
    state: Arc<std::sync::Mutex<ClientResumeState>>,
) {
    // APP_ACK 受信: helper からの helper_committed_offset を受け取り、
    // input replay buffer の破棄範囲を進める。
    {
        let state = state.clone();
        RUNTIME.spawn(async move {
            loop {
                let mut frame = [0u8; 9];
                match crecv.read_exact(&mut frame).await {
                    Ok(()) if frame[0] == resume_client::APP_ACK => {
                        let offset = u64::from_be_bytes(frame[1..9].try_into().unwrap());
                        state.lock().unwrap().replay_buffer.advance_start(offset);
                    }
                    _ => break,
                }
            }
        });
    }

    // APP_ACK 送信: client_delivered_offset（S→C の受信確認）を 200ms ごとに送る。
    RUNTIME.spawn(async move {
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let current = state.lock().unwrap().client_delivered_offset;
            if current == last_sent {
                continue;
            }
            let mut frame = Vec::with_capacity(9);
            frame.push(resume_client::APP_ACK);
            frame.extend_from_slice(&current.to_be_bytes());
            if csend.write_all(&frame).await.is_err() {
                break;
            }
            last_sent = current;
        }
    });
}

async fn try_connect_isekai_pipe_quic(
    config: &IsekaiPipeQuicConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<resume_client::ReattachableStream, String> {
    let handshake = bootstrap_via_ssh(config, host_key_callback).await?;
    connect_isekai_pipe_quic_stream(&config.ssh_host, &handshake).await
}

// ── SSH接続プーリング(isekai-pipe QUICファミリー) ────────────
//
// `archive/ISEKAI_SSH_DESIGN.md`「QUIC系トランスポート(isekai-pipeファミリー)への拡張」
// 節参照。isekai-pipeのwireプロトコルは「1 QUIC connection = 1 data stream」の
// 1対1構造しか持たないため、QUIC接続を複数タブで共有する時点で、その上のネストした
// SSH `client::Handle`も自動的に1個だけになる。したがって「ブートストラップSSH→
// ヘルパー起動→QUICハンドシェイク→ネストしたSSH認証」という一連の処理全体を、
// プレーンSSHの`client::Handle`プーリングと同じ形(1プールエントリに複数タブが
// `channel_open_session()`するだけ)で扱える。

/// isekai-pipe QUIC接続の同一性を決める識別子。パスワード認証は`for_config`が`None`を
/// 返す(常にプール対象外)。フィールドの根拠は`SshPoolKey`と同様(`pool.rs`参照)。
#[derive(Clone, PartialEq, Eq, Hash)]
struct IsekaiPipeQuicPoolKey {
    ssh_host: String,
    ssh_port: u16,
    username: String,
    auth_identity: String,
    /// (host, port, username, auth_identity)。ブートストラップ用踏み台の同一性判定にのみ使う。
    jump: Option<(String, u16, String, String)>,
    /// ヘルパーの固定待受ポート指定。タブによって食い違うと同一ヘルパーインスタンスに
    /// 繋がる保証が無いためキーに含める。
    bind_port: Option<u16>,
}

impl IsekaiPipeQuicPoolKey {
    fn for_config(config: &IsekaiPipeQuicConfig) -> Option<Self> {
        let auth_identity = crate::pool::auth_identity_fingerprint(&config.auth)?;
        let jump = match &config.jump {
            None => None,
            Some(j) => {
                let jump_auth_identity = crate::pool::auth_identity_fingerprint(&j.auth)?;
                Some((j.host.clone(), j.port, j.username.clone(), jump_auth_identity))
            }
        };
        Some(IsekaiPipeQuicPoolKey {
            ssh_host: config.ssh_host.clone(),
            ssh_port: config.ssh_port,
            username: config.username.clone(),
            auth_identity,
            jump,
            bind_port: config.bind_port,
        })
    }
}

static ISEKAI_PIPE_QUIC_POOL: std::sync::LazyLock<crate::pool::PoolMap<IsekaiPipeQuicPoolKey, PooledSshHandle>> =
    std::sync::LazyLock::new(crate::pool::new_pool_map);

/// QUIC接続確立コスト(ヘルパー起動＋QUICハンドシェイク＋ネスト認証)はプレーンSSHの
/// TCP接続よりも明らかに高いため、プレーンSSH(30秒、`pool::PLAIN_SSH_IDLE_GRACE`)より
/// 長い猶予を置く。
const ISEKAI_PIPE_QUIC_IDLE_GRACE: Duration = Duration::from_secs(90);

enum AcquireError {
    /// ブートストラップSSH/QUICハンドシェイク自体の失敗。`connect_auto`はこの場合のみ
    /// プレーンSSHへフォールバックする(既存の挙動を維持)。
    DialFailed(String),
    /// ダイヤル自体は成功したが、その後(ネストしたSSH認証等)で失敗。フォールバック対象外
    /// (既存の挙動を維持、`Disconnected`イベントで通常通り報告する)。
    PostDialFailed(String),
}

/// ダイヤル(ブートストラップ+QUICハンドシェイク)からネストしたSSH認証までを毎回
/// フルで行う。プールにヒットしなかった場合、またはこのタブがプールの確立担当に
/// なった場合にのみ呼ばれる。
async fn establish_fresh(
    config: &mut IsekaiPipeQuicConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
    event_tx: &tokio::sync::mpsc::Sender<TransportEvent>,
) -> Result<PooledSshHandle, AcquireError> {
    let stream = try_connect_isekai_pipe_quic(config, host_key_callback)
        .await
        .map_err(AcquireError::DialFailed)?;
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });
    // IsekaiPipeQuicConfig は agent forwarding 未対応（プロファイルの `SshConfig.agent_forward`
    // 相当のフィールドをまだ持たない）。
    establish_ssh_handle_over_stream(russh_config, stream, &config.username, &mut config.auth, false, event_tx)
        .await
        .map_err(AcquireError::PostDialFailed)
}

enum AcquireOutcome {
    Attached(Arc<PooledSshHandle>, Option<IsekaiPipeQuicPoolKey>),
    DialFailed(String),
    OtherFailed(String),
}

/// プールにヒットすればダイヤル(ブートストラップ+QUICハンドシェイク+ネスト認証)を
/// 丸ごとスキップし、既存の認証済みHandleを返す。ヒットしなければ`establish_fresh`で
/// ゼロから確立してプールへ登録する。
async fn acquire_pooled_handle(
    config: &mut IsekaiPipeQuicConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
    event_tx: &tokio::sync::mpsc::Sender<TransportEvent>,
) -> AcquireOutcome {
    match IsekaiPipeQuicPoolKey::for_config(config) {
        None => match establish_fresh(config, host_key_callback, event_tx).await {
            Ok(p) => AcquireOutcome::Attached(Arc::new(p), None),
            Err(AcquireError::DialFailed(m)) => AcquireOutcome::DialFailed(m),
            Err(AcquireError::PostDialFailed(m)) => AcquireOutcome::OtherFailed(m),
        },
        Some(key) => match crate::pool::try_attach(&ISEKAI_PIPE_QUIC_POOL, &key) {
            crate::pool::AttachOutcome::Ready(v) => {
                zeroize_ssh_auth(&mut config.auth);
                AcquireOutcome::Attached(v, Some(key))
            }
            crate::pool::AttachOutcome::Waiter(rx) => {
                zeroize_ssh_auth(&mut config.auth);
                match crate::pool::wait_for_establish(rx).await {
                    Ok(v) => AcquireOutcome::Attached(v, Some(key)),
                    Err(m) => AcquireOutcome::OtherFailed(m),
                }
            }
            crate::pool::AttachOutcome::Establisher => {
                match establish_fresh(config, host_key_callback, event_tx).await {
                    Ok(p) => AcquireOutcome::Attached(
                        crate::pool::publish_success(&ISEKAI_PIPE_QUIC_POOL, &key, p), Some(key),
                    ),
                    Err(AcquireError::DialFailed(m)) => {
                        crate::pool::publish_failure(&ISEKAI_PIPE_QUIC_POOL, &key, m.clone());
                        AcquireOutcome::DialFailed(m)
                    }
                    Err(AcquireError::PostDialFailed(m)) => {
                        crate::pool::publish_failure(&ISEKAI_PIPE_QUIC_POOL, &key, m.clone());
                        AcquireOutcome::OtherFailed(m)
                    }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    //! `IsekaiPipeQuicSession` を orchestrator レベルではなく直接使い、SSH ブートストラップ →
    //! isekai-helper QUIC → russh チャネル → 実シェルコマンド実行までを通しで検証する。
    //! 実 sshd（127.0.0.1:22）+ `ISEKAI_PIPE_BOOTSTRAP_TEST_KEY` が必要な opt-in テスト。
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    // ── spawn_bootstrap_host_key_forwarder: 実SSH/QUIC不要のユニットテスト ──
    // Task #56: ブートストラップ用SSH接続のホスト鍵イベントが、以前のように無条件で
    // 承認されるのではなく、本セッションのcallback(Kotlin側のKnownHostRepositoryを
    // 参照する既存のTOFU/変更検知ロジック相当)の判断どおりに承認/拒否されることを検証する。

    struct FixedResponseCallback {
        /// このfingerprintに一致する場合のみ承認する(=既知ホストの正しい鍵)。
        /// それ以外(=ホスト鍵変更・MITMシナリオ)は拒否する。
        trusted_fingerprint: String,
    }

    impl SessionCallback for FixedResponseCallback {
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_host_key(&self, fingerprint: String) -> bool { fingerprint == self.trusted_fingerprint }
        fn on_connected(&self) {}
        fn on_disconnected(&self, _reason: Option<String>) {}
        fn on_screen_update(&self, _update: crate::ScreenUpdate) {}
        fn on_trzsz_request(&self, _t: String, _m: String, _n: Option<String>, _s: Option<u64>) {}
        fn on_trzsz_download_chunk(&self, _t: String, _d: Vec<u8>, _l: bool) {}
        fn on_trzsz_progress(&self, _t: String, _tr: u64, _to: Option<u64>) {}
        fn on_trzsz_finished(&self, _t: String, _s: bool, _m: Option<String>) {}
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: crate::ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
    }

    /// 既知ホストと異なる鍵(ホスト鍵変更/MITMシナリオ)を返した場合、ブートストラップ
    /// 接続が拒否されることを確認する。修正前は`reply.send(true)`固定で、この
    /// シナリオでも常に接続が承認されてしまっていた。
    #[tokio::test]
    async fn bootstrap_host_key_forwarder_rejects_changed_host_key() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(4);
        spawn_bootstrap_host_key_forwarder(
            event_rx,
            Some(Arc::new(FixedResponseCallback { trusted_fingerprint: "known-good-fp".to_string() })),
        );

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(TransportEvent::HostKey("attacker-fp-after-mitm".to_string(), reply_tx))
            .await
            .unwrap();
        assert!(!reply_rx.await.unwrap(), "changed host key must be rejected, not silently accepted");
    }

    /// 既知ホストと同じ鍵(通常の再接続)なら承認されることも確認する(拒否一辺倒の
    /// 実装への逃げを防ぐ)。
    #[tokio::test]
    async fn bootstrap_host_key_forwarder_accepts_matching_known_host_key() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(4);
        spawn_bootstrap_host_key_forwarder(
            event_rx,
            Some(Arc::new(FixedResponseCallback { trusted_fingerprint: "known-good-fp".to_string() })),
        );

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx.send(TransportEvent::HostKey("known-good-fp".to_string(), reply_tx)).await.unwrap();
        assert!(reply_rx.await.unwrap(), "matching known host key must be accepted");
    }

    /// callback を取得できなかった場合(プログラミングエラー等)は、フェイルセーフとして
    /// 常に拒否する(無条件承認にフォールバックしてはいけない)。
    #[tokio::test]
    async fn bootstrap_host_key_forwarder_rejects_when_callback_missing() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(4);
        spawn_bootstrap_host_key_forwarder(event_rx, None);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx.send(TransportEvent::HostKey("whatever-fp".to_string(), reply_tx)).await.unwrap();
        assert!(!reply_rx.await.unwrap(), "missing callback must fail closed (reject), not fail open");
    }

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
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: crate::ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
    }

    #[tokio::test]
    async fn full_stack_bootstrap_quic_and_shell_command() {
        let Ok(key_path) = std::env::var("ISEKAI_PIPE_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: ISEKAI_PIPE_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        // 秘密鍵の実体はテストなので PEM のまま渡す（本番の SshAuth::PublicKey と同じ形）。
        let config = IsekaiPipeQuicConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
        };

        let session = create_isekai_pipe_quic_session(config);
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

    /// Phase 8-3: 実際に QUIC connection を切断してから復旧させ、
    /// `ReattachableStream` が russh にエラーを見せずに同じ SSH セッションを
    /// 継続させることを検証する。`debug_fault::shared_injector()` はプロセス
    /// グローバルな状態なので、**このテストは他のテストと同時実行しないこと**
    /// （`cargo test --lib isekai_pipe_quic_transport::tests::resume_survives_connection_cut`
    /// のように単独実行する）。
    #[tokio::test]
    async fn resume_survives_connection_cut() {
        let Ok(key_path) = std::env::var("ISEKAI_PIPE_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: ISEKAI_PIPE_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        let config = IsekaiPipeQuicConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
        };

        crate::debug_fault::shared_injector().restore();
        crate::debug_fault::shared_injector().set_latency(Duration::ZERO);
        crate::debug_fault::shared_injector().set_loss_rate(0.0);

        let session = create_isekai_pipe_quic_session(config);
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let callback = TestCallback { buf: buf.clone(), notify: notify.clone() };
        session.connect(Box::new(callback)).expect("connect() call failed");

        tokio::time::sleep(Duration::from_millis(800)).await;
        session.send(b"echo before-cut\n".to_vec());
        wait_for_output(&buf, &notify, "before-cut", Duration::from_secs(10)).await;

        // control stream が確立し session_id が発行されるまで少し待ってから切断する
        // （control stream 未確立のまま切ると resume できず Failed になるのは仕様通り）。
        tokio::time::sleep(Duration::from_millis(500)).await;

        eprintln!("test: cutting connection");
        crate::debug_fault::shared_injector().cut();
        // ReattachableStream が失敗を検知し、reattach タスクが 1 回目の
        // リトライを試みる程度の時間だけ切断したままにする。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        eprintln!("test: restoring connection");
        crate::debug_fault::shared_injector().restore();

        // reattach が完了して同じセッションで応答が返ってくることを確認する。
        session.send(b"echo after-resume\n".to_vec());
        wait_for_output(&buf, &notify, "after-resume", Duration::from_secs(20)).await;

        session.disconnect();
        crate::debug_fault::shared_injector().restore();
    }

    async fn wait_for_output(
        buf: &Arc<StdMutex<Vec<u8>>>,
        notify: &Arc<Notify>,
        needle: &str,
        timeout: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let b = buf.lock().unwrap();
                if String::from_utf8_lossy(&b).contains(needle) {
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; got so far: {:?}",
                    String::from_utf8_lossy(&buf.lock().unwrap())
                );
            }
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .ok();
        }
    }
}

// ── IsekaiPipeQuicPoolKey: 実SSH/QUIC不要のユニットテスト ──────────
//
// プーリングの要である`IsekaiPipeQuicPoolKey::for_config`の同一性判定は、既存の
// opt-in e2eテスト(実sshd必須)だけではカバーされていなかった。`pool.rs`の
// `SshPoolKey`テストと同じ観点を、QUIC固有のフィールド(`bind_port`)も含めて検証する。
#[cfg(test)]
mod pool_key_tests {
    use super::*;

    fn password_config() -> IsekaiPipeQuicConfig {
        IsekaiPipeQuicConfig {
            ssh_host: "host".into(),
            ssh_port: 22,
            username: "user".into(),
            auth: SshAuth::Password { password: "hunter2".into() },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
        }
    }

    fn key_config(seed: u8) -> IsekaiPipeQuicConfig {
        use russh_keys::ssh_key::private::Ed25519Keypair;
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let key = russh_keys::PrivateKey::from(keypair);
        IsekaiPipeQuicConfig {
            ssh_host: "host".into(),
            ssh_port: 22,
            username: "user".into(),
            auth: SshAuth::PublicKey {
                private_key_pem: key.to_openssh(Default::default()).unwrap().as_bytes().to_vec(),
            },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
        }
    }

    #[test]
    fn password_auth_never_produces_a_pool_key() {
        assert!(IsekaiPipeQuicPoolKey::for_config(&password_config()).is_none());
    }

    #[test]
    fn same_config_produces_equal_keys() {
        let a = IsekaiPipeQuicPoolKey::for_config(&key_config(1)).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&key_config(1)).unwrap();
        assert!(a == b);
    }

    #[test]
    fn different_keys_produce_different_pool_keys() {
        let a = IsekaiPipeQuicPoolKey::for_config(&key_config(1)).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&key_config(2)).unwrap();
        assert!(a != b);
    }

    #[test]
    fn different_ssh_host_produces_different_pool_keys() {
        let mut cfg_b = key_config(1);
        cfg_b.ssh_host = "other-host".into();
        let a = IsekaiPipeQuicPoolKey::for_config(&key_config(1)).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&cfg_b).unwrap();
        assert!(a != b);
    }

    #[test]
    fn different_bind_port_produces_different_pool_keys() {
        let mut cfg_a = key_config(1);
        cfg_a.bind_port = Some(45000);
        let mut cfg_b = key_config(1);
        cfg_b.bind_port = Some(45001);
        let a = IsekaiPipeQuicPoolKey::for_config(&cfg_a).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&cfg_b).unwrap();
        assert!(a != b, "a fixed helper listen port mismatch must not share a pooled connection");
    }

    #[test]
    fn same_bind_port_produces_equal_keys() {
        let mut cfg_a = key_config(1);
        cfg_a.bind_port = Some(45000);
        let mut cfg_b = key_config(1);
        cfg_b.bind_port = Some(45000);
        let a = IsekaiPipeQuicPoolKey::for_config(&cfg_a).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&cfg_b).unwrap();
        assert!(a == b);
    }

    #[test]
    fn different_jump_produces_different_pool_keys() {
        let mut cfg_a = key_config(1);
        let mut cfg_b = key_config(1);
        let jump_auth = |seed: u8| {
            use russh_keys::ssh_key::private::Ed25519Keypair;
            let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
            let key = russh_keys::PrivateKey::from(keypair);
            SshAuth::PublicKey {
                private_key_pem: key.to_openssh(Default::default()).unwrap().as_bytes().to_vec(),
            }
        };
        cfg_a.jump = Some(JumpConfig { host: "jump-a".into(), port: 22, username: "j".into(), auth: jump_auth(9) });
        cfg_b.jump = Some(JumpConfig { host: "jump-b".into(), port: 22, username: "j".into(), auth: jump_auth(9) });
        let a = IsekaiPipeQuicPoolKey::for_config(&cfg_a).unwrap();
        let b = IsekaiPipeQuicPoolKey::for_config(&cfg_b).unwrap();
        assert!(a != b);
    }

    #[test]
    fn jump_with_password_auth_makes_the_whole_config_unpoolable() {
        let mut cfg = key_config(1);
        cfg.jump = Some(JumpConfig {
            host: "jump".into(), port: 22, username: "j".into(),
            auth: SshAuth::Password { password: "hunter2".into() },
        });
        assert!(
            IsekaiPipeQuicPoolKey::for_config(&cfg).is_none(),
            "a password-authenticated jump host has no stable identity, so pooling must be skipped entirely"
        );
    }
}
