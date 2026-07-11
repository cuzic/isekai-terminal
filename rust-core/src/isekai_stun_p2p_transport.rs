//! Phase 10: STUN+SSH rendezvous による P2P QUIC トランスポート。
//!
//! `isekai_pipe_quic_transport.rs`（Phase 7、SSH経由到達アドレスへ直接QUIC接続）とは異なり、
//! こちらは isekai-terminal・isekai-helper の双方が STUN(RFC 5389) で自分自身の
//! NAT外から見えるアドレスを調べ、その値を（ライブなシグナリングチャネル無しで）
//! 既存の SSH ブートストラップチャネルに相乗りさせて交換し、直接の UDP 穴あけ
//! （simultaneous open）を試みる。relay は一切経路に登場しない。
//!
//! これは `isekai-link-masque` クレートが実装する MASQUE(RFC 9298 系、独自capsule)ベースの
//! relay 経由 P2P（`isekai_link_relay_transport.rs`、未実装、別のトランスポート）とは
//! 完全に独立した、もっと単純な別方式である。relay 版は relay が常時中継できるため
//! 穴あけ不成立時もフォールバックがあるが、こちらは穴あけ不成立時のフォールバックを
//! 持たない（ユーザーが別の `TransportPreference` に切り替える運用、PLAN.md Phase 10参照）。
//!
//! 具体的な手順（`try_connect_isekai_stun_p2p`）:
//! 1. isekai-terminal 自身がこれから QUIC にも使う **同一の** UDP ソケットで STUN 問い合わせを行い、
//!    自分の観測アドレスを得る。
//! 2. SSH ブートストラップ（`helper_bootstrap::ensure_helper_running`）を、`--stun-server`/
//!    `--punch-peer <自分の観測アドレス>` 付きで起動する。isekai-helper 側もこの起動時点で
//!    自分の観測アドレスを STUN で調べ、こちらの観測アドレスへ probe パケットを送り始める
//!    （`isekai-helper/src/main.rs`）。
//! 3. ハンドシェイク JSON から isekai-helper 側の観測アドレス（`stun_observed_addr`）を受け取る。
//! 4. 同じソケットから isekai-helper 側の観測アドレスへ probe を送る（simultaneous open）。
//! 5. そのままそのソケットを noq の QUIC endpoint に渡し、`isekai_pipe_quic_transport.rs` と
//!    同じ HELLO/proof/ACK クライアントロジックで接続を確立する。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use log::{info, warn};
use russh::client;

use crate::helper_bootstrap::{self, BootstrapError, IsekaiPipeBinaries, IsekaiPipeHandshake, IsekaiPipeP2pMode};
use crate::isekai_pipe_quic_transport::{
    self, spawn_bootstrap_host_key_forwarder, ISEKAI_PIPE_BIN_AARCH64, ISEKAI_PIPE_BIN_X86_64, ISEKAI_PIPE_VERSION,
};
use crate::resume_client::{self, ClientResumeState};
use crate::transport::{
    authenticate_session, connect_via_jump_or_direct, run_ssh_channel_loop,
    TransportCommand, TransportEvent,
};
use crate::{init_logger, CellData, JumpConfig, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;

/// C→S input replay buffer の既定上限（`isekai_pipe_quic_transport.rs` と揃える）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;
/// control stream を開く/CONTROL_ACK を待つタイムアウト。
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);
/// simultaneous open のための probe 送信回数・間隔。isekai-helper 側
/// （`isekai-helper/src/main.rs`）と同じ値を使う。
const PUNCH_PROBE_COUNT: u32 = 5;
const PUNCH_PROBE_INTERVAL: Duration = Duration::from_millis(150);
/// STUN 問い合わせ・穴あけを含む接続確立全体のタイムアウト。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct IsekaiStunP2pConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ブートストラップ用SSH接続の踏み台(ProxyJump)。`SshConfig::jump`参照。
    pub jump: Option<JumpConfig>,
    /// isekai-terminal・isekai-helper の双方が自分自身の観測アドレスを調べるのに使う
    /// STUN サーバー(`host:port`)のリスト。パブリックな STUN サーバー(例: Google の
    /// `stun.l.google.com:19302`)でよい—双方が同じサーバーを使う必要は無く、
    /// それぞれ自分にとって疎通できるものを指定すればよい。空であってはならない
    /// （呼び出し側が既定値にフォールバックすること）。先頭の1件が実際の
    /// STUN+SSHランデブー穴あけ機構（自分自身の観測アドレス取得・isekai-helper起動時の
    /// `--stun-server`/`--punch-peer`）に使われ、残りは`BootstrapRequestV2`の
    /// `client_candidates`（isekai-bootstrap crate共有化 Phase 2c、`#20b`と同じ仕組み）
    /// として追加の穴あけ候補をサーバー側へ渡すためだけに使われる（冗長性向上、
    /// 複数STUNサーバーの応答が異なるNATマッピングを示す場合の取りこぼし対策）。
    pub stun_servers: Vec<String>,
}

