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
static LOG_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn policy_override() -> &'static Mutex<Option<ReconnectPolicy>> {
    POLICY_OVERRIDE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn reconnect_policy_override() -> Option<ReconnectPolicy> {
    *policy_override().lock()
}

fn log_path() -> &'static Mutex<Option<String>> {
    LOG_PATH.get_or_init(|| Mutex::new(None))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn record(event: impl AsRef<str>) {
    let Some(path) = log_path().lock().clone() else { return; };
    let line = format!("{} {}\n", now_millis(), event.as_ref());
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(crate) fn is_enabled() -> bool {
    log_path().lock().is_some()
}

#[uniffi::export]
pub fn debug_set_reconnect_log_path(path: String) {
    *log_path().lock() = Some(path);
}

#[uniffi::export]
pub fn debug_set_reconnect_policy(tick_secs: u32, retry_interval_secs: u32, timeout_secs: u32) {
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
    *policy_override().lock() = None;
    record("debug_clear_reconnect_policy");
}

#[uniffi::export]
pub fn debug_dump_reconnect_log() -> String {
    let Some(path) = log_path().lock().clone() else { return String::new(); };
    read_to_string(path).unwrap_or_default()
}

#[uniffi::export]
pub fn debug_clear_reconnect_log() {
    let Some(path) = log_path().lock().clone() else { return; };
    let _ = remove_file(path);
}
