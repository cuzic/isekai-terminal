//! 最小限のSOCKS4/4a・SOCKS5サーバー実装(Phase 12 P2-2、Dynamic port forward(-D)用)。
//!
//! 認証は行わない(SOCKS5は "no authentication" のみサポート)。ローカルの信頼できる
//! クライアント(この端末のアプリ)からのCONNECTのみを想定した最小限の実装であり、
//! BIND/UDP ASSOCIATEコマンドはサポートしない。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub(crate) enum SocksError {
    Io(std::io::Error),
    UnsupportedVersion(u8),
    UnsupportedCommand(u8),
    UnsupportedAddressType(u8),
    Malformed(&'static str),
}

impl std::fmt::Display for SocksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocksError::Io(e) => write!(f, "io error: {e}"),
            SocksError::UnsupportedVersion(v) => write!(f, "unsupported SOCKS version: {v}"),
            SocksError::UnsupportedCommand(c) => write!(f, "unsupported SOCKS command: {c}"),
            SocksError::UnsupportedAddressType(a) => write!(f, "unsupported SOCKS address type: {a}"),
            SocksError::Malformed(msg) => write!(f, "malformed SOCKS request: {msg}"),
        }
    }
}

impl From<std::io::Error> for SocksError {
    fn from(e: std::io::Error) -> Self {
        SocksError::Io(e)
    }
}

/// クライアントとSOCKSハンドシェイクを行い、要求された接続先 `(host, port)` を返す。
/// 成功応答はこの関数の中で書き込み済みなので、呼び出し側はこの後すぐ生バイト中継に
/// 入ってよい。失敗時は可能な範囲で失敗応答を書き込んでから `Err` を返す。
pub(crate) async fn negotiate(stream: &mut TcpStream) -> Result<(String, u16), SocksError> {
    let version = stream.read_u8().await?;
    match version {
        0x04 => negotiate_socks4(stream).await,
        0x05 => negotiate_socks5(stream).await,
        v => Err(SocksError::UnsupportedVersion(v)),
    }
}

// ── SOCKS4 / SOCKS4a ──────────────────────────────────────

async fn negotiate_socks4(stream: &mut TcpStream) -> Result<(String, u16), SocksError> {
    let cmd = stream.read_u8().await?;
    if cmd != 0x01 {
        write_socks4_reply(stream, false).await.ok();
        return Err(SocksError::UnsupportedCommand(cmd));
    }
    let port = stream.read_u16().await?;
    let mut ip_bytes = [0u8; 4];
    stream.read_exact(&mut ip_bytes).await?;

    // userid(NUL終端、内容は無視)。
    read_until_nul(stream).await?;

    // SOCKS4a: アドレスが 0.0.0.x (x != 0) の形の場合、userid の後にドメイン名(NUL終端)が
    // 続く合図。
    let host = if ip_bytes[0] == 0 && ip_bytes[1] == 0 && ip_bytes[2] == 0 && ip_bytes[3] != 0 {
        let domain_bytes = read_until_nul(stream).await?;
        String::from_utf8(domain_bytes).map_err(|_| SocksError::Malformed("invalid SOCKS4a hostname"))?
    } else {
        std::net::Ipv4Addr::from(ip_bytes).to_string()
    };

    write_socks4_reply(stream, true).await?;
    Ok((host, port))
}

async fn read_until_nul(stream: &mut TcpStream) -> Result<Vec<u8>, SocksError> {
    let mut buf = Vec::new();
    loop {
        let b = stream.read_u8().await?;
        if b == 0 {
            break;
        }
        buf.push(b);
        if buf.len() > 255 {
            return Err(SocksError::Malformed("field too long"));
        }
    }
    Ok(buf)
}

async fn write_socks4_reply(stream: &mut TcpStream, granted: bool) -> std::io::Result<()> {
    let mut reply = [0u8; 8];
    reply[1] = if granted { 0x5A } else { 0x5B };
    stream.write_all(&reply).await
}

// ── SOCKS5 ────────────────────────────────────────────────

async fn negotiate_socks5(stream: &mut TcpStream) -> Result<(String, u16), SocksError> {
    let nmethods = stream.read_u8().await?;
    let mut methods = vec![0u8; nmethods as usize];
    stream.read_exact(&mut methods).await?;
    // 認証無し(0x00)のみサポートする。クライアントの提示メソッド一覧に0x00が
    // 含まれるかは確認せず常にこれを選ぶ(ローカルの信頼できるクライアント専用の
    // 用途のため実務上問題ない、既存の trust boundary はSSH認証自体が担う)。
    stream.write_all(&[0x05, 0x00]).await?;

    let ver = stream.read_u8().await?;
    if ver != 0x05 {
        return Err(SocksError::UnsupportedVersion(ver));
    }
    let cmd = stream.read_u8().await?;
    let _rsv = stream.read_u8().await?;
    let atyp = stream.read_u8().await?;

    if cmd != 0x01 {
        write_socks5_reply(stream, 0x07 /* command not supported */).await.ok();
        return Err(SocksError::UnsupportedCommand(cmd));
    }

    let host = match atyp {
        0x01 => {
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            std::net::Ipv4Addr::from(buf).to_string()
        }
        0x03 => {
            let len = stream.read_u8().await? as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            String::from_utf8(buf).map_err(|_| SocksError::Malformed("invalid SOCKS5 hostname"))?
        }
        0x04 => {
            let mut buf = [0u8; 16];
            stream.read_exact(&mut buf).await?;
            std::net::Ipv6Addr::from(buf).to_string()
        }
        a => {
            write_socks5_reply(stream, 0x08 /* address type not supported */).await.ok();
            return Err(SocksError::UnsupportedAddressType(a));
        }
    };
    let port = stream.read_u16().await?;

    write_socks5_reply(stream, 0x00 /* succeeded */).await?;
    Ok((host, port))
}

