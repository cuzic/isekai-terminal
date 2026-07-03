//! 実機検証用: `helper_quic_transport.rs` の QUIC クライアントソケットに
//! 注入するフォルト（遅延・ロス・完全断）を、アプリ実行中に Kotlin 側から
//! 動的に切り替えるための UniFFI エクスポート。
//!
//! 既定値（遅延0・ロス率0・cut無し）では `FaultyUdpSocket` は素通しの
//! ラッパーとして動作するため、これらの関数を一度も呼ばない限り通常利用時
//! の挙動には一切影響しない。Kotlin 側は `app/src/debug` ソースセット配下
//! （リリースビルドには含まれない）の `BroadcastReceiver` からのみ呼び出す
//! 想定。adb 経由で `adb shell am broadcast` から遠隔操作できる。

use std::sync::OnceLock;
use std::time::Duration;

use crate::faulty_udp_socket::UdpFaultInjector;

static INJECTOR: OnceLock<UdpFaultInjector> = OnceLock::new();

pub(crate) fn shared_injector() -> UdpFaultInjector {
    INJECTOR.get_or_init(UdpFaultInjector::new).clone()
}

/// `helper_quic` の QUIC クライアントソケットの片道遅延をミリ秒で設定する。
#[uniffi::export]
pub fn debug_set_udp_fault_latency_ms(ms: u32) {
    shared_injector().set_latency(Duration::from_millis(ms as u64));
}

/// パケットロス率を千分率（0〜1000）で設定する。
#[uniffi::export]
pub fn debug_set_udp_fault_loss_permille(permille: u32) {
    shared_injector().set_loss_rate(permille.min(1000) as f64 / 1000.0);
}

/// 完全なネットワーク断（電波圏外相当）を発生させる。
#[uniffi::export]
pub fn debug_cut_udp_fault() {
    shared_injector().cut();
}

/// `debug_cut_udp_fault()` で発生させた完全断を解除する。
#[uniffi::export]
pub fn debug_restore_udp_fault() {
    shared_injector().restore();
}

/// 遅延・ロス・完全断すべてを既定値（無効）へ戻す。
#[uniffi::export]
pub fn debug_clear_udp_fault() {
    let injector = shared_injector();
    injector.set_latency(Duration::ZERO);
    injector.set_loss_rate(0.0);
    injector.restore();
}
