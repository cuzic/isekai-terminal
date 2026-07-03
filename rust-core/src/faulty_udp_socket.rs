//! `quinn::AsyncUdpSocket` を包み、UDP データグラム単位でパケットロス・遅延・
//! 完全断をシミュレートする。デバッグ用フォルトインジェクションのため
//! 本番コード（`helper_quic_transport.rs`）にも配線されているが、
//! `debug_fault::shared_injector()` の既定値（遅延0・ロス0・cut無し）では
//! 素通しのラッパーとして動作し、通常利用時の挙動には影響しない。
//! 有効化は `debug_fault.rs` が export する `debug_set_udp_fault_*` 関数
//! （UniFFI 経由で Kotlin から呼べる）を介してのみ行う。
//!
//! `faulty_stream.rs` は QUIC ストリーム確立後のアプリ層バイトを遅延させる
//! だけなので、実際に quinn のパス検証・コネクションマイグレーション判定
//! （RFC 9000 §9）には一切影響しない。本モジュールは quinn が実際に UDP
//! データグラムを送受信する層そのものをラップするため、ロス率や遅延を
//! 上げたときに quinn の輻輳制御・PTO 再送・パス検証が実際にどう振る舞う
//! かを検証できる。
//!
//! さらに `quinn::Endpoint::rebind()`（quinn 自身のテストスイートも使う
//! 手法）と組み合わせれば、ローカル環境だけで「ネットワーク切り替え」を
//! 再現できる。クライアントのエンドポイントを新しい `FaultyUdpSocket` に
//! rebind すると、quinn-proto から見てローカルアドレスが変わったのと等価
//! になり、実機で Wi-Fi→5G/Tailscale に切り替わったときと同じパス検証・
//! マイグレーション処理が走る。
//!
//! GRO（1 バッファに複数データグラムが詰まっている場合）は簡略化のため
//! バッファ単位でロス/遅延を判定する（個々のデータグラムには分割しない）。
//! テスト用途としては十分だが、本番の高スループット経路には使わない。

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use quinn::udp::{EcnCodepoint, RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use rand::Rng;

struct FaultState {
    latency_ms: AtomicU64,
    loss_permille: AtomicU64,
    cut: AtomicBool,
}

/// `FaultyUdpSocket` を操作するハンドル。クローンして保持すれば、接続中でも
/// 遅延・ロス率・完全断をテスト側から動的に変更できる。
#[derive(Clone)]
pub(crate) struct UdpFaultInjector {
    state: Arc<FaultState>,
}

impl UdpFaultInjector {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(FaultState {
                latency_ms: AtomicU64::new(0),
                loss_permille: AtomicU64::new(0),
                cut: AtomicBool::new(false),
            }),
        }
    }

    /// 送受信データグラムそれぞれに加える片道遅延。
    pub(crate) fn set_latency(&self, latency: Duration) {
        self.state.latency_ms.store(latency.as_millis() as u64, Ordering::Relaxed);
    }

    /// 0.0〜1.0 のパケットロス率。ロスしたデータグラムはバイトも欠落し、
    /// 相手には届かない（faulty_stream.rs と異なり実際に破棄する）。
    pub(crate) fn set_loss_rate(&self, rate: f64) {
        let permille = (rate.clamp(0.0, 1.0) * 1000.0).round() as u64;
        self.state.loss_permille.store(permille, Ordering::Relaxed);
    }

    /// 完全なネットワーク断（電波圏外相当）。送受信とも全データグラムを破棄する。
    pub(crate) fn cut(&self) {
        self.state.cut.store(true, Ordering::Relaxed);
    }

    pub(crate) fn restore(&self) {
        self.state.cut.store(false, Ordering::Relaxed);
    }

    /// Phase 9-5: `multipath_transport.rs`のnoq用ソケットも同じinjectorを再利用する
    /// ため`pub(crate)`にしてある（元は`FaultyUdpSocket`内部専用のprivateだった）。
    pub(crate) fn is_cut(&self) -> bool {
        self.state.cut.load(Ordering::Relaxed)
    }

    pub(crate) fn should_drop(&self) -> bool {
        let permille = self.state.loss_permille.load(Ordering::Relaxed);
        permille > 0 && rand::thread_rng().gen_range(0..1000) < permille
    }

    pub(crate) fn latency(&self) -> Duration {
        Duration::from_millis(self.state.latency_ms.load(Ordering::Relaxed))
    }
}