#[derive(uniffi::Object)]
pub struct IsekaiStunP2pSession {
    config: IsekaiStunP2pConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_isekai_stun_p2p_session(config: IsekaiStunP2pConfig) -> Arc<IsekaiStunP2pSession> {
    init_logger();
    Arc::new(IsekaiStunP2pSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl IsekaiStunP2pSession {
    /// STUN+SSH rendezvous による直接 P2P QUIC のみを試す。フォールバック無し
    /// （穴あけが成立しなければ接続失敗として扱う。PLAN.md Phase 10 の設計判断参照）。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        // ブートストラップ用SSHのホスト鍵検証を本セッションのcallbackに委譲する
        // (`isekai_pipe_quic_transport::bootstrap_helper_via_ssh`のNOTE参照)。
        let host_key_callback = self.core.callback();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                try_connect_isekai_stun_p2p(&config, host_key_callback),
            )
            .await
            {
                Ok(Ok(stream)) => run_over_stream(config, stream, cmd_rx, event_tx).await,
                Ok(Err(e)) => {
                    warn!("isekai_stun_p2p: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
                Err(_) => {
                    warn!("isekai_stun_p2p: connect timed out");
                    event_tx
                        .send(TransportEvent::Disconnected {
                            reason: Some("timed out establishing P2P connection".to_string()),
                        })
                        .await
                        .ok();
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
impl IsekaiStunP2pSession {
    /// Phase 12: per-session theme。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── STUN 問い合わせ・ブートストラップ ─────────────────────

/// `stun_servers`(`host:port`文字列のリスト)を順にDNS解決する。解決できないエントリは
/// 警告ログを出してスキップする(1件の設定ミス/一時的な名前解決失敗で接続全体を諦めない
/// ため — `isekai-bootstrap::client_candidates::collect_client_stun_candidates`が
/// 個々のSTUN問い合わせ失敗をスキップするのと同じ設計判断)。1件も解決できなければ
/// エラーにする(先頭のエントリが実際のSTUN+SSHランデブー機構に必須のため)。
async fn resolve_stun_servers(entries: &[String]) -> Result<Vec<SocketAddr>, String> {
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        match tokio::net::lookup_host(entry).await {
            Ok(mut addrs) => match addrs.next() {
                Some(addr) => resolved.push(addr),
                None => warn!("isekai_stun_p2p: STUNサーバー{entry:?}のDNS解決結果が空のためスキップします"),
            },
            Err(e) => warn!("isekai_stun_p2p: STUNサーバー{entry:?}のDNS解決に失敗したためスキップします: {e}"),
        }
    }
    if resolved.is_empty() {
        return Err("設定されたSTUNサーバーを1件も解決できませんでした".to_string());
    }
    Ok(resolved)
}

/// 自分自身の STUN 観測アドレスを調べつつ、その同じソケットで SSH ブートストラップ経由の
/// isekai-helper 起動・穴あけ probe 送信・QUIC 接続確立までを行う。
async fn try_connect_isekai_stun_p2p(
    config: &IsekaiStunP2pConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<resume_client::ReattachableStream, String> {
    // 先頭のエントリが実際のSTUN+SSHランデブー穴あけ機構に使う「主」STUNサーバー、
    // 残り(あれば)はブートストラップ時の追加client_candidatesとしてのみ使う
    // (`IsekaiStunP2pConfig::stun_servers`のdocコメント参照)。
    let stun_addrs = resolve_stun_servers(&config.stun_servers).await?;
    let stun_addr = stun_addrs[0];

    // STUN問い合わせ・穴あけprobe送信・QUIC接続を同一ソケットで行う必要があるため、
    // 生の std ソケットを bind し、noq に渡す前に raw な send_to/recv_from で使う
    // （isekai-stun crate のドキュメント参照）。
    let std_socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("ソケットのbindに失敗: {e}"))?;
    std_socket.set_nonblocking(true).map_err(|e| format!("set_nonblocking失敗: {e}"))?;
    let raw_socket = Arc::new(
        tokio::net::UdpSocket::from_std(std_socket)
            .map_err(|e| format!("tokioソケットへの変換に失敗: {e}"))?,
    );

    let our_observed_addr = isekai_stun::query_stun(&raw_socket, stun_addr)
        .await
        .map_err(|e| format!("自分自身のSTUN観測アドレス取得に失敗: {e}"))?;
    info!("isekai_stun_p2p: our observed address is {our_observed_addr} (via {stun_addr})");

    let handshake =
        bootstrap_via_ssh_with_punch(config, stun_addr, our_observed_addr, &stun_addrs, host_key_callback).await?;

    let peer_addr: SocketAddr = handshake
        .stun_observed_addr()
        .ok_or_else(|| {
            "isekai-helper がSTUN観測アドレスを報告しませんでした（--stun-server指定でも \
             STUN問い合わせ自体に失敗した可能性があります）"
                .to_string()
        })?
        .parse()
        .map_err(|e| format!("isekai-helper が返したSTUN観測アドレスが不正: {e}"))?;
    info!("isekai_stun_p2p: peer observed address is {peer_addr}");

    // simultaneous open: isekai-helper 側も起動直後に我々の観測アドレスへ probe を
    // 送っている（`isekai-helper/src/main.rs`）ため、双方から同時に相手へ向けて
    // パケットを送ることで、Full Cone/Restricted Cone 型 NAT 越しの穴あけを狙う。
    for _ in 0..PUNCH_PROBE_COUNT {
        let _ = raw_socket.send_to(b"isekai-punch", peer_addr).await;
        tokio::time::sleep(PUNCH_PROBE_INTERVAL).await;
    }

    // `raw_socket`はStrong参照1つのはず(ここでunwrapして所有権を取り戻す) ——
    // STUN問い合わせ・穴あけprobe送信・QUIC接続を同一ソケットで行うという
    // このtransportの制約(`isekai_transport::stun_p2p::connect_stun_p2p_on_socket`の
    // docコメント参照)を保つため、isekai-transport側の`wrap_bound_socket`に
    // 渡す前にArcから中身を取り出す。
    let raw_socket = Arc::try_unwrap(raw_socket)
        .map_err(|_| "内部エラー: raw_socketの参照が複数残っています".to_string())?;
    connect_stun_p2p_stream(raw_socket, peer_addr, &handshake).await
}

/// ProxyJump対応のSSH接続を張り、`--stun-server`/`--punch-peer`付きでisekai-helperを
/// ブートストラップ起動する。`isekai_pipe_quic_transport::bootstrap_helper_via_ssh`と
/// ほぼ同じ処理だが、STUN関連の2引数を渡す点のみ異なるため、コード共有はせず
/// そのまま複製している（呼び出し元の型(`IsekaiStunP2pConfig`/`IsekaiPipeQuicConfig`)が
/// 異なり、関数抽出すると引数が増えて可読性が落ちるため）。ホスト鍵検証ループ
/// (`spawn_bootstrap_host_key_forwarder`)自体は共有する。
async fn bootstrap_via_ssh_with_punch(
    config: &IsekaiStunP2pConfig,
    stun_server: SocketAddr,
    punch_peer: SocketAddr,
    stun_servers: &[SocketAddr],
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<IsekaiPipeHandshake, String> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
    spawn_bootstrap_host_key_forwarder(event_rx, host_key_callback);

    let russh_config = Arc::new(client::Config::default());
    let mut established = connect_via_jump_or_direct(
        &config.jump, russh_config, &config.ssh_host, config.ssh_port, event_tx,
    )
    .await
    .map_err(|e| format!("bootstrap SSH connect failed: {e}"))?;

    let (authenticated, _) =
        authenticate_session(&mut established.handle, &config.username, &config.auth).await;
    if !authenticated {
        return Err("bootstrap SSH authentication failed".to_string());
    }

    let binaries = IsekaiPipeBinaries { x86_64: ISEKAI_PIPE_BIN_X86_64, aarch64: ISEKAI_PIPE_BIN_AARCH64 };
    let p2p_mode = IsekaiPipeP2pMode::Stun { stun_server, punch_peer: Some(punch_peer) };
    helper_bootstrap::ensure_helper_running(
        &mut established.handle,
        &binaries,
        ISEKAI_PIPE_VERSION,
        "127.0.0.1:22",
        None,
        &p2p_mode,
        stun_servers,
    )
    .await
    .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────
// `isekai_pipe_quic_transport.rs` と同じワイヤー契約(HELPER_PROTOCOL.md)を再利用する。
// 唯一の違いは、接続先アドレス解決を DNS ではなく STUN 観測アドレスの交換で行うことと、
// QUIC endpoint に STUN 問い合わせ・穴あけ probe 送信で使ったのと同一のソケットを
// 渡すこと。

async fn connect_stun_p2p_stream(
    socket: tokio::net::UdpSocket,
    peer_addr: SocketAddr,
    handshake: &IsekaiPipeHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let cert_sha256_hex = handshake.cert_sha256().to_string();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let target = isekai_transport::StunP2pTarget {
        peer_addr,
        server_name: isekai_pipe_quic_transport::QUIC_SERVER_NAME.to_string(),
        cert_sha256_hex,
        session_secret,
    };
    let factory = crate::android_quic_endpoint::factory();
    let identity =
        isekai_transport::CandidateIdentity { kind: "stun-p2p", source: "n/a", provider: "n/a", id: "isekai_stun_p2p" };
    let (conn, data_stream, proof) =
        isekai_transport::connect_stun_p2p_on_socket(&factory, socket, &target, identity)
            .await
            .map_err(|e| e.to_string())?;
    info!("isekai_stun_p2p: ATTACH ok — handing off to SSH");

    let resume_state = Arc::new(std::sync::Mutex::new(ClientResumeState::new(
        DEFAULT_RESUME_BUFFER_SIZE,
    )));

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
                        "isekai_stun_p2p: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    let counters = Arc::new(isekai_transport::resume::AppAckCounters::new());
                    isekai_transport::resume::spawn_app_ack_tasks(control.stream, counters.clone());
                    isekai_pipe_quic_transport::spawn_app_ack_bridge(resume_state, counters);
                }
                Ok(Err(e)) => {
                    info!("isekai_stun_p2p: control stream handshake failed ({e}), continuing without resume support");
                }
                Err(_) => {
                    info!("isekai_stun_p2p: control stream not accepted within timeout, continuing without resume support");
                }
            }
        });
    }

    // Phase 10 の既知の制約: reattach(RESUME) は穴あけ済みの元ソケットを再利用せず、
    // 新規エフェメラルソケットから `peer_addr` へ直接繋ぎ直すだけで、STUN再問い合わせ・
    // 再穴あけは行わない。NATマッピングが生きている間の一時的なQUIC切断(パケットロス等)
    // からの復旧はこれで十分カバーできるが、NATマッピング自体が失われるような長時間の
    // 切断・ネットワーク切り替えからは復旧できない（その場合はユーザーが再接続する）。
    // relay版は relay が常時経路に残るためこの制約が無い、というのが2方式の設計上の
    // トレードオフ(PLAN.md Phase 10参照)。isekai-transportのSTUN P2Pにはresume概念自体が
    // 無いため(stun_p2p.rsのモジュールdoc参照)、reattachは`RelayTarget{helper_addr:
    // peer_addr, ..}`とみなしてreconnect_and_resume(直接dial+RESUME)を呼ぶだけにする。
    let reattach_fn: resume_client::ReattachFn<quicmux::AnyByteStreamReadHalf, quicmux::AnyByteStreamWriteHalf> = Arc::new(move |session_id, client_sent_offset, client_delivered_offset| {
        let target = isekai_transport::RelayTarget {
            helper_addr: peer_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
            session_secret: target.session_secret.clone(),
        };
        let factory = crate::android_quic_endpoint::factory();
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
            info!("isekai_stun_p2p: resume succeeded, helper_committed_offset={}", outcome.helper_committed_offset);
            let (read, write) = outcome.data_stream.split();
            Ok(resume_client::ReattachResult { read, write, helper_committed_offset: outcome.helper_committed_offset.get() })
        })
    });

    let (data_read, data_write) = data_stream.split();
    Ok(resume_client::ReattachableStream::new(data_read, data_write, resume_state, reattach_fn))
}

async fn run_over_stream(
    mut config: IsekaiStunP2pConfig,
    stream: resume_client::ReattachableStream,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });

    // STUN P2P(実験的opt-in機能)はSSH接続プーリング(`archive/ISEKAI_SSH_DESIGN.md`
    // 「今後の課題」参照)のスコープ外。タブごとに毎回新規のQUIC接続・ネストしたSSH認証を
    // 行う、これまでと同じ挙動のまま。
    let pooled = match crate::transport::establish_ssh_handle_over_stream(
        russh_config, stream, &config.username, &mut config.auth, false, &event_tx,
    ).await {
        Ok(p) => p,
        Err(msg) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
            return;
        }
    };

    // IsekaiStunP2pConfig は agent forwarding 未対応（`IsekaiPipeQuicConfig` と同様）。
    run_ssh_channel_loop(&pooled, config.cols, config.rows, false, false, cmd_rx, event_tx).await;
}

