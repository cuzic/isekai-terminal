//! noq multipath spike: server half.
//!
//! Listens on 0.0.0.0:<port>, writes its self-signed cert to <cert_out>,
//! and logs every PathEvent it sees on incoming connections (echoing any
//! bytes received on a bidi stream back to the client).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use noq::{Endpoint, PathEvent, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio_stream::StreamExt;

#[derive(Parser, Debug)]
struct Opt {
    #[clap(long, default_value = "45820")]
    port: u16,
    #[clap(long, default_value = "/tmp/noq-spike-cert.der")]
    cert_out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let opt = Opt::parse();

    let cert = rcgen::generate_simple_self_signed(vec!["noq-spike".into()])?;
    let cert_der = CertificateDer::from(cert.cert);
    let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    std::fs::write(&opt.cert_out, &cert_der)?;
    println!("[server] wrote cert to {}", opt.cert_out.display());

    let mut server_config = ServerConfig::with_single_cert(vec![cert_der], priv_key.into())?;
    {
        let t = Arc::get_mut(&mut server_config.transport).unwrap();
        t.max_concurrent_multipath_paths(8);
    }

    // [::] with bindv6only=0 (Linux default, confirmed on this box) also
    // accepts IPv4 traffic on the same socket, so a single noq multipath
    // Connection can see both the Wi-Fi (IPv6-only network, path0) and
    // Cellular (IPv4, path1) client paths without running two servers.
    let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), opt.port);
    let endpoint = Endpoint::server(server_config, addr)?;
    println!("[server] listening on [::]:{} (dual-stack)", opt.port);

    loop {
        let incoming = match endpoint.accept().await {
            Some(i) => i,
            None => break,
        };
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    println!("[server] handshake failed: {e}");
                    return;
                }
            };
            println!(
                "[server] connection established from {:?}",
                conn.path(noq::PathId::ZERO).and_then(|p| p.remote_address().ok())
            );

            let mut events = conn.path_events();
            let events_conn = conn.clone();
            tokio::spawn(async move {
                loop {
                    match events.next().await {
                        Some(Ok(PathEvent::Established { id, .. })) => {
                            let info = events_conn
                                .path(id)
                                .map(|p| (p.remote_address().ok(), p.local_ip().ok().flatten()));
                            println!("[server] path established: {id:?} info={info:?}");
                        }
                        Some(Ok(other)) => println!("[server] path event: {other:?}"),
                        Some(Err(e)) => println!("[server] path event lagged: {e:?}"),
                        None => break,
                    }
                }
            });

            loop {
                match conn.accept_bi().await {
                    Ok((mut send, mut recv)) => {
                        let data = match recv.read_to_end(64 * 1024).await {
                            Ok(d) => d,
                            Err(e) => {
                                println!("[server] read error: {e}");
                                continue;
                            }
                        };
                        println!(
                            "[server] recv {} bytes: {:?}",
                            data.len(),
                            String::from_utf8_lossy(&data)
                        );
                        let _ = send.write_all(b"ack: ").await;
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                    Err(e) => {
                        println!("[server] connection closed: {e}");
                        break;
                    }
                }
            }
        });
    }
    Ok(())
}
