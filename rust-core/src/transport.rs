use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};
use parking_lot::Mutex;
use russh::{client, ChannelMsg};
use russh_keys::{HashAlg, PrivateKey, PublicKey};
use tokio::net::TcpListener;

use crate::agent_forward;
use crate::theme::Theme;
use crate::{ForwardState, JumpConfig, SshAuth};

// ── Transport command / event ────────────────────────────

/// Kotlin → transport task: SSH I/O 命令
pub(crate) enum TransportCommand {
    WriteStdin(Vec<u8>),
    Resize { cols: u32, rows: u32 },
    Disconnect,
    /// ローカルポートフォワード(-L)を追加する。`id` は呼び出し側が一意に割り振る。
    AddLocalForward {
        id: String,
        bind_addr: String,
        bind_port: u16,
        remote_host: String,
        remote_port: u16,
    },
    /// リモートポートフォワード(-R)を追加する。SSHサーバー側に`bind_addr:bind_port`を
    /// listenさせ(`tcpip_forward`)、そこへの接続を`target_host:target_port`
    /// (クライアントから見たローカルターゲット)へ中継する。
    AddRemoteForward {
        id: String,
        bind_addr: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    /// SOCKS4/5プロキシ(-D)を追加する。`bind_addr:bind_port`でSOCKSクライアントを
    /// 受け付け、接続ごとにSOCKSハンドシェイクで宛先を読み取ってから中継する。
    AddDynamicForward {
        id: String,
        bind_addr: String,
        bind_port: u16,
    },
    /// `id` の待受を停止する(新規 accept を止める。既存の中継コピーは自然終了に任せる)。
    RemoveForward { id: String },
}

/// transport task → session_event_loop: SSH 状態通知
pub(crate) enum TransportEvent {
    HostKey(String, tokio::sync::oneshot::Sender<bool>),
    Connected,
    Stdout(Vec<u8>),
    Resized { cols: u32, rows: u32 },
    Disconnected { reason: Option<String> },
    /// マルチパスtransport専用（`multipath_transport.rs`の`PathBroker`から発火）。
    NoViablePath,
    ForwardStateChanged { id: String, state: ForwardState },
    /// SSH agent forwarding: サーバー（またはサーバー上の他プロセス）が、転送された
    /// エージェント経由でこの鍵を使った署名を要求してきた。署名は必ずユーザー確認を
    /// 経てから行う（既定 OFF・opt-in の機能であっても、要求ごとの確認は必須）。
    /// `reply` に `true` を送ると署名を実行し、`false`／drop（タイムアウト含む）なら拒否する。
    AgentSignRequest {
        key_fingerprint: String,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
}

/// Kotlin → session_event_loop: trzsz 操作（transport を経由しない）
pub(crate) enum SessionCmd {
    TrzszAcceptUpload  { transfer_id: String, file_name: String, file_size: u64, mode: u32 },
    TrzszChunk         { transfer_id: String, data: Vec<u8>, is_last: bool },
    TrzszAcceptDownload { transfer_id: String },
    TrzszCancel        { transfer_id: String },
    /// Phase 12: per-session theme。以降にパースされるSGRの色解決にのみ反映される。
    SetTheme(Theme),
}

// ── russh Handler ────────────────────────────────────────

pub(crate) struct RusshEventHandler {
    pub(crate) event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
    /// SSH agent forwarding が有効かつ公開鍵認証成功後にのみ `Some` になる、
    /// 転送する秘密鍵（認証に使ったのと同じ鍵を共有する。鍵の追加受け渡しは不要）。
    /// `run_ssh_channel_loop` が認証成功後にセットするため `Mutex` 越しに共有する。
    pub(crate) agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    /// リモートポートフォワード(-R)の経路表: サーバー側で実際に bind されたポート番号 →
    /// (クライアントから見たローカルターゲットのホスト, ポート)。`tcpip_forward` 成功時に
    /// `run_ssh_channel_loop` が登録し、`server_channel_open_forwarded_tcpip` が
    /// `connected_port` をキーに引いて中継先を決める。
    pub(crate) remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
}

impl RusshEventHandler {
    /// agent forwarding・リモートポートフォワードを使わない transport（QUIC 等）向けの
    /// 簡易コンストラクタ。
    pub(crate) fn new(event_tx: tokio::sync::mpsc::Sender<TransportEvent>) -> Self {
        RusshEventHandler {
            event_tx,
            agent_key: Arc::new(Mutex::new(None)),
            remote_forwards: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl client::Handler for RusshEventHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.event_tx.send(TransportEvent::HostKey(fp, reply_tx)).await.ok();
        Ok(reply_rx.await.unwrap_or(false))
    }

    /// サーバーが agent-forward チャネルを開き返してきた時に呼ばれる
    /// （こちらが `channel.agent_forward(true)` を送っていた場合のみ発生する）。
    /// チャネル I/O はハンドラをブロックしないよう別タスクで処理する。
    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let key = self.agent_key.lock().clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(agent_forward::serve_agent_channel(channel, key, event_tx));
        Ok(())
    }

    /// リモートポートフォワード(-R)経由でサーバーが新規接続を通知してきた時に呼ばれる
    /// （こちらが `tcpip_forward(bind_addr, bind_port)` していた場合のみ発生する）。
    /// `connected_port` で経路表を引き、対応するローカルターゲットへ中継する。
    /// 経路表に無いポート(既にremoveされた等)の場合はチャネルをそのまま閉じる。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let target = self.remote_forwards.lock().get(&(connected_port as u16)).cloned();
        let Some((target_host, target_port)) = target else {
            warn!(
                "remote-forward: no route for connected_port={} (originator={}:{}), closing",
                connected_port, originator_address, originator_port
            );
            return Ok(());
        };
        let originator_address = originator_address.to_string();
        tokio::spawn(async move {
            debug!(
                "remote-forward: connection from {}:{} -> relaying to {}:{}",
                originator_address, originator_port, target_host, target_port
            );
            let mut target_stream = match tokio::net::TcpStream::connect((target_host.as_str(), target_port)).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("remote-forward: connect to {}:{} failed: {}", target_host, target_port, e);
                    return;
                }
            };
            let mut channel_stream = channel.into_stream();
            match tokio::io::copy_bidirectional(&mut channel_stream, &mut target_stream).await {
                Ok((to_target, to_remote)) => {
                    debug!("remote-forward: closed (sent {} bytes, received {} bytes)", to_target, to_remote);
                }
                Err(e) => {
                    debug!("remote-forward: copy ended: {}", e);
                }
            }
        });
        Ok(())
    }
}

// ── SSH 認証（TCP・QUIC・ProxyJump 共通）────────────────

/// `session` に対して `auth` で認証する。公開鍵認証が成功した場合はその鍵も返す
/// （agent forwarding で転送先の署名要求に同じ鍵を使い回すため。鍵の追加受け渡しは
/// 不要という設計）。
pub(crate) async fn authenticate_session(
    session: &mut client::Handle<RusshEventHandler>,
    username: &str,
    auth: &SshAuth,
) -> (bool, Option<Arc<PrivateKey>>) {
    match auth {
        SshAuth::Password { password } => {
            let ok = session.authenticate_password(username, password).await.ok().unwrap_or(false);
            (ok, None)
        }
        SshAuth::PublicKey { private_key_pem } => match PrivateKey::from_openssh(private_key_pem) {
            Ok(key) => {
                let key = Arc::new(key);
                let ok = session.authenticate_publickey(username, key.clone()).await.ok().unwrap_or(false);
                (ok, ok.then_some(key))
            }
            Err(e) => {
                warn!("ssh: private key parse failed: {}", e);
                (false, None)
            }
        },
    }
}

/// タスク#65: パスワード・復号済み秘密鍵PEMのベストエフォートなメモリゼロ化。
///
/// `SshAuth` は UniFFI の `Enum` としてKotlin側と直接やり取りされる公開型のため、
/// フィールドの型自体を`zeroize::Zeroizing<_>`に変えたり`Drop`を実装したりすると
/// (UniFFIの`FfiConverter`生成コードがフィールドをムーブして取り出す都合上コンパイルが
/// 通らない・要バインディング再生成になる)ため、型はそのままに、もう使い終わった時点で
/// 呼び出し側から明示的にこの関数を呼んでヒープ上の実体を上書きする方式にしている。
/// `run_ssh_channel_loop` は接続ごとに一度しか認証しないため、認証成功/失敗を問わず
/// 呼び出し直後に呼べば安全(以降そのメモリを`SshAuth`として再利用することはない)。
pub(crate) fn zeroize_ssh_auth(auth: &mut SshAuth) {
    use zeroize::Zeroize;
    match auth {
        SshAuth::Password { password } => password.zeroize(),
        SshAuth::PublicKey { private_key_pem } => private_key_pem.zeroize(),
    }
}

