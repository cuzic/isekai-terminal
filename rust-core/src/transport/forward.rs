//! ポートフォワード共通(-L/-R/-D)。[`super::ssh_handler::run_ssh_channel_loop`]の
//! `TransportCommand::AddLocalForward`/`AddRemoteForward`/`AddDynamicForward`ハンドラ
//! から呼ばれる、待受・SOCKSネゴシエーション・後始末の実体。`run_local_forward`と
//! `run_dynamic_forward`はaccept→channel_open→copyの骨格は似ているが、SOCKS
//! negotiationの有無で失敗通知・ログ文脈が異なるため、共通ヘルパー化はしていない
//! (Codexレビュー、rust-core/transport.rs分割タスク: 最初から共通化すると動作差分を
//! 生みやすい。共通化は`ForwardState`の挙動を保つテストを揃えた上での第二段階とする)。

use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};
use parking_lot::Mutex;
use russh::client;
use tokio::net::TcpListener;

use crate::ForwardState;

use super::ssh_handler::{RusshEventHandler, TransportEvent};

/// `addr` がループバック（127.0.0.0/8・::1）または文字列 "localhost"（大小無視）かどうか。
/// `allow_non_loopback_forward_bind == false` の場合の bind 許可判定に使う。
pub(super) fn is_loopback_bind_address(addr: &str) -> bool {
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }
    addr.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// 非ループバックbindを`ForwardState::Failed`として拒否する共通処理
/// (-L/-R/-Dいずれも`allow_non_loopback_forward_bind == false`なら同じ判定)。
pub(super) async fn reject_non_loopback_bind(
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
pub(super) enum ActiveForward {
    /// Local(-L)/Dynamic(-D): クライアント側の待受タスク。除去時はabortする。
    Task(tokio::task::JoinHandle<()>),
    /// Remote(-R): サーバー側に`tcpip_forward`で登録した内容。除去時は
    /// `cancel_tcpip_forward`をサーバーへ送り、`remote_forwards`経路表からも消す。
    Remote { bind_addr: String, bound_port: u16 },
}

/// [ActiveForward] 1件を後始末する(除去時・同一id上書き時・セッション終了時の3箇所で共通)。
pub(super) fn teardown_forward(
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
pub(super) async fn run_local_forward(
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
pub(super) async fn run_dynamic_forward(
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
    use russh_keys::PrivateKey;
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
        fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
        fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
        fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> { None }
        fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> { None }
        fn on_rebind_state_changed(&self, _state: crate::rebind_manager::RebindPublicState) {}
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
