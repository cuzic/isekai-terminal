//! A [`noq::AsyncUdpSocket`] that fans out across one default (OS-routed)
//! socket plus any number of named paths, picking which socket to send
//! from by matching [`noq::udp::Transmit::src_ip`] against each named
//! path's local IP.
//!
//! This is the piece [`quicsock-quinn`](https://crates.io/crates/quicsock-quinn)
//! deliberately doesn't have: quinn has no concept of multiple simultaneous
//! paths per connection, so there's nothing to fan out across there. noq
//! does (that's the whole point of the fork), and multipath's actual value
//! — e.g. a phone's Wi-Fi plus a USB/Bluetooth tethering path kept warm as
//! standby — requires exactly this kind of "which physical interface does
//! this outgoing datagram leave from" control.
//!
//! The trait implementation here (`poll_recv`'s "check the default socket,
//! then poll every named socket" loop; `create_sender`'s "route by
//! `src_ip`, default socket if nothing matches" logic) was extracted and
//! generalized from a real, hardware-verified implementation
//! (`multipath_transport.rs`'s `MultiUdpSocket`/`NamedUdpSocket` in the
//! isekai-terminal project, Android's `Network.bindSocket()`-based physical
//! Wi-Fi/cellular multipath, Phase 9-4/9-5).
//!
//! # Interface binding vs. bringing your own socket
//!
//! [`NamedPath::bind`] goes through [`quicsock::bind_udp`], which is the
//! right choice on platforms where a plain `setsockopt`-level interface
//! restriction is meaningful and permitted. It is very much **not** the
//! right choice everywhere — notably Android, where physical interface
//! selection is mediated by `android.net.Network.bindSocket()` (a
//! Binder/netd-backed API with its own routing-policy side effects), not a
//! raw kernel `SO_BINDTODEVICE`/`IP_BOUND_IF` call: a plain `bind()` to an
//! interface's local IP has been verified on real hardware to silently do
//! nothing (traffic still leaves via the default network regardless of the
//! socket's source address — Android's routing is UID/fwmark-based policy
//! routing, not address-based), and raw `SO_BINDTODEVICE`-style calls are
//! generally unavailable to a sandboxed app process in the first place. For
//! that reason [`NamedPath`] and [`MultiPathSocket`] are generic over a
//! [`PathSocket`] rather than hard-wired to a `quicsock`-bound
//! [`tokio::net::UdpSocket`]: [`NamedPath::from_socket`] accepts a socket
//! that was already bound/restricted by whatever mechanism is correct for
//! the platform (e.g. Android's `Network.bindSocket()` on the Kotlin/JNI
//! side, then imported here via the standard [`std::os::fd::FromRawFd`]),
//! without `quicsock` needing to know anything about Android at all.
//!
//! # Why this is generic over the socket type at all
//!
//! The other reason for [`PathSocket`], beyond bring-your-own-binding: it
//! lets a caller substitute an instrumented socket (packet loss/latency
//! injection, deliberate cuts) for testing, and have that instrumentation
//! exercise the *exact* fan-out/routing code that ships in production,
//! rather than maintaining a second, hand-synced copy of this logic for
//! test builds. That's not a hypothetical concern for this code's lineage:
//! the isekai-terminal project this was extracted from has hit more than
//! one bug that only reproduced on real hardware and never in a
//! non-instrumented local/loopback test (Phase 8-4b, Phase 9-5) — a second,
//! divergent test-only implementation of this same fan-out logic would be
//! exactly the kind of gap that class of bug hides in.
//!
//! # Verification status
//!
//! Tested (loopback, real interface binding via `quicsock::bind_udp`
//! against the actual loopback interface index) on Linux only — this
//! crate's own `poll_recv`/`create_sender` logic is platform-independent
//! (it only calls methods on the `PathSocket` trait, all of which forward
//! to plain `tokio::net::UdpSocket` methods for the default `S`), so the
//! main risk on other platforms is `quicsock`'s own interface-binding step
//! (used by `NamedPath::bind`, not `NamedPath::from_socket`), which is
//! covered by its own verification notes. Separately: this crate could not
//! even be *type-checked* for macOS in this development environment
//! (`cargo check --target aarch64-apple-darwin`), because `noq`/`quinn`'s
//! `ring` (TLS crypto) dependency needs a real Apple C cross-toolchain to
//! compile its C code, which isn't available here — `quicsock` itself (no
//! crypto deps) does type-check cleanly for that target, so this is a
//! limitation of this adapter crate's dependency tree, not of `quicsock`.
//!
//! # A known noq limitation this crate inherits
//!
//! [noq issue #738](https://github.com/n0-computer/noq/issues/738) is a
//! confirmed bug (root-caused down to the exact internal dispatch function
//! via a local noq fork with instrumented logging, and reproduced on real
//! Android hardware) where `noq::Connection::open_path()` with an
//! explicit `local_ip` never receives `PATH_RESPONSE` frames for that path,
//! so the path is abandoned as `ValidationFailed` after retries — even
//! though the raw UDP send/receive on that socket demonstrably works.
//! **This crate does not work around that bug.** It fans out sends/receives
//! across sockets *before* handing bytes to noq, which is a different code
//! path from `open_path`'s internal path-management — whether that sidesteps
//! issue #738 or hits some adjacent variant of it has not been verified on
//! real hardware for this crate specifically (only the original Android
//! implementation this was extracted from has real-hardware verification,
//! and that verification was for physical-interface *rebind* via
//! `Endpoint::rebind_abstract()`, not this pre-connection socket fan-out
//! approach). Verify on your own target platform before relying on this.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};
use quicsock::InterfaceIndex;
use tokio::io::ReadBuf;