/// [`SshConfig::jump`] が設定されていれば、まず踏み台ホストへ接続・認証し、
/// `channel_open_direct_tcpip` で `target_host:target_port` へのチャネルを開いた上に
/// ネストしたSSHセッションを張る（`ssh -J` 相当）。未設定なら直接 TCP 接続する。
///
/// 返り値の踏み台側 `Handle`（`Some` の場合）は、戻り値の対象セッションが使う
/// トンネルの実体を保持しているため、呼び出し元は対象セッションの利用が終わるまで
/// **必ず生かしたまま(drop しない)保持すること**。
pub(crate) struct EstablishedSession {
    pub(crate) handle: client::Handle<RusshEventHandler>,
    pub(crate) agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    pub(crate) remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
    /// 保持するだけで参照はしない(トンネルの接続を生かしておくためだけの目的)。
    _jump_handle: Option<client::Handle<RusshEventHandler>>,
}

pub(crate) async fn connect_via_jump_or_direct(
    jump: &Option<JumpConfig>,
    russh_config: Arc<client::Config>,
    target_host: &str,
    target_port: u16,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) -> Result<EstablishedSession, String> {
    let Some(jump) = jump else {
        let addr = format!("{target_host}:{target_port}");
        info!("ssh: TCP connecting to {}", addr);
        let handler = RusshEventHandler::new(event_tx);
        let agent_key = handler.agent_key.clone();
        let remote_forwards = handler.remote_forwards.clone();
        let handle = client::connect(russh_config, addr.as_str(), handler)
            .await
            .map_err(|e| format!("TCP connect to {addr} failed: {e}"))?;
        info!("ssh: TCP connected to {}", addr);
        return Ok(EstablishedSession { handle, agent_key, remote_forwards, _jump_handle: None });
    };

    let jump_addr = format!("{}:{}", jump.host, jump.port);
    info!("ssh(jump): TCP connecting to {}", jump_addr);
    let jump_handler = RusshEventHandler::new(event_tx.clone());
    let mut jump_handle = client::connect(russh_config.clone(), jump_addr.as_str(), jump_handler)
        .await
        .map_err(|e| format!("jump host TCP connect to {jump_addr} failed: {e}"))?;

    let (authenticated, _) = authenticate_session(&mut jump_handle, &jump.username, &jump.auth).await;
    if !authenticated {
        return Err(format!("jump host authentication failed for {}@{}", jump.username, jump_addr));
    }
    info!("ssh(jump): auth ok, opening direct-tcpip to {}:{}", target_host, target_port);

    let channel = jump_handle
        .channel_open_direct_tcpip(target_host, target_port as u32, "127.0.0.1", 0)
        .await
        .map_err(|e| format!("jump host direct-tcpip to {target_host}:{target_port} failed: {e}"))?;
    let stream = channel.into_stream();

    let target_handler = RusshEventHandler::new(event_tx);
    let agent_key = target_handler.agent_key.clone();
    let remote_forwards = target_handler.remote_forwards.clone();
    let handle = client::connect_stream(russh_config, stream, target_handler)
        .await
        .map_err(|e| format!("SSH handshake over jump tunnel to {target_host}:{target_port} failed: {e}"))?;
    info!("ssh: connected to {}:{} via jump {}", target_host, target_port, jump_addr);

    Ok(EstablishedSession { handle, agent_key, remote_forwards, _jump_handle: Some(jump_handle) })
}

// ── SSH接続プーリング用: 認証済みHandleの確立とチャネルの追加 ──
//
// SSH接続プーリング(`archive/ISEKAI_SSH_DESIGN.md`「2026-07-07: 上記オープンな課題の
// 調査・設計確定」節)により、「認証済み`client::Handle`を確立する」処理と
// 「そのHandle上に1本SSHチャネルを開いてI/Oループを回す」処理を分離する。前者は
// プールにヒットした2本目以降のタブではスキップされ、後者は毎回(プールの有無に
// 関わらず)タブごとに1回ずつ行われる。

/// 複数タブ(チャネル)から共有される、認証済みの`client::Handle`。プレーンSSH・
/// isekai-pipe QUIC系(ネストしたSSH)いずれの確立方法でも同じ形にまとめる
/// (`run_ssh_channel_loop`から見れば、TCPの上かQUICトンネルの上かは区別不要なため)。
pub(crate) struct PooledSshHandle {
    pub(crate) handle: Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>,
    agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
    /// 踏み台経由の場合、対象への接続が続く限り保持し続ける必要がある
    /// (`EstablishedSession::_jump_handle`と同じ理由)。QUICネスト経由(踏み台なし)では`None`。
    _jump_handle: Option<client::Handle<RusshEventHandler>>,
}

/// 未認証の`client::Handle`(TCP直結・踏み台経由・QUICトンネル経由いずれでも可)に対して
/// 認証を行い、成功したら[PooledSshHandle]へラップする。`agent_forward`はプールキーの
/// 一部でもあるため、プールエントリ全体に対して1回だけ`agent_key`を設定すればよい
/// (2本目以降のチャネルは個別に認証しないため、チャネル単位で毎回設定する必要が無い)。
async fn finish_establishing_handle(
    mut handle: client::Handle<RusshEventHandler>,
    agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
    jump_handle: Option<client::Handle<RusshEventHandler>>,
    username: &str,
    auth: &mut SshAuth,
    agent_forward: bool,
) -> Result<PooledSshHandle, String> {
    let auth_method = match auth {
        SshAuth::Password { .. } => "password",
        SshAuth::PublicKey { .. } => "pubkey",
    };
    info!("ssh: auth {} for {}", auth_method, username);

    let (authenticated, authed_key) = authenticate_session(&mut handle, username, &*auth).await;
    // タスク#65: 認証に使い終わったので、平文の認証情報(パスワード・復号済み秘密鍵PEM)を
    // ここで即座にゼロ化する(このHandleの以降の処理で`auth`が再び必要になることはない)。
    zeroize_ssh_auth(auth);

    if !authenticated {
        warn!("ssh: auth {} failed for {}", auth_method, username);
        return Err("Authentication failed".to_string());
    }
    info!("ssh: auth ok");

    if agent_forward {
        if let Some(key) = authed_key {
            *agent_key.lock() = Some(key);
        } else {
            debug!("ssh: agent_forward requested but auth method is not publickey — ignoring");
        }
    }

    Ok(PooledSshHandle {
        handle: Arc::new(tokio::sync::Mutex::new(handle)),
        agent_key,
        remote_forwards,
        _jump_handle: jump_handle,
    })
}

/// プレーンSSH(TCP直結・踏み台経由)用の確立関数。`connect_via_jump_or_direct` +
/// 認証をまとめて行う。
pub(crate) async fn establish_ssh_handle(
    jump: &Option<JumpConfig>,
    russh_config: Arc<client::Config>,
    host: &str,
    port: u16,
    username: &str,
    auth: &mut SshAuth,
    agent_forward: bool,
    event_tx: &tokio::sync::mpsc::Sender<TransportEvent>,
) -> Result<PooledSshHandle, String> {
    let established = connect_via_jump_or_direct(jump, russh_config, host, port, event_tx.clone()).await?;
    finish_establishing_handle(
        established.handle, established.agent_key, established.remote_forwards,
        established._jump_handle, username, auth, agent_forward,
    ).await
}

/// isekai-pipe QUIC系(ネストしたSSH、`client::connect_stream`)用の確立関数。呼び出し元が
/// QUIC接続確立(ヘルパー起動・QUICハンドシェイク等)を済ませ、`stream`を渡す。踏み台は
/// QUIC確立側(ヘルパー起動用ブートストラップSSH)で既に使われているため、ここでは扱わない
/// (`_jump_handle`は常に`None`)。
pub(crate) async fn establish_ssh_handle_over_stream<S>(
    russh_config: Arc<client::Config>,
    stream: S,
    username: &str,
    auth: &mut SshAuth,
    agent_forward: bool,
    event_tx: &tokio::sync::mpsc::Sender<TransportEvent>,
) -> Result<PooledSshHandle, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let handler = RusshEventHandler::new(event_tx.clone());
    let agent_key = handler.agent_key.clone();
    let remote_forwards = handler.remote_forwards.clone();
    let handle = client::connect_stream(russh_config, stream, handler)
        .await
        .map_err(|e| e.to_string())?;
    finish_establishing_handle(handle, agent_key, remote_forwards, None, username, auth, agent_forward).await
}

// ── SSH チャネルループ（TCP・QUIC 共通）─────────────────

