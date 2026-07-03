//! Mechanical smoke test for `DualUdpSocket`, entirely on loopback.
//!
//! Binds two sockets (127.0.0.1 and 127.0.0.2), wraps them in a
//! `DualUdpSocket`, and drives a real noq connection + multipath `open_path`
//! through it -- proving the trait impl itself is correct before pointing
//! it at real Android-bound fds.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use noq::{ClientConfig, Endpoint, FourTuple, PathId, PathStatus, ServerConfig, TokioRuntime, TransportConfig};
use noq_multipath_spike::dual_fd_socket::{DualUdpSocket, NamedUdpSocket};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // --- server: plain single-socket noq endpoint on loopback ---
    let cert = rcgen::generate_simple_self_signed(vec!["noq-spike".into()])?;
    let cert_der = CertificateDer::from(cert.cert);
    let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut server_config = ServerConfig::with_single_cert(vec![cert_der.clone()], priv_key.into())?;
    {
        let t = Arc::get_mut(&mut server_config.transport).unwrap();
        t.max_concurrent_multipath_paths(8);
    }
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let server_ep = Endpoint::server(server_config, server_addr)?;
    let server_port = server_ep.local_addr()?.port();
    println!("[loopback] server on 127.0.0.1:{server_port}");

    tokio::spawn({
        let server_ep = server_ep.clone();
        async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            println!("[loopback][server] connection established");
            loop {
                match conn.accept_bi().await {
                    Ok((mut send, mut recv)) => {
                        let data = recv.read_to_end(4096).await.unwrap_or_default();
                        let _ = send.write_all(b"ack:").await;
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // --- client: DualUdpSocket over two loopback addrs standing in for
    // "wifi" (127.0.0.1) and "cellular" (127.0.0.2) ---
    let primary_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let secondary_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
    let primary_sock = Arc::new(
        tokio::net::UdpSocket::bind(SocketAddr::new(primary_ip, 0))
            .await
            .context("bind primary")?,
    );
    let secondary_sock = Arc::new(
        tokio::net::UdpSocket::bind(SocketAddr::new(secondary_ip, 0))
            .await
            .context("bind secondary")?,
    );
    let dual = DualUdpSocket {
        primary: NamedUdpSocket { label: "wifi", local_ip: primary_ip, socket: primary_sock },
        secondary: NamedUdpSocket { label: "cellular", local_ip: secondary_ip, socket: secondary_sock },
    };

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der)?;
    let mut client_config = ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = TransportConfig::default();
    transport.max_concurrent_multipath_paths(8);
    client_config.transport_config(Arc::new(transport));

    let endpoint = Endpoint::new_with_abstract_socket(
        Default::default(),
        None,
        Box::new(dual),
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config);

    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse()?;
    println!("[loopback][client] connecting path0 (primary/wifi) -> {server_addr}");
    let connection = endpoint.connect(server_addr, "noq-spike")?.await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"via primary").await?;
    send.finish()?;
    println!(
        "[loopback][client] path0 echo: {:?}",
        String::from_utf8_lossy(&recv.read_to_end(4096).await?)
    );

    let secondary_target = FourTuple::new(server_addr, Some(secondary_ip));
    println!("[loopback][client] opening path1 (secondary/cellular) local_ip={secondary_ip}");
    let path1 = connection.open_path(secondary_target, PathStatus::Available).await?;
    println!(
        "[loopback][client] path1 established: id={:?} local_ip={:?}",
        path1.id(),
        path1.local_ip()
    );
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"via secondary").await?;
    send.finish()?;
    println!(
        "[loopback][client] path1 echo: {:?}",
        String::from_utf8_lossy(&recv.read_to_end(4096).await?)
    );

    if let Some(path0) = connection.path(PathId::ZERO) {
        let _ = path0.close();
        println!("[loopback][client] closed path0 (primary)");
    }
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"via secondary only").await?;
    send.finish()?;
    println!(
        "[loopback][client] post-close echo (should still work via secondary): {:?}",
        String::from_utf8_lossy(&recv.read_to_end(4096).await?)
    );

    println!("[loopback] DUAL-FD SOCKET SMOKE TEST OK");
    connection.close(0u32.into(), b"done");
    Ok(())
}
