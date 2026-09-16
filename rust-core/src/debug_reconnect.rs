//! Android実機スパイク用の再接続計測フック。
//!
//! Kotlin側の呼び出し口はdebugソースセット配下にだけ置き、通常時は何も呼ばれない。
//! ここではRust側イベントをアプリprivate領域へ追記し、adb再接続後にUniFFI経由で
//! dump/clearできるようにする。

use std::fs::{OpenOptions, read_to_string, remove_file};
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::orchestrator::ReconnectPolicy;

static POLICY_OVERRIDE: OnceLock<Mutex<Option<ReconnectPolicy>>> = OnceLock::new();

fn policy_override() -> &'static Mutex<Option<ReconnectPolicy>> {
    POLICY_OVERRIDE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn reconnect_policy_override() -> Option<ReconnectPolicy> {
    if !cfg!(debug_assertions) {
        return None;
    }
    *policy_override().lock()
}

fn default_log_path() -> String {
    std::env::var("ISEKAI_RECONNECT_DEBUG_LOG")
        .unwrap_or_else(|_| "/data/data/tools.isekai.terminal/files/debug-reconnect-events.log".to_string())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn record(event: impl AsRef<str>) {
    if !cfg!(debug_assertions) {
        return;
    }
    let line = format!("{} {}\n", now_millis(), event.as_ref());
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(default_log_path()) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[uniffi::export]
pub fn debug_set_reconnect_policy(tick_secs: u32, retry_interval_secs: u32, timeout_secs: u32) {
    if !cfg!(debug_assertions) {
        return;
    }
    let policy = ReconnectPolicy {
        tick: Duration::from_secs(tick_secs.max(1) as u64),
        retry_interval: Duration::from_secs(retry_interval_secs.max(1) as u64),
        timeout: Duration::from_secs(timeout_secs.max(1) as u64),
    };
    *policy_override().lock() = Some(policy);
    record(format!(
        "debug_set_reconnect_policy tick_secs={} retry_interval_secs={} timeout_secs={}",
        policy.tick.as_secs(),
        policy.retry_interval.as_secs(),
        policy.timeout.as_secs()
    ));
}

#[uniffi::export]
pub fn debug_clear_reconnect_policy() {
    if !cfg!(debug_assertions) {
        return;
    }
    *policy_override().lock() = None;
    record("debug_clear_reconnect_policy");
}

#[uniffi::export]
pub fn debug_dump_reconnect_log() -> String {
    if !cfg!(debug_assertions) {
        return String::new();
    }
    read_to_string(default_log_path()).unwrap_or_default()
}

#[uniffi::export]
pub fn debug_clear_reconnect_log() {
    if !cfg!(debug_assertions) {
        return;
    }
    let _ = remove_file(default_log_path());
}
