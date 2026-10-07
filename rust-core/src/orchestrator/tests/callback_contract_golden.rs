//! ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 13: Android/iOSのcallback契約golden(Rust側)。
//!
//! orchestratorのシナリオ(Step 8a′の退出経路(a)〜(f)・再接続ループのギブアップ・高速な再接続の連続)を
//! 本番と同じshell経路で走らせ、`OrchestratorCallback`へ届いた列を射影したものを
//! `rust-core/tests/golden/callback_contract/<scenario>.json`と比較する。Kotlin
//! (`android/src/test/.../CallbackContractGoldenReplayTest.kt`)とSwift
//! (`ios/Tests/IsekaiTerminalCoreLogicTests/CallbackContractGoldenReplayTests.swift`)は同じファイルを読み、
//! 各プラットフォームの転送実装へreplayする。
//!
//! - **射影**(ADR Q17): メソッド名・`ConnectionEdge`のvariant・`generation`・公開状態のタグ、と
//!   `Connected{host}`/`Established{host}`のhost(テスト用の固定値。秘密は載せない、§3-3)と
//!   `Established`の`upstream_failover`(#175)。理由文字列・
//!   `Reconnecting`の秒数は載せない。`Reconnecting`の連続(再接続ループがtickごとに出すカウントダウンの
//!   再公開)は1件に畳む(tick方針を変えただけでgoldenが変わらないように)。
//! - **順序**: 全シナリオを`start_paused`のcurrent-threadランタイムで走らせるので、別task(再接続ループ・
//!   debounce)由来の公開も含めて列は決定的。さらに配信は`PublicationQueue`がreducerの適用順に直列化する。
//! - **不一致時**: `<scenario>.actual.json`を書いて失敗する(CIでは`rust-core-test-check.yml`が失敗時に
//!   artifactとして回収する)。内容を確認したうえで`<scenario>.json`としてコミットする(ADR §7)。
//!   ローカルで再生成する場合だけ`ISEKAI_UPDATE_CALLBACK_GOLDEN=1`で上書きする(CIでは設定しない)。

use super::*;
use crate::{ConnectionEdge, ConnectionPublicState};
use std::path::PathBuf;
use std::time::Duration;

/// goldenを上書き更新する明示フラグ。CIでは設定しない(不一致は常に失敗にする)。
const UPDATE_ENV: &str = "ISEKAI_UPDATE_CALLBACK_GOLDEN";

const HOST: &str = "example.com";
const OTHER_HOST: &str = "other.example.com";

/// goldenに含めるシナリオの一覧。ディレクトリにこれ以外の`.json`があれば失敗する(古いgoldenの残骸)。
const SCENARIOS: &[&str] = &[
    "a_user_disconnect",
    "b_transport_error",
    "c_network_lost_debounce",
    "d_manual_connect_while_connected",
    "e_foreground_resume_reconnect",
    "f_reconnect_loop_success",
    "reconnect_gives_up",
    "fast_reconnect_cycles",
    "upstream_failover_reconnects",
];

const RECONNECTING_LINE: &str = r#"{"method": "on_connection_state_changed", "state": "Reconnecting"}"#;

/// tick 10ms・試行間隔20ms(=2tickごと)。タイムアウトはシナリオ中に満了しない長さ。
fn golden_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        tick: Duration::from_millis(10),
        retry_interval: Duration::from_millis(20),
        timeout: Duration::from_secs(60),
    }
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden").join("callback_contract")
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("文字列のJSONエンコードは失敗しない")
}