struct PendingRecv {
    release_at: Instant,
    data: Vec<u8>,
    meta: RecvMeta,
}

struct OwnedTransmit {
    destination: SocketAddr,
    ecn: Option<EcnCodepoint>,
    contents: Vec<u8>,
    segment_size: Option<usize>,
    src_ip: Option<std::net::IpAddr>,
}

/// `inner` を包み、`UdpFaultInjector` の設定に従って各データグラムの送受信に
/// 遅延・ロス・完全断を適用する。`quinn::Endpoint::new_with_abstract_socket`
/// または `Endpoint::rebind_abstract` にそのまま渡せる。
pub(crate) struct FaultyUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    injector: UdpFaultInjector,
    pending_recv: Mutex<VecDeque<PendingRecv>>,
    send_tx: tokio::sync::mpsc::UnboundedSender<(OwnedTransmit, Duration)>,
}

impl fmt::Debug for FaultyUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultyUdpSocket").finish_non_exhaustive()
    }
}

impl FaultyUdpSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>, injector: UdpFaultInjector) -> Arc<Self> {
        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_send_pump(inner.clone(), send_rx);
        Arc::new(Self {
            inner,
            injector,
            pending_recv: Mutex::new(VecDeque::new()),
            send_tx,
        })
    }
}

/// 遅延送信キューを消費し、指定時間待ってから実際にソケットへ送出する
/// バックグラウンドタスク。ベストエフォート（`WouldBlock` は諦める）で
/// よい: テスト用途であり、実測スループットの精度は求めない。
fn spawn_send_pump(
    inner: Arc<dyn AsyncUdpSocket>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(OwnedTransmit, Duration)>,
) {
    tokio::spawn(async move {
        while let Some((t, delay)) = rx.recv().await {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let transmit = Transmit {
                destination: t.destination,
                ecn: t.ecn,
                contents: &t.contents,
                segment_size: t.segment_size,
                src_ip: t.src_ip,
            };
            let _ = inner.try_send(&transmit);
        }
    });
}

