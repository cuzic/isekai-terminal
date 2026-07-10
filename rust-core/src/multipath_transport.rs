//! Phase 9-2: 受動的マルチパスフェイルオーバー（Tailscale⇔直接アドレス、第一段）。
//! 設計の背景・スコープは `/home/cuzic/.claude/plans/typed-dancing-codd.md` および
//! `PLAN.md` の「Phase 9」節を参照。
//!
//! `isekai_pipe_quic_transport.rs`（Phase 7/8、単一パス + 完全喪失後の明示的な`RESUME`
//! 再接続。後にquinnからnoqへ移行）とは別の新規トランスポート。こちらは`noq`のQUIC
//! multipathを使い、同一QUICコネクションの中にpath0（`ssh_host`、通常は
//! Tailscale経由アドレス）とpath1（`direct_host`、直接到達可能なアドレス）を
//! 同時に張っておく。少なくとも一方のpathが生きている限りコネクション自体が
//! 死なないため、このトランスポートには独自のresume/reattach層は無い
//! （SSHセッションが載っているのはコネクション1本であり、pathの生死は
//! アプリ層から見て透過的——`noq`が内部でどのpathを使うか選ぶ）。
//!
//! `isekai_pipe_quic_transport.rs`のPhase 7/8コードは一切変更していない
//! （既存の3 e2eテスト+isekai-terminal-core 66テストで無回帰を確認済み、Phase 9-1）。
//! HELPER_PROTOCOL.mdのHELLO/ACK/proof契約・埋め込みヘルパーバイナリ・
//! ブートストラップロジックはそちらの`pub(crate)`公開分をそのまま再利用する。

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::os::fd::{FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use hmac::{Hmac, Mac};
use log::{info, warn};
use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};

