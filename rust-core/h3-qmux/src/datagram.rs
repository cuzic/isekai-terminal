//! Support for the h3-datagram crate.
//!
//! Direct port of `h3-noq`'s `datagram.rs`; `qmux::Session::send_datagram`
//! is synchronous (matching `noq::Connection::send_datagram`'s shape) and
//! `recv_datagram` is `async fn` (matching `noq::Connection::read_datagram`),
//! so the same `stream::unfold` bridging pattern applies unchanged.

use std::task::{ready, Poll};

use futures_util::{stream, StreamExt};
use h3_datagram::datagram::EncodedDatagram;
use h3_datagram::quic_traits::{DatagramConnectionExt, RecvDatagram, SendDatagram, SendDatagramErrorIncoming};

use h3_datagram::ConnectionErrorIncoming;

use bytes::{Buf, Bytes};
use h3_datagram_error::convert_h3_error_to_datagram_error;
use web_transport_trait::Session as _;

use crate::{convert_connection_error, BoxStreamSync, Connection};

/// A Struct which allows sending datagrams over a qmux session.
pub struct SendDatagramHandler {
    session: qmux::Session,
}

impl<B: Buf> SendDatagram<B> for SendDatagramHandler {
    fn send_datagram<T: Into<EncodedDatagram<B>>>(&mut self, data: T) -> Result<(), SendDatagramErrorIncoming> {
        let mut buf: EncodedDatagram<B> = data.into();
        self.session
            .send_datagram(buf.copy_to_bytes(buf.remaining()))
            .map_err(convert_send_datagram_error)
    }
}

/// A Struct which allows receiving datagrams over a qmux session.
pub struct RecvDatagramHandler {
    datagrams: BoxStreamSync<'static, Result<Bytes, qmux::Error>>,
}

impl RecvDatagram for RecvDatagramHandler {
    type Buffer = Bytes;
    fn poll_incoming_datagram(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Result<Self::Buffer, ConnectionErrorIncoming>> {
        Poll::Ready(
            ready!(self.datagrams.poll_next_unpin(cx))
                .expect("self.datagrams never returns None")
                .map_err(|e| convert_h3_error_to_datagram_error(convert_connection_error(e))),
        )
    }
}

impl<B: Buf> DatagramConnectionExt<B> for Connection {
    type SendDatagramHandler = SendDatagramHandler;
    type RecvDatagramHandler = RecvDatagramHandler;

    fn send_datagram_handler(&self) -> Self::SendDatagramHandler {
        SendDatagramHandler { session: self.session.clone() }
    }

    fn recv_datagram_handler(&self) -> Self::RecvDatagramHandler {
        RecvDatagramHandler {
            datagrams: Box::pin(stream::unfold(self.session.clone(), |session| async {
                Some((session.recv_datagram().await, session))
            })),
        }
    }
}

fn convert_send_datagram_error(error: qmux::Error) -> SendDatagramErrorIncoming {
    match error {
        qmux::Error::DatagramsUnsupported => SendDatagramErrorIncoming::NotAvailable,
        qmux::Error::FrameTooLarge => SendDatagramErrorIncoming::TooLarge,
        other => SendDatagramErrorIncoming::ConnectionError(convert_h3_error_to_datagram_error(
            convert_connection_error(other),
        )),
    }
}
