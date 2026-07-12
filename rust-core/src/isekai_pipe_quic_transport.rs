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
use log::{info, warn};
use russh::client;

use isekai_protocol::attach::{
    decode_attach_response, AttachRejectReason, AttachResponse, AttemptId, ATTACH_READY_FRAME_LEN, ATTEMPT_ID_LEN,
    FRAME_ATTACH_READY, FRAME_REJECT_STALE_GENERATION, STALE_GENERATION_REJECT_FRAME_LEN,
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

/// C→S input replay buffer の既定上限（helper 側 `DEFAULT_RESUME_BUFFER_SIZE` と揃える）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;
/// control stream を開く/CONTROL_ACK を待つタイムアウト（helper 側 `HELLO_TIMEOUT` と揃える）。
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

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

// `SessionOrchestrator`(orchestrator.rs)がActiveSession::IsekaiPipeQuicとして
// 内部的に使う実装。両OSともSessionOrchestrator/OrchestratorCallbackへ移行済みのため
// (2026-07-11)、UniFFIへの公開はやめてクレート内部専用にした。
pub(crate) struct IsekaiPipeQuicSession {
    config: IsekaiPipeQuicConfig,
    core: SessionCore,
}

pub(crate) fn create_isekai_pipe_quic_session(config: IsekaiPipeQuicConfig) -> Arc<IsekaiPipeQuicSession> {
    init_logger();
    Arc::new(IsekaiPipeQuicSession { config, core: SessionCore::new() })
}

impl IsekaiPipeQuicSession {
    /// 明示的にヘルパー経由 QUIC のみを試す（フォールバック無し）。SSH接続プーリング
    /// (`archive/ISEKAI_SSH_DESIGN.md`参照)により、同一ホスト/ユーザー/鍵/ブートストラップ
    /// パラメータへ既にプールされたHandleがあれば、ブートストラップSSH・ヘルパー起動・
    /// QUICハンドシェイク・ネストしたSSH認証を丸ごとスキップして新しいSSHチャネルだけ開く。
    pub(crate) fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
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
    pub(crate) fn connect_auto(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
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

    pub(crate) fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub(crate) fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub(crate) fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub(crate) fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub(crate) fn disconnect(&self) { self.core.disconnect(); }

    pub(crate) fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub(crate) fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub(crate) fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub(crate) fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }

    /// Phase 12: per-session theme。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── 証明書ピン留め ───────────────────────────────────────
// かつてここにあった`PinnedCertVerifier`は、最後の呼び出し元だった
// multipath_transport.rsがisekai_transport::system::client_config_for
// (同等のpin検証ロジックを内蔵)経由に移行したことで完全に不要になったため削除した
// (isekai-terminal-core/isekai-transport crate共有化)。

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

// ── ATTACH v2 補助（multipath_transport.rs と共有） ───────────────
// isekai_pipe_quic_transport.rs 自身は自前のQUIC接続確立/ATTACH実装を
// isekai-transport経由へ置き換え済み(isekai-terminal-core/isekai-transport
// crate共有化 Phase 1c)だが、multipath_transport.rs(Phase 9、物理マルチパス、
// isekai-transportにまだ対応する抽象が無いため対象外)はこの節の関数群を
// 引き続き参照する。

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

/// isekai-transport::relay::RelayTarget"の`server_name`はisekai-pipe serveが
/// 一切検証しない固定文字列でよい(`RemoteSpec::server_name`のdocコメント参照、
/// 証明書検証は`cert_sha256_hex`のピン留めのみで行う)。Android側3経路(direct/
/// relay/STUN)で揃えて使ってきた既存の値をそのまま踏襲する。
pub(crate) const QUIC_SERVER_NAME: &str = "isekai-pipe.local";

async fn connect_isekai_pipe_quic_stream(
    ssh_host: &str,
    handshake: &IsekaiPipeHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let remote = resolve_direct_by_bootstrap_host(ssh_host, handshake).await?;
    let cert_sha256_hex = handshake.cert_sha256().to_string();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let target = isekai_transport::RelayTarget {
        helper_addr: remote,
        server_name: QUIC_SERVER_NAME.to_string(),
        cert_sha256_hex,
        session_secret,
        // No local-port-range restriction on Android today — this is a
        // desktop-firewall/NAT concern `isekai-ssh`'s CLI callers opted into
        // (`#@isekai local-bind-port-range`), not yet exposed to the app.
        local_bind_port_range: None,
    };
    let factory = crate::android_quic_endpoint::factory();
    let (conn, data_stream, proof) = isekai_transport::connect_via_relay_with_connection(&factory, &target)
        .await
        .map_err(|e| e.to_string())?;
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
    // 並行に進める。`conn`はここでしか使わないためそのままmoveする
    // （isekai-transportの`Box<dyn QuicConnection>`は`noq::Connection`と違い
    // `Clone`ではないが、streamが内部でconnectionを生かし続けるため元々clone
    // する必要は無かった）。
    {
        let resume_state = resume_state.clone();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(
                CONTROL_STREAM_TIMEOUT,
                isekai_transport::resume::open_control_stream(&conn, &proof),
            )
            .await
            {
                Ok(Ok(control)) => {
                    let session_id = *control.session_id.as_bytes();
                    info!(
                        "isekai_pipe_quic: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    let counters = Arc::new(isekai_transport::resume::AppAckCounters::new());
                    isekai_transport::resume::spawn_app_ack_tasks(control.stream, counters.clone());
                    spawn_app_ack_bridge(resume_state, counters);
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
    // クロージャ。`target`を捕捉する。
    let reattach_fn: resume_client::ReattachFn<quicmux::AnyByteStreamReadHalf, quicmux::AnyByteStreamWriteHalf> = Arc::new({
        let resume_state = resume_state.clone();
        move |session_id, client_sent_offset, client_delivered_offset| {
            let factory = crate::android_quic_endpoint::factory();
            let target = target.clone();
            let resume_state = resume_state.clone();
            Box::pin(async move {
                let outcome = isekai_transport::resume::reconnect_and_resume(
                    &factory,
                    &target,
                    isekai_transport::SessionId::from_bytes(session_id),
                    isekai_transport::C2hSentOffset::new(client_sent_offset),
                    isekai_transport::H2cClientDeliveredOffset::new(client_delivered_offset),
                )
                .await
                .map_err(|e| e.to_string())?;
                info!("isekai_pipe_quic: resume succeeded, helper_committed_offset={}", outcome.helper_committed_offset);
                spawn_control_stream_reestablishment_after_resume(
                    "isekai_pipe_quic",
                    outcome.connection.clone(),
                    target.session_secret.clone(),
                    resume_state,
                );
                let (read, write) = outcome.data_stream.split();
                Ok(resume_client::ReattachResult { read, write, helper_committed_offset: outcome.helper_committed_offset.get() })
            })
        }
    });

    let (data_read, data_write) = data_stream.split();
    Ok(resume_client::ReattachableStream::new(data_read, data_write, resume_state, reattach_fn))
}

/// isekai-transportの`AppAckCounters`(atomicベース)とAndroid側の
/// `ClientResumeState`(replay_buffer/client_delivered_offsetベース)を
/// 定期的に橋渡しする。`isekai_transport::resume::spawn_app_ack_tasks`は
/// `AppAckCounters`のみを更新するため、`resume_client::ReattachableStream`が
/// 既に使っている`ClientResumeState`ベースのreplay/offset管理と直接には
/// つながらない — 200ms間隔(APP_ACK自体の送信間隔と同じ)でどちらの方向も
/// 同期する(isekai-terminal-core/isekai-transport crate共有化 Phase 1c)。
pub(crate) fn spawn_app_ack_bridge(
    resume_state: Arc<std::sync::Mutex<ClientResumeState>>,
    counters: Arc<isekai_transport::resume::AppAckCounters>,
) {
    RUNTIME.spawn(async move {
        let mut last_delivered_synced = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let current_delivered = {
                let mut st = resume_state.lock().unwrap();
                st.replay_buffer.advance_start(counters.c2h_helper_committed_offset());
                st.client_delivered_offset
            };
            if current_delivered > last_delivered_synced {
                counters.advance_h2c_client_delivered_offset(current_delivered - last_delivered_synced);
                last_delivered_synced = current_delivered;
            }
        }
    });
}

/// RESUME成功後に`conn`(reconnect_and_resumeが返した新しいconnection)上で
/// control streamを再確立し、成功すればAPP_ACKベースのバッファtrimming
/// (`spawn_app_ack_tasks`+`spawn_app_ack_bridge`)を再開する — 初回ATTACH後の
/// control stream確立(この関数の呼び出し元3ファイルそれぞれの
/// `connect_*_stream`冒頭)の、reattach版。これが無いと最初の1回だけ
/// resumeが効いて以降のreattachでは`ClientResumeState.replay_buffer`の
/// trimming/`client_delivered_offset`同期が止まったままになる
/// (isekai-transport側`resume::finish_via_resume`が同じ理由でRESUME後に
/// control streamを再確立しているのと同じ問題、quicmux-server-resume
/// Stage Bで発見)。初回接続時と同じくfire-and-forgetかつ
/// `CONTROL_STREAM_TIMEOUT`で打ち切る — 旧verのhelper相手にcontrol stream
/// が確立できない場合でも、resumeしたdata streamをSSHセッションへ渡すのを
/// 遅らせてはいけない。
pub(crate) fn spawn_control_stream_reestablishment_after_resume(
    log_prefix: &'static str,
    conn: quicmux::AnyMuxConnection,
    session_secret: Vec<u8>,
    resume_state: Arc<std::sync::Mutex<ClientResumeState>>,
) {
    RUNTIME.spawn(async move {
        let established = async {
            let proof = isekai_transport::compute_proof(&conn, &session_secret, b"")
                .await
                .map_err(|e| e.to_string())?;
            isekai_transport::resume::open_control_stream(&conn, &proof)
                .await
                .map_err(|e| e.to_string())
        };
        match tokio::time::timeout(CONTROL_STREAM_TIMEOUT, established).await {
            Ok(Ok(control)) => {
                info!("{log_prefix}: control stream re-established after resume, session_id={}", control.session_id);
                let counters = Arc::new(isekai_transport::resume::AppAckCounters::new());
                isekai_transport::resume::spawn_app_ack_tasks(control.stream, counters.clone());
                spawn_app_ack_bridge(resume_state, counters);
            }
            Ok(Err(e)) => {
                info!("{log_prefix}: control stream re-establishment after resume failed ({e}), continuing without resume support until the next reattach");
            }
            Err(_) => {
                info!("{log_prefix}: control stream re-establishment after resume timed out, continuing without resume support until the next reattach");
            }
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
        fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
        fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
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
        fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
        fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
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
