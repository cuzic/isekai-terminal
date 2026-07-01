use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use quinn::crypto::rustls::QuicServerConfig;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

// isekai-helper: 認証付き QUIC↔TCP リレー。
// 契約の詳細は /HELPER_PROTOCOL.md を参照。このファイルはその契約の実装。

type HmacSha256 = Hmac<Sha256>;

const EXPORTER_LABEL: &[u8] = b"isekai-helper-auth-v1";
const ALPN: &[u8] = b"isekai-helper/1";

const FRAME_HELLO: u8 = 0x01;
const FRAME_ACK: u8 = 0x02;
const FRAME_REJECT_TARGET: u8 = 0xFC;
const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
const FRAME_REJECT_AUTH: u8 = 0xFF;

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

struct Args {
    target: SocketAddr,
    bind: SocketAddr,
    idle_timeout: u64,
    max_idle_lifetime: u64,
    once: bool,
    log_level: String,
}

fn next_val(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    iter.next().ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn print_help() {
    println!("isekai-helper - authenticated QUIC-to-TCP relay (see HELPER_PROTOCOL.md)");
    println!();
    println!("USAGE:");
    println!("    isekai-helper [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --target <ADDR:PORT>          relay destination (default: 127.0.0.1:22)");
    println!("    --bind <ADDR:PORT>             QUIC bind address (default: 0.0.0.0:0)");
    println!("    --idle-timeout <SECS>          QUIC transport idle timeout (default: 30)");
    println!("    --max-idle-lifetime <SECS>     self-exit after this many seconds with no active connection (default: 600)");
    println!("    --once                         exit after the first connection closes");
    println!("    --log-level <LEVEL>            error|warn|info|debug|trace (default: info)");
    println!("    --version                      print version and exit");
    println!("    -h, --help                     print this help message");
}

fn parse_args() -> Result<Args> {
    let mut target: SocketAddr = "127.0.0.1:22".parse().unwrap();
    let mut bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let mut idle_timeout = 30u64;
    let mut max_idle_lifetime = 600u64;
    let mut once = false;
    let mut log_level = "info".to_string();

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--target" => {
                target = next_val(&mut iter, "--target")?
                    .parse()
                    .context("invalid --target value")?;
            }
            "--bind" => {
                bind = next_val(&mut iter, "--bind")?
                    .parse()
                    .context("invalid --bind value")?;
            }
            "--idle-timeout" => {
                idle_timeout = next_val(&mut iter, "--idle-timeout")?
                    .parse()
                    .context("invalid --idle-timeout value")?;
            }
            "--max-idle-lifetime" => {
                max_idle_lifetime = next_val(&mut iter, "--max-idle-lifetime")?
                    .parse()
                    .context("invalid --max-idle-lifetime value")?;
            }
            "--once" => once = true,
            "--log-level" => log_level = next_val(&mut iter, "--log-level")?,
            "--version" => {
                println!("isekai-helper {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        target,
        bind,
        idle_timeout,
        max_idle_lifetime,
        once,
        log_level,
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(64); // EX_USAGE
        }
    };

    // ログは stderr にのみ出力する。stdout はハンドシェイク JSON 専用。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .target(env_logger::Target::Stderr)
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;

    // session_secret をランダム生成する（CLI 引数や環境変数には載せない）。
    let mut session_secret = [0u8; 32];
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut session_secret);
    }

    // 起動のたびに ephemeral な自己署名証明書を生成する（永続化しない）。
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["isekai-helper.local".to_string()])?;
    let cert_der = cert.der().clone();
    let cert_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&cert_der);
        hex_lower(&hasher.finalize())
    };
    let key_der = key_pair.serialize_der();

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| anyhow!("failed to build private key: {e}"))?;

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT / early data はクライアント・サーバー双方で無効化する契約（HELPER_PROTOCOL.md 参照）。
    // rustls は max_early_data_size を明示的に増やさない限り 0-RTT を送出しないが、契約として明示する。
    server_crypto.max_early_data_size = 0;

    let idle_timeout_cfg = quinn::IdleTimeout::try_from(Duration::from_secs(args.idle_timeout))
        .map_err(|_| anyhow!("invalid --idle-timeout"))?;
    let keep_alive = Duration::from_secs((args.idle_timeout / 3).max(1));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(idle_timeout_cfg));
    transport.keep_alive_interval(Some(keep_alive));
    // Phase 7 のスコープでは 1 QUIC connection につき 1 bidirectional stream のみを許可する。
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    transport.datagram_receive_buffer_size(None);

    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
    server_config.transport_config(Arc::new(transport));
    // preferred_address は明示的に設定しない（QUIC-Exfil 対策、既定で未使用）。

    let endpoint = quinn::Endpoint::server(server_config, args.bind)?;
    let listen_port = endpoint.local_addr()?.port();

    // 起動ハンドシェイク JSON を stdout に1行だけ出力し、明示的に flush する。
    let handshake = serde_json::json!({
        "v": 1,
        "listen_port": listen_port,
        "cert_sha256": cert_sha256,
        "session_secret": base64::engine::general_purpose::STANDARD.encode(session_secret),
    });
    {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{handshake}").context("failed to write handshake to stdout")?;
        stdout.flush().context("failed to flush stdout handshake")?;
    }

    log::info!(
        "isekai-helper listening on udp/{} (target={}, cert_sha256={})",
        endpoint.local_addr()?,
        args.target,
        cert_sha256
    );

    let active = Arc::new(AtomicBool::new(false));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let idle_shutdown = Arc::new(Notify::new());

    // --max-idle-lifetime の監視タスク。
    // アクティブな接続が無く、かつ最後の接続終了（または起動）からこの秒数が経過したら自己終了する。
    {
        let active = active.clone();
        let last_activity = last_activity.clone();
        let max_idle = Duration::from_secs(args.max_idle_lifetime);
        let idle_shutdown = idle_shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if active.load(Ordering::SeqCst) {
                    continue;
                }
                let elapsed = last_activity.lock().await.elapsed();
                if elapsed >= max_idle {
                    log::info!("no active connection for {elapsed:?}, self-terminating");
                    idle_shutdown.notify_one();
                    break;
                }
            }
        });
    }

    let once = args.once;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("shutdown requested, closing endpoint");
                endpoint.close(0u32.into(), b"shutdown");
                break;
            }
            _ = idle_shutdown.notified() => {
                log::info!("max-idle-lifetime reached, closing endpoint");
                endpoint.close(0u32.into(), b"idle-timeout");
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let target = args.target;
                let secret = session_secret;
                let active = active.clone();
                let last_activity = last_activity.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let remote = conn.remote_address();
                            log::info!("QUIC connection established from {remote}");
                            if let Err(e) = handle_connection(conn, target, secret, active).await {
                                log::warn!("connection from {remote} ended: {e:#}");
                            }
                        }
                        Err(e) => log::warn!("failed to accept connection: {e:#}"),
                    }
                    *last_activity.lock().await = Instant::now();
                });
                if once {
                    endpoint.close(0u32.into(), b"once");
                    break;
                }
            }
        }
    }

    endpoint.wait_idle().await;
    Ok(())
}