#[cfg(test)]
mod tests {
    //! ループバック上の実 sshd（127.0.0.1:22）+ ローカルモックSTUNサーバーに対する
    //! E2Eテスト。`ISEKAI_PIPE_BOOTSTRAP_TEST_KEY`（鍵ファイルパス）が設定されていない環境
    //! では自動的にスキップする（`isekai_pipe_quic_transport.rs`のテストと同じ opt-in 方式）。
    //!
    //! ループバック経由なのでNATは介在せず「本当の」穴あけにはならないが、
    //! STUN問い合わせ→SSHブートストラップ経由でのアドレス交換→probe送信→
    //! そのソケットでのQUIC接続確立、という一連のコードパスは実際に実行される。
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    /// 最小のモックSTUNサーバー(RFC 5389 Binding Request/Response)。受け取った
    /// Binding Requestの送信元アドレスをそのままXOR-MAPPED-ADDRESSとして返すだけ。
    /// `isekai-helper/tests/e2e.rs`の同名ヘルパーと同じ実装（バイト単位で揃えてある）。
    fn spawn_mock_stun_server() -> SocketAddr {
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = server.recv_from(&mut buf) else { break };
                if n < 20 {
                    continue;
                }
                let transaction_id = &buf[8..20];
                let SocketAddr::V4(from_v4) = from else { continue };

                let magic_cookie: u32 = 0x2112_A442;
                let xport = from_v4.port() ^ ((magic_cookie >> 16) as u16);
                let xaddr = u32::from(*from_v4.ip()) ^ magic_cookie;

                let mut resp = Vec::with_capacity(32);
                resp.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success Response
                resp.extend_from_slice(&12u16.to_be_bytes()); // 4(attr header) + 8(attr value)
                resp.extend_from_slice(&magic_cookie.to_be_bytes());
                resp.extend_from_slice(transaction_id);
                resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
                resp.extend_from_slice(&8u16.to_be_bytes());
                resp.push(0);
                resp.push(0x01); // family: IPv4
                resp.extend_from_slice(&xport.to_be_bytes());
                resp.extend_from_slice(&xaddr.to_be_bytes());

                let _ = server.send_to(&resp, from);
            }
        });
        addr
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
        fn on_clipboard_write(&self, _text: String) {}
        fn on_clipboard_pull_request(&self) -> Option<String> { None }
    }

    #[tokio::test]
    async fn full_stack_stun_bootstrap_quic_and_shell_command() {
        let Ok(key_path) = std::env::var("ISEKAI_PIPE_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: ISEKAI_PIPE_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        let stun_server = spawn_mock_stun_server();

        let config = IsekaiStunP2pConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
            jump: None,
            stun_servers: vec![stun_server.to_string()],
        };

        let session = create_isekai_stun_p2p_session(config);
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let callback = TestCallback { buf: buf.clone(), notify: notify.clone() };
        session.connect(Box::new(callback)).expect("connect() call failed");

        tokio::time::sleep(Duration::from_millis(800)).await;
        session.send(b"echo stun-p2p-ok\n".to_vec());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            {
                let b = buf.lock().unwrap();
                if String::from_utf8_lossy(&b).contains("stun-p2p-ok") {
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