use crate::isekai_pipe_quic_transport::{self, ALPN, EXPORTER_LABEL};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, encode_attach_activate, encode_attach_hello, AttachActivate, AttachHello,
    AttachProof, AttachResponse, ConnectionGeneration,
};
use crate::transport::{run_ssh_channel_loop, TransportCommand, TransportEvent};
use crate::{init_logger, CellData, JumpConfig, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;
use base64::Engine as _;
use isekai_transport::multipath::{connect_multipath_with_socket, MultipathConnection};
use isekai_transport::path_health::{self, PathHealthEvent, PathHealthTracker, PathLabel, PathState};
use isekai_transport::types::RemoteSpec;
use russh::client;

type HmacSha256 = Hmac<Sha256>;
use sha2::Sha256;

/// path1 (`direct_host`) を開けるまでのリトライ回数と初回バックオフ。
/// 単発 8 秒タイムアウトより緩くする（Phase 7-7 スパイクの知見: ロスの多い
/// セルラー回線では `open_path` が最初の 1 回で失敗/タイムアウトすることがある）。
const OPEN_PATH_MAX_ATTEMPTS: u32 = 3;
const OPEN_PATH_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const OPEN_PATH_TIMEOUT: Duration = Duration::from_secs(8);

/// direct_host（Tailscaleを介さない外部到達アドレス）向けにisekai-helperを
/// 待ち受けさせる固定UDPポート。物理Wi-Fi/セルラーpath候補も含め、direct_host宛の
/// 全pathはこの同じポートに接続する（QUICのmultipathは宛先ポートを揃えたまま
/// 送信元4-tupleだけ変える設計のため）。エフェメラルポートだとサーバー側ファイア
/// ウォールで事前に許可できず外形疎通できない（実機検証で確認、Phase 9-4）ので
/// 固定値にしている。ユーザーはこのポートをサーバー側で開けておく必要がある
/// （将来的にはプロファイル単位で設定可能にする余地を残す。現状は固定値）。
const DIRECT_MULTIPATH_BIND_PORT: u16 = 45823;

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct MultipathIsekaiPipeQuicConfig {
    /// ブートストラップに使う SSH ホスト。通常は Tailscale 経由アドレス（path0）。
    pub ssh_host: String,
    pub ssh_port: u16,
    /// 同じ isekai-helper への直接到達アドレス（path1、および Phase 9-4 の物理
    /// path2/path3 の宛先）。指定が無ければ multipath 化されず path0 のみで動く
    /// （通常の Phase 7 相当の耐性のみ）。
    pub direct_host: Option<String>,
    /// Phase 9-4追加検証: セルラー物理path候補だけ`direct_host`とは別のリモートアドレス
    /// （例: IPv6）へ向ける。実機検証で、同一remoteアドレスに異なるlocal IPで複数
    /// `open_path()`するとnoq側でPATH_CHALLENGE/RESPONSEの突き合わせがことごとく
    /// `ValidationFailed`になる現象を確認した——remoteもlocalも異なる、完全にユニークな
    /// FourTupleにすることでこれを回避できるかを検証するためのフィールド。未指定なら
    /// 従来通り`direct_host`と同じアドレスを使う（後方互換）。isekai-helperは`--bind`で
    /// IPv6ワイルドカード（`[::]:port`）待受にすることで同一ソケットがIPv4/IPv6両方を
    /// 受け付けるため、サーバー側の追加ポート開放は不要。
    pub cellular_remote_host: Option<String>,
    /// Phase 9-4（実験的機能、既定 OFF）: `Network.bindSocket()` で Wi-Fi に明示的に
    /// バインドした UDP ソケットの生 fd（Kotlin 側で `ParcelFileDescriptor.detachFd()`
    /// 済み、所有権はこちらに移る）。`wifi_local_ip` はそのソケットのローカル IP。
    /// どちらか一方だけが `None` なら物理 Wi-Fi path は開かない。Tailscale 稼働中は
    /// `bindSocket()` 自体が失敗する（VPN ロック）ため、その場合 Kotlin 側から
    /// この値は渡ってこない（自然に候補から外れる、日和見的ポリシー）。
    pub wifi_fd: Option<i32>,
    pub wifi_local_ip: Option<String>,
    /// Phase 9-4: セルラー版（`wifi_fd`/`wifi_local_ip` と同じ扱い）。
    pub cellular_fd: Option<i32>,
    pub cellular_local_ip: Option<String>,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ブートストラップ用SSH接続の踏み台(ProxyJump)。`SshConfig::jump`参照。
    pub jump: Option<JumpConfig>,
    /// isekai-helperのQUIC待受ポートをユーザー指定で固定する(`None`なら、
    /// `direct_host`が設定されている場合のみ既定値`DIRECT_MULTIPATH_BIND_PORT`を使う、
    /// 未設定ならエフェメラル)。値の解決はKotlin側(`ConnectionProfile.helperBindPort`)で
    /// 行い、ここには既に解決済みの値だけを渡すのが本来の想定だが、後方互換のため
    /// `None`の場合はRust側で従来通りの既定値フォールバックを維持する
    /// (`IsekaiPipeQuicConfig.bind_port`のdocコメントも参照)。
    pub bind_port: Option<u16>,
}

/// noq issue #738（`open_path()`に`local_ip`明示指定した新規pathでPATH_RESPONSEが
/// 処理されない不具合）を踏まずに「WiFiは繋がっているがupstreamが死んでいる」状況
/// から脱出するための代替手段。`open_path()`で追加pathを開くのではなく、
/// `Endpoint::rebind_abstract()`でendpoint全体の送受信ソケットを丸ごと差し替える
/// （＝既存の全pathがこの新しいソケット経由になる、NATリバインド相当のAPI）。
struct RebindRequest {
    fd: RawFd,
    local_ip: IpAddr,
    /// 本番では常に`debug_fault::shared_injector()`（既存のグローバルフォルト注入と
    /// 同じインスタンス）。テストでは独立した`UdpFaultInjector::new()`を渡すことで、
    /// 「現在のpathだけ遮断されていて、rebind先は生きている」という部分障害を
    /// プロセスグローバルな状態に頼らず再現できる（Phase 9-4b追加検証）。
    injector: crate::faulty_udp_socket::UdpFaultInjector,
}

#[derive(uniffi::Object)]
pub struct MultipathIsekaiPipeQuicSession {
    config: MultipathIsekaiPipeQuicConfig,
    core: SessionCore,
    rebind_tx: StdMutex<Option<tokio::sync::mpsc::Sender<RebindRequest>>>,
}

#[uniffi::export]
pub fn create_multipath_isekai_pipe_quic_session(config: MultipathIsekaiPipeQuicConfig) -> Arc<MultipathIsekaiPipeQuicSession> {
    init_logger();
    Arc::new(MultipathIsekaiPipeQuicSession { config, core: SessionCore::new(), rebind_tx: StdMutex::new(None) })
}

#[uniffi::export]
impl MultipathIsekaiPipeQuicSession {
    /// フォールバック無し。path0/path1 のブートストラップ・QUIC 接続に失敗したら
    /// エラーを返す（`TransportPreference::IsekaiPipeQuicMultipath` 相当）。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        let (rebind_tx, rebind_rx) = tokio::sync::mpsc::channel(4);
        *self.rebind_tx.lock().unwrap() = Some(rebind_tx);
        // ブートストラップ用SSHのホスト鍵検証を本セッションのcallbackに委譲する
        // (`isekai_pipe_quic_transport::bootstrap_helper_via_ssh`のNOTE参照)。
        let host_key_callback = self.core.callback();
        RUNTIME.spawn(async move {
            match try_connect_multipath(&config, rebind_rx, event_tx.clone(), host_key_callback).await {
                Ok(stream) => run_over_stream(config, stream, cmd_rx, event_tx).await,
                Err(e) => {
                    warn!("multipath_quic: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
            }
        });
        Ok(())
    }

    /// 「WiFiは繋がっているがupstreamが死んでいる」等を検知したKotlin側から呼ぶ。
    /// `fd`は`Network.bindSocket()`済み・`ParcelFileDescriptor.detachFd()`済みの生fd
    /// （所有権はこちらに移る）。接続確立前や既にrebind中の場合は素通りする
    /// （エラーにはしない——呼び出し側は日和見的に呼べばよい）。
    pub fn rebind_to_fd(&self, fd: i32, local_ip: String) {
        let Ok(local_ip) = local_ip.parse::<IpAddr>() else {
            warn!("multipath_quic: rebind_to_fd: invalid local_ip {local_ip:?}");
            return;
        };
        let tx = self.rebind_tx.lock().unwrap().clone();
        let Some(tx) = tx else {
            warn!("multipath_quic: rebind_to_fd called before connect() established a session");
            return;
        };
        let req = RebindRequest { fd: fd as RawFd, local_ip, injector: crate::debug_fault::shared_injector() };
        if tx.try_send(req).is_err() {
            warn!("multipath_quic: rebind_to_fd: request channel full or closed, dropping fd={fd}");
        }
    }

    pub fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub fn disconnect(&self) { self.core.disconnect(); }

    pub fn trzsz_accept_upload(&self, transfer_id: String, file_name: String, file_size: u64, mode: u32) {
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
    /// 何もしない(物理Wi-Fi/セルラー切替はpath0/path1のmultipath自体が別途担う、
    /// `rebind_to_fd`参照)。
    pub fn notify_network_lost(&self) {
        self.core.notify_network_lost(true);
    }
}

// SessionOrchestrator からのみ呼ばれる内部API(uniffi には直接は出さない)。
impl MultipathIsekaiPipeQuicSession {
    /// Phase 12: per-session theme。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── path health（`isekai_transport::path_health`、旧`PathBroker`）─────────
//
// `PathCandidateId`（固定4種）/`PathState`/`PathBroker`は、isekai-transport/
// isekai-terminal-core crate共有化の一環でisekai_transport::path_healthへ
// 一般化・移植した（`PathHealthTracker`/`PathLabel`/`PathState`）。以下の
// ラベルは今までの`PathCandidateId`の4種にそれぞれ対応する。

const PRIMARY_LABEL: &str = isekai_transport::multipath::PRIMARY_PATH_LABEL;
const SECONDARY_LABEL: &str = "secondary";
const PHYSICAL_WIFI_LABEL: &str = "physical-wifi";
const PHYSICAL_CELLULAR_LABEL: &str = "physical-cellular";

// ── Phase 9-4: 物理Wi-Fi/セルラー無線を束ねる `MultiUdpSocket` ─────────
//
// `noq-multipath-spike/src/dual_fd_socket.rs`の`DualUdpSocket`（bindされた
// ソケット2本ちょうどを束ねる、実機スパイク専用）を一般化したもの。
// path0（ssh_host）/path1（direct_host）はOSのデフォルトルーティング任せで
// 十分だった（Phase 7-7で実証済み、bindSocket不要）ため`default`ソケットを
// そのまま使い、物理Wi-Fi/セルラーだけが`Network.bindSocket()`で明示的に
// バインドされた別ソケット（`named`）を必要とする。送信は`transmit.src_ip`が
// `named`のいずれかのlocal_ipと一致すればそのソケット、それ以外は`default`。

pub(crate) struct NamedUdpSocket {
    pub(crate) local_ip: IpAddr,
    pub(crate) socket: Arc<tokio::net::UdpSocket>,
}

/// Phase 9-5実機検証用: `isekai_pipe_quic_transport.rs`/`faulty_udp_socket.rs`が既に
/// 使っている`debug_fault::shared_injector()`（`UdpFaultInjector`）をそのまま
/// 再利用する。新しいフォルト注入state・adb broadcast・UniFFI関数は一切増やさない
/// ——既存の`isekai-fault-latency300`/`isekai-fault-loss200`等のclipwireターゲット
/// （`FaultInjectionReceiver`→`debug_set_udp_fault_*`）がこのトランスポートにも
/// そのまま効くようになるだけ。既定値（遅延0・ロス0・cut無し）では素通しなので
/// 通常利用時の挙動には影響しない。
pub(crate) struct MultiUdpSocket {
    pub(crate) default: Arc<tokio::net::UdpSocket>,
    pub(crate) named: Vec<NamedUdpSocket>,
    pub(crate) injector: crate::faulty_udp_socket::UdpFaultInjector,
}

impl fmt::Debug for MultiUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiUdpSocket")
            .field("named_ips", &self.named.iter().map(|n| n.local_ip).collect::<Vec<_>>())
            .finish()
    }
}

impl AsyncUdpSocket for MultiUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(MultiUdpSender {
            default: self.default.clone(),
            named: self.named.iter().map(|n| (n.local_ip, n.socket.clone())).collect(),
            injector: self.injector.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut TaskContext<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
            let result: Option<io::Result<(SocketAddr, Option<IpAddr>, usize)>> =
                match self.default.poll_recv_from(cx, &mut read_buf) {
                    Poll::Ready(res) => Some(res.map(|addr| (addr, None, read_buf.filled().len()))),
                    Poll::Pending => None,
                };
            let result = if result.is_some() {
                result
            } else {
                let mut hit = None;
                for named in &self.named {
                    let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
                    if let Poll::Ready(res) = named.socket.poll_recv_from(cx, &mut read_buf) {
                        hit = Some(res.map(|addr| (addr, Some(named.local_ip), read_buf.filled().len())));
                        break;
                    }
                }
                hit
            };
            let Some(result) = result else { return Poll::Pending };
            let (addr, dst_ip, len) = match result {
                Ok(v) => v,
                Err(e) => return Poll::Ready(Err(e)),
            };
            if self.injector.is_cut() || self.injector.should_drop() {
                // 電波圏外/ロス相当: この datagram は破棄して再度 poll する
                // （faulty_udp_socket.rs と同じ方針、Phase 9-5実機検証用）。
                continue;
            }
            let mut m = RecvMeta::default();
            m.addr = addr;
            m.len = len;
            m.stride = len;
            m.dst_ip = dst_ip;
            meta[0] = m;
            return Poll::Ready(Ok(1));
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.default.local_addr()
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn may_fragment(&self) -> bool {
        true
    }
}

struct MultiUdpSender {
    default: Arc<tokio::net::UdpSocket>,
    named: Vec<(IpAddr, Arc<tokio::net::UdpSocket>)>,
    injector: crate::faulty_udp_socket::UdpFaultInjector,
}

impl fmt::Debug for MultiUdpSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MultiUdpSender")
    }
}

impl MultiUdpSender {
    fn pick(&self, src_ip: Option<IpAddr>) -> &Arc<tokio::net::UdpSocket> {
        if let Some(ip) = src_ip {
            if let Some((_, sock)) = self.named.iter().find(|(named_ip, _)| *named_ip == ip) {
                return sock;
            }
        }
        &self.default
    }
}

impl UdpSender for MultiUdpSender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if self.injector.is_cut() || self.injector.should_drop() {
            // 実ネットワークでも送信側はロスを検知できないのと同様、成功したふりをする。
            return Poll::Ready(Ok(()));
        }
        let delay = self.injector.latency();
        let sock = self.pick(transmit.src_ip).clone();
        if !delay.is_zero() {
            // faulty_udp_socket.rs の spawn_send_pump と同じ方針: 遅延分だけ
            // バックグラウンドで待ってから実際に送る（呼び出し元はブロックしない）。
            let contents = transmit.contents.to_vec();
            let destination = transmit.destination;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = sock.send_to(&contents, destination).await;
            });
            return Poll::Ready(Ok(()));
        }
        match sock.poll_send_to(cx, transmit.contents, transmit.destination) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

