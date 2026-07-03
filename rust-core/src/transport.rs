use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};
use parking_lot::Mutex;
use russh::{client, ChannelMsg};
use russh_keys::{HashAlg, PrivateKey, PublicKey};
use tokio::net::TcpListener;

use crate::agent_forward;
use crate::{ForwardState, SshAuth};

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
}

// ── russh Handler ────────────────────────────────────────

pub(crate) struct RusshEventHandler {
    pub(crate) event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
    /// SSH agent forwarding が有効かつ公開鍵認証成功後にのみ `Some` になる、
    /// 転送する秘密鍵（認証に使ったのと同じ鍵を共有する。鍵の追加受け渡しは不要）。
    /// `run_ssh_channel_loop` が認証成功後にセットするため `Mutex` 越しに共有する。
    pub(crate) agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
}

impl RusshEventHandler {
    /// agent forwarding を使わない transport（QUIC 等）向けの簡易コンストラクタ。
    pub(crate) fn new(event_tx: tokio::sync::mpsc::Sender<TransportEvent>) -> Self {
        RusshEventHandler { event_tx, agent_key: Arc::new(Mutex::new(None)) }
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
}

// ── SSH チャネルループ（TCP・QUIC 共通）─────────────────

pub(crate) async fn run_ssh_channel_loop(
    username: &str,
    auth: &SshAuth,
    cols: u32,
    rows: u32,
    agent_forward: bool,
    agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    mut session: client::Handle<RusshEventHandler>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let auth_method = match auth {
        SshAuth::Password { .. } => "password",
        SshAuth::PublicKey { .. } => "pubkey",
    };
    info!("ssh: auth {} for {}", auth_method, username);

    // 公開鍵認証に使った鍵を保持しておく。agent forwarding が有効なら、この同じ鍵を
    // 転送先の署名要求にも使う（鍵の追加受け渡しは不要という設計）。
    let mut authed_key: Option<Arc<PrivateKey>> = None;

    let authenticated = match auth {
        SshAuth::Password { password } => session
            .authenticate_password(username, password)
            .await
            .ok()
            .unwrap_or(false),
        SshAuth::PublicKey { private_key_pem } => {
            match PrivateKey::from_openssh(private_key_pem) {
                Ok(key) => {
                    let key = Arc::new(key);
                    let ok = session
                        .authenticate_publickey(username, key.clone())
                        .await
                        .ok()
                        .unwrap_or(false);
                    if ok {
                        authed_key = Some(key);
                    }
                    ok
                }
                Err(e) => {
                    warn!("ssh: private key parse failed: {}", e);
                    false
                }
            }
        }
    };

    if !authenticated {
        warn!("ssh: auth {} failed for {}", auth_method, username);
        event_tx.send(TransportEvent::Disconnected { reason: Some("Authentication failed".into()) }).await.ok();
        return;
    }
    info!("ssh: auth ok");

    if agent_forward {
        if let Some(key) = &authed_key {
            *agent_key.lock() = Some(key.clone());
        } else {
            debug!("ssh: agent_forward requested but auth method is not publickey — ignoring");
        }
    }

    let mut channel = match session.channel_open_session().await {
        Ok(c) => { info!("ssh: session channel opened"); c }
        Err(e) => {
            warn!("ssh: channel_open_session failed: {}", e);
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    if agent_forward && authed_key.is_some() {
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

    // シェル用チャネルの確立以降、`session`(Handle)自体はもう `&mut` operations
    // (認証等)には使わない。ポートフォワードは `channel_open_direct_tcpip(&self, ...)`
    // だけを要求するので、Arc で包んで待受タスクに共有する(Handle は Clone 不可だが
    // 内部の russh mpsc sender はこの経路で複数タスクから安全に共有できる)。
    let session = Arc::new(session);
    let mut forward_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

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
                        info!("forward[{}]: add {}:{} -> {}:{}", id, bind_addr, bind_port, remote_host, remote_port);
                        let task = tokio::spawn(run_local_forward(
                            id.clone(), bind_addr, bind_port, remote_host, remote_port,
                            session.clone(), event_tx.clone(),
                        ));
                        if let Some(old) = forward_tasks.insert(id, task) {
                            old.abort();
                        }
                    }
                    Some(TransportCommand::RemoveForward { id }) => {
                        info!("forward[{}]: remove requested", id);
                        if let Some(task) = forward_tasks.remove(&id) {
                            task.abort();
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

    for (id, task) in forward_tasks.drain() {
        debug!("forward[{}]: aborting on session teardown", id);
        task.abort();
    }
    info!("ssh: I/O loop exited");
}

// ── ローカルポートフォワード(-L) ──────────────────────────

/// `bind_addr:bind_port` で待受し、accept ごとに `channel_open_direct_tcpip` で
/// リモート `remote_host:remote_port` への SSH チャネルを開き、TCP ソケットと
/// 双方向にバイトを中継する。待受確立/失敗を `ForwardStateChanged` で通知する。
async fn run_local_forward(
    id: String,
    bind_addr: String,
    bind_port: u16,
    remote_host: String,
    remote_port: u16,
    handle: Arc<client::Handle<RusshEventHandler>>,
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
            let channel = match handle
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
}