/// What [`NamedPath`]/[`MultiPathSocket`] need from a single path's
/// underlying socket — a minimal, `tokio::net::UdpSocket`-shaped interface
/// (blanket-implemented for it below) that a caller can also implement for
/// their own wrapper type, e.g. one that injects packet loss/latency for
/// testing, or one built from a socket bound by a platform-specific
/// mechanism `quicsock` doesn't (and shouldn't) know about — see the module
/// docs' two sections on why this exists rather than a hard-wired
/// `Arc<tokio::net::UdpSocket>`.
pub trait PathSocket: Send + Sync + 'static {
    /// Same contract as [`tokio::net::UdpSocket::poll_send_to`].
    fn poll_send_to(&self, cx: &mut Context<'_>, buf: &[u8], target: SocketAddr) -> Poll<io::Result<usize>>;
    /// Same contract as [`tokio::net::UdpSocket::poll_recv_from`].
    fn poll_recv_from(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<SocketAddr>>;
    /// Same contract as [`tokio::net::UdpSocket::local_addr`].
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

impl PathSocket for tokio::net::UdpSocket {
    fn poll_send_to(&self, cx: &mut Context<'_>, buf: &[u8], target: SocketAddr) -> Poll<io::Result<usize>> {
        tokio::net::UdpSocket::poll_send_to(self, cx, buf, target)
    }

    fn poll_recv_from(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<SocketAddr>> {
        tokio::net::UdpSocket::poll_recv_from(self, cx, buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        tokio::net::UdpSocket::local_addr(self)
    }
}

/// One path's socket, tagged with the local IP address
/// [`Transmit::src_ip`] should match to route a send through it.
///
/// `local_ip` is supplied by the caller rather than derived from the
/// socket's own `local_addr()`, because a socket bound to a wildcard
/// address (`0.0.0.0`/`::`) would report that wildcard back, not the
/// interface's real address — callers already know the interface's address
/// (e.g. from [`quicsock::discovery`], or from whatever platform API chose
/// it) at the point they choose which interface to bind to in the first
/// place.
pub struct NamedPath<S = tokio::net::UdpSocket> {
    local_ip: IpAddr,
    socket: Arc<S>,
}

impl NamedPath<tokio::net::UdpSocket> {
    /// Binds a new path to `interface` via [`quicsock::bind_udp`], tagged
    /// with `local_ip` for send routing. See the module docs' "Interface
    /// binding vs. bringing your own socket" section for when this isn't
    /// the right constructor — [`NamedPath::from_socket`] is the
    /// alternative.
    pub fn bind(interface: InterfaceIndex, local_ip: IpAddr, port: u16) -> io::Result<Self> {
        let local_addr = SocketAddr::new(local_ip, port);
        let socket = quicsock::bind_udp(interface, local_addr)?;
        socket.set_nonblocking(true)?;
        let socket = Arc::new(tokio::net::UdpSocket::from_std(socket.into())?);
        Ok(Self { local_ip, socket })
    }
}

impl<S: PathSocket> NamedPath<S> {
    /// Wraps an already-bound/already-restricted socket as a named path,
    /// without going through [`quicsock::bind_udp`] at all — see the module
    /// docs' "Interface binding vs. bringing your own socket" section.
    pub fn from_socket(local_ip: IpAddr, socket: S) -> Self {
        Self { local_ip, socket: Arc::new(socket) }
    }

    /// The address this path's socket is actually bound to (useful when a
    /// port of `0` was used at bind time and the OS assigned one).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// A [`noq::AsyncUdpSocket`] over one default (unrestricted, OS-routed)
/// socket plus any number of [`NamedPath`]s.
///
/// The default socket exists because not every path needs interface
/// binding — a "primary" path reached via the OS's normal default route
/// often doesn't (see the module docs' isekai-terminal precedent, where the
/// Tailscale-vs-direct-address paths used the default route and only the
/// physical Wi-Fi/cellular paths needed explicit binding). If every path
/// you want should be restricted, restrict the default socket too before
/// passing it in — `MultiPathSocket` doesn't require it to be unrestricted,
/// only that there is one.
pub struct MultiPathSocket<S = tokio::net::UdpSocket> {
    default: Arc<S>,
    named: Vec<NamedPath<S>>,
}

impl<S: PathSocket> MultiPathSocket<S> {
    /// `default` is used for both sending (when no [`NamedPath`]'s
    /// `local_ip` matches a [`Transmit::src_ip`]) and as one of the sockets
    /// polled for incoming datagrams.
    pub fn new(default: S, named: Vec<NamedPath<S>>) -> Self {
        Self { default: Arc::new(default), named }
    }
}

impl<S> fmt::Debug for MultiPathSocket<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiPathSocket")
            .field("named_ips", &self.named.iter().map(|n| n.local_ip).collect::<Vec<_>>())
            .finish()
    }
}

impl<S: PathSocket> AsyncUdpSocket for MultiPathSocket<S> {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(MultiPathSender {
            default: self.default.clone(),
            named: self.named.iter().map(|n| (n.local_ip, n.socket.clone())).collect(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut read_buf = ReadBuf::new(&mut bufs[0]);
        if let Poll::Ready(res) = self.default.poll_recv_from(cx, &mut read_buf) {
            return Poll::Ready(res.map(|addr| {
                meta[0] = recv_meta(addr, None, read_buf.filled().len());
                1
            }));
        }
        for named in &self.named {
            let mut read_buf = ReadBuf::new(&mut bufs[0]);
            if let Poll::Ready(res) = named.socket.poll_recv_from(cx, &mut read_buf) {
                return Poll::Ready(res.map(|addr| {
                    meta[0] = recv_meta(addr, Some(named.local_ip), read_buf.filled().len());
                    1
                }));
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.default.local_addr()
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn may_fragment(&self) -> bool {
        true
    }
}

fn recv_meta(addr: SocketAddr, dst_ip: Option<IpAddr>, len: usize) -> RecvMeta {
    let mut m = RecvMeta::default();
    m.addr = addr;
    m.len = len;
    m.stride = len;
    m.dst_ip = dst_ip;
    m
}

struct MultiPathSender<S> {
    default: Arc<S>,
    named: Vec<(IpAddr, Arc<S>)>,
}

impl<S> fmt::Debug for MultiPathSender<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MultiPathSender")
    }
}

impl<S: PathSocket> MultiPathSender<S> {
    fn pick(&self, src_ip: Option<IpAddr>) -> &Arc<S> {
        if let Some(ip) = src_ip {
            if let Some((_, sock)) = self.named.iter().find(|(named_ip, _)| *named_ip == ip) {
                return sock;
            }
        }
        &self.default
    }
}

impl<S: PathSocket> UdpSender for MultiPathSender<S> {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let sock = self.pick(transmit.src_ip);
        match sock.poll_send_to(cx, transmit.contents, transmit.destination) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    // Every platform this crate targets has a loopback interface, and Linux
    // (this crate's development platform) lets any 127.0.0.0/8 address bind
    // to it — so `NamedPath::bind` against a real interface index is
    // possible in a portable test without needing a second real NIC.
    fn loopback_index() -> InterfaceIndex {
        let iface = netdev::get_interfaces()
            .into_iter()
            .find(|i| i.is_loopback())
            .expect("this machine should have a loopback interface");
        InterfaceIndex(iface.index)
    }

    // No generic parameter here despite `MultiPathSender<S>` being generic:
    // `create_sender()` already returns a type-erased `Pin<Box<dyn
    // UdpSender>>`, so by the time a caller has a `sender` to pass in, `S`
    // has nothing left to say.
    async fn send(sender: &mut Pin<Box<dyn UdpSender>>, transmit: &Transmit<'_>) -> io::Result<()> {
        std::future::poll_fn(|cx| sender.as_mut().poll_send(transmit, cx)).await
    }

    async fn recv<S: PathSocket>(socket: &mut MultiPathSocket<S>) -> (RecvMeta, Vec<u8>) {
        let mut buf = [0u8; 64];
        let mut meta = [RecvMeta::default()];
        std::future::poll_fn(|cx| {
            let mut bufs = [std::io::IoSliceMut::new(&mut buf)];
            socket.poll_recv(cx, &mut bufs, &mut meta)
        })
        .await
        .expect("recv should succeed");
        (meta[0], buf[..meta[0].len].to_vec())
    }

    #[tokio::test]
    async fn default_only_echoes_like_a_plain_udp_socket() {
        let default = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut multi = MultiPathSocket::new(default, Vec::new());

        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let multi_addr = multi.local_addr().unwrap();
        peer.send_to(b"hello", multi_addr).await.unwrap();

        let (meta, payload) = recv(&mut multi).await;
        assert_eq!(payload, b"hello");
        assert_eq!(meta.dst_ip, None, "default socket recv should not report a named local_ip");
    }

    #[tokio::test]
    async fn sender_routes_by_src_ip_to_the_matching_named_path_and_default_otherwise() {
        let lo = loopback_index();
        let default = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let named_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        let named = NamedPath::bind(lo, named_ip, 0).unwrap();

        let default_peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let default_peer_addr = default_peer.local_addr().unwrap();
        let named_peer = tokio::net::UdpSocket::bind((named_ip, 0)).await.unwrap();
        let named_peer_addr = named_peer.local_addr().unwrap();

        let multi = MultiPathSocket::new(default, vec![named]);
        let mut sender = multi.create_sender();

        // No src_ip set: should go out the default socket.
        send(
            &mut sender,
            &Transmit { destination: default_peer_addr, ecn: None, contents: b"via-default", segment_size: None, src_ip: None },
        )
        .await
        .unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = default_peer.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"via-default");
        assert_ne!(from.ip(), named_ip, "sanity: default_peer heard from the default socket, not the named one");

        // src_ip matching the named path: should go out that socket instead.
        send(
            &mut sender,
            &Transmit { destination: named_peer_addr, ecn: None, contents: b"via-named", segment_size: None, src_ip: Some(named_ip) },
        )
        .await
        .unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = named_peer.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"via-named");
        assert_eq!(from.ip(), named_ip, "the named-path send should actually leave from named_ip");
    }

    /// A minimal `PathSocket` that can drop every datagram it's asked to
    /// send — not a full fault-injection story (that's each real caller's
    /// own concern, e.g. isekai-terminal's `UdpFaultInjector`), just enough
    /// to prove `MultiPathSocket`/`NamedPath` genuinely don't care what `S`
    /// is, which is the entire point of genericizing over `PathSocket`.
    struct DropAllSends {
        inner: tokio::net::UdpSocket,
        dropped_a_send: Arc<AtomicBool>,
    }

    impl PathSocket for DropAllSends {
        fn poll_send_to(&self, _cx: &mut Context<'_>, buf: &[u8], _target: SocketAddr) -> Poll<io::Result<usize>> {
            self.dropped_a_send.store(true, Ordering::SeqCst);
            Poll::Ready(Ok(buf.len())) // report success, like a real dropped-in-flight UDP datagram would
        }

        fn poll_recv_from(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<SocketAddr>> {
            self.inner.poll_recv_from(cx, buf)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
    }

    #[tokio::test]
    async fn multipath_socket_works_with_a_non_tokio_udpsocket_path_type() {
        let inner = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dropped_a_send = Arc::new(AtomicBool::new(false));
        let default = DropAllSends { inner, dropped_a_send: dropped_a_send.clone() };
        let multi: MultiPathSocket<DropAllSends> = MultiPathSocket::new(default, Vec::new());

        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut sender = multi.create_sender();
        send(
            &mut sender,
            &Transmit { destination: peer.local_addr().unwrap(), ecn: None, contents: b"never arrives", segment_size: None, src_ip: None },
        )
        .await
        .unwrap();

        assert!(dropped_a_send.load(Ordering::SeqCst), "MultiPathSender should have dispatched through DropAllSends::poll_send_to");
        // DropAllSends::poll_send_to reports success without actually
        // writing to its inner socket, so the peer should never see this
        // datagram - proving MultiPathSocket/MultiPathSender genuinely
        // dispatch through the generic `S: PathSocket`, not a hard-wired
        // tokio::net::UdpSocket path bypassing it.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), peer.recv(&mut [0u8; 64])).await.is_err(),
            "the dropped send must never have reached the peer"
        );
    }
}