/// [pooled]（既に認証済み）に対して新しいSSHチャネル(セッション/PTY/シェル)を1本開き、
/// そのチャネルのI/Oループを回す。プールにヒットした2本目以降のタブも最初のタブも、
/// この関数から始まる(呼び出し元が先に確立関数を呼ぶかプールから取得するかだけが違う)。
pub(crate) async fn run_ssh_channel_loop(
    pooled: &PooledSshHandle,
    cols: u32,
    rows: u32,
    agent_forward: bool,
    allow_non_loopback_forward_bind: bool,
    mut cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let mut channel = match pooled.handle.lock().await.channel_open_session().await {
        Ok(c) => { info!("ssh: session channel opened"); c }
        Err(e) => {
            warn!("ssh: channel_open_session failed: {}", e);
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    if agent_forward && pooled.agent_key.lock().is_some() {
        info!("ssh: requesting agent forwarding");
        if let Err(e) = channel.agent_forward(true).await {
            warn!("ssh: agent_forward request failed: {}", e);
        }
    }

    info!("ssh: requesting PTY {}x{}", cols, rows);
    if channel.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[]).await.is_err()
        || channel.request_shell(false).await.is_err()
    {
        warn!("ssh: PTY or shell request failed");
        event_tx.send(TransportEvent::Disconnected { reason: Some("PTY/shell request failed".into()) }).await.ok();
        return;
    }
    info!("ssh: shell started — entering I/O loop");

    event_tx.send(TransportEvent::Connected).await.ok();

    // シェル用チャネルの確立以降、認証等の `&mut self` operations は使わないが、
    // Phase 12 P2-2 で追加した `tcpip_forward`/`cancel_tcpip_forward`(リモート
    // ポートフォワード)は `&mut self` を要求する(SSHのglobal requestは同時に1件しか
    // in-flightにできないというプロトコル制約をAPI形上も表している)ため、
    // `channel_open_direct_tcpip(&self, ...)` のみで済んでいたPhase 7時点の
    // `Arc<Handle>` 共有では足りなくなった。`Arc<tokio::sync::Mutex<Handle>>` に変更し、
    // 待受タスク側は必要な呼び出しの間だけ lock する(Handle は Clone 不可のため、
    // 複数タスクからの共有自体は元々このAPI境界でしかできない)。
    //
    // SSH接続プーリング後は、この`Arc<Mutex<Handle>>`は自タブ専用ではなく[pooled]から
    // 複製した「プールエントリと共有」のハンドルになる。複数タブが同じHandleに対して
    // 独立にforwardを追加/削除しても、`remote_forwards`(ポート→転送先の経路表)は
    // [pooled]から複製したものを共有するため経路表自体は一貫する。
    let session = pooled.handle.clone();
    let remote_forwards = pooled.remote_forwards.clone();
    let mut active_forwards: HashMap<String, ActiveForward> = HashMap::new();

    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        debug!("ssh: stdout {} bytes", data.len());
                        event_tx.send(TransportEvent::Stdout(data.to_vec())).await.ok();
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        info!("ssh: remote exited status={}", exit_status);
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                    None => {
                        info!("ssh: channel closed by peer");
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(TransportCommand::WriteStdin(data)) => {
                        info!("ssh: stdin {} bytes", data.len());
                        if let Err(e) = channel.data(data.as_ref()).await {
                            warn!("ssh: channel.data write failed: {}", e);
                        }
                    }
                    Some(TransportCommand::Resize { cols, rows }) => {
                        info!("ssh: PTY resize {}x{}", cols, rows);
                        channel.window_change(cols, rows, 0, 0).await.ok();
                        event_tx.send(TransportEvent::Resized { cols, rows }).await.ok();
                    }
                    Some(TransportCommand::AddLocalForward { id, bind_addr, bind_port, remote_host, remote_port }) => {
                        if !allow_non_loopback_forward_bind && !is_loopback_bind_address(&bind_addr) {
                            reject_non_loopback_bind(&event_tx, id, &bind_addr).await;
                        } else {
                            info!("forward[{}]: add(local) {}:{} -> {}:{}", id, bind_addr, bind_port, remote_host, remote_port);
                            let task = tokio::spawn(run_local_forward(
                                id.clone(), bind_addr, bind_port, remote_host, remote_port,
                                session.clone(), event_tx.clone(),
                            ));
                            if let Some(old) = active_forwards.insert(id, ActiveForward::Task(task)) {
                                teardown_forward(old, session.clone(), remote_forwards.clone());
                            }
                        }
                    }
                    Some(TransportCommand::AddDynamicForward { id, bind_addr, bind_port }) => {
                        if !allow_non_loopback_forward_bind && !is_loopback_bind_address(&bind_addr) {
                            reject_non_loopback_bind(&event_tx, id, &bind_addr).await;
                        } else {
                            info!("forward[{}]: add(dynamic/SOCKS) {}:{}", id, bind_addr, bind_port);
                            let task = tokio::spawn(run_dynamic_forward(
                                id.clone(), bind_addr, bind_port, session.clone(), event_tx.clone(),
                            ));
                            if let Some(old) = active_forwards.insert(id, ActiveForward::Task(task)) {
                                teardown_forward(old, session.clone(), remote_forwards.clone());
                            }
                        }
                    }
                    Some(TransportCommand::AddRemoteForward { id, bind_addr, bind_port, target_host, target_port }) => {
                        if !allow_non_loopback_forward_bind && !is_loopback_bind_address(&bind_addr) {
                            reject_non_loopback_bind(&event_tx, id, &bind_addr).await;
                        } else {
                            info!("forward[{}]: add(remote) {}:{} -> {}:{}", id, bind_addr, bind_port, target_host, target_port);
                            match session.lock().await.tcpip_forward(bind_addr.clone(), bind_port as u32).await {
                                Ok(bound_port) => {
                                    let bound_port = if bind_port == 0 { bound_port as u16 } else { bind_port };
                                    remote_forwards.lock().insert(bound_port, (target_host, target_port));
                                    if let Some(old) = active_forwards.insert(
                                        id.clone(),
                                        ActiveForward::Remote { bind_addr, bound_port },
                                    ) {
                                        teardown_forward(old, session.clone(), remote_forwards.clone());
                                    }
                                    event_tx.send(TransportEvent::ForwardStateChanged {
                                        id, state: ForwardState::Listening,
                                    }).await.ok();
                                }
                                Err(e) => {
                                    warn!("forward[{}]: tcpip_forward {}:{} failed: {}", id, bind_addr, bind_port, e);
                                    event_tx.send(TransportEvent::ForwardStateChanged {
                                        id, state: ForwardState::Failed { reason: e.to_string() },
                                    }).await.ok();
                                }
                            }
                        }
                    }
                    Some(TransportCommand::RemoveForward { id }) => {
                        info!("forward[{}]: remove requested", id);
                        if let Some(old) = active_forwards.remove(&id) {
                            teardown_forward(old, session.clone(), remote_forwards.clone());
                            event_tx.send(TransportEvent::ForwardStateChanged {
                                id, state: ForwardState::Stopped,
                            }).await.ok();
                        }
                    }
                    Some(TransportCommand::Disconnect) | None => {
                        info!("ssh: disconnect requested");
                        channel.eof().await.ok();
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                }
            }
        }
    }

    for (id, forward) in active_forwards.drain() {
        debug!("forward[{}]: tearing down on session teardown", id);
        teardown_forward(forward, session.clone(), remote_forwards.clone());
    }
    info!("ssh: I/O loop exited");
}

// ── ポートフォワード共通(-L/-R/-D) ──────────────────────────

/// `addr` がループバック（127.0.0.0/8・::1）または文字列 "localhost"（大小無視）かどうか。
/// `allow_non_loopback_forward_bind == false` の場合の bind 許可判定に使う。
fn is_loopback_bind_address(addr: &str) -> bool {
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }
    addr.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// 非ループバックbindを`ForwardState::Failed`として拒否する共通処理
/// (-L/-R/-Dいずれも`allow_non_loopback_forward_bind == false`なら同じ判定)。
async fn reject_non_loopback_bind(
    event_tx: &tokio::sync::mpsc::Sender<TransportEvent>,
    id: String,
    bind_addr: &str,
) {
    warn!(
        "forward[{}]: rejecting non-loopback bind {} (allow_non_loopback_forward_bind=false)",
        id, bind_addr
    );
    event_tx.send(TransportEvent::ForwardStateChanged {
        id,
        state: ForwardState::Failed {
            reason: format!(
                "bind address {bind_addr} is not loopback and allow_non_loopback_forward_bind is false"
            ),
        },
    }).await.ok();
}

/// 稼働中のポートフォワード1件分の状態。`-L`/`-D`はクライアント側の待受タスクを、
/// `-R`はサーバー側に登録した`tcpip_forward`をそれぞれ後始末する必要があるため分ける。
enum ActiveForward {
    /// Local(-L)/Dynamic(-D): クライアント側の待受タスク。除去時はabortする。
    Task(tokio::task::JoinHandle<()>),
    /// Remote(-R): サーバー側に`tcpip_forward`で登録した内容。除去時は
    /// `cancel_tcpip_forward`をサーバーへ送り、`remote_forwards`経路表からも消す。
    Remote { bind_addr: String, bound_port: u16 },
}