async fn handle_connection(
    conn: quinn::Connection,
    target: SocketAddr,
    session_secret: [u8; 32],
    active: Arc<AtomicBool>,
) -> Result<()> {
    // HELLO を一定時間内に送ってこない connection は close する
    // （QUIC connection だけ張って stream を開かない妨害を防ぐ）。
    let (send, recv, hello) = tokio::time::timeout(HELLO_TIMEOUT, async {
        let (send, mut recv) = conn.accept_bi().await.context("no stream opened")?;
        let mut hello = [0u8; 33];
        recv.read_exact(&mut hello)
            .await
            .context("failed to read HELLO frame")?;
        Ok::<_, anyhow::Error>((send, recv, hello))
    })
    .await
    .context("HELLO timeout")??;

    handle_stream(conn, send, recv, hello, target, session_secret, active).await
}

/// 拒否フレームを送出し、`finish()` 後に `stopped()` で peer への到達を待ってから返す。
/// これをせずに呼び出し元が即座に `conn` を drop すると、1 byte の応答が飛ぶ前に
/// QUIC connection が暗黙に閉じられ、client 側が payload を読めないことがある
/// （実測で確認済みのバグ）。
async fn reject(send: &mut quinn::SendStream, code: u8) {
    if send.write_all(&[code]).await.is_ok() {
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    }
}