/// Kotlin側で`Network.bindSocket()`→`ParcelFileDescriptor.detachFd()`した生fdから
/// `tokio::net::UdpSocket`を作る。
///
/// # Safety
/// 呼び出し元がこのfdの所有権を完全に引き渡していること（`detachFd()`でJava側の
/// fdsan所有権タグを外し済みであること）が前提。そうでない場合、drop時にcloseした
/// 際にfdsanがプロセスをabortする（実機スパイクで確認済みの罠、
/// `NoqDualFdMultipathSpikeTest.kt`のコメント参照）。
fn udp_socket_from_raw_fd(fd: RawFd) -> Result<Arc<tokio::net::UdpSocket>, String> {
    let std_sock = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
    std_sock.set_nonblocking(true).map_err(|e| format!("set_nonblocking failed: {e}"))?;
    let tokio_sock =
        tokio::net::UdpSocket::from_std(std_sock).map_err(|e| format!("UdpSocket::from_std failed: {e}"))?;
    Ok(Arc::new(tokio_sock))
}

// ── QUIC 接続（noq、HELPER_PROTOCOL.md契約はisekai_pipe_quic_transport.rsと共通） ──
//
// クライアント設定の構築(証明書pin・TLS/transportチューニング)は
// `isekai_transport::multipath::connect_multipath_with_socket`が内部で
// `isekai_transport::system::client_config_for(cert_sha256_hex, true)`を使って
// 行うようになったため、ここにあった`build_pinned_client_config`は削除した
// (isekai-terminal-core/isekai-transport crate共有化)。
// 挙動差(旧: max_concurrent_bidi_streams=2・max_idle_timeout無し / 新:
// max_concurrent_bidi_streams=1・max_idle_timeout=15s+keep_alive=5s)は、
// このトランスポートが同時に2本目のstreamを開くことは無く(常に`hello_ack`が
// 開く1本のみ)、かつ全pathが死んだ場合でもAndroid独自のNoViablePath検知
// (health check間隔3秒×3回=9秒)がnoqのidle timeout(15秒)より先に発火するため、
// 無害と判断した。

/// `session_secret` と QUIC connection の exporter から proof を計算する
/// （ATTACH の `extra` には proof transcript を渡す。`isekai_pipe_quic_transport::
/// compute_proof` と同じロジックだが、このトランスポートは独立実装として自前で持つ）。
fn compute_proof(conn: &noq::Connection, session_secret: &[u8], extra: &[u8]) -> Result<[u8; 32], String> {
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

/// ATTACH v2 ハンドシェイク（`ATTACH_HELLO`/`AttachReadyV2`/`ATTACH_ACTIVATE`）を行い、
/// 以降 SSH のパススルーに使えるデータ stream を返す。session_id/attempt_id 採番・reject
/// メッセージ整形・レスポンス読み取りは `isekai_pipe_quic_transport` の共有ヘルパーを
/// 使う（Android には generation を進める fencing 層が無いので常に
/// `ConnectionGeneration::INITIAL`）。
async fn hello_ack(
    conn: &noq::Connection,
    session_secret: &[u8],
) -> Result<(noq::SendStream, noq::RecvStream), String> {
    let session_id = isekai_pipe_quic_transport::random_session_id();
    let generation = ConnectionGeneration::INITIAL;
    let attempt_id = isekai_pipe_quic_transport::random_attempt_id();
    // No client-configurable resume-grace concept on Android yet — `0` means
    // "no preference, use the server's own default/max".
    let requested_resume_grace_secs = 0;
    let transcript = attach_hello_proof_transcript(&session_id, generation, &attempt_id, requested_resume_grace_secs);
    let proof = AttachProof::new(compute_proof(conn, session_secret, &transcript)?);

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi failed: {e}"))?;
    let hello = AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs, proof };
    send.write_all(&encode_attach_hello(&hello))
        .await
        .map_err(|e| format!("ATTACH_HELLO write failed: {e}"))?;

    match isekai_pipe_quic_transport::read_attach_response(&mut recv).await? {
        AttachResponse::Ready { attach_token, .. } => {
            let activate = AttachActivate { session_id, generation, attempt_id, attach_token };
            send.write_all(&encode_attach_activate(&activate))
                .await
                .map_err(|e| format!("ATTACH_ACTIVATE write failed: {e}"))?;
            Ok((send, recv))
        }
        AttachResponse::Reject(reason) => Err(isekai_pipe_quic_transport::attach_reject_message(reason)),
    }
}

/// Phase 9-4: 物理無線に明示的にバインドされたpath候補1本分（`RawFd`は
/// `MultiUdpSocket`構築時に消費され所有権が移る）。
pub(crate) struct PhysicalPathCandidate {
    pub(crate) candidate: PathLabel,
    pub(crate) fd: RawFd,
    pub(crate) local_ip: IpAddr,
    /// この候補が接続を試みるリモートアドレス。通常は`direct_host`（path1と同じ）だが、
    /// `cellular_remote_host`が設定されていればセルラー候補だけ別アドレス（IPv6等）を使う。
    pub(crate) target_addr: SocketAddr,
}