/// [ActiveForward] 1件を後始末する(除去時・同一id上書き時・セッション終了時の3箇所で共通)。
fn teardown_forward(
    forward: ActiveForward,
    session: Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>,
    remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
) {
    match forward {
        ActiveForward::Task(task) => {
            task.abort();
        }
        ActiveForward::Remote { bind_addr, bound_port } => {
            remote_forwards.lock().remove(&bound_port);
            tokio::spawn(async move {
                if let Err(e) = session.lock().await.cancel_tcpip_forward(bind_addr.clone(), bound_port as u32).await {
                    warn!("remote-forward: cancel_tcpip_forward {}:{} failed: {}", bind_addr, bound_port, e);
                }
            });
        }
    }
}

/// `bind_addr:bind_port` で待受し、accept ごとに `channel_open_direct_tcpip` で
/// リモート `remote_host:remote_port` への SSH チャネルを開き、TCP ソケットと
/// 双方向にバイトを中継する。待受確立/失敗を `ForwardStateChanged` で通知する。
async fn run_local_forward(
    id: String,
    bind_addr: String,
    bind_port: u16,
    remote_host: String,
    remote_port: u16,
    handle: Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let listener = match TcpListener::bind((bind_addr.as_str(), bind_port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("forward[{}]: bind {}:{} failed: {}", id, bind_addr, bind_port, e);
            event_tx.send(TransportEvent::ForwardStateChanged {
                id, state: ForwardState::Failed { reason: e.to_string() },
            }).await.ok();
            return;
        }
    };
    info!("forward[{}]: listening on {}:{}", id, bind_addr, bind_port);
    event_tx.send(TransportEvent::ForwardStateChanged {
        id: id.clone(), state: ForwardState::Listening,
    }).await.ok();

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("forward[{}]: accept failed: {}", id, e);
                break;
            }
        };
        debug!("forward[{}]: accepted from {}", id, peer_addr);
        let handle = handle.clone();
        let remote_host = remote_host.clone();
        let fwd_id = id.clone();
        tokio::spawn(async move {
            let originator_ip = peer_addr.ip().to_string();
            let originator_port = peer_addr.port() as u32;
            let channel = match handle.lock().await
                .channel_open_direct_tcpip(remote_host.as_str(), remote_port as u32, originator_ip.as_str(), originator_port)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("forward[{}]: channel_open_direct_tcpip to {}:{} failed: {}", fwd_id, remote_host, remote_port, e);
                    return;
                }
            };
            let mut tcp_stream = tcp_stream;
            let mut channel_stream = channel.into_stream();
            match tokio::io::copy_bidirectional(&mut tcp_stream, &mut channel_stream).await {
                Ok((to_remote, to_local)) => {
                    debug!("forward[{}]: closed (sent {} bytes, received {} bytes)", fwd_id, to_remote, to_local);
                }
                Err(e) => {
                    debug!("forward[{}]: copy ended: {}", fwd_id, e);
                }
            }
        });
    }

    event_tx.send(TransportEvent::ForwardStateChanged { id, state: ForwardState::Stopped }).await.ok();
}

// ── SOCKSプロキシ(-D、Dynamic port forward) ─────────────────

/// `bind_addr:bind_port` でSOCKS4/4a/5クライアントを受け付け、接続ごとにSOCKSハンドシェイク
/// (`crate::socks::negotiate`)で宛先を読み取ってから `channel_open_direct_tcpip` で
/// SSHサーバー経由の接続へ中継する。待受確立/失敗を `ForwardStateChanged` で通知する。
async fn run_dynamic_forward(
    id: String,
    bind_addr: String,
    bind_port: u16,
    handle: Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let listener = match TcpListener::bind((bind_addr.as_str(), bind_port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("forward[{}]: bind {}:{} failed: {}", id, bind_addr, bind_port, e);
            event_tx.send(TransportEvent::ForwardStateChanged {
                id, state: ForwardState::Failed { reason: e.to_string() },
            }).await.ok();
            return;
        }
    };
    info!("forward[{}]: listening (SOCKS) on {}:{}", id, bind_addr, bind_port);
    event_tx.send(TransportEvent::ForwardStateChanged {
        id: id.clone(), state: ForwardState::Listening,
    }).await.ok();

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("forward[{}]: accept failed: {}", id, e);
                break;
            }
        };
        debug!("forward[{}]: accepted (SOCKS) from {}", id, peer_addr);
        let handle = handle.clone();
        let fwd_id = id.clone();
        tokio::spawn(async move {
            let mut tcp_stream = tcp_stream;
            let (target_host, target_port) = match crate::socks::negotiate(&mut tcp_stream).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("forward[{}]: SOCKS negotiation from {} failed: {}", fwd_id, peer_addr, e);
                    return;
                }
            };
            let originator_ip = peer_addr.ip().to_string();
            let originator_port = peer_addr.port() as u32;
            let channel = match handle.lock().await
                .channel_open_direct_tcpip(target_host.as_str(), target_port as u32, originator_ip.as_str(), originator_port)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("forward[{}]: channel_open_direct_tcpip to {}:{} failed: {}", fwd_id, target_host, target_port, e);
                    return;
                }
            };
            let mut channel_stream = channel.into_stream();
            match tokio::io::copy_bidirectional(&mut tcp_stream, &mut channel_stream).await {
                Ok((to_remote, to_local)) => {
                    debug!("forward[{}]: closed (sent {} bytes, received {} bytes)", fwd_id, to_remote, to_local);
                }
                Err(e) => {
                    debug!("forward[{}]: copy ended: {}", fwd_id, e);
                }
            }
        });
    }

    event_tx.send(TransportEvent::ForwardStateChanged { id, state: ForwardState::Stopped }).await.ok();
}

// ── e2e テスト: ダミー TCP エコーサーバ + 自前 SSH サーバ経由の -L 中継 ──
//
// 実 sshd は使わず、in-process の russh server を「相手ホスト」役として
// 起動する。クライアント(SessionOrchestrator)が `-L bindPort:remoteHost:remotePort`
// で接続すると、サーバー側の `channel_open_direct_tcpip` がダミーエコーサーバへ
// TCP 接続して双方向コピーする(実際の sshd がリモート側で行う処理と同じ)。
#[cfg(test)]
mod local_forward_e2e_tests {
    use super::*;
    use crate::{
        create_session_orchestrator, ConnectionPublicState, ForwardType, OrchestratorCallback,
        PortForward, ScreenUpdate, SshAuth, SshConfig, TrzszPublicState,
    };
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

    #[allow(dead_code)]
    enum TestEvent {
        Connection(ConnectionPublicState),
        Forward(String, ForwardState),
    }

    struct TestCallback {
        tx: UnboundedSender<TestEvent>,
    }

