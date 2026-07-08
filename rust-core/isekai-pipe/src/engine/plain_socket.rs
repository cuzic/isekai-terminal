//! Wraps an already-bound `tokio::net::UdpSocket` as a `noq::AsyncUdpSocket`
//! so it can be handed to `noq::Endpoint::new_with_abstract_socket` *after*
//! having been used directly (raw `send_to`/`recv_from`) for a STUN query
//! and/or hole-punch probes — see `main.rs`'s `--stun-server`/`--punch-peer`
//! handling. Doing the STUN/probe step before wrapping (rather than trying
//! to share the socket with an already-running noq endpoint) avoids a race
//! between noq's internal `poll_recv` and our own raw reads on the same
//! socket.
//!
//! This is a plain pass-through with no fault injection (unlike
//! `isekai-terminal-core`'s `faulty_udp_socket.rs`/`multipath_transport.rs`, which are
//! Android-app-only debug tooling isekai-helper has no access to as a
//! separate crate) — it exists solely to let a socket be used twice (once
//! raw, once as a QUIC transport), not to alter behavior.

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};

pub(crate) struct PlainUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
}

impl PlainUdpSocket {
    pub(crate) fn new(inner: Arc<tokio::net::UdpSocket>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for PlainUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainUdpSocket").finish_non_exhaustive()
    }
}

struct PlainUdpSender {
    inner: Arc<tokio::net::UdpSocket>,
}

impl std::fmt::Debug for PlainUdpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlainUdpSender")
    }
}

impl UdpSender for PlainUdpSender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.inner.poll_send_to(cx, transmit.contents, transmit.destination) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncUdpSocket for PlainUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(PlainUdpSender { inner: self.inner.clone() })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
        match self.inner.poll_recv_from(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(addr)) => {
                let len = read_buf.filled().len();
                let mut m = RecvMeta::default();
                m.addr = addr;
                m.len = len;
                m.stride = len;
                meta[0] = m;
                Poll::Ready(Ok(1))
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}