/// path0 に接続し、path1（`direct_host`が指定されていれば）と、Phase 9-4の
/// 物理path候補（`physical`、`Network.bindSocket()`済みのfdから構築）を追加で
/// 開く。path1・物理pathいずれも確立に失敗して致命的エラーにはしない
/// （path0 だけで従来通り動く）。
///
/// path0（primary）自体の接続確立・health monitor起動・path_eventsリスナーは
/// `isekai_transport::multipath::connect_multipath_with_socket`へ委譲する
/// (isekai-terminal-core/isekai-transport crate共有化)。path1・物理pathの
/// 追加open_pathは意図的にこの関数から`connect_multipath_with_socket`の
/// `secondaries`引数を経由せず、常に空を渡した上でこの関数自身が直列に開く——
/// 複数の物理path候補が絡む場合の「同時に複数open_pathすると先頭以外が失敗する」
/// 実機検証結果(下記コメント参照)による厳密な直列化保証を、2つの独立したタスクに
/// 分割することなく維持するため。`isekai_transport::path_health`
/// (`PathHealthTracker`/`PathState`/`spawn_health_monitor`/`notify_if_no_viable_path`)
/// は共通で使う。
async fn establish_multipath_connection(
    path0_addr: SocketAddr,
    path1_addr: Option<SocketAddr>,
    physical: Vec<PhysicalPathCandidate>,
    cert_sha256_hex: &str,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
    injector: crate::faulty_udp_socket::UdpFaultInjector,
) -> Result<(noq::Connection, PathHealthTracker, noq::Endpoint), String> {
    // isekai_transport::path_health::spawn_health_monitorは自分専用のイベント型
    // (PathHealthEvent)を使うので、Androidの既存TransportEventチャンネルへ
    // NoViablePathだけ橋渡しする小タスクを立てる。
    let (health_tx, mut health_rx) = tokio::sync::mpsc::channel::<PathHealthEvent>(8);
    {
        let event_tx = event_tx.clone();
        RUNTIME.spawn(async move {
            while let Some(PathHealthEvent::NoViablePath) = health_rx.recv().await {
                let _ = event_tx.send(TransportEvent::NoViablePath).await;
            }
        });
    }

    // 物理path候補を開くのに使う (candidate, local_ip, target_addr) の対応は、fdの
    // 所有権をMultiUdpSocketへ渡す前に控えておく。
    let physical_targets: Vec<(PathLabel, IpAddr, SocketAddr)> =
        physical.iter().map(|p| (p.candidate.clone(), p.local_ip, p.target_addr)).collect();

    // path0/path1のみ（物理候補なし）でもnoq::Endpoint::client(...)の素のソケットは
    // 使わず、常にMultiUdpSocketを通す（`named`が空なら`default`だけの薄いラッパー
    // になるだけで、Phase 9-2/9-3の挙動は変えない）。こうすることで`injector`
    // （本番では`debug_fault::shared_injector()`、テストでは独立したインスタンスも
    // 注入可能）がこのトランスポートにも一律に効くようになる（Phase 9-5実機検証用、
    // Phase 9-4b追加検証でテスト用に注入可能化）。
    let default_sock = Arc::new(
        tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("default socket bind failed: {e}"))?,
    );
    let mut named = Vec::with_capacity(physical.len());
    for p in physical {
        let socket = udp_socket_from_raw_fd(p.fd)?;
        named.push(NamedUdpSocket { local_ip: p.local_ip, socket });
    }
    let multi = MultiUdpSocket { default: default_sock, named, injector };

    let primary = RemoteSpec {
        addr: path0_addr,
        server_name: "isekai-pipe.local".to_string(),
        cert_sha256_hex: cert_sha256_hex.to_string(),
    };
    info!("multipath_quic: connecting path0 -> {path0_addr}");
    let MultipathConnection { conn, tracker, endpoint } =
        connect_multipath_with_socket(Box::new(multi), primary, Vec::new(), health_tx.clone())
            .await
            .map_err(|e| e.to_string())?;
    info!("multipath_quic: path0 established");

    if let Some(path1_addr) = path1_addr {
        // Phase 9-4追加調査: 当初はSecondary/物理path候補を全て同時にspawnしていたが、
        // 実機検証で「同時に複数open_pathすると、Secondary以外は毎回ValidationFailedに
        // なる」現象を確認した（remoteアドレスを完全に分けても再現したため、宛先の重複が
        // 原因ではない）。CID払い出しやanti-amplification制限が複数同時オープンで
        // 競合している可能性が高いとみて、1本ずつ確立を待ってから次を開く直列化に変更。
        let conn2 = conn.clone();
        let tracker2 = tracker.clone();
        let health_tx2 = health_tx.clone();
        RUNTIME.spawn(async move {
            open_path_with_retry(&conn2, path1_addr, None, SECONDARY_LABEL.into(), &tracker2, &health_tx2).await;

            // Phase 9-4: 物理path候補は明示的にbindされたローカルIPから、それぞれの
            // target_addr（既定はdirect_host=path1_addrと同じ、cellular_remote_host
            // 指定時はセルラーのみ別アドレス）へ開く。Tailscale経由アドレス（path0）宛には
            // 送れない（bindSocket自体がVPN稼働中は失敗するため、そもそもここに来ない）。
            for (candidate, local_ip, target_addr) in physical_targets {
                open_path_with_retry(&conn2, target_addr, Some(local_ip), candidate, &tracker2, &health_tx2).await;
            }
        });
    } else if !physical_targets.is_empty() {
        warn!("multipath_quic: physical path candidates given but direct_host is unset; skipping (no target address)");
    }

    Ok((conn, tracker, endpoint))
}

