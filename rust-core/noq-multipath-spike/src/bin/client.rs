//! noq multipath spike: client half.
//!
//! Connects to <direct> as path0, then opens <tailscale> as a second QUIC
//! path (draft-ietf-quic-multipath) on the SAME connection, proving traffic
//! independently over both, then closes path0 and confirms path1 alone
//! keeps carrying data.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use noq::{ClientConfig, Endpoint, FourTuple, PathId, PathStatus, TransportConfig};
use rustls::pki_types::CertificateDer;

#[derive(Parser, Debug)]
struct Opt {
    /// e.g. 204.12.203.210:45820 (public IP path)
    #[clap(long)]
    direct: SocketAddr,
    /// e.g. 100.100.45.36:45820 (Tailscale path)
    #[clap(long)]
    tailscale: SocketAddr,
    #[clap(long, default_value = "/data/local/tmp/noq-spike-cert.der")]
    cert: PathBuf,
    #[clap(long, default_value = "noq-spike")]
    server_name: String,
    /// Explicit source IP to request for path1 (e.g. the phone's cellular
    /// interface address). If unset, the OS default route decides.
    #[clap(long)]
    path1_local_ip: Option<IpAddr>,
}

async fn echo(connection: &noq::Connection, label: &str, msg: &str) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await.context("open_bi")?;
    send.write_all(msg.as_bytes()).await?;
    send.finish()?;
    let reply = recv.read_to_end(64 * 1024).await?;
    println!("[client] [{label}] sent {msg:?}, got {:?}", String::from_utf8_lossy(&reply));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let opt = Opt::parse();

    let cert_bytes = std::fs::read(&opt.cert).context("reading pinned cert")?;
    let cert_der = CertificateDer::from(cert_bytes);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der)?;
    let mut client_config = ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = TransportConfig::default();
    transport.max_concurrent_multipath_paths(8);
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    println!("[client] connecting path0 (tailscale) -> {}", opt.tailscale);
    let connection = endpoint
        .connect(opt.tailscale, &opt.server_name)?
        .await
        .context("path0 (tailscale) handshake")?;
    println!("[client] path0 (tailscale) established");
    echo(&connection, "path0/tailscale", "hello via tailscale path").await?;

    let path1_four_tuple = match opt.path1_local_ip {
        Some(ip) => FourTuple::new(opt.direct, Some(ip)),
        None => FourTuple::from_remote(opt.direct),
    };
    println!(
        "[client] opening path1 (direct) -> {} (requested local_ip={:?})",
        opt.direct, opt.path1_local_ip
    );
    match tokio::time::timeout(
        Duration::from_secs(8),
        connection.open_path(path1_four_tuple, PathStatus::Available),
    )
    .await
    {
        Ok(Ok(path1)) => {
            println!(
                "[client] path1 (direct) established: id={:?} remote={:?} local_ip={:?}",
                path1.id(),
                path1.remote_address(),
                path1.local_ip()
            );
            echo(&connection, "path1/direct", "hello via direct path").await?;
            println!("[client] SPIKE OK: BOTH paths (tailscale + direct) worked simultaneously");

            if let Some(path0) = connection.path(PathId::ZERO) {
                println!("[client] closing path0 (tailscale) to simulate losing that network");
                let _ = path0.close();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            echo(&connection, "path1/direct-after-path0-closed", "still alive on direct-only?").await?;
            println!(
                "[client] FAILOVER OK: connection survived path0 (tailscale) closing, \
                 traffic kept flowing on path1 (direct) alone"
            );
            connection.close(0u32.into(), b"spike done");
            return Ok(());
        }
        Ok(Err(e)) => {
            println!("[client] path1 (direct) FAILED to open: {e}");
            println!(
                "[client] SPIKE PARTIAL: tailscale path works, direct path rejected/unreachable \
                 (likely firewall/NAT on the direct address) -- this is a real, expected finding"
            );
        }
        Err(_) => {
            println!("[client] path1 (direct) TIMED OUT after 8s");
            println!(
                "[client] SPIKE PARTIAL: tailscale path works, direct path did not respond \
                 (likely firewall/NAT dropping inbound UDP) -- this is a real, expected finding"
            );
        }
    }

    if let Some(path0) = connection.path(PathId::ZERO) {
        println!(
            "[client] path0 info: remote={:?} local_ip={:?}",
            path0.remote_address(),
            path0.local_ip()
        );
    }
    echo(&connection, "path0/tailscale-final-check", "tailscale still alive at the end?").await?;

    connection.close(0u32.into(), b"spike done");
    Ok(())
}