fn state_line(state: &ConnectionPublicState) -> String {
    let tag = match state {
        ConnectionPublicState::Connected { host } => {
            return format!(
                r#"{{"method": "on_connection_state_changed", "state": "Connected", "host": {}}}"#,
                json_str(host)
            );
        }
        ConnectionPublicState::Connecting => "Connecting",
        ConnectionPublicState::Disconnected { .. } => "Disconnected",
        ConnectionPublicState::Error { .. } => "Error",
        ConnectionPublicState::Reconnecting { .. } => "Reconnecting",
    };
    format!(r#"{{"method": "on_connection_state_changed", "state": "{tag}"}}"#)
}

fn edge_line(edge: &ConnectionEdge, generation: u64) -> String {
    match edge {
        ConnectionEdge::Established { host, upstream_failover } => format!(
            r#"{{"method": "on_connection_edge", "edge": "Established", "host": {}, "upstream_failover": {upstream_failover}, "generation": {generation}}}"#,
            json_str(host)
        ),
        ConnectionEdge::Lost => {
            format!(r#"{{"method": "on_connection_edge", "edge": "Lost", "generation": {generation}}}"#)
        }
    }
}

/// `RecordingCallback`が記録した列を、`event_order`の順に射影する。
fn trace_of(cb: &RecordingCallback) -> Vec<String> {
    let order = cb.event_order.lock().unwrap().clone();
    let states = cb.connection_states.lock().unwrap().clone();
    let edges = cb.edges.lock().unwrap().clone();
    let resumes = cb.foreground_resumes.lock().unwrap().clone();
    let (mut si, mut ei, mut ri) = (0usize, 0usize, 0usize);
    let mut out: Vec<String> = Vec::new();
    for kind in order {
        let line = match kind {
            "connection_state_changed" => {
                si += 1;
                state_line(&states[si - 1])
            }
            "edge_established" | "edge_lost" => {
                ei += 1;
                let (edge, generation) = &edges[ei - 1];
                edge_line(edge, *generation)
            }
            "foreground_resume" => {
                ri += 1;
                format!(r#"{{"method": "on_foreground_resume", "did_reconnect": {}}}"#, resumes[ri - 1])
            }
            other => panic!("RecordingCallbackの未知のevent_order: {other}"),
        };
        if line == RECONNECTING_LINE && out.last().map(String::as_str) == Some(RECONNECTING_LINE) {
            continue;
        }
        out.push(line);
    }
    assert_eq!(
        (si, ei, ri),
        (states.len(), edges.len(), resumes.len()),
        "event_orderと各記録の件数が食い違う"
    );
    out
}

fn render(scenario: &str, description: &str, lines: &[String]) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!("  \"scenario\": {},\n", json_str(scenario)));
    s.push_str(&format!("  \"description\": {},\n", json_str(description)));
    s.push_str("  \"events\": [\n");
    for (i, line) in lines.iter().enumerate() {
        s.push_str("    ");
        s.push_str(line);
        if i + 1 < lines.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");
    s
}

/// 届いた列を射影してgoldenと比較する。比較の前に、エッジ列がStep 8a′の契約を満たすことも確かめる
/// (契約違反の列をgoldenとして固定しない)。
fn check_golden(cb: &RecordingCallback, scenario: &str, description: &str) {
    assert!(SCENARIOS.contains(&scenario), "SCENARIOSに未登録のシナリオ: {scenario}");
    assert_edge_contract(&edges_of(cb));
    let rendered = render(scenario, description, &trace_of(cb));
    let dir = golden_dir();
    let path = dir.join(format!("{scenario}.json"));
    let actual_path = dir.join(format!("{scenario}.actual.json"));
    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        std::fs::create_dir_all(&dir).expect("goldenディレクトリを作れない");
        std::fs::write(&path, &rendered).expect("goldenを書けない");
        let _ = std::fs::remove_file(&actual_path);
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
    if expected != rendered {
        let _ = std::fs::create_dir_all(&dir);
        let wrote = std::fs::write(&actual_path, &rendered).is_ok();
        panic!(
            "callback契約goldenと一致しない: {scenario}\n\
             --- expected ({}) ---\n{expected}\n\
             --- actual ({}{}) ---\n{rendered}\n\
             callback列の変更が意図どおりなら、actualの内容を確認して{scenario}.jsonとしてコミットする\n\
             (Kotlin/Swiftのreplayテストも同じファイルを読む。ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 13 / §7)。",
            path.display(),
            actual_path.display(),
            if wrote { "" } else { "、書き込み失敗" },
        );
    }
    let _ = std::fs::remove_file(&actual_path);
}

/// ディレクトリに`SCENARIOS`以外のgolden(`.actual.json`を除く)が残っていない。
#[test]
fn golden_dir_contains_exactly_the_registered_scenarios() {
    let mut found: Vec<String> = std::fs::read_dir(golden_dir())
        .expect("goldenディレクトリが読めない")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".json") && !name.ends_with(".actual.json"))
        .map(|name| name.trim_end_matches(".json").to_string())
        .collect();
    found.sort();
    let mut expected: Vec<String> = SCENARIOS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(found, expected, "goldenファイルの集合がSCENARIOSと一致しない");
}