    impl OrchestratorCallback for TestCallback {
        fn on_connection_state_changed(&self, state: ConnectionPublicState) {
            let _ = self.tx.send(TestEvent::Connection(state));
        }
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool { true }
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_trzsz_state_changed(&self, _state: TrzszPublicState) {}
        fn on_download_complete(&self, _file_name: Option<String>, _data: Vec<u8>) {}
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, id: String, state: ForwardState) {
            let _ = self.tx.send(TestEvent::Forward(id, state));
        }
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
        fn on_clipboard_write(&self, _text: String) {}
        fn on_clipboard_pull_request(&self) -> Option<String> { None }
    }

    /// 受け取ったバイト列をそのまま返すだけのダミー TCP サーバ。
    async fn spawn_echo_server() -> SocketAddr {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// "相手ホスト"役の最小 SSH サーバ。パスワード認証は無条件で許可し、
    /// direct-tcpip の open 要求が来たら常にダミーエコーサーバへ接続して中継する
    /// (実運用では sshd がリクエストされた remote_host:remote_port へ繋ぐ処理に相当)。
    #[derive(Clone)]
    struct FakeSshServer { echo_addr: SocketAddr }

    impl server::Server for FakeSshServer {
        type Handler = FakeSshHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> FakeSshHandler {
            FakeSshHandler { echo_addr: self.echo_addr }
        }
    }

    #[derive(Clone)]
    struct FakeSshHandler { echo_addr: SocketAddr }

    #[async_trait::async_trait]
    impl server::Handler for FakeSshHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: RusshChannel<ServerMsg>,
            _host_to_connect: &str,
            _port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            let echo_addr = self.echo_addr;
            tokio::spawn(async move {
                let mut outbound = match TokioTcpStream::connect(echo_addr).await {
                    Ok(s) => s,
                    Err(e) => { warn!("test server: connect to echo failed: {}", e); return; }
                };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut outbound).await;
            });
            Ok(true)
        }

        /// クライアントの `-R`(リモートポートフォワード)要求。実際に `address:port` で
        /// listenし、着信ごとに `forwarded-tcpip` チャネルをクライアントへ開き返す
        /// (実sshdの-R処理を模したテスト用の最小実装)。
        async fn tcpip_forward(
            &mut self,
            address: &str,
            port: &mut u32,
            session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            let bind_addr = format!("{}:{}", address, port);
            let listener = match TokioTcpListener::bind(&bind_addr).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("test server: tcpip_forward bind {} failed: {}", bind_addr, e);
                    return Ok(false);
                }
            };
            let bound_port = listener.local_addr().unwrap().port() as u32;
            *port = bound_port;
            let handle = session.handle();
            let address = address.to_string();
            tokio::spawn(async move {
                loop {
                    let (tcp, peer) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let originator_ip = peer.ip().to_string();
                    let originator_port = peer.port() as u32;
                    let handle = handle.clone();
                    let address = address.clone();
                    tokio::spawn(async move {
                        let mut tcp = tcp;
                        match handle
                            .channel_open_forwarded_tcpip(address.as_str(), bound_port, originator_ip.as_str(), originator_port)
                            .await
                        {
                            Ok(channel) => {
                                let mut stream = channel.into_stream();
                                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                            }
                            Err(e) => {
                                warn!("test server: channel_open_forwarded_tcpip failed: {}", e);
                            }
                        }
                    });
                }
            });
            Ok(true)
        }
    }

    async fn spawn_fake_ssh_server(echo_addr: SocketAddr) -> SocketAddr {
        let keypair = Ed25519Keypair::from_seed(&[7u8; 32]);
        let host_key = PrivateKey::from(keypair);
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut sh = FakeSshServer { echo_addr };
        tokio::spawn(async move {
            use server::Server as _;
            if let Err(e) = sh.run_on_socket(config, &listener).await {
                warn!("test ssh server: run_on_socket exited: {}", e);
            }
        });
        addr
    }

    #[test]
    fn local_forward_relays_bytes_end_to_end() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let echo_addr = spawn_echo_server().await;
            let ssh_addr = spawn_fake_ssh_server(echo_addr).await;

            // OS に空きポートを選ばせてから即座に閉じ、そのポート番号を -L の bind_port に使う。
            let probe = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let bind_port = probe.local_addr().unwrap().port();
            drop(probe);

            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let callback: Box<dyn OrchestratorCallback> = Box::new(TestCallback { tx });
            let orchestrator = create_session_orchestrator(callback);

            let config = SshConfig {
                host: ssh_addr.ip().to_string(),
                port: ssh_addr.port(),
                username: "tester".into(),
                auth: SshAuth::Password { password: "anything".into() },
                cols: 80,
                rows: 24,
                forwards: vec![PortForward {
                    forward_type: ForwardType::Local,
                    bind_address: "127.0.0.1".into(),
                    bind_port,
                    // fake server は host_to_connect を無視して常に echo_addr へ繋ぐので
                    // ここは実在しないホスト名でもよい(本物の sshd ならここへ接続する)。
                    remote_host: "upstream.invalid".into(),
                    remote_port: echo_addr.port(),
                }],
                agent_forward: false,
                jump: None,
                allow_non_loopback_forward_bind: false,
            };

            orchestrator.connect(config).expect("connect() should not fail synchronously");

            let mut listening = false;
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Forward(_, ForwardState::Listening))) => { listening = true; break; }
                    Ok(Some(TestEvent::Forward(_, ForwardState::Failed { reason }))) => {
                        panic!("forward reported Failed before Listening: {}", reason);
                    }
                    _ => continue,
                }
            }
            assert!(listening, "forward did not report Listening within timeout");

            let mut client = tokio::time::timeout(
                Duration::from_secs(5),
                TokioTcpStream::connect(("127.0.0.1", bind_port)),
            ).await.expect("connect to forwarded port timed out")
             .expect("connect to forwarded port failed");

            client.write_all(b"hello-forward").await.unwrap();
            let mut buf = [0u8; 32];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await.expect("read from forwarded port timed out")
                .expect("read from forwarded port failed");
            assert_eq!(&buf[..n], b"hello-forward");

            orchestrator.disconnect();
        });
    }

    #[test]
    fn is_loopback_bind_address_recognizes_loopback_forms() {
        assert!(super::is_loopback_bind_address("127.0.0.1"));
        assert!(super::is_loopback_bind_address("127.5.6.7"));
        assert!(super::is_loopback_bind_address("::1"));
        assert!(super::is_loopback_bind_address("localhost"));
        assert!(super::is_loopback_bind_address("LOCALHOST"));
        assert!(!super::is_loopback_bind_address("0.0.0.0"));
        assert!(!super::is_loopback_bind_address("10.0.0.5"));
        assert!(!super::is_loopback_bind_address("192.168.1.1"));
        assert!(!super::is_loopback_bind_address("not-an-address"));
    }

    #[test]
    fn non_loopback_forward_bind_is_rejected_when_not_allowed() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let echo_addr = spawn_echo_server().await;
            let ssh_addr = spawn_fake_ssh_server(echo_addr).await;

            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let callback: Box<dyn OrchestratorCallback> = Box::new(TestCallback { tx });
            let orchestrator = create_session_orchestrator(callback);

            let config = SshConfig {
                host: ssh_addr.ip().to_string(),
                port: ssh_addr.port(),
                username: "tester".into(),
                auth: SshAuth::Password { password: "anything".into() },
                cols: 80,
                rows: 24,
                forwards: vec![PortForward {
                    forward_type: ForwardType::Local,
                    // 実際にbindを試みる前にコア側で拒否されるはずなので、この
                    // アドレスに実在のNICが無くてもテストは成立する。
                    bind_address: "203.0.113.1".into(),
                    bind_port: 0,
                    remote_host: "upstream.invalid".into(),
                    remote_port: echo_addr.port(),
                }],
                agent_forward: false,
                jump: None,
                allow_non_loopback_forward_bind: false,
            };

            orchestrator.connect(config).expect("connect() should not fail synchronously");

            let mut failed_reason = None;
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Forward(_, ForwardState::Listening))) => {
                        panic!("non-loopback bind should have been rejected, but started listening");
                    }
                    Ok(Some(TestEvent::Forward(_, ForwardState::Failed { reason }))) => {
                        failed_reason = Some(reason);
                        break;
                    }
                    _ => continue,
                }
            }
            let reason = failed_reason.expect("forward did not report Failed within timeout");
            assert!(reason.contains("allow_non_loopback_forward_bind"), "unexpected reason: {reason}");

            orchestrator.disconnect();
        });
    }

    #[test]
    fn remote_forward_relays_bytes_end_to_end() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let echo_addr = spawn_echo_server().await;
            let ssh_addr = spawn_fake_ssh_server(echo_addr).await;

            // OSに空きポートを選ばせてから即座に閉じ、そのポート番号を-Rの bind_port
            // (=サーバー側がlistenするポート)に使う。
            let probe = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let bind_port = probe.local_addr().unwrap().port();
            drop(probe);

            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let callback: Box<dyn OrchestratorCallback> = Box::new(TestCallback { tx });
            let orchestrator = create_session_orchestrator(callback);

            let config = SshConfig {
                host: ssh_addr.ip().to_string(),
                port: ssh_addr.port(),
                username: "tester".into(),
                auth: SshAuth::Password { password: "anything".into() },
                cols: 80,
                rows: 24,
                forwards: vec![PortForward {
                    forward_type: ForwardType::Remote,
                    bind_address: "127.0.0.1".into(),
                    bind_port,
                    // -Rの場合、remote_host/remote_portは「クライアントから見たローカル
                    // ターゲット」を指す(Localとは逆に、実際に接続するのはクライアント側の
                    // このコードなので、Localの-L用テストと違い実在しないホスト名は使えない)。
                    remote_host: echo_addr.ip().to_string(),
                    remote_port: echo_addr.port(),
                }],
                agent_forward: false,
                jump: None,
                allow_non_loopback_forward_bind: false,
            };

            orchestrator.connect(config).expect("connect() should not fail synchronously");

            let mut listening = false;
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Forward(_, ForwardState::Listening))) => { listening = true; break; }
                    Ok(Some(TestEvent::Forward(_, ForwardState::Failed { reason }))) => {
                        panic!("remote forward reported Failed before Listening: {}", reason);
                    }
                    _ => continue,
                }
            }
            assert!(listening, "remote forward did not report Listening within timeout");

            // "インターネット側"から、SSHサーバーがlistenしたポートへ接続する。
            let mut client = tokio::time::timeout(
                Duration::from_secs(5),
                TokioTcpStream::connect(("127.0.0.1", bind_port)),
            ).await.expect("connect to remote-forwarded port timed out")
             .expect("connect to remote-forwarded port failed");

            client.write_all(b"hello-remote-forward").await.unwrap();
            let mut buf = [0u8; 32];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await.expect("read from remote-forwarded port timed out")
                .expect("read from remote-forwarded port failed");
            assert_eq!(&buf[..n], b"hello-remote-forward");

            orchestrator.disconnect();
        });
    }

    #[test]
    fn dynamic_forward_socks5_relays_bytes_end_to_end() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let echo_addr = spawn_echo_server().await;
            let ssh_addr = spawn_fake_ssh_server(echo_addr).await;

            let probe = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let bind_port = probe.local_addr().unwrap().port();
            drop(probe);

            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let callback: Box<dyn OrchestratorCallback> = Box::new(TestCallback { tx });
            let orchestrator = create_session_orchestrator(callback);

            let config = SshConfig {
                host: ssh_addr.ip().to_string(),
                port: ssh_addr.port(),
                username: "tester".into(),
                auth: SshAuth::Password { password: "anything".into() },
                cols: 80,
                rows: 24,
                forwards: vec![PortForward {
                    forward_type: ForwardType::Dynamic,
                    bind_address: "127.0.0.1".into(),
                    bind_port,
                    remote_host: String::new(),
                    remote_port: 0,
                }],
                agent_forward: false,
                jump: None,
                allow_non_loopback_forward_bind: false,
            };

            orchestrator.connect(config).expect("connect() should not fail synchronously");

            let mut listening = false;
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Forward(_, ForwardState::Listening))) => { listening = true; break; }
                    Ok(Some(TestEvent::Forward(_, ForwardState::Failed { reason }))) => {
                        panic!("dynamic forward reported Failed before Listening: {}", reason);
                    }
                    _ => continue,
                }
            }
            assert!(listening, "dynamic forward did not report Listening within timeout");

            let mut client = tokio::time::timeout(
                Duration::from_secs(5),
                TokioTcpStream::connect(("127.0.0.1", bind_port)),
            ).await.expect("connect to SOCKS port timed out")
             .expect("connect to SOCKS port failed");

            // SOCKS5ハンドシェイク: no-auth選択 → fakeサーバーは宛先を無視して常に
            // echoへ繋ぐので宛先自体は何でもよい。
            client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut method_reply = [0u8; 2];
            client.read_exact(&mut method_reply).await.unwrap();
            assert_eq!(method_reply, [0x05, 0x00]);

            client.write_all(&[0x05, 0x01, 0x00, 0x01, 93, 184, 216, 34, 0x00, 0x50]).await.unwrap();
            let mut connect_reply = [0u8; 10];
            client.read_exact(&mut connect_reply).await.unwrap();
            assert_eq!(connect_reply[1], 0x00, "SOCKS CONNECT should succeed");

            client.write_all(b"hello-dynamic-forward").await.unwrap();
            let mut buf = [0u8; 32];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await.expect("read from SOCKS relay timed out")
                .expect("read from SOCKS relay failed");
            assert_eq!(&buf[..n], b"hello-dynamic-forward");

            orchestrator.disconnect();
        });
    }
}

