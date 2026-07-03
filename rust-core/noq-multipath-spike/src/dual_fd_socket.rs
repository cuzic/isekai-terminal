//! A [`noq::AsyncUdpSocket`] that multiplexes two independently-bound UDP
//! sockets (e.g. one bound to Android's Wi-Fi `Network`, one bound to its
//! Cellular `Network` via `android_setsocknetwork`/`Network.bindSocket`)
//! behind a single noq `Endpoint`.
//!
//! noq's native multipath support picks which underlying socket to send
//! from purely via `Transmit::src_ip` (see noq-proto's `FourTuple`, which
//! only tracks a source *IP*, not a bound network). On Android, sending
//! from a non-default network's source IP on a socket that was never bound
//! to that `Network` fails with EIO (confirmed on-device): the kernel/netd
//! policy routing rejects it. Binding two physically separate sockets ahead
//! of time and dispatching by `src_ip` here is what makes noq's multipath
//! actually ride two different radios.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};

/// Diagnostic trail for the spike: Android app stdout/stderr isn't reliably
/// forwarded to logcat, so this is drained by the JNI bridge and returned in
/// the result string instead of relying on `eprintln!`.
pub fn debug_log() -> &'static Mutex<Vec<String>> {
    static LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn log(msg: String) {
    debug_log().lock().unwrap().push(msg);
}

pub struct NamedUdpSocket {
    pub label: &'static str,
    pub local_ip: IpAddr,
    pub socket: Arc<tokio::net::UdpSocket>,
}

impl fmt::Debug for NamedUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamedUdpSocket")
            .field("label", &self.label)
            .field("local_ip", &self.local_ip)
            .finish()
    }
}

/// Multiplexes exactly two pre-bound sockets. `primary` is used whenever a
/// [`Transmit`] doesn't request a specific `src_ip` (i.e. path0, opened via
/// `Endpoint::connect`); `secondary` is used when `src_ip` matches its
/// `local_ip` (i.e. an additional path opened via `Connection::open_path`
/// with that IP).
#[derive(Debug)]
pub struct DualUdpSocket {
    pub primary: NamedUdpSocket,
    pub secondary: NamedUdpSocket,
}

impl AsyncUdpSocket for DualUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(DualUdpSender {
            primary: self.primary.socket.clone(),
            secondary: self.secondary.socket.clone(),
            secondary_ip: self.secondary.local_ip,
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        for named in [&self.primary, &self.secondary] {
            let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
            match named.socket.poll_recv_from(cx, &mut read_buf) {
                Poll::Ready(Ok(addr)) => {
                    let len = read_buf.filled().len();
                    log(format!(
                        "RECV on {} (local_ip={}): {} bytes from {}",
                        named.label, named.local_ip, len, addr
                    ));
                    let mut m = RecvMeta::default();
                    m.addr = addr;
                    m.len = len;
                    m.stride = len;
                    m.dst_ip = Some(named.local_ip);
                    meta[0] = m;
                    return Poll::Ready(Ok(1));
                }
                Poll::Ready(Err(e)) => {
                    log(format!("RECV ERROR on {}: {e}", named.label));
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => continue,
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.primary.socket.local_addr()
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn may_fragment(&self) -> bool {
        true
    }
}

struct DualUdpSender {
    primary: Arc<tokio::net::UdpSocket>,
    secondary: Arc<tokio::net::UdpSocket>,
    secondary_ip: IpAddr,
}

impl fmt::Debug for DualUdpSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DualUdpSender")
    }
}

impl DualUdpSender {
    fn pick(&self, src_ip: Option<IpAddr>) -> &Arc<tokio::net::UdpSocket> {
        match src_ip {
            Some(ip) if ip == self.secondary_ip => &self.secondary,
            _ => &self.primary,
        }
    }
}

impl UdpSender for DualUdpSender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let sock = self.pick(transmit.src_ip).clone();
        let which = if transmit.src_ip == Some(self.secondary_ip) { "secondary" } else { "primary" };
        match sock.poll_send_to(cx, transmit.contents, transmit.destination) {
            Poll::Ready(Ok(n)) => {
                log(format!(
                    "SEND via {which} (src_ip={:?}): {n}/{} bytes to {}",
                    transmit.src_ip, transmit.contents.len(), transmit.destination
                ));
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                log(format!("SEND ERROR via {which}: {e}"));
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}
