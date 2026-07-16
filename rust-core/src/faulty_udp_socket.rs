//! `noq::AsyncUdpSocket` を包み、UDP データグラム単位でパケットロス・遅延・
//! 完全断をシミュレートする。デバッグ用フォルトインジェクションのため
//! 本番コード（`isekai_pipe_quic_transport.rs`）にも配線されているが、
//! `debug_fault::shared_injector()` の既定値（遅延0・ロス0・cut無し）では
//! 素通しのラッパーとして動作し、通常利用時の挙動には影響しない。
//! 有効化は `debug_fault.rs` が export する `debug_set_udp_fault_*` 関数
//! （UniFFI 経由で Kotlin から呼べる）を介してのみ行う。
//!
//! `faulty_stream.rs` は QUIC ストリーム確立後のアプリ層バイトを遅延させる
//! だけなので、実際に noq のパス検証・コネクションマイグレーション判定
//! （RFC 9000 §9）には一切影響しない。本モジュールは noq が実際に UDP
//! データグラムを送受信する層そのものをラップするため、ロス率や遅延を
//! 上げたときに noq の輻輳制御・PTO 再送・パス検証が実際にどう振る舞う
//! かを検証できる。
//!
//! さらに `noq::Endpoint::rebind_abstract()`（noq 自身のテストスイートも使う
//! 手法）と組み合わせれば、ローカル環境だけで「ネットワーク切り替え」を
//! 再現できる。クライアントのエンドポイントを新しい `FaultyUdpSocket` に
//! rebind すると、noq-proto から見てローカルアドレスが変わったのと等価
//! になり、実機で Wi-Fi→5G/Tailscale に切り替わったときと同じパス検証・
//! マイグレーション処理が走る。
//!
//! GRO（1 バッファに複数データグラムが詰まっている場合）は簡略化のため
//! バッファ単位でロス/遅延を判定する（個々のデータグラムには分割しない）。
//! テスト用途としては十分だが、本番の高スループット経路には使わない。
//! (`multipath_transport.rs`の`MultiUdpSocket`と同様、素の`tokio::net::UdpSocket`
//! をラップするため、そもそも1回の`poll_recv`で1データグラムしか扱わない。)

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};
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

/// `inner` を包み、`UdpFaultInjector` の設定に従って各データグラムの送受信に
/// 遅延・ロス・完全断を適用する。`noq::Endpoint::new_with_abstract_socket`
/// または `Endpoint::rebind_abstract` にそのまま渡せる。
///
/// `poll_recv` が `&mut self` を取る（noqのAsyncUdpSocketの流儀）ため、
/// 受信側の遅延キュー(`pending_recv`)はMutex無しの単純な`VecDeque`でよい。
pub(crate) struct FaultyUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
    injector: UdpFaultInjector,
    pending_recv: VecDeque<PendingRecv>,
    /// 遅延キューの解放時刻に自分自身を再起床させるためのタイマー。
    /// `poll_recv`のローカル変数として`Sleep`を作って1回pollしてすぐdropすると、
    /// dropの時点でタイマーホイールへの登録も解除されてしまい、実際には一度も
    /// 発火せず永遠に再起床しない（新規データグラムが来ない限りpollされない）
    /// というバグになる。フィールドとして`poll_recv`呼び出しをまたいで保持する
    /// ことで、登録したwakerが本当に発火するまで生き残るようにする。
    wake_timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl fmt::Debug for FaultyUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultyUdpSocket").finish_non_exhaustive()
    }
}

impl FaultyUdpSocket {
    pub(crate) fn new(inner: Arc<tokio::net::UdpSocket>, injector: UdpFaultInjector) -> Self {
        Self {
            inner,
            injector,
            pending_recv: VecDeque::new(),
            wake_timer: None,
        }
    }
}

/// `create_sender`が呼ばれるたびに払い出される送信専用ハンドル。ソケット本体
/// (`FaultyUdpSocket`)とは独立して`Pin<Box<dyn UdpSender>>`として保持されるため、
/// 遅延送信はこのハンドル自身が`tokio::spawn`でバックグラウンド化する
/// （`multipath_transport.rs`の`MultiUdpSender`と同じ方針）。
struct FaultySender {
    inner: Arc<tokio::net::UdpSocket>,
    injector: UdpFaultInjector,
}

