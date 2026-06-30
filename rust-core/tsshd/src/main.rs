use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

#[derive(Debug, Deserialize)]
struct Handshake {
    ssh_host: String,
    ssh_port: u16,
    #[serde(default)]
    #[allow(dead_code)]
    cols: u16,
    #[serde(default)]
    #[allow(dead_code)]
    rows: u16,
}

struct Args {
    port: u16,
    log_level: String,
}

fn parse_args() -> Result<Args> {
    let mut port = 2222u16;
    let mut log_level = "info".to_string();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                let v = iter.next().ok_or_else(|| anyhow!("--port requires a value"))?;
                port = v.parse().context("invalid --port value")?;
            }
            "--log-level" => {
                log_level = iter.next().ok_or_else(|| anyhow!("--log-level requires a value"))?;
            }
            "-h" | "--help" => {
                println!("tsshd - QUIC to SSH proxy daemon");
                println!();
                println!("USAGE:");
                println!("    tsshd [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("    --port <PORT>           UDP port to listen on (default: 2222)");
                println!("    --log-level <LEVEL>     Log level: error|warn|info|debug|trace (default: info)");
                println!("    -h, --help              Print this help message");
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    Ok(Args { port, log_level })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(args.log_level.clone()),
    )
    .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;

    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["tsshd.local".to_string(), "localhost".to_string()])?;
    let cert_der = cert.der().clone();
    let key_der = key_pair.serialize_der();

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| anyhow!("failed to build private key: {e}"))?;

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    server_crypto.alpn_protocols = vec![b"tsshd".to_vec()];

    let server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));

    let addr: SocketAddr = format!("0.0.0.0:{}", args.port).parse()?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    log::info!("tsshd listening on udp/{}", addr);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("shutdown requested, closing endpoint");
                endpoint.close(0u32.into(), b"shutdown");
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let remote = conn.remote_address();
                            log::info!("connection established from {}", remote);
                            if let Err(e) = handle_connection(conn).await {
                                log::warn!("connection from {} ended: {:#}", remote, e);
                            }
                        }
                        Err(e) => log::warn!("failed to accept connection: {:#}", e),
                    }
                });
            }
        }
    }

    endpoint.wait_idle().await;
    Ok(())
}

async fn handle_connection(conn: quinn::Connection) -> Result<()> {
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(send, recv).await {
                        log::warn!("stream ended: {:#}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_))
            | Err(quinn::ConnectionError::ConnectionClosed(_))
            | Err(quinn::ConnectionError::LocallyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
}

async fn handle_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let handshake = match read_handshake(&mut recv).await {
        Ok(h) => h,
        Err(e) => {
            let _ = send.write_all(format!("ERR:{}\n", e).as_bytes()).await;
            let _ = send.finish();
            return Err(e);
        }
    };

    let target = format!("{}:{}", handshake.ssh_host, handshake.ssh_port);
    log::info!("proxying stream to {}", target);

    let tcp = match TcpStream::connect(&target).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send
                .write_all(format!("ERR:failed to connect to {}: {}\n", target, e).as_bytes())
                .await;
            let _ = send.finish();
            return Err(anyhow!("connect to {} failed: {}", target, e));
        }
    };

    send.write_all(b"OK\n").await?;

    let mut tcp = tcp;
    let mut quic = QuicDuplex { send, recv };
    match tokio::io::copy_bidirectional(&mut quic, &mut tcp).await {
        Ok((to_ssh, to_client)) => {
            log::info!(
                "stream to {} closed ({} bytes -> ssh, {} bytes -> client)",
                target,
                to_ssh,
                to_client
            );
        }
        Err(e) => log::warn!("proxy to {} error: {}", target, e),
    }
    Ok(())
}

async fn read_handshake(recv: &mut quinn::RecvStream) -> Result<Handshake> {
    let mut buf = Vec::with_capacity(128);
    loop {
        let b = recv
            .read_u8()
            .await
            .context("failed reading handshake byte")?;
        if b == b'\n' {
            break;
        }
        buf.push(b);
        if buf.len() > 4096 {
            return Err(anyhow!("handshake too long"));
        }
    }
    let handshake: Handshake =
        serde_json::from_slice(&buf).context("invalid handshake JSON")?;
    Ok(handshake)
}

struct QuicDuplex {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl tokio::io::AsyncRead for QuicDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        <quinn::RecvStream as tokio::io::AsyncRead>::poll_read(
            std::pin::Pin::new(&mut self.recv),
            cx,
            buf,
        )
    }
}

impl tokio::io::AsyncWrite for QuicDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        <quinn::SendStream as tokio::io::AsyncWrite>::poll_write(
            std::pin::Pin::new(&mut self.send),
            cx,
            buf,
        )
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send).poll_shutdown(cx)
    }
}
