//! [`crate::rebind_manager::RebindManager`](純粋状態機械)が返す
//! [`crate::rebind_manager::RebindAction`]を実際に実行するためのI/Oポート
//! (trait)定義。実装は#22のDriver(`multipath_transport.rs`に置く予定)が持つ
//! ——ここでは型だけを定義し、`RebindManager`自身はこれらのtraitに一切
//! 依存しない(`timed_fsm`同様、zero I/Oのまま保つため)。
//!
//! 各メソッドは`resume_client.rs`の`ByteHalfRead`/`ByteHalfWrite`と同じ
//! native async-fn-in-trait(`impl Future<...> + Send`)記法を使う。このcrate
//! では`async-trait`マクロは外部trait([`russh::client::Handler`]等)を実装する
//! ときだけ使っており、内部専用traitはRPITITで済ませる慣習になっている。

use std::future::Future;
use std::net::IpAddr;
use std::os::fd::RawFd;

/// WiFi/セルラーいずれかの物理インターフェースに明示的にバインドされたfd。
///
/// # fd所有権ポリシー
///
/// 疎通確認用と本番rebind用のfdは、たとえ同じ理由(WiFiへの復帰)のために
/// 取得する場合でも常に**別々に**[`PlatformFdSource`]から新規取得する。
/// 一時的な疎通確認に使ったfdをそのまま本番の[`RebindExecutor::rebind`]へ
/// 渡そうとすると、疎通確認用に作った一時`noq::Endpoint`がそのfdの所有権を
/// 握ってしまい競合する(#10のタスク設計メモ参照)。このポリシーにより、
/// fdの受け渡し・所有権追跡自体が不要になる。
#[derive(Debug)]
pub(crate) struct BoundFd {
    pub(crate) fd: RawFd,
    pub(crate) local_ip: IpAddr,
}

/// Kotlin/Swiftへ「WiFi-bound fdをくれ」「セルラー-bound fdをくれ」を要求する。
///
/// 既存の`OrchestratorCallback`(`orchestrator.rs`)と同じ、UniFFI callback越しの
/// 実装を想定する。実装側:
/// - Android: `PhysicalPathProvider.acquireWifiOnly()`/`acquireCellularOnly()`
///   (#20で追加)
/// - iOS: `IP_BOUND_IF`/`IPV6_BOUND_IF`ベースの新規コンポーネント(#15)
///
/// 判断ロジックは一切持たない(`rust-ssot.md`準拠) —— 呼ばれたら要求された
/// 種類のfdを取得して返すだけ。
pub(crate) trait PlatformFdSource: Send + Sync {
    /// WiFi-bound fdを1本取得する。取得できなければ`None`
    /// (WiFi自体が使えない・権限が無い等)。
    fn acquire_wifi_fd(&self) -> impl Future<Output = Option<BoundFd>> + Send;

    /// セルラー-bound fdを1本取得する。取得できなければ`None`。
    fn acquire_cellular_fd(&self) -> impl Future<Output = Option<BoundFd>> + Send;
}

/// 与えられたfdで実際にWiFiの疎通確認を行う
/// (一時`noq::Endpoint`を作ってpingする実装を#22で行う)。
pub(crate) trait WifiProbeExecutor: Send + Sync {
    /// 疎通確認できれば`true`。`fd`の所有権はこのメソッドに移り、
    /// 呼び出し後は破棄してよい(本番rebindに使い回さない、上記ポリシー参照)。
    fn probe(&self, fd: BoundFd) -> impl Future<Output = bool> + Send;
}

/// 与えられたfdで実際に`Endpoint::rebind_abstract()`
/// (既存の`MultipathIsekaiPipeQuicSession::rebind_to_fd`と同じ経路)を呼ぶ。
pub(crate) trait RebindExecutor: Send + Sync {
    fn rebind(&self, fd: BoundFd);
}