/// (a) ユーザー`disconnect()`。
#[tokio::test(start_paused = true)]
async fn golden_a_user_disconnect() {
    let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let adapter = connect_and_establish(&orch, HOST);
    orch.disconnect();
    adapter.on_disconnected(Some("closed by user".to_string()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    check_golden(&cb, "a_user_disconnect", "(a) 接続後にユーザーがdisconnect()した。自動再接続はしない");
}

/// (b) トランスポートエラー。自動再接続ループが始まり、最初の`Reconnecting`までを記録する。
#[tokio::test(start_paused = true)]
async fn golden_b_transport_error() {
    let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let adapter = connect_and_establish(&orch, HOST);
    adapter.on_disconnected(Some("peer closed".to_string()));
    tokio::time::sleep(Duration::from_millis(5)).await;
    check_golden(&cb, "b_transport_error", "(b) 接続後のトランスポートエラー。Lostの後に自動再接続ループのReconnectingが続く");
}

/// (c) TCPのnetwork-lost debounce満了。後から届く旧セッションの同じ世代の切断通知は何も出さない。
#[tokio::test(start_paused = true)]
async fn golden_c_network_lost_debounce() {
    let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let adapter = connect_and_establish(&orch, HOST);
    orch.notify_network_path_changed(false);
    tokio::time::sleep(Duration::from_millis(35)).await;
    adapter.on_disconnected(Some("broken pipe".to_string()));
    check_golden(
        &cb,
        "c_network_lost_debounce",
        "(c) TCP接続中のネットワーク喪失がdebounce後も続いた。遅れて届く同じ世代の切断通知は何も出さない",
    );
}

/// (d) Connected中の手動`connect_*`。旧世代の`Lost`が`Connecting`公開より前に届く。
#[tokio::test(start_paused = true)]
async fn golden_d_manual_connect_while_connected() {
    let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let old = connect_and_establish(&orch, HOST);
    let new = orch.begin_connect(ssh_attempt(OTHER_HOST)).expect("Connected中のconnectは受理されるはず");
    old.on_connected(); // 旧世代の遅延コールバックは無視される
    new.on_connected();
    check_golden(
        &cb,
        "d_manual_connect_while_connected",
        "(d) Connected中に別ホストへ手動接続した。旧世代のLostの後に新しい世代のEstablishedが届く",
    );
}

/// (e) Suspended後のフォアグラウンド復帰(切断を起こさずConnectedのまま復帰する、ADR m-R4-5の手順)。
#[tokio::test(start_paused = true)]
async fn golden_e_foreground_resume_reconnect() {
    let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let _old = connect_and_establish(&orch, HOST);
    orch.notify_did_enter_background(30_000);
    orch.notify_background_budget_expired();
    orch.notify_will_enter_foreground();
    let new = adapters.lock().unwrap().pop().expect("フォアグラウンド復帰で再接続を試みるはず");
    new.on_connected();
    check_golden(
        &cb,
        "e_foreground_resume_reconnect",
        "(e) バックグラウンド猶予切れの後にフォアグラウンド復帰し再接続した。Connected公開を挟まずLostが届く",
    );
}

/// (f) 切断→自動再接続ループの試行成功。
#[tokio::test(start_paused = true)]
async fn golden_f_reconnect_loop_success() {
    let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let adapter = connect_and_establish(&orch, HOST);
    adapter.on_disconnected(Some("peer closed".to_string()));
    tokio::time::sleep(Duration::from_millis(25)).await;
    let attempt = adapters.lock().unwrap().pop().expect("試行間隔の経過後に再接続を試みるはず");
    attempt.on_connected();
    check_golden(&cb, "f_reconnect_loop_success", "(f) 切断後の自動再接続ループの試行が成功した");
}

/// 再接続ループのギブアップ(試行が結果を返さないままタイムアウト)。
#[tokio::test(start_paused = true)]
async fn golden_reconnect_gives_up() {
    let policy = ReconnectPolicy { timeout: Duration::from_millis(100), ..golden_policy() };
    let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), policy);
    let adapter = connect_and_establish(&orch, HOST);
    adapter.on_disconnected(Some("peer closed".to_string()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active, "タイムアウトでループは止まっているはず");
    check_golden(&cb, "reconnect_gives_up", "切断後の自動再接続ループがタイムアウトでギブアップした");
}

/// 切断→自動再接続の成功を3回続ける(Kotlinの旧`StateFlow`ミラーがconflationで取りこぼしえた形)。
#[tokio::test(start_paused = true)]
async fn golden_fast_reconnect_cycles() {
    let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let mut current = connect_and_establish(&orch, HOST);
    for cycle in 0..3 {
        current.on_disconnected(Some("peer closed".to_string()));
        tokio::time::sleep(Duration::from_millis(25)).await;
        let attempt = adapters.lock().unwrap().pop().unwrap_or_else(|| panic!("cycle {cycle}: 再接続を試みるはず"));
        attempt.on_connected();
        current = attempt;
    }
    check_golden(
        &cb,
        "fast_reconnect_cycles",
        "切断から自動再接続の成功までを3回続けた。世代ごとにLostとEstablishedが1回ずつ届く",
    );
}

/// #175: upstream failoverを有効にしたマルチパス接続の後、自動再接続ループの成功とフォアグラウンド復帰の
/// 再接続(どちらもKotlinの`connectPane`を通らない)が続く。全世代の`Established`が
/// `upstream_failover: true`を運ぶ(Kotlin/Swiftはこれをエッジごとに適用するだけ)。
#[tokio::test(start_paused = true)]
async fn golden_upstream_failover_reconnects() {
    let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), golden_policy());
    let first = orch.begin_connect(multipath_attempt(HOST, true)).expect("Idle中のconnectは受理されるはず");
    first.on_connected();
    first.on_disconnected(Some("peer closed".to_string()));
    tokio::time::sleep(Duration::from_millis(25)).await;
    let looped = adapters.lock().unwrap().pop().expect("試行間隔の経過後に再接続を試みるはず");
    looped.on_connected();
    orch.notify_did_enter_background(30_000);
    orch.notify_background_budget_expired();
    orch.notify_will_enter_foreground();
    let resumed = adapters.lock().unwrap().pop().expect("フォアグラウンド復帰で再接続を試みるはず");
    resumed.on_connected();
    check_golden(
        &cb,
        "upstream_failover_reconnects",
        "upstream failover有効のマルチパス接続が、自動再接続ループとフォアグラウンド復帰で張り直された。全世代のEstablishedがupstream_failover: trueを運ぶ",
    );
}