/// helper 側の HELLO 検証・target 接続・中継を行う。
#[allow(clippy::too_many_arguments)]
async fn handle_stream(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
    hello: [u8; 33],
    target: SocketAddr,
    session_secret: [u8; 32],
    active: Arc<AtomicBool>,
) -> Result<()> {
    if hello[0] != FRAME_HELLO {
        reject(&mut send, FRAME_REJECT_UNSUPPORTED).await;
        return Err(anyhow!("unexpected frame type: {:#x}", hello[0]));
    }
    let client_proof = &hello[1..33];

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;

    let mut mac =
        HmacSha256::new_from_slice(&session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    let expected = mac.finalize().into_bytes();

    if client_proof.ct_eq(expected.as_slice()).unwrap_u8() != 1 {
        reject(&mut send, FRAME_REJECT_AUTH).await;
        return Err(anyhow!("proof mismatch, rejecting"));
    }

    // 同時アクティブ接続は1本まで。target への TCP 接続成功直後に slot を確保する
    // （HELPER_PROTOCOL.md「ハンドシェイクの処理順序」参照）。
    if active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        reject(&mut send, FRAME_REJECT_DUPLICATE).await;
        return Err(anyhow!("duplicate active connection rejected"));
    }

    let mut recv = recv;
    let result = relay(&mut send, &mut recv, target).await;
    active.store(false, Ordering::SeqCst);
    result
}

async fn relay(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    target: SocketAddr,
) -> Result<()> {
    let tcp = match TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            reject(send, FRAME_REJECT_TARGET).await;
            return Err(anyhow!("connect to {target} failed: {e}"));
        }
    };

    send.write_all(&[FRAME_ACK]).await?;

    let mut tcp = tcp;
    let mut quic = QuicDuplex { send, recv };
    // 通常終了は stream の finish / half-close を優先し、Connection::close() は使わない
    // （異常系・protocol violation のみ close を使う方針、HELPER_PROTOCOL.md 参照）。
    match tokio::io::copy_bidirectional(&mut quic, &mut tcp).await {
        Ok((to_target, to_client)) => {
            log::info!(
                "relay to {target} closed ({to_target} bytes -> target, {to_client} bytes -> client)"
            );
        }
        Err(e) => log::warn!("relay to {target} error: {e}"),
    }
    Ok(())
}

struct QuicDuplex<'a> {
    send: &'a mut quinn::SendStream,
    recv: &'a mut quinn::RecvStream,
}

impl tokio::io::AsyncRead for QuicDuplex<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        <quinn::RecvStream as tokio::io::AsyncRead>::poll_read(
            std::pin::Pin::new(&mut *self.recv),
            cx,
            buf,
        )
    }
}

impl tokio::io::AsyncWrite for QuicDuplex<'_> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        <quinn::SendStream as tokio::io::AsyncWrite>::poll_write(
            std::pin::Pin::new(&mut *self.send),
            cx,
            buf,
        )
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        <quinn::SendStream as tokio::io::AsyncWrite>::poll_flush(
            std::pin::Pin::new(&mut *self.send),
            cx,
        )
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        <quinn::SendStream as tokio::io::AsyncWrite>::poll_shutdown(
            std::pin::Pin::new(&mut *self.send),
            cx,
        )
    }
}