/// path1（`local_ip=None`、OSデフォルトルーティング）・物理path候補
/// （`local_ip=Some(..)`、`MultiUdpSocket`が送信元IPで振り分ける）共通の
/// リトライ付きopen_path処理。`isekai_transport::multipath`内の同名の
/// 内部関数は`local_ip=None`専用のためAndroidでは再利用できず(物理path候補が
/// `local_ip=Some(..)`を必要とする)、`isekai_transport::path_health`の型を
/// 使いつつこの関数自体はAndroid側に残す。
async fn open_path_with_retry(
    conn: &noq::Connection,
    target_addr: SocketAddr,
    local_ip: Option<IpAddr>,
    candidate: PathLabel,
    tracker: &PathHealthTracker,
    event_tx: &tokio::sync::mpsc::Sender<PathHealthEvent>,
) {
    let four_tuple = match local_ip {
        Some(ip) => noq::FourTuple::new(target_addr, Some(ip)),
        None => noq::FourTuple::from_remote(target_addr),
    };
    let mut backoff = OPEN_PATH_INITIAL_BACKOFF;
    for attempt in 1..=OPEN_PATH_MAX_ATTEMPTS {
        info!("multipath_quic: opening path {candidate:?} -> {target_addr} (local_ip={local_ip:?}, attempt {attempt}/{OPEN_PATH_MAX_ATTEMPTS})");
        let result =
            tokio::time::timeout(OPEN_PATH_TIMEOUT, conn.open_path(four_tuple, noq::PathStatus::Available)).await;
        match result {
            Ok(Ok(path)) => {
                info!("multipath_quic: path {candidate:?} established: id={:?}", path.id());
                tracker.register_path(path.id(), candidate.clone());
                tracker.set(candidate.clone(), PathState::Validated);
                path_health::spawn_health_monitor(conn.clone(), path.id(), candidate, tracker.clone(), event_tx.clone());
                return;
            }
            Ok(Err(e)) => warn!("multipath_quic: path {candidate:?} open_path failed (attempt {attempt}): {e}"),
            Err(_) => {
                warn!("multipath_quic: path {candidate:?} open_path timed out after {OPEN_PATH_TIMEOUT:?} (attempt {attempt})")
            }
        }
        tracker.set(candidate.clone(), PathState::Failed);
        if attempt < OPEN_PATH_MAX_ATTEMPTS {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }
    warn!("multipath_quic: giving up on path {candidate:?} after {OPEN_PATH_MAX_ATTEMPTS} attempts");
    path_health::notify_if_no_viable_path(tracker, event_tx);
}

/// `MultipathIsekaiPipeQuicConfig`のwifi_fd/wifi_local_ip・cellular_fd/cellular_local_ip
/// から`PhysicalPathCandidate`を組み立てる。fdとlocal_ipが両方揃っている場合のみ
/// 候補にする（片方だけ来ることは想定しないが、防御的に無視する）。ローカルIPの
/// パースに失敗した場合もその候補だけ無視する（他の候補・path0/path1には影響しない）。
/// `default_target`（＝path1_addr、direct_host）が各候補の既定リモートアドレス。
/// `config.cellular_remote_host`が設定されていればセルラー候補だけそちらを使う
/// （同一remoteに複数local IPでopen_pathするとnoq側でvalidationが失敗する実機での
/// 発見に対する回避策の検証用、Phase 9-4追加調査）。
async fn physical_path_candidates(
    config: &MultipathIsekaiPipeQuicConfig,
    default_target: SocketAddr,
    listen_port: u16,
) -> Vec<PhysicalPathCandidate> {
    let mut out = Vec::new();
    if let (Some(fd), Some(ip)) = (config.wifi_fd, &config.wifi_local_ip) {
        match ip.parse::<IpAddr>() {
            Ok(local_ip) => out.push(PhysicalPathCandidate {
                candidate: PHYSICAL_WIFI_LABEL.into(),
                fd,
                local_ip,
                target_addr: default_target,
            }),
            Err(e) => warn!("multipath_quic: invalid wifi_local_ip {ip:?}: {e}"),
        }
    }
    if let (Some(fd), Some(ip)) = (config.cellular_fd, &config.cellular_local_ip) {
        match ip.parse::<IpAddr>() {
            Ok(local_ip) => {
                let target_addr = match &config.cellular_remote_host {
                    Some(host) => match tokio::net::lookup_host((host.as_str(), listen_port)).await {
                        Ok(mut it) => it.next().unwrap_or(default_target),
                        Err(e) => {
                            warn!("multipath_quic: cellular_remote_host DNS lookup failed ({e}), falling back to direct_host");
                            default_target
                        }
                    },
                    None => default_target,
                };
                out.push(PhysicalPathCandidate {
                    candidate: PHYSICAL_CELLULAR_LABEL.into(),
                    fd,
                    local_ip,
                    target_addr,
                })
            }
            Err(e) => warn!("multipath_quic: invalid cellular_local_ip {ip:?}: {e}"),
        }
    }
    out
}

async fn try_connect_multipath(
    config: &MultipathIsekaiPipeQuicConfig,
    rebind_rx: tokio::sync::mpsc::Receiver<RebindRequest>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<(noq::SendStream, noq::RecvStream), String> {
    // ユーザーが明示指定していればそれを優先し、無指定ならdirect_host使用時のみ
    // 既定の固定ポートにフォールバックする(後方互換)。
    let bind_port = config.bind_port
        .or_else(|| config.direct_host.is_some().then_some(DIRECT_MULTIPATH_BIND_PORT));
    let handshake = isekai_pipe_quic_transport::bootstrap_helper_via_ssh(
        &config.ssh_host, config.ssh_port, &config.username, &config.auth, &config.jump, bind_port,
        &crate::helper_bootstrap::IsekaiPipeP2pMode::None, host_key_callback,
    )
    .await?;

    let cert_sha256_hex = handshake.cert_sha256().to_string();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let path0_addr = isekai_pipe_quic_transport::resolve_direct_by_bootstrap_host(&config.ssh_host, &handshake)
        .await
        .map_err(|e| format!("multipath path0: {e}"))?;

    let direct_by_bootstrap_host_port = handshake
        .direct_by_bootstrap_host_port()
        .ok_or("handshake did not advertise a direct-by-bootstrap-host candidate")?;
    let path1_addr = match &config.direct_host {
        Some(host) => tokio::net::lookup_host((host.as_str(), direct_by_bootstrap_host_port))
            .await
            .ok()
            .and_then(|mut it| it.next()),
        None => None,
    };
    if config.direct_host.is_some() && path1_addr.is_none() {
        warn!("multipath_quic: direct_host set but DNS resolution failed; continuing with path0 only");
    }

    let physical = match path1_addr {
        Some(addr) => physical_path_candidates(config, addr, direct_by_bootstrap_host_port).await,
        None => Vec::new(),
    };
    let (conn, _broker, endpoint) = establish_multipath_connection(
        path0_addr, path1_addr, physical, &cert_sha256_hex, event_tx, crate::debug_fault::shared_injector(),
    )
    .await?;
    let (send, recv) = hello_ack(&conn, &session_secret).await?;
    info!("multipath_quic: HELLO/ACK ok — handing off to SSH");
    spawn_rebind_listener(endpoint, rebind_rx);
    Ok((send, recv))
}

/// `rebind_to_fd`からの要求を待ち受け、`Endpoint::rebind_abstract()`でendpointの
/// ソケットを丸ごと差し替える。物理pathのopen_pathとは異なりnoq issue #738の
/// バグを踏まない（新規pathの追加検証ではなく、既存endpoint全体のNATリバインド
/// 相当の操作のため）。
fn spawn_rebind_listener(endpoint: noq::Endpoint, mut rebind_rx: tokio::sync::mpsc::Receiver<RebindRequest>) {
    RUNTIME.spawn(async move {
        while let Some(req) = rebind_rx.recv().await {
            let socket = match udp_socket_from_raw_fd(req.fd) {
                Ok(s) => s,
                Err(e) => {
                    warn!("multipath_quic: rebind: invalid fd {}: {e}", req.fd);
                    continue;
                }
            };
            let multi = MultiUdpSocket { default: socket, named: Vec::new(), injector: req.injector };
            match endpoint.rebind_abstract(Box::new(multi)) {
                Ok(()) => info!("multipath_quic: rebind to local_ip={} succeeded", req.local_ip),
                Err(e) => warn!("multipath_quic: rebind to local_ip={} failed: {e}", req.local_ip),
            }
        }
    });
}

async fn run_over_stream(
    mut config: MultipathIsekaiPipeQuicConfig,
    (send, recv): (noq::SendStream, noq::RecvStream),
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });

    // path0/path1 の内訳はアプリ層から見えない単一の双方向バイトストリーム
    // （noqが内部でpathを選ぶ）。resume/reattach層は無いので、Phase 7の
    // resume_client::ReattachableStreamのような特別なラッパーは不要——
    // quinnと同様recv/sendはtokio::io::AsyncRead/AsyncWriteを実装しているので
    // そのままjoinしてrusshに渡す。
    let stream = tokio::io::join(recv, send);

    // マルチパス(実験的opt-in機能)はSSH接続プーリング(`archive/ISEKAI_SSH_DESIGN.md`
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

    // MultipathIsekaiPipeQuicConfig は agent forwarding 未対応（IsekaiPipeQuicConfig と同様）。
    run_ssh_channel_loop(&pooled, config.cols, config.rows, false, false, cmd_rx, event_tx).await;
}