impl fmt::Debug for FaultySender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FaultySender")
    }
}

impl UdpSender for FaultySender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.injector.is_cut() {
            // 電波圏外相当: 送信は成功したように振る舞いつつ実際には破棄する
            // (実ネットワークでも送信側はロスを検知できない)。
            return Poll::Ready(Ok(()));
        }
        if self.injector.should_drop() {
            return Poll::Ready(Ok(()));
        }
        let delay = self.injector.latency();
        if delay.is_zero() {
            return match self.inner.poll_send_to(cx, transmit.contents, transmit.destination) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            };
        }
        let sock = self.inner.clone();
        let contents = transmit.contents.to_vec();
        let destination = transmit.destination;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = sock.send_to(&contents, destination).await;
        });
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for FaultyUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(FaultySender {
            inner: self.inner.clone(),
            injector: self.injector.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            // 1. 解放時刻を過ぎた遅延データグラムがあれば優先的に返す。
            if matches!(self.pending_recv.front(), Some(p) if p.release_at <= Instant::now()) {
                let item = self.pending_recv.pop_front().unwrap();
                self.wake_timer = None;
                let n = item.data.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&item.data[..n]);
                let mut m = item.meta;
                m.len = n;
                m.stride = n;
                meta[0] = m;
                return Poll::Ready(Ok(1));
            }

            let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
            match self.inner.poll_recv_from(cx, &mut read_buf) {
                Poll::Pending => {
                    // 遅延キューに未解放分が残っていれば、その解放時刻に
                    // 自分自身を再起床させるタイマーを登録しておく(新規データ
                    // グラムが二度と来ない場合、inner の readiness だけでは
                    // 再起床できないため)。`wake_timer`はフィールドとして
                    // `poll_recv`呼び出しをまたいで保持し、登録したwakerが
                    // 実際に発火するまで生かしておく(ローカル変数のSleepを
                    // 即dropすると発火前にタイマー登録ごと解除されてしまう)。
                    match self.pending_recv.front() {
                        Some(p) => {
                            let wait = p.release_at.saturating_duration_since(Instant::now());
                            let timer =
                                self.wake_timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(wait)));
                            let _ = timer.as_mut().poll(cx);
                        }
                        None => self.wake_timer = None,
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(addr)) => {
                    if self.injector.is_cut() {
                        continue; // 電波圏外相当: 受信データを全て破棄して再 poll
                    }
                    if self.injector.should_drop() {
                        continue;
                    }
                    let len = read_buf.filled().len();
                    let mut m = RecvMeta::default();
                    m.addr = addr;
                    m.len = len;
                    m.stride = len;
                    let delay = self.injector.latency();
                    if delay.is_zero() {
                        meta[0] = m;
                        return Poll::Ready(Ok(1));
                    }
                    self.pending_recv.push_back(PendingRecv {
                        release_at: Instant::now() + delay,
                        data: bufs[0][..len].to_vec(),
                        meta: m,
                    });
                    continue; // 遅延キュー行き → 再度 inner を poll
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// バインドアドレスから `FaultyUdpSocket` を作る。呼び出し側で `Box::new()` して
/// `noq::Endpoint::new_with_abstract_socket` / `Endpoint::rebind_abstract` の
/// 引数にそのまま渡せる。
pub(crate) fn bind_faulty_udp_socket(
    bind_addr: SocketAddr,
    injector: UdpFaultInjector,
) -> io::Result<FaultyUdpSocket> {
    let std_socket = std::net::UdpSocket::bind(bind_addr)?;
    std_socket.set_nonblocking(true)?;
    let inner = Arc::new(tokio::net::UdpSocket::from_std(std_socket)?);
    Ok(FaultyUdpSocket::new(inner, injector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use std::net::Ipv4Addr;

    /// noqの既定`max_idle_timeout`(30秒)は通常十分だが、このワーカーは複数の
    /// `claude`エージェント/Gradleデーモンが同時稼働することが常態化しており
    /// (load averageが7を超えることも観測済み)、単発のQUIC往復が
    /// `ConnectionLost(TimedOut)`で失敗することを確認した——注入した遅延/ロス
    /// (最大でも50ms/10%程度)自体が原因ではなく、CPUスケジューリング待ちの間に
    /// 相手からの応答が来ないまま既定のidle timeoutへ近づいてしまうため。
    /// 本番の`quic_transport::build_client_config`が同じ理由で
    /// `max_idle_timeout(300s)`にしているのに合わせ、テストでも現実的な余裕を持たせる
    /// (このファイルの各テストは自前で`tokio::time::timeout`によるタイトな期限を
    /// 併用しているため、コネクション自体を長生きさせても「本当のバグを見逃す」
    /// 方向には倒れない)。
    fn generous_transport_config() -> noq::TransportConfig {
        let mut transport = noq::TransportConfig::default();
        // quic_transport::build_client_config(本番のtsshd QUIC transport)と同じ300秒に
        // 揃える。120秒でもこのマシンの一番重い瞬間には足りなかったため。
        transport.max_idle_timeout(Some(Duration::from_secs(300).try_into().unwrap()));
        transport
    }

    fn server_endpoint() -> (noq::Endpoint, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der.into())
            .unwrap();
        let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap();
        let mut server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
        server_config.transport_config(Arc::new(generous_transport_config()));
        let endpoint = noq::Endpoint::server(
            server_config,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        )
        .unwrap();
        (endpoint, cert_der.to_vec())
    }

    fn client_config_trusting(cert_der: &[u8]) -> noq::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(cert_der.to_vec())).unwrap();
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
        let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
        client_config.transport_config(Arc::new(generous_transport_config()));
        client_config
    }

    fn faulty_client_endpoint(injector: UdpFaultInjector, client_config: noq::ClientConfig) -> noq::Endpoint {
        let socket = bind_faulty_udp_socket(
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            injector,
        )
        .unwrap();
        let endpoint = noq::Endpoint::new_with_abstract_socket(
            noq::EndpointConfig::default(),
            None,
            Box::new(socket),
            Arc::new(noq::TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(client_config);
        endpoint
    }

    #[tokio::test]
    async fn connects_and_exchanges_data_under_loss_and_latency() {
        let _ = rustls::crypto::ring::default_provider().install_default();
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

    /// 2026-07-16: 300秒の`generous_transport_config`(本番`quic_transport::
    /// build_client_config`と同じ値)を使っていても、CI(GitHub-hosted
    /// `ubuntu-24.04`)の重い瞬間には1回のpost-rebind往復がその300秒フルに
    /// 達して`ConnectionLost(TimedOut)`になることを確認した
    /// (rebind前の疎通・注入したロス率10%自体は原因ではない——300秒はPTO
    /// バックオフだけで説明できる長さではなく、スケジューリング待ちの累積)。
    /// QUIC接続は一度idle timeoutすると恒久的に死ぬ(RFC 9000)ため、同じ
    /// `conn`へのリトライは無意味。かといってこのテストの`max_idle_timeout`
    /// だけを300秒より延ばすと、本番設定と意図的に揃えている値から乖離して
    /// しまう(`generous_transport_config`のdoc参照)。
    ///
    /// リトライ自体はテスト本文に埋め込まず、CIランナー側(`cargo-nextest`の
    /// per-test override、`rust-core/.config/nextest.toml`参照)に寄せてある
    /// ——このモジュール(`faulty_udp_socket::tests::*`)は既知のCPU競合起因
    /// flakyテスト群として`retries`が設定されているため、シナリオ全体
    /// (新規server/client/connection)がnextestによって最初から作り直されて
    /// 再試行される。各段階を短いタイムアウト(`REBIND_SURVIVAL_ROUND_TRIP_
    /// TIMEOUT`)で区切ってあるのは、テスト本文の外でリトライしていても
    /// 「どの段階で詰まったか」の診断性を保つため。
    #[tokio::test]
    async fn rebind_to_new_faulty_socket_survives_as_network_switch() {
        run_rebind_survival_scenario().await.unwrap();
    }

    /// 各往復のタイムアウト上限。接続の`max_idle_timeout`(300秒)より大幅に
    /// 短く取り、詰まった箇所を早期に特定できるようにする(通常時の往復は
    /// 注入遅延込みでも数十msオーダーなので、実用上は誤検知しない余裕)。
    const REBIND_SURVIVAL_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(30);

    async fn run_rebind_survival_scenario() -> Result<(), String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // noq 自身のテストスイートと同じ手法で、クライアント側エンドポイントを
        // 新しいローカルソケットに rebind する。noq-proto から見るとローカル
        // アドレスの変化であり、実機で Wi-Fi → 5G/Tailscale に切り替わった
        // ときと同じパス検証が走る。
        let (server, cert_der) = server_endpoint();
        let server_addr = server.local_addr().map_err(|e| format!("server_addr: {e:?}"))?;

        tokio::spawn(async move {
            let Some(incoming) = server.accept().await else { return };
            let Ok(conn) = incoming.await else { return };
            loop {
                let Ok((mut send, mut recv)) = conn.accept_bi().await else { break };
                let mut buf = [0u8; 5];
                if recv.read_exact(&mut buf).await.is_err() {
                    break;
                }
                if send.write_all(&buf).await.is_err() {
                    break;
                }
                let _ = send.finish();
            }
        });

        let injector_a = UdpFaultInjector::new();
        injector_a.set_latency(Duration::from_millis(10));
        let client = faulty_client_endpoint(injector_a, client_config_trusting(&cert_der));

        let conn = client
            .connect(server_addr, "localhost")
            .map_err(|e| format!("connect: {e:?}"))?
            .await
            .map_err(|e| format!("connect await: {e:?}"))?;

        // 「ネットワーク A」経由での疎通確認
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi (A): {e:?}"))?;
        send.write_all(b"neta1").await.map_err(|e| format!("write (A): {e:?}"))?;
        send.finish().map_err(|e| format!("finish (A): {e:?}"))?;
        let mut buf = [0u8; 5];
        tokio::time::timeout(REBIND_SURVIVAL_ROUND_TRIP_TIMEOUT, recv.read_exact(&mut buf))
            .await
            .map_err(|_| "read (A) timed out".to_string())?
            .map_err(|e| format!("read (A): {e:?}"))?;
        assert_eq!(&buf, b"neta1");

        let exporter_before = {
            let mut e = [0u8; 32];
            conn.export_keying_material(&mut e, b"roaming-test", b"")
                .map_err(|e| format!("export_keying_material (before): {e:?}"))?;
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
        .map_err(|e| format!("bind_faulty_udp_socket: {e:?}"))?;
        client
            .rebind_abstract(Box::new(new_socket))
            .map_err(|e| format!("rebind_abstract: {e:?}"))?;

        // rebind 後も同一コネクションとして通信が続くことを確認する
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi (B): {e:?}"))?;
        send.write_all(b"netb1").await.map_err(|e| format!("write (B): {e:?}"))?;
        send.finish().map_err(|e| format!("finish (B): {e:?}"))?;
        let mut buf = [0u8; 5];
        tokio::time::timeout(REBIND_SURVIVAL_ROUND_TRIP_TIMEOUT, recv.read_exact(&mut buf))
            .await
            .map_err(|_| "read (B, post-rebind) timed out".to_string())?
            .map_err(|e| format!("read (B, post-rebind): {e:?}"))?;
        assert_eq!(&buf, b"netb1");

        let exporter_after = {
            let mut e = [0u8; 32];
            conn.export_keying_material(&mut e, b"roaming-test", b"")
                .map_err(|e| format!("export_keying_material (after): {e:?}"))?;
            e
        };
        assert_eq!(
            exporter_before, exporter_after,
            "rebind はマイグレーションであり再接続ではないため exporter は不変のはず"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cut_causes_connection_to_stall_then_recover_after_restore() {
        let _ = rustls::crypto::ring::default_provider().install_default();
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
