//! Android実機スパイク用の再接続計測フック。
//!
//! `cfg!(debug_assertions)`には依存しない(Android向けRustライブラリはdebug APKでも
//! `--release`でビルドされるため常にfalseになる)。debug専用であることの担保は
//! `debug_fault.rs`と同じくKotlin側の呼び出し口(`MainActivity.onCreate()`の
//! `BuildConfig.DEBUG`ガード、`android/src/debug`のbroadcast受信口)にある——
//! Rust側はログパスが未設定なら常にno-opという設計で二重に安全側に倒している。
//! ここではRust側イベントをアプリprivate領域へ追記し、adb再接続後にUniFFI経由で
//! dump/clearできるようにする。

use std::fs::{OpenOptions, read_to_string, remove_file};
use std::io::Write;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::orchestrator::{ReconnectPolicy, SessionOrchestrator};

static POLICY_OVERRIDE: OnceLock<Mutex<Option<ReconnectPolicy>>> = OnceLock::new();
static LOG_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static ORCHESTRATORS: OnceLock<Mutex<Vec<Weak<SessionOrchestrator>>>> = OnceLock::new();

fn policy_override() -> &'static Mutex<Option<ReconnectPolicy>> {
    POLICY_OVERRIDE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn reconnect_policy_override() -> Option<ReconnectPolicy> {
    *policy_override().lock()
}

/// `create_session_orchestrator`から呼ばれ、新しいorchestratorをレジストリへ
/// 弱参照で登録する。`debug_set_reconnect_policy`/`debug_clear_reconnect_policy`が
/// 生きている全セッションへ即座に反映するために使う(rust-ssot: セッションの
/// 状態と意思決定はRust側に置き、Kotlin側にミラー状態のレジストリを作らない)。
pub(crate) fn register_orchestrator(o: &Arc<SessionOrchestrator>) {
    let list = ORCHESTRATORS.get_or_init(|| Mutex::new(Vec::new()));
    let mut list = list.lock();
    list.retain(|w| w.strong_count() > 0);
    list.push(Arc::downgrade(o));
}

fn for_each_live_orchestrator(f: impl Fn(&Arc<SessionOrchestrator>)) {
    let Some(list) = ORCHESTRATORS.get() else { return };
    let list = list.lock();
    for weak in list.iter() {
        if let Some(o) = weak.upgrade() {
            f(&o);
        }
    }
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
    *log_path().lock() = Some(path.clone());
    // 有効化された瞬間そのものを記録する(record()自体がis_enabled()経由で
    // no-opになる窓と区別するため、パス設定直後にここで書く)。
    record(format!("log_enabled path={path}"));
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
    // 実行中の(=既にorchestrator生成済みの)セッションにも次のtickを待たず
    // 即座に反映する。次のspawn_reconnect_loopのtickごとの読み直しと合わせて、
    // 「アプリ起動→接続→policy上書き」という自然な操作順序でも空振りしない。
    for_each_live_orchestrator(|o| o.apply_reconnect_policy_override());
}

#[uniffi::export]
pub fn debug_clear_reconnect_policy() {
    *policy_override().lock() = None;
    record("debug_clear_reconnect_policy");
    for_each_live_orchestrator(|o| o.apply_reconnect_policy_override());
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