impl AsyncUdpSocket for FaultyUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if self.injector.is_cut() {
            // 電波圏外相当: 送信は成功したように振る舞いつつ実際には破棄する
            // (実ネットワークでも送信側はロスを検知できない)。
            return Ok(());
        }
        if self.injector.should_drop() {
            return Ok(());
        }
        let delay = self.injector.latency();
        if delay.is_zero() {
            return self.inner.try_send(transmit);
        }
        let owned = OwnedTransmit {
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents: transmit.contents.to_vec(),
            segment_size: transmit.segment_size,
            src_ip: transmit.src_ip,
        };
        let _ = self.send_tx.send((owned, delay));
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            // 1. 解放時刻を過ぎた遅延データグラムがあれば優先的に返す。
            {
                let mut pending = self.pending_recv.lock().unwrap();
                if matches!(pending.front(), Some(p) if p.release_at <= Instant::now()) {
                    let item = pending.pop_front().unwrap();
                    drop(pending);
                    let n = item.data.len().min(bufs[0].len());
                    bufs[0][..n].copy_from_slice(&item.data[..n]);
                    let mut m = item.meta;
                    m.len = n;
                    m.stride = n;
                    meta[0] = m;
                    return Poll::Ready(Ok(1));
                }
            }

            match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Pending => {
                    // 遅延キューに未解放分が残っていれば、その解放時刻でも
                    // 再度 poll されるようタイマーを登録しておく
                    // (新規データグラムが二度と来ない場合、inner の readiness
                    // だけでは再起床できないため)。
                    let wait = {
                        let pending = self.pending_recv.lock().unwrap();
                        pending.front().map(|p| p.release_at.saturating_duration_since(Instant::now()))
                    };
                    if let Some(wait) = wait {
                        let sleep = tokio::time::sleep(wait);
                        tokio::pin!(sleep);
                        let _ = sleep.poll(cx);
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(n)) => {
                    if self.injector.is_cut() {
                        continue; // 電波圏外相当: 受信データを全て破棄して再 poll
                    }
                    let mut immediate: Option<usize> = None;
                    for i in 0..n {
                        if self.injector.should_drop() {
                            continue;
                        }
                        let delay = self.injector.latency();
                        if delay.is_zero() && immediate.is_none() {
                            immediate = Some(i);
                            continue;
                        }
                        let len = meta[i].len;
                        self.pending_recv.lock().unwrap().push_back(PendingRecv {
                            release_at: Instant::now() + delay,
                            data: bufs[i][..len].to_vec(),
                            meta: meta[i],
                        });
                    }
                    match immediate {
                        Some(0) => return Poll::Ready(Ok(1)),
                        Some(i) => {
                            let len = meta[i].len;
                            let mut copied = vec![0u8; len];
                            copied.copy_from_slice(&bufs[i][..len]);
                            bufs[0][..len].copy_from_slice(&copied);
                            let mut m = meta[i];
                            m.len = len;
                            m.stride = len;
                            meta[0] = m;
                            return Poll::Ready(Ok(1));
                        }
                        None => continue, // 全件ドロップ/遅延キュー行き → 再度 inner を poll
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// バインドアドレスから `FaultyUdpSocket` を作り、tokio ランタイムに包んで返す。
/// `quinn::Endpoint::new_with_abstract_socket` / `Endpoint::rebind_abstract` の
/// 引数にそのまま渡せる。
pub(crate) fn bind_faulty_udp_socket(
    bind_addr: SocketAddr,
    injector: UdpFaultInjector,
) -> io::Result<Arc<FaultyUdpSocket>> {
    let std_socket = std::net::UdpSocket::bind(bind_addr)?;
    std_socket.set_nonblocking(true)?;
    let runtime = quinn::TokioRuntime;
    let inner = quinn::Runtime::wrap_udp_socket(&runtime, std_socket)?;
    Ok(FaultyUdpSocket::new(inner, injector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use std::net::Ipv4Addr;

    fn server_endpoint() -> (quinn::Endpoint, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der.into())
            .unwrap();
        let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap();
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
        let endpoint = quinn::Endpoint::server(
            server_config,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        )
        .unwrap();
        (endpoint, cert_der.to_vec())
    }

    fn client_config_trusting(cert_der: &[u8]) -> quinn::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(cert_der.to_vec())).unwrap();
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
        quinn::ClientConfig::new(Arc::new(quic_crypto))
    }

    fn faulty_client_endpoint(injector: UdpFaultInjector, client_config: quinn::ClientConfig) -> quinn::Endpoint {
        let socket = bind_faulty_udp_socket(
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            injector,
        )
        .unwrap();
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(client_config);
        endpoint
    }

    #[tokio::test]
    async fn connects_and_exchanges_data_under_loss_and_latency() {
        let (server, cert_der) = server_endpoint();
        let server_addr = server.local_addr().unwrap();

        tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let mut buf = [0u8; 5];
            recv.read_exact(&mut buf).await.unwrap();
            send.write_all(&buf).await.unwrap();
            send.finish().unwrap();
            // テストが読み切るまで接続を保持する。20% ロス下では PTO 再送の
            // 積み重ねでハンドシェイク〜データ交換完了に数秒かかることが
            // あるため、2 秒では時々間に合わずフレーキーだった（実測で確認）。
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let injector = UdpFaultInjector::new();
        injector.set_latency(Duration::from_millis(20));
        injector.set_loss_rate(0.2);
        let client = faulty_client_endpoint(injector, client_config_trusting(&cert_der));

        let conn = client
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"hello").await.unwrap();
        send.finish().unwrap();

        let mut buf = [0u8; 5];
        recv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn rebind_to_new_faulty_socket_survives_as_network_switch() {
        // quinn 自身のテストスイート (tests.rs: rebind_recv) と同じ手法で、
        // クライアント側エンドポイントを新しいローカルソケットに rebind する。
        // quinn-proto から見るとローカルアドレスの変化であり、実機で
        // Wi-Fi → 5G/Tailscale に切り替わったときと同じパス検証が走る。
        let (server, cert_der) = server_endpoint();
        let server_addr = server.local_addr().unwrap();

        tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            loop {
                let Ok((mut send, mut recv)) = conn.accept_bi().await else { break };
                let mut buf = [0u8; 5];
                if recv.read_exact(&mut buf).await.is_err() {
                    break;
                }
                send.write_all(&buf).await.unwrap();
                send.finish().unwrap();
            }
        });

        let injector_a = UdpFaultInjector::new();
        injector_a.set_latency(Duration::from_millis(10));
        let client = faulty_client_endpoint(injector_a, client_config_trusting(&cert_der));

        let conn = client
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();

        // 「ネットワーク A」経由での疎通確認
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"neta1").await.unwrap();
        send.finish().unwrap();
        let mut buf = [0u8; 5];
        recv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"neta1");

        let exporter_before = {
            let mut e = [0u8; 32];
            conn.export_keying_material(&mut e, b"roaming-test", b"").unwrap();
            e
        };

        // 環境悪化を模したうえで「ネットワーク B」へ切り替える
        let injector_b = UdpFaultInjector::new();
        injector_b.set_latency(Duration::from_millis(50));
        injector_b.set_loss_rate(0.1);
        let new_socket = bind_faulty_udp_socket(
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            injector_b,
        )
        .unwrap();
        client.rebind_abstract(new_socket).unwrap();

        // rebind 後も同一コネクションとして通信が続くことを確認する
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"netb1").await.unwrap();
        send.finish().unwrap();
        let mut buf = [0u8; 5];
        recv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"netb1");

        let exporter_after = {
            let mut e = [0u8; 32];
            conn.export_keying_material(&mut e, b"roaming-test", b"").unwrap();
            e
        };
        assert_eq!(
            exporter_before, exporter_after,
            "rebind はマイグレーションであり再接続ではないため exporter は不変のはず"
        );
    }

    #[tokio::test]
    async fn cut_causes_connection_to_stall_then_recover_after_restore() {
        let (server, cert_der) = server_endpoint();
        let server_addr = server.local_addr().unwrap();

        tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            loop {
                let Ok((mut send, mut recv)) = conn.accept_bi().await else { break };
                let mut buf = [0u8; 5];
                if recv.read_exact(&mut buf).await.is_err() {
                    break;
                }
                send.write_all(&buf).await.unwrap();
                send.finish().unwrap();
            }
        });

        let injector = UdpFaultInjector::new();
        let client = faulty_client_endpoint(injector.clone(), client_config_trusting(&cert_der));
        let conn = client.connect(server_addr, "localhost").unwrap().await.unwrap();

        injector.cut();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        // cut 中は相手に届かないはずなので、応答が来ないことをタイムアウトで確認する
        send.write_all(b"lost!").await.unwrap();
        send.finish().unwrap();

        let (mut send2, mut recv2) = conn.open_bi().await.unwrap();
        let timed_out = tokio::time::timeout(Duration::from_millis(300), async {
            send2.write_all(b"nop!!").await.unwrap();
            send2.finish().unwrap();
            let mut buf = [0u8; 5];
            recv2.read_exact(&mut buf).await.unwrap();
        })
        .await
        .is_err();
        assert!(timed_out, "cut 中は応答が届かないはず");

        injector.restore();
        let (mut send3, mut recv3) = conn.open_bi().await.unwrap();
        send3.write_all(b"back!").await.unwrap();
        send3.finish().unwrap();
        let mut buf = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(5), recv3.read_exact(&mut buf))
            .await
            .expect("restore 後は復旧するはず")
            .unwrap();
        assert_eq!(&buf, b"back!");
    }
}