#[cfg(test)]
mod proxy_jump_e2e_tests {
    use super::*;
    use crate::JumpConfig;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use std::net::SocketAddr;
    use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};

    /// 対象ホスト役の最小 SSH サーバ。パスワード認証は無条件で許可し、
    /// セッションチャネルの open だけ受け付ける(シェル/PTYまでは要らない —
    /// ここではネストしたSSHハンドシェイクとチャネル開設ができることだけを検証する)。
    #[derive(Clone)]
    struct TargetSshServer;

    impl server::Server for TargetSshServer {
        type Handler = TargetSshHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> TargetSshHandler {
            TargetSshHandler
        }
    }

    #[derive(Clone)]
    struct TargetSshHandler;

    #[async_trait::async_trait]
    impl server::Handler for TargetSshHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    /// 踏み台ホスト役の最小 SSH サーバ。パスワード認証は無条件で許可し、
    /// `channel_open_direct_tcpip` が要求してきた `host_to_connect:port_to_connect`
    /// へ実際にTCP接続してバイトを中継する(本物のsshdの`-J`/ProxyJump時の挙動と同じ)。
    #[derive(Clone)]
    struct JumpSshServer;

    impl server::Server for JumpSshServer {
        type Handler = JumpSshHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> JumpSshHandler {
            JumpSshHandler
        }
    }

    #[derive(Clone)]
    struct JumpSshHandler;

    #[async_trait::async_trait]
    impl server::Handler for JumpSshHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: RusshChannel<ServerMsg>,
            host_to_connect: &str,
            port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            let target = format!("{host_to_connect}:{port_to_connect}");
            tokio::spawn(async move {
                let mut outbound = match TokioTcpStream::connect(&target).await {
                    Ok(s) => s,
                    Err(e) => { warn!("test jump server: connect to {} failed: {}", target, e); return; }
                };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut outbound).await;
            });
            Ok(true)
        }
    }

    async fn spawn_ssh_server<S: server::Server<Handler = H> + Send + 'static, H>(
        mut server: S,
        seed: u8,
    ) -> SocketAddr
    where
        H: server::Handler + Send + 'static,
    {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let host_key = PrivateKey::from(keypair);
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Err(e) = server.run_on_socket(config, &listener).await {
                warn!("test ssh server: run_on_socket exited: {}", e);
            }
        });
        addr
    }

    #[test]
    fn connect_via_jump_reaches_target_through_tunneled_ssh_session() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let target_addr = spawn_ssh_server(TargetSshServer, 11).await;
            let jump_addr = spawn_ssh_server(JumpSshServer, 22).await;

            let jump = JumpConfig {
                host: jump_addr.ip().to_string(),
                port: jump_addr.port(),
                username: "jumper".into(),
                auth: SshAuth::Password { password: "anything".into() },
            };

            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
            // check_server_key はホスト鍵の信頼確認を待つので、テストでは常に許可する。
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if let TransportEvent::HostKey(_, reply) = event {
                        let _ = reply.send(true);
                    }
                }
            });

            let russh_config = Arc::new(client::Config::default());
            let mut established = connect_via_jump_or_direct(
                &Some(jump),
                russh_config,
                &target_addr.ip().to_string(),
                target_addr.port(),
                event_tx,
            )
            .await
            .expect("connect_via_jump_or_direct should succeed");

            let target_auth = SshAuth::Password { password: "anything".into() };
            let (authenticated, _) =
                authenticate_session(&mut established.handle, "tester", &target_auth).await;
            assert!(authenticated, "authentication over the jump-tunneled session should succeed");

            // The tunneled session should behave like an ordinary SSH connection
            // beyond just authenticating: confirm we can actually open a channel
            // on the target through it.
            established
                .handle
                .channel_open_session()
                .await
                .expect("opening a channel on the target through the jump tunnel should succeed");
        });
    }
}

// ── e2e テスト: SSH接続プーリング(タスク#3/#4) ──────────────
//
// 認証済みの`client::Handle`を複数タブが共有し、2本目以降は`channel_open_session()`だけで
// 済むこと(サーバー側が観測する認証回数で検証する)、および1タブのチャネルが切断されても
// 他タブのチャネルに影響しないことを、in-processのrusshサーバーで検証する。
#[cfg(test)]
mod pooling_e2e_tests {
    use super::*;
    use crate::{
        create_session_orchestrator, ConnectionPublicState, OrchestratorCallback, ScreenUpdate,
        SshAuth, SshConfig, TrzszPublicState,
    };
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::{Channel as RusshChannel, ChannelId, CryptoVec, Pty};
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use crate::faulty_stream::{FaultInjector, FaultyStream};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    #[allow(dead_code)]
    enum TestEvent {
        Connection(ConnectionPublicState),
        Data(Vec<u8>),
    }

    struct TestCallback {
        tx: UnboundedSender<TestEvent>,
    }

    impl OrchestratorCallback for TestCallback {
        fn on_connection_state_changed(&self, state: ConnectionPublicState) {
            let _ = self.tx.send(TestEvent::Connection(state));
        }
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool { true }
        fn on_data(&self, data: Vec<u8>) {
            let _ = self.tx.send(TestEvent::Data(data));
        }
        fn on_trzsz_state_changed(&self, _state: TrzszPublicState) {}
        fn on_download_complete(&self, _file_name: Option<String>, _data: Vec<u8>) {}
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
        fn on_clipboard_write(&self, _text: String) {}
        fn on_clipboard_pull_request(&self) -> Option<String> { None }
    }

    /// 公開鍵認証を無条件で受け入れつつ認証回数を数え、シェルチャネルへ書き込まれた
    /// バイト列をそのままechoし返す最小SSHサーバ。プーリングが効いていれば
    /// 複数タブ(=複数`channel_open_session()`)でも`auth_count`は1のまま増えない
    /// (プールにヒットしなければ、タブごとに新規TCP接続・新規認証が走り増える)。
    #[derive(Clone)]
    struct CountingEchoServer {
        auth_count: Arc<AtomicUsize>,
    }

    impl server::Server for CountingEchoServer {
        type Handler = CountingEchoHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CountingEchoHandler {
            CountingEchoHandler { auth_count: self.auth_count.clone() }
        }
    }