#[cfg(test)]
mod tests {
    //! `establish_multipath_connection` を loopback 上の2アドレス
    //! （127.0.0.1 / 127.0.0.2、いずれも同一の noq サーバーへの別経路）で
    //! 直接検証する。実 SSH ブートストラップは経由しない（`try_connect_multipath`
    //! ではなく `establish_multipath_connection` を直接呼ぶ）ので、実機・実
    //! ネットワーク不要でCIから常時実行できる。
    use super::*;
    use isekai_protocol::attach::{
        decode_attach_activate, decode_attach_hello, encode_attach_response, AttachRejectReason, AttachToken,
        ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
    };
    use std::net::{IpAddr, Ipv4Addr};

    /// 実UDP/QUICを使うテストのpath検証待ちで共通に使うポーリング上限。
    ///
    /// このワーカーは複数の`claude`エージェント/Gradleデーモンが同時稼働する開発機で
    /// 動くことが常態化しており(`uptime`のload averageが4〜5になることがある)、
    /// もともとの固定`for _ in 0..50 { sleep(100ms) }`(=5秒)では、path確立自体は
    /// 正常でも単にCPUスケジューリング待ちで間に合わずflakyに失敗することを確認した
    /// (`HEALTH_CHECK_INTERVAL`が3秒であることを踏まえても、5秒は1周分の余裕しかない)。
    /// 実際に壊れている場合はこの上限まで待っても永遠にVICmatchしないため、上限自体を
    /// 緩めても「本当のバグを見逃す」方向には倒れない。
    const PATH_VALIDATION_POLL_TIMEOUT: Duration = Duration::from_secs(20);
    const PATH_VALIDATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

