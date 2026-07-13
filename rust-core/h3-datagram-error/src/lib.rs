//! `convert_h3_error_to_datagram_error`: the one byte-identical helper that
//! was duplicated verbatim between `h3-noq`'s and `h3-qmux`'s `datagram.rs`
//! (Codexレビュー, rust-core全体のリファクタタスク #19: h3-noq/h3-qmuxの
//! Connection/OpenStreams/BidiStream/RecvStream/SendStreamはbackend差分
//! [stream id・error型] が大きく、共通trait化は当面見送り。datagram adapter
//! の中でこの関数だけが入出力ともh3/h3-datagramの型のみで完結し、backend
//! 固有の要素を一切持たない純粋な変換だったため、ここだけを切り出す)。
//!
//! `convert_send_datagram_error`・`convert_connection_error`は両crateとも
//! backend固有のエラー列挙型([`noq::SendDatagramError`]/[`qmux::Error`]等)
//! を入力に取り、variant数も一致しないため、意図的に各crateへ残したまま
//! (無理に共通化すると片方に存在しないvariantを握り潰すことになる)。

use h3::quic::ConnectionErrorIncoming;

pub fn convert_h3_error_to_datagram_error(error: ConnectionErrorIncoming) -> h3_datagram::ConnectionErrorIncoming {
    match error {
        ConnectionErrorIncoming::ApplicationClose { error_code } => {
            h3_datagram::ConnectionErrorIncoming::ApplicationClose { error_code }
        }
        ConnectionErrorIncoming::Timeout => h3_datagram::ConnectionErrorIncoming::Timeout,
        ConnectionErrorIncoming::InternalError(err) => h3_datagram::ConnectionErrorIncoming::InternalError(err),
        ConnectionErrorIncoming::Undefined(error) => h3_datagram::ConnectionErrorIncoming::Undefined(error),
    }
}