    #[derive(Clone)]
    struct CountingEchoHandler {
        auth_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl server::Handler for CountingEchoHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self, _user: &str, _public_key: &russh_keys::ssh_key::PublicKey,
        ) -> Result<Auth, Self::Error> {
            self.auth_count.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn pty_request(
            &mut self, channel: ChannelId, _term: &str, _cols: u32, _rows: u32,
            _pix_width: u32, _pix_height: u32, _modes: &[(Pty, u32)], session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self, channel: ChannelId, session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn data(
            &mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            // タスク#4: 「リモートシェルプロセスがexitする」を模す特殊トリガー。
            // このチャネルだけexit-status通知+closeし、他のチャネル(=他タブ、
            // 同じclient::Handleを共有している場合)には一切触れない。
            if data == b"__test_exit__" {
                session.exit_status_request(channel, 0)?;
                session.close(channel)?;
                return Ok(());
            }
            session.data(channel, CryptoVec::from(data.to_vec()))?;
            Ok(())
        }
    }

    async fn spawn_counting_echo_server(auth_count: Arc<AtomicUsize>) -> SocketAddr {
        let keypair = Ed25519Keypair::from_seed(&[42u8; 32]);
        let host_key = russh_keys::PrivateKey::from(keypair);
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut sh = CountingEchoServer { auth_count };
        tokio::spawn(async move {
            use server::Server as _;
            if let Err(e) = sh.run_on_socket(config, &listener).await {
                warn!("test ssh server: run_on_socket exited: {}", e);
            }
        });
        addr
    }

    fn key_auth(seed: u8) -> SshAuth {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let key = russh_keys::PrivateKey::from(keypair);
        SshAuth::PublicKey {
            private_key_pem: key.to_openssh(Default::default()).unwrap().as_bytes().to_vec(),
        }
    }

    fn ssh_config(host: SocketAddr, auth: SshAuth) -> SshConfig {
        SshConfig {
            host: host.ip().to_string(),
            port: host.port(),
            username: "tester".into(),
            auth,
            cols: 80,
            rows: 24,
            forwards: Vec::new(),
            agent_forward: false,
            jump: None,
            allow_non_loopback_forward_bind: false,
        }
    }

    async fn wait_connected(rx: &mut UnboundedReceiver<TestEvent>) {
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(TestEvent::Connection(ConnectionPublicState::Connected { .. }))) => return,
                Ok(Some(TestEvent::Connection(ConnectionPublicState::Error { message }))) => {
                    panic!("connection reported Error before Connected: {message}");
                }
                Ok(Some(TestEvent::Connection(ConnectionPublicState::Disconnected { reason }))) => {
                    panic!("connection reported Disconnected before Connected: {reason:?}");
                }
                _ => continue,
            }
        }
        panic!("did not become Connected within timeout");
    }

    async fn wait_echo(rx: &mut UnboundedReceiver<TestEvent>, expected: &[u8]) {
        let mut got = Vec::new();
        for _ in 0..50 {
            if got.windows(expected.len().max(1)).any(|w| w == expected) {
                return;
            }
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(TestEvent::Data(data))) => got.extend_from_slice(&data),
                _ => continue,
            }
        }
        panic!("did not observe expected echo {:?} within timeout, got {:?}", expected, got);
    }

    #[test]
    fn two_tabs_to_same_key_share_one_authenticated_connection() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(1);

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            orch_a.connect(ssh_config(addr, auth.clone())).expect("tab A connect should not fail synchronously");
            wait_connected(&mut rx_a).await;

            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));
            orch_b.connect(ssh_config(addr, auth)).expect("tab B connect should not fail synchronously");
            wait_connected(&mut rx_b).await;

            assert_eq!(
                auth_count.load(Ordering::SeqCst), 1,
                "second tab should reuse the pooled connection instead of authenticating again"
            );

            // 両方のチャネルが独立に動作することを確認する。
            orch_a.send(b"hello-a".to_vec());
            wait_echo(&mut rx_a, b"hello-a").await;
            orch_b.send(b"hello-b".to_vec());
            wait_echo(&mut rx_b, b"hello-b").await;

            // タブAを切断してもタブBのチャネルは影響を受けない(共有Handleは生き続ける)。
            orch_a.disconnect();
            orch_b.send(b"still-alive".to_vec());
            wait_echo(&mut rx_b, b"still-alive").await;

            orch_b.disconnect();
        });
    }

    #[test]
    fn two_tabs_with_different_keys_do_not_share_a_connection() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            orch_a.connect(ssh_config(addr, key_auth(1))).expect("tab A connect should not fail synchronously");
            wait_connected(&mut rx_a).await;

            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));
            orch_b.connect(ssh_config(addr, key_auth(2))).expect("tab B connect should not fail synchronously");
            wait_connected(&mut rx_b).await;

            assert_eq!(
                auth_count.load(Ordering::SeqCst), 2,
                "different keys to the same host must not share a pooled connection"
            );

            orch_a.disconnect();
            orch_b.disconnect();
        });
    }

    #[test]
    fn three_tabs_share_one_connection_and_survive_partial_disconnects() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(80);

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            orch_a.connect(ssh_config(addr, auth.clone())).expect("tab A connect should not fail synchronously");
            wait_connected(&mut rx_a).await;

            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));
            orch_b.connect(ssh_config(addr, auth.clone())).expect("tab B connect should not fail synchronously");
            wait_connected(&mut rx_b).await;

            let (tx_c, mut rx_c) = unbounded_channel::<TestEvent>();
            let orch_c = create_session_orchestrator(Box::new(TestCallback { tx: tx_c }));
            orch_c.connect(ssh_config(addr, auth)).expect("tab C connect should not fail synchronously");
            wait_connected(&mut rx_c).await;

            assert_eq!(
                auth_count.load(Ordering::SeqCst), 1,
                "three tabs to the same key should share a single authenticated connection"
            );

            orch_a.send(b"a".to_vec());
            wait_echo(&mut rx_a, b"a").await;
            orch_b.send(b"b".to_vec());
            wait_echo(&mut rx_b, b"b").await;
            orch_c.send(b"c".to_vec());
            wait_echo(&mut rx_c, b"c").await;

            // タブAを切断してもB・Cのチャネルは無事(共有Handleは refcount=2 でまだ生きている)。
            orch_a.disconnect();
            orch_b.send(b"b-after-a-gone".to_vec());
            wait_echo(&mut rx_b, b"b-after-a-gone").await;
            orch_c.send(b"c-after-a-gone".to_vec());
            wait_echo(&mut rx_c, b"c-after-a-gone").await;

            // 続けてタブBも切断してもCのチャネルはまだ無事(refcount=1)。
            orch_b.disconnect();
            orch_c.send(b"c-after-b-gone".to_vec());
            wait_echo(&mut rx_c, b"c-after-b-gone").await;

            orch_c.disconnect();
        });
    }

    #[test]
    fn concurrent_connects_to_same_key_only_authenticate_once() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(90);

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));

            // どちらも完了を待たずに立て続けにconnect()する。プール側の「確立中」状態
            // (Connecting/Waiter)を、synthetic な型ではなく実際の非同期I/Oのタイミングで踏む。
            orch_a.connect(ssh_config(addr, auth.clone())).expect("tab A connect should not fail synchronously");
            orch_b.connect(ssh_config(addr, auth)).expect("tab B connect should not fail synchronously");

            wait_connected(&mut rx_a).await;
            wait_connected(&mut rx_b).await;

            assert_eq!(
                auth_count.load(Ordering::SeqCst), 1,
                "connecting two tabs back-to-back without waiting must not race into two separate authentications"
            );

            orch_a.disconnect();
            orch_b.disconnect();
        });
    }

    #[test]
    fn different_agent_forward_settings_do_not_share_a_pooled_connection() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(100);

            let mut config_a = ssh_config(addr, auth.clone());
            config_a.agent_forward = false;
            let mut config_b = ssh_config(addr, auth);
            config_b.agent_forward = true;

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            orch_a.connect(config_a).expect("tab A connect should not fail synchronously");
            wait_connected(&mut rx_a).await;

            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));
            orch_b.connect(config_b).expect("tab B connect should not fail synchronously");
            wait_connected(&mut rx_b).await;

            assert_eq!(
                auth_count.load(Ordering::SeqCst), 2,
                "differing agent_forward settings must not share the same pooled Handle \
                 (agent_key is set once per Handle, not per channel)"
            );

            orch_a.disconnect();
            orch_b.disconnect();
        });
    }

    #[test]
    fn pooled_connection_is_reestablished_after_idle_grace_elapses() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(110);
            let key = crate::pool::SshPoolKey::for_target(
                &addr.ip().to_string(), addr.port(), "tester", &auth, false, &None,
            ).expect("pubkey auth should produce a pool key");

            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if let TransportEvent::HostKey(_, reply) = event {
                        let _ = reply.send(true);
                    }
                }
            });

            // 1本目: 確立してプールへ登録する(本番の`run_russh_transport`が行うのと同じ手順)。
            let mut auth1 = auth.clone();
            match crate::pool::try_attach(&crate::pool::SSH_POOL, &key) {
                crate::pool::AttachOutcome::Establisher => {
                    let pooled = establish_ssh_handle(
                        &None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
                        "tester", &mut auth1, false, &event_tx,
                    ).await.expect("establish should succeed");
                    crate::pool::publish_success(&crate::pool::SSH_POOL, &key, pooled);
                }
                _ => panic!("a brand new key must be the Establisher"),
            }
            assert_eq!(auth_count.load(Ordering::SeqCst), 1);

            // 短い猶予(本番は30秒だが、テストでは待てないので直接短い値でreleaseする)で
            // 参照を手放し、猶予経過後にプールエントリが消えることを確認する。
            crate::pool::release(&crate::pool::SSH_POOL, key.clone(), Duration::from_millis(30));
            let mut removed = false;
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if !crate::pool::SSH_POOL.lock().contains_key(&key) {
                    removed = true;
                    break;
                }
            }
            assert!(removed, "pool entry should be removed once the idle grace elapses");

            // 次のアタッチはEstablisherに戻り、サーバーは2回目の認証を観測する。
            let mut auth2 = auth;
            match crate::pool::try_attach(&crate::pool::SSH_POOL, &key) {
                crate::pool::AttachOutcome::Establisher => {
                    let pooled = establish_ssh_handle(
                        &None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
                        "tester", &mut auth2, false, &event_tx,
                    ).await.expect("re-establish should succeed");
                    crate::pool::publish_success(&crate::pool::SSH_POOL, &key, pooled);
                }
                _ => panic!("after expiry, the next tab must become the Establisher again"),
            }
            assert_eq!(
                auth_count.load(Ordering::SeqCst), 2,
                "a new connection must re-authenticate once the previously pooled one has expired"
            );

            // 後始末: このテストが共有staticの`SSH_POOL`に残留エントリを残さないようにする。
            crate::pool::release(&crate::pool::SSH_POOL, key.clone(), Duration::from_millis(10));
            for _ in 0..50 {
                if !crate::pool::SSH_POOL.lock().contains_key(&key) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    // ── タスク#4: 共有SSH接続における障害分離 ──────────────────

    /// 単発のチャネルを共有Handle上に開き、`Connected`を受け取ってから
    /// `(コマンド送信端, イベント受信端)`を返す。オーケストレータ/`SessionCore`を
    /// 経由せず`run_ssh_channel_loop`を直接叩くことで、プールされたHandleを
    /// 複数「タブ」で共有する状況を最小構成で再現する。
    async fn spawn_pooled_tab(
        pooled: Arc<PooledSshHandle>,
    ) -> (tokio::sync::mpsc::Sender<TransportCommand>, tokio::sync::mpsc::Receiver<TransportEvent>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<TransportCommand>(16);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<TransportEvent>(16);
        tokio::spawn(async move {
            run_ssh_channel_loop(&pooled, 80, 24, false, false, cmd_rx, event_tx).await;
        });
        match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
            Ok(Some(TransportEvent::Connected)) => {}
            Ok(Some(_)) => panic!("expected Connected as the first event"),
            Ok(None) => panic!("channel loop exited before reporting Connected"),
            Err(_) => panic!("timed out waiting for Connected"),
        }
        (cmd_tx, event_rx)
    }

    async fn expect_disconnected(rx: &mut tokio::sync::mpsc::Receiver<TransportEvent>, context: &str) {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(TransportEvent::Disconnected { .. })) => {}
            Ok(Some(_)) => panic!("{context}: expected Disconnected, got a different event"),
            Ok(None) => {} // チャネル終了(送信端drop)も「切断された」の一種として許容する。
            Err(_) => panic!("{context}: timed out waiting for Disconnected"),
        }
    }

    /// 基盤の接続そのものが失われた場合の"fate sharing": プールされた1本の
    /// `client::Handle`を共有する全タブが、他タブの個別事情(チャネル終了等)とは
    /// 違って一斉に`Disconnected`になるべきことを検証する。生TCP接続を
    /// `FaultyStream`(元々TCP/QUIC両対応で作られたテスト用故障注入ラッパー、
    /// 従来multipath/QUIC系のテストでのみ使われていた)で包み、`inject.cut()`で
    /// 基盤接続を強制的に切断する。
    #[test]
    fn underlying_connection_loss_disconnects_all_sharing_tabs() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(120);
            let key = crate::pool::SshPoolKey::for_target(
                &addr.ip().to_string(), addr.port(), "tester", &auth, false, &None,
            ).expect("pubkey auth should produce a pool key");

            let (hostkey_tx, mut hostkey_rx) = tokio::sync::mpsc::channel::<TransportEvent>(16);
            tokio::spawn(async move {
                while let Some(event) = hostkey_rx.recv().await {
                    if let TransportEvent::HostKey(_, reply) = event {
                        let _ = reply.send(true);
                    }
                }
            });

            // 生TCPをFaultyStreamで包んでからHandleを確立する。QUICネスト用の
            // `establish_ssh_handle_over_stream`は任意のAsyncRead+AsyncWriteを
            // 受け付けるので、「故障注入可能なプレーンSSH接続」としてそのまま使える。
            let tcp = TokioTcpStream::connect(addr).await.expect("tcp connect should succeed");
            let injector = FaultInjector::new();
            let faulty = FaultyStream::new(tcp, injector.clone());
            let mut auth1 = auth;
            let pooled = establish_ssh_handle_over_stream(
                Arc::new(client::Config::default()), faulty, "tester", &mut auth1, false, &hostkey_tx,
            ).await.expect("establish over the faulty-wrapped TCP stream should succeed");
            let pooled = crate::pool::publish_success(&crate::pool::SSH_POOL, &key, pooled);
            assert_eq!(auth_count.load(Ordering::SeqCst), 1);

            // 3タブがこの1本のHandleを共有する。
            let (_cmd_a, mut rx_a) = spawn_pooled_tab(pooled.clone()).await;
            let (_cmd_b, mut rx_b) = spawn_pooled_tab(pooled.clone()).await;
            let (_cmd_c, mut rx_c) = spawn_pooled_tab(pooled.clone()).await;

            // 基盤接続そのものを切断する(TCP RST相当)。個別チャネルの問題ではなく
            // 接続そのものの喪失なので、共有中の全タブに伝播する"べき"。
            injector.cut();

            expect_disconnected(&mut rx_a, "tab A").await;
            expect_disconnected(&mut rx_b, "tab B").await;
            expect_disconnected(&mut rx_c, "tab C").await;

            crate::pool::release(&crate::pool::SSH_POOL, key.clone(), Duration::from_millis(10));
        });
    }

    /// 個別チャネルの終了(リモートシェルプロセスの`exit`等)は、他タブに伝播
    /// "してはいけない"ことを検証する。`underlying_connection_loss_...`とは
    /// 対になるテストで、「伝播すべきもの」と「伝播してはいけないもの」の境界を
    /// 両方とも実際のI/Oで確認する。
    #[test]
    fn one_tab_remote_exit_does_not_disconnect_sibling_tabs() {
        crate::init_logger();
        let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
        rt.block_on(async {
            let auth_count = Arc::new(AtomicUsize::new(0));
            let addr = spawn_counting_echo_server(auth_count.clone()).await;
            let auth = key_auth(130);

            let (tx_a, mut rx_a) = unbounded_channel::<TestEvent>();
            let orch_a = create_session_orchestrator(Box::new(TestCallback { tx: tx_a }));
            orch_a.connect(ssh_config(addr, auth.clone())).expect("tab A connect should not fail synchronously");
            wait_connected(&mut rx_a).await;

            let (tx_b, mut rx_b) = unbounded_channel::<TestEvent>();
            let orch_b = create_session_orchestrator(Box::new(TestCallback { tx: tx_b }));
            orch_b.connect(ssh_config(addr, auth)).expect("tab B connect should not fail synchronously");
            wait_connected(&mut rx_b).await;

            assert_eq!(auth_count.load(Ordering::SeqCst), 1, "both tabs should share one pooled connection");

            // タブAのリモート側だけ"exit"させる(サーバー側がそのチャネルだけexit-status
            // 通知+closeする、基盤のTCP接続やタブBのチャネルには一切触れない)。
            orch_a.send(b"__test_exit__".to_vec());
            let mut tab_a_disconnected = false;
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx_a.recv()).await {
                    Ok(Some(TestEvent::Connection(ConnectionPublicState::Disconnected { .. }))) => {
                        tab_a_disconnected = true;
                        break;
                    }
                    _ => continue,
                }
            }
            assert!(tab_a_disconnected, "tab A should observe Disconnected after its remote channel exits");

            // タブBは無事: 共有Handle自体は生きているので送受信できる。
            orch_b.send(b"still-here".to_vec());
            wait_echo(&mut rx_b, b"still-here").await;

            orch_b.disconnect();
        });
    }
}