    /// `tracker.get(id)`が`want`になるまで`PATH_VALIDATION_POLL_TIMEOUT`を上限にポーリングする。
    /// 上限に達した場合は最後に観測した状態を返す(呼び出し側でassert_eqのメッセージに使う)。
    async fn poll_until_path_state(tracker: &PathHealthTracker, id: &PathLabel, want: PathState) -> PathState {
        let mut last = tracker.get(id);
        let deadline = tokio::time::Instant::now() + PATH_VALIDATION_POLL_TIMEOUT;
        while last != want && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(PATH_VALIDATION_POLL_INTERVAL).await;
            last = tracker.get(id);
        }
        last
    }

    async fn start_test_server() -> (u16, String, [u8; 32]) {
        let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::Digest;
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let mut session_secret = [0u8; 32];
        {
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut session_secret);
        }

        let mut server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        server_crypto.alpn_protocols = vec![ALPN.to_vec()];
        server_crypto.max_early_data_size = 0;
        let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap();

        let mut transport = noq::TransportConfig::default();
        transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(2));
        transport.max_concurrent_multipath_paths(8);
        let mut server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
        server_config.transport_config(Arc::new(transport));

        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let endpoint = noq::Endpoint::server(server_config, bind_addr).unwrap();
        let port = endpoint.local_addr().unwrap().port();

        let secret_for_server = session_secret;
        tokio::spawn(async move {
            loop {
                let Some(incoming) = endpoint.accept().await else { break };
                let secret = secret_for_server;
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    loop {
                        let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
                        let secret = secret;
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            // ATTACH v2: type(1) + session_id(16) + generation(8) +
                            // attempt_id(16) + requested_resume_grace_secs(4) + proof(32)
                            // — `hello_ack`（このテストが検証対象とする本番クライアント
                            // ロジック）の送信フォーマットと揃える。
                            let mut hello_buf = [0u8; ATTACH_HELLO_FRAME_LEN];
                            if recv.read_exact(&mut hello_buf).await.is_err() { return; }
                            let Ok(hello) = decode_attach_hello(&hello_buf) else { return; };
                            let mut exporter = [0u8; 32];
                            if conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").is_err() { return; }
                            // proof = HMAC(secret, exporter || attach_hello_proof_transcript(..))。
                            let transcript = attach_hello_proof_transcript(
                                &hello.session_id, hello.generation, &hello.attempt_id, hello.requested_resume_grace_secs,
                            );
                            let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
                            mac.update(&exporter);
                            mac.update(&transcript);
                            let expected: [u8; 32] = mac.finalize().into_bytes().into();
                            if !hello.proof.ct_eq(&AttachProof::new(expected)) {
                                let _ = send
                                    .write_all(&encode_attach_response(&AttachResponse::Reject(
                                        AttachRejectReason::Auth,
                                    )))
                                    .await;
                                return;
                            }
                            // AttachReadyV2: 受け取った識別子をそのままエコーし、任意の
                            // attach_token を返す（テストは値そのものは検証しない）。
                            let ready = AttachResponse::Ready {
                                session_id: hello.session_id,
                                generation: hello.generation,
                                attempt_id: hello.attempt_id,
                                negotiated_resume_grace_secs: 0,
                                attach_token: AttachToken::new([0u8; 16]),
                            };
                            let _ = send.write_all(&encode_attach_response(&ready)).await;
                            // ATTACH_ACTIVATE をちょうど 1 フレーム分だけ読む（後続のデータ
                            // stream を食わないよう read_to_end は使わない）。
                            let mut activate_buf = [0u8; ATTACH_ACTIVATE_FRAME_LEN];
                            if recv.read_exact(&mut activate_buf).await.is_err() { return; }
                            if decode_attach_activate(&activate_buf).is_err() { return; }
                            if let Ok(data) = recv.read_to_end(4096).await {
                                let mut reply = b"echo:".to_vec();
                                reply.extend_from_slice(&data);
                                let _ = send.write_all(&reply).await;
                                let _ = send.finish();
                            }
                        });
                    }
                });
            }
        });

        (port, cert_sha256_hex, session_secret)
    }

    #[tokio::test]
    async fn path0_and_path1_both_serve_hello_ack() {
        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let path1: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();

        let (conn, tracker, _endpoint) = establish_multipath_connection(path0, Some(path1), Vec::new(), &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();
        assert_eq!(tracker.get(&PRIMARY_LABEL.into()), PathState::Validated);

        let (send, recv) = hello_ack(&conn, &secret).await.unwrap();
        drop(send);
        drop(recv);

        // path1 の確立はバックグラウンドタスクなので少し待つ。
        let state = poll_until_path_state(&tracker, &SECONDARY_LABEL.into(), PathState::Validated).await;
        assert_eq!(state, PathState::Validated, "path1 should validate within timeout");
        assert!(tracker.any_validated());

        conn.close(0u32.into(), b"test done");
    }

    #[tokio::test]
    async fn path0_only_when_direct_host_absent() {
        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (conn, tracker, _endpoint) = establish_multipath_connection(path0, None, Vec::new(), &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();
        let (send, recv) = hello_ack(&conn, &secret).await.unwrap();
        drop(send);
        drop(recv);

        // 確立直後にPrimaryが一時的にDegraded判定されることがある(健全性チェックが
        // heavy load下でRTTを大きく観測した場合)。すぐ回復するはずなのでポーリングするが、
        // load averageが高い開発機ではDegradedのまま回復しないことさえある——これは
        // 「実際にRTTが閾値を超えている」という健全性チェックとしては正しい結果であり、
        // このテストが検証したい「direct_hostが無ければpath0だけが確立し、path1は
        // 開かれない」こととは無関係。Validated/Degradedのどちらでも「到達はしている」
        // ことに変わりは無いので、両方を許容する(Unknown/Failedなら本当に確立して
        // いないので、そちらは今まで通り失敗として扱う)。
        let state = poll_until_path_state(&tracker, &PRIMARY_LABEL.into(), PathState::Validated).await;
        assert!(
            matches!(state, PathState::Validated | PathState::Degraded),
            "path0 should have established (Validated or Degraded), got {state:?}"
        );
        assert_eq!(tracker.get(&SECONDARY_LABEL.into()), PathState::Unknown);

        conn.close(0u32.into(), b"test done");
    }

    /// noq issue #738の回避策の検証: `open_path()`で複数pathを同時に開くのではなく、
    /// `Endpoint::rebind_abstract()`でendpoint全体のソケットを丸ごと差し替えても
    /// （＝ローカルアドレスが127.0.0.1→127.0.0.4に変わっても）コネクションが
    /// 生き続け、新しいbi-directionalストリームでechoの往復に応答できることを確認する
    /// （「WiFiのupstreamが死んでいる」検知→セルラーへrebindのシナリオの土台）。
    #[tokio::test]
    async fn connection_survives_rebind_to_new_local_address() {
        use std::net::Ipv4Addr;

        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (conn, tracker, endpoint) =
            establish_multipath_connection(path0, None, Vec::new(), &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();
        assert_eq!(tracker.get(&PRIMARY_LABEL.into()), PathState::Validated);

        // rebind前: 通常のecho往復が動くことを確認。
        {
            let (mut send, mut recv) = hello_ack(&conn, &secret).await.unwrap();
            send.write_all(b"before-rebind").await.unwrap();
            send.finish().unwrap();
            let reply = recv.read_to_end(4096).await.unwrap();
            assert_eq!(reply, b"echo:before-rebind");
        }

        // 新しいループバックアドレス（127.0.0.4）にbindした生ソケットへrebind。
        let new_local = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 4)), 0);
        let new_std_sock = std::net::UdpSocket::bind(new_local).unwrap();
        new_std_sock.set_nonblocking(true).unwrap();
        let new_tokio_sock = Arc::new(tokio::net::UdpSocket::from_std(new_std_sock).unwrap());
        let multi = MultiUdpSocket {
            default: new_tokio_sock,
            named: Vec::new(),
            injector: crate::debug_fault::shared_injector(),
        };
        endpoint.rebind_abstract(Box::new(multi)).unwrap();

        // rebind後: 新しいbi-directionalストリームでもecho往復が動くことを確認
        // （＝コネクションがローカルアドレス変更を生き延びた）。
        {
            let (mut send, mut recv) = hello_ack(&conn, &secret).await.unwrap();
            send.write_all(b"after-rebind").await.unwrap();
            send.finish().unwrap();
            let reply = recv.read_to_end(4096).await.unwrap();
            assert_eq!(reply, b"echo:after-rebind");
        }

        conn.close(0u32.into(), b"test done");
    }

    /// path0が切れてもpath1だけでコネクションが生き続け、新しいHELLO/ACKの
    /// 往復に応答できることを確認する（受動的フェイルオーバーの核心）。
    #[tokio::test]
    async fn connection_survives_after_path0_closes() {
        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let path1: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();

        let (conn, tracker, _endpoint) = establish_multipath_connection(path0, Some(path1), Vec::new(), &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();
        let state = poll_until_path_state(&tracker, &SECONDARY_LABEL.into(), PathState::Validated).await;
        assert_eq!(state, PathState::Validated, "path1 should validate within timeout");

        if let Some(p0) = conn.path(noq::PathId::ZERO) {
            let _ = p0.close();
        }

        // path0 close後もコネクションは生きており、新しいHELLO/ACK往復に応答できる。
        // heavy load下ではclose直後すぐには応答できないことがあるため、固定の1回
        // sleep+1回試行ではなくリトライする(本当に生き延びていなければ何度試しても
        // 失敗するので、リトライを増やしても偽陽性にはならない)。
        let mut last_err = None;
        let mut result = None;
        for attempt in 0..10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            match hello_ack(&conn, &secret).await {
                Ok(pair) => { result = Some(pair); break; }
                Err(e) => { last_err = Some((attempt, e)); }
            }
        }
        let (send, recv) = result.unwrap_or_else(|| {
            panic!("connection should survive path0 closing (last error: {last_err:?})")
        });
        drop(send);
        drop(recv);

        conn.close(0u32.into(), b"test done");
    }

    /// Phase 9-4: `MultiUdpSocket`（物理path用）の配線を、実Android APIを使わずに
    /// loopback上の生fdで検証する。127.0.0.3にbindしたソケットの生fdを
    /// `PhysicalPathCandidate`として渡し、path0（127.0.0.1、defaultソケット経由）+
    /// 物理path（127.0.0.3、明示バインドされたソケット経由）が同一コネクション内で
    /// 両方確立し、HELLO/ACKに応答できることを確認する。
    #[tokio::test]
    async fn physical_path_candidate_establishes_via_multi_udp_socket() {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // path1 (default socket) と物理path (bound socket) はどちらも同じ
        // direct_host 相当のアドレスへ向かうのが実際のフロー通り。宛先自体は
        // path0 と同じでも構わない（noq側でsrc_ip/portが異なる別pathとして
        // 扱われる）が、path1側は既存テスト同様127.0.0.2を使い、path0とpath1が
        // 別pathとして確立することも一緒に確認する。
        let direct: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();

        let physical_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(physical_ip, 0)).unwrap();
        let fd = std_sock.as_raw_fd();
        // udp_socket_from_raw_fd は fd の所有権を引き取って drop 時に close する前提
        // （Kotlin側の detachFd() 相当）。into_raw_fd() で std_sock 側の所有権を放棄する。
        let _ = std_sock.into_raw_fd();

        let physical = vec![PhysicalPathCandidate {
            candidate: PHYSICAL_WIFI_LABEL.into(),
            fd,
            local_ip: physical_ip,
            target_addr: direct,
        }];
        let (conn, tracker, _endpoint) =
            establish_multipath_connection(path0, Some(direct), physical, &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();

        let state = poll_until_path_state(&tracker, &PHYSICAL_WIFI_LABEL.into(), PathState::Validated).await;
        assert_eq!(
            state, PathState::Validated,
            "physical wifi path should validate within timeout",
        );

        let (send, recv) = hello_ack(&conn, &secret).await.unwrap();
        drop(send);
        drop(recv);

        conn.close(0u32.into(), b"test done");
    }

    // `classify_path_health`/`has_zero_response`のsynthetic PathStatsテスト、および
    // `PathBroker`単体テスト(旧`broker_register_and_degraded_transition`)は
    // isekai_transport::path_healthへ移植済み(そちらのunit testと重複するため削除、
    // isekai-terminal-core/isekai-transport crate共有化)。

    /// Phase 9-5実機検証の前段: loopbackで実際に`debug_fault`（既存のフォルト注入
    /// インフラ、`isekai_pipe_quic_transport.rs`/`faulty_udp_socket.rs`と共有）を使って
    /// 遅延を注入し、ヘルスモニタが本当にPathState::Degradedへ遷移させ、
    /// 遅延解除後にValidatedへ回復することを確認する。
    ///
    /// `debug_fault::shared_injector()`はプロセスグローバルな状態なので、
    /// このテストは他のフォルト注入系テストと同時実行しないこと
    /// （`cargo test -p isekai-terminal-core --lib multipath_transport::tests::path0_degrades_and_recovers_under_injected_latency`
    /// のように単独実行する）。
    #[tokio::test]
    async fn path0_degrades_and_recovers_under_injected_latency() {
        crate::debug_fault::shared_injector().restore();
        crate::debug_fault::shared_injector().set_latency(Duration::ZERO);
        crate::debug_fault::shared_injector().set_loss_rate(0.0);

        let (port, cert_sha256_hex, _secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (conn, tracker, _endpoint) = establish_multipath_connection(path0, None, Vec::new(), &cert_sha256_hex, tokio::sync::mpsc::channel(8).0, crate::debug_fault::shared_injector()).await.unwrap();
        assert_eq!(tracker.get(&PRIMARY_LABEL.into()), PathState::Validated);

        // DEGRADED_RTT_THRESHOLD(800ms)を大きく超える片道遅延を注入する。
        // noqのRTT平滑化（RFC 9002 のEMA、smoothed_rtt = 7/8*old + 1/8*latest）は
        // 小さめの遅延（900ms程度）だと閾値超えまで10サンプル以上要ることが実測で
        // 判明した（798msで頭打ちに近づいて収束が遅い）ため、EMAが1サンプルでも
        // 確実に閾値を超えるよう大きめの値（5秒＝往復10秒）を注入する。
        crate::debug_fault::shared_injector().set_latency(Duration::from_millis(5000));

        let became_degraded = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if tracker.get(&PRIMARY_LABEL.into()) == PathState::Degraded {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(became_degraded, "path0 should become Degraded under 5s injected one-way latency");

        // 回復: 遅延を止めれば連続2回健全チェックでAvailableに戻る
        // （ただしEMAが下がりきるまでは1回目の健全判定でもRTTがまだ閾値を
        // 超えている可能性があるため、ここも十分待つ）。
        crate::debug_fault::shared_injector().set_latency(Duration::ZERO);
        let recovered = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if tracker.get(&PRIMARY_LABEL.into()) == PathState::Validated {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(recovered, "path0 should recover to Validated once latency is removed");

        crate::debug_fault::shared_injector().restore();
        conn.close(0u32.into(), b"test done");
    }

    /// ユーザー提案の検証: 「本物のキャプティブポータルが無くても、UDPを丸ごと
    /// 遮断するfault injectionで模擬できるはず。応答が返って来ないことで判断すれば
    /// よい」という指摘の通り、`debug_fault`のCUT（既存のPhase 9-5実機検証で使った
    /// のと同じ仕組み）だけでNoViablePath検知が動くことを確認する。キャプティブ
    /// ポータルはQUICから見れば100%ロスと区別が付かないため、これは実質的に
    /// 同じ状況を再現している。プロセスグローバルな`debug_fault::shared_injector()`
    /// ではなく独立した`UdpFaultInjector::new()`を使うので、他のフォルト注入系
    /// テストと並行実行しても安全。
    #[tokio::test]
    async fn no_viable_path_fires_when_udp_fully_cut() {
        let injector = crate::faulty_udp_socket::UdpFaultInjector::new();

        let (port, cert_sha256_hex, _secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let (conn, tracker, _endpoint) =
            establish_multipath_connection(path0, None, Vec::new(), &cert_sha256_hex, event_tx, injector.clone())
                .await
                .unwrap();
        assert_eq!(tracker.get(&PRIMARY_LABEL.into()), PathState::Validated);

        // 「WiFiはあるがupstreamが死んでいる」相当: 応答が一切返ってこない状態にする。
        injector.cut();

        let got_no_viable_path = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if matches!(event_rx.recv().await, Some(TransportEvent::NoViablePath)) {
                    return true;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(got_no_viable_path, "NoViablePath should fire once the only path goes fully unresponsive");
        assert!(!tracker.any_validated());

        conn.close(0u32.into(), b"test done");
    }

    /// ユーザー提案の第2段: 「プロセスグローバルではなく部分障害をエミュレートできない
    /// か」に応えたテスト。「現在のpath（WiFi相当）」と「rebind先（セルラー相当）」に
    /// それぞれ独立した`UdpFaultInjector`を割り当てることで、debug_fault一つでは
    /// 検証できなかった「本当に別の生きている経路へ切り替わればセッションが継続する」
    /// ところまで、プロセスグローバル状態に頼らずloopbackだけで実証する。
    #[tokio::test]
    async fn session_survives_rebind_when_only_current_path_is_cut() {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let wifi_injector = crate::faulty_udp_socket::UdpFaultInjector::new();
        let cellular_injector = crate::faulty_udp_socket::UdpFaultInjector::new();

        let (port, cert_sha256_hex, secret) = start_test_server().await;
        let path0: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let (conn, tracker, endpoint) = establish_multipath_connection(
            path0, None, Vec::new(), &cert_sha256_hex, event_tx, wifi_injector.clone(),
        )
        .await
        .unwrap();
        assert_eq!(tracker.get(&PRIMARY_LABEL.into()), PathState::Validated);

        // rebind前: 通常のecho往復が動くことを確認。
        {
            let (send, recv) = hello_ack(&conn, &secret).await.unwrap();
            drop(send);
            drop(recv);
        }

        // 「WiFi」だけを遮断する。「セルラー」（cellular_injector）はまだ一切
        // 関与していないので、この時点でも生きている。
        wifi_injector.cut();

        let got_no_viable_path = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if matches!(event_rx.recv().await, Some(TransportEvent::NoViablePath)) {
                    return true;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(got_no_viable_path, "NoViablePath should fire once the current (wifi) path goes unresponsive");

        // 「セルラー」に見立てた別のloopbackソケット（127.0.0.6）を、クリーンな
        // （cutされていない）独立したinjectorでrebindする——本物のキャプティブ
        // ポータルで「WiFiだけ死んでいてセルラーは生きている」状況の再現。
        let cellular_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 6));
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(cellular_ip, 0)).unwrap();
        let fd = std_sock.as_raw_fd();
        let _ = std_sock.into_raw_fd(); // rebind先に所有権を渡す（detachFd相当）

        let (rebind_tx, rebind_rx) = tokio::sync::mpsc::channel(4);
        spawn_rebind_listener(endpoint, rebind_rx);
        rebind_tx
            .send(RebindRequest { fd, local_ip: cellular_ip, injector: cellular_injector })
            .await
            .unwrap();
        // rebind_abstract()自体は同期的だが、rebind_listenerタスクへのディスパッチを
        // 待つため少し待機する。
        tokio::time::sleep(Duration::from_millis(200)).await;

        // rebind後: 「セルラー」経由の新しいストリームでecho往復が動くこと——
        // つまり、debug_fault単体では確認できなかった「rebind後に本当に生きている
        // 経路へ切り替わってセッションが継続する」ことを実証する。
        let recovered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok((mut send, mut recv)) = hello_ack(&conn, &secret).await {
                    if send.write_all(b"after-rebind-to-cellular").await.is_ok() {
                        let _ = send.finish();
                        if let Ok(reply) = recv.read_to_end(4096).await {
                            if reply == b"echo:after-rebind-to-cellular" {
                                return true;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(recovered, "session should survive and keep working after rebinding to the unaffected cellular path");

        conn.close(0u32.into(), b"test done");
    }
}
