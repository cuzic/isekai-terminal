//! JNI entry point so a real Android app process (which alone can call
//! `ConnectivityManager.requestNetwork()` + `Network.bindSocket()`) can hand
//! two already-bound UDP fds to noq's multipath `Endpoint` in-process.
//!
//! Deliberately kept separate from `isekai-terminal-core`'s UniFFI surface: `noq` is
//! still an experimental dependency for this spike, not something the
//! production crate should carry yet.

use std::net::{IpAddr, SocketAddr};
use std::os::fd::FromRawFd;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn last_panic_message() -> &'static Mutex<Option<String>> {
    static CELL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            *last_panic_message().lock().unwrap() = Some(info.to_string());
        }));
    });
}

use jni::objects::{JClass, JString};
use jni::sys::jint;
use jni::JNIEnv;

use noq::{ClientConfig, Endpoint, FourTuple, PathId, PathStatus, TokioRuntime, TransportConfig};
use rustls::pki_types::CertificateDer;

use crate::dual_fd_socket::{DualUdpSocket, NamedUdpSocket};

fn jstring_to_string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

async fn run(
    wifi_fd: i32,
    wifi_ip: IpAddr,
    cellular_fd: i32,
    cellular_ip: IpAddr,
    direct_addr: SocketAddr,
    tailscale_addr: SocketAddr,
    cert_path: String,
    server_name: String,
) -> anyhow::Result<Vec<String>> {
    crate::dual_fd_socket::debug_log().lock().unwrap().clear();
    let mut log = Vec::new();
    macro_rules! l {
        ($($arg:tt)*) => { log.push(format!($($arg)*)) };
    }

    // SAFETY: these fds were just handed to us by the JVM caller, which
    // bound them via `Network.bindSocket()` and is not going to touch them
    // again -- ownership transfers to us here.
    let wifi_std = unsafe { std::net::UdpSocket::from_raw_fd(wifi_fd) };
    let cellular_std = unsafe { std::net::UdpSocket::from_raw_fd(cellular_fd) };
    wifi_std.set_nonblocking(true)?;
    cellular_std.set_nonblocking(true)?;
    let wifi_sock = Arc::new(tokio::net::UdpSocket::from_std(wifi_std)?);
    let cellular_sock = Arc::new(tokio::net::UdpSocket::from_std(cellular_std)?);
    l!("wifi fd bound, local_addr={:?}", wifi_sock.local_addr());
    l!("cellular fd bound, local_addr={:?}", cellular_sock.local_addr());

    let dual = DualUdpSocket {
        primary: NamedUdpSocket { label: "wifi", local_ip: wifi_ip, socket: wifi_sock },
        secondary: NamedUdpSocket { label: "cellular", local_ip: cellular_ip, socket: cellular_sock },
    };

    let cert_der = CertificateDer::from(std::fs::read(&cert_path)?);
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

    l!("connecting path0 (wifi) -> {tailscale_addr} [tailscale]");
    let connection = endpoint.connect(tailscale_addr, &server_name)?.await?;
    l!("path0 (tailscale/wifi-bound) established");
    {
        let (mut send, mut recv) = connection.open_bi().await?;
        send.write_all(b"via path0").await?;
        send.finish()?;
        l!("path0 echo: {:?}", String::from_utf8_lossy(&recv.read_to_end(4096).await?));
    }

    l!("opening path1 (cellular-bound fd, local_ip={cellular_ip}) -> {direct_addr} [direct]");
    let path1_target = FourTuple::new(direct_addr, Some(cellular_ip));
    match tokio::time::timeout(
        Duration::from_secs(8),
        connection.open_path(path1_target, PathStatus::Available),
    )
    .await
    {
        Ok(Ok(path1)) => {
            l!("path1 established: id={:?} local_ip={:?}", path1.id(), path1.local_ip());
            let (mut send, mut recv) = connection.open_bi().await?;
            send.write_all(b"via path1 (cellular)").await?;
            send.finish()?;
            l!("path1 echo: {:?}", String::from_utf8_lossy(&recv.read_to_end(4096).await?));
            l!("RESULT: OK -- cellular-bound fd multipath works end-to-end");

            if let Some(path0) = connection.path(PathId::ZERO) {
                let _ = path0.close();
                l!("closed path0 (wifi/tailscale)");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (mut send, mut recv) = connection.open_bi().await?;
            send.write_all(b"cellular only now").await?;
            send.finish()?;
            l!(
                "post-close echo via cellular-only: {:?}",
                String::from_utf8_lossy(&recv.read_to_end(4096).await?)
            );
            l!("RESULT: FAILOVER OK -- survived closing path0, cellular path1 alone kept working");
        }
        Ok(Err(e)) => l!("RESULT: FAILED -- open_path error: {e}"),
        Err(_) => l!("RESULT: FAILED -- open_path timed out after 8s"),
    }

    connection.close(0u32.into(), b"spike done");
    log.push("--- dual_fd_socket send/recv trail ---".to_string());
    log.extend(crate::dual_fd_socket::debug_log().lock().unwrap().drain(..));
    Ok(log)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_tools_isekai_terminal_NoqMultipathSpike_runDualFdSpike<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    wifi_fd: jint,
    wifi_ip: JString<'local>,
    cellular_fd: jint,
    cellular_ip: JString<'local>,
    direct_addr: JString<'local>,
    tailscale_addr: JString<'local>,
    cert_path: JString<'local>,
    server_name: JString<'local>,
) -> jni::sys::jstring {
    install_panic_hook();
    let wifi_ip_s = jstring_to_string(&mut env, &wifi_ip);
    let cellular_ip_s = jstring_to_string(&mut env, &cellular_ip);
    let direct_addr_s = jstring_to_string(&mut env, &direct_addr);
    let tailscale_addr_s = jstring_to_string(&mut env, &tailscale_addr);
    let cert_path_s = jstring_to_string(&mut env, &cert_path);
    let server_name_s = jstring_to_string(&mut env, &server_name);

    *last_panic_message().lock().unwrap() = None;
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<Vec<String>> {
        let wifi_ip: IpAddr = wifi_ip_s.parse()?;
        let cellular_ip: IpAddr = cellular_ip_s.parse()?;
        let direct_addr: SocketAddr = direct_addr_s.parse()?;
        let tailscale_addr: SocketAddr = tailscale_addr_s.parse()?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(run(
            wifi_fd,
            wifi_ip,
            cellular_fd,
            cellular_ip,
            direct_addr,
            tailscale_addr,
            cert_path_s,
            server_name_s,
        ))
    }));

    let out = match caught {
        Ok(Ok(log)) => log.join("\n"),
        Ok(Err(e)) => format!("SPIKE ERROR: {e:#}"),
        Err(_) => {
            let msg = last_panic_message().lock().unwrap().take();
            format!("SPIKE PANIC: {}", msg.unwrap_or_else(|| "<no panic message captured>".into()))
        }
    };
    env.new_string(out).unwrap_or_default().into_raw()
}