async fn write_socks5_reply(stream: &mut TcpStream, rep: u8) -> std::io::Result<()> {
    // BND.ADDR/BND.PORTはIPv4の0.0.0.0:0を返す(SSHトンネル越しの中継であり、
    // クライアントから見て意味のある実待受アドレスが無いため)。
    let reply = [0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    stream.write_all(&reply).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// テスト用にループバック上のTCPペアを作る(server側=SOCKSサーバー役、
    /// client側=SOCKSクライアント役)。
    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server, client) = tokio::join!(listener.accept(), TcpStream::connect(addr));
        (server.unwrap().0, client.unwrap())
    }

    #[tokio::test]
    async fn socks5_connect_with_ipv4_address() {
        let (mut server_side, mut client_side) = loopback_pair().await;

        let client_task = tokio::spawn(async move {
            // greeting: ver=5, nmethods=1, methods=[0x00]
            client_side.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut method_reply = [0u8; 2];
            client_side.read_exact(&mut method_reply).await.unwrap();
            assert_eq!(method_reply, [0x05, 0x00]);

            // CONNECT request: ver=5, cmd=1, rsv=0, atyp=1(IPv4), addr=93.184.216.34, port=80
            client_side
                .write_all(&[0x05, 0x01, 0x00, 0x01, 93, 184, 216, 34, 0x00, 0x50])
                .await
                .unwrap();
            let mut reply = [0u8; 10];
            client_side.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[0], 0x05);
            assert_eq!(reply[1], 0x00); // succeeded
        });

        let (host, port) = negotiate(&mut server_side).await.unwrap();
        assert_eq!(host, "93.184.216.34");
        assert_eq!(port, 80);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_connect_with_domain_name() {
        let (mut server_side, mut client_side) = loopback_pair().await;

        let domain = b"example.com";
        let client_task = tokio::spawn(async move {
            client_side.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut method_reply = [0u8; 2];
            client_side.read_exact(&mut method_reply).await.unwrap();

            let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
            req.extend_from_slice(domain);
            req.extend_from_slice(&443u16.to_be_bytes());
            client_side.write_all(&req).await.unwrap();

            let mut reply = [0u8; 10];
            client_side.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 0x00);
        });

        let (host, port) = negotiate(&mut server_side).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn socks4_connect_with_ipv4_address() {
        let (mut server_side, mut client_side) = loopback_pair().await;

        let client_task = tokio::spawn(async move {
            // ver=4, cmd=1(CONNECT), port=80, ip=93.184.216.34, userid="" + NUL
            let mut req = vec![0x04, 0x01];
            req.extend_from_slice(&80u16.to_be_bytes());
            req.extend_from_slice(&[93, 184, 216, 34]);
            req.push(0x00);
            client_side.write_all(&req).await.unwrap();

            let mut reply = [0u8; 8];
            client_side.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 0x5A); // granted
        });

        let (host, port) = negotiate(&mut server_side).await.unwrap();
        assert_eq!(host, "93.184.216.34");
        assert_eq!(port, 80);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn socks4a_connect_with_domain_name() {
        let (mut server_side, mut client_side) = loopback_pair().await;

        let client_task = tokio::spawn(async move {
            // ver=4, cmd=1, port=22, ip=0.0.0.1(SOCKS4a marker), userid="" + NUL, hostname + NUL
            let mut req = vec![0x04, 0x01];
            req.extend_from_slice(&22u16.to_be_bytes());
            req.extend_from_slice(&[0, 0, 0, 1]);
            req.push(0x00);
            req.extend_from_slice(b"example.com");
            req.push(0x00);
            client_side.write_all(&req).await.unwrap();

            let mut reply = [0u8; 8];
            client_side.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 0x5A);
        });

        let (host, port) = negotiate(&mut server_side).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 22);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_version_is_rejected() {
        let (mut server_side, mut client_side) = loopback_pair().await;

        let client_task = tokio::spawn(async move {
            client_side.write_all(&[0x06]).await.unwrap();
        });

        let result = negotiate(&mut server_side).await;
        assert!(matches!(result, Err(SocksError::UnsupportedVersion(0x06))));
        client_task.await.unwrap();
    }
}
