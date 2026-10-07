//! serverとclientの時間定数の**関係**を固定するテスト
//! (ADR_DETERMINISTIC_NETWORK_SIMULATION_L1.md §4.5(b)のうち、本番コードを変えずに書けるもの)。
//!
//! serverの定数(`HELLO_TIMEOUT`・`PREEMPT_WAIT_TIMEOUT`は`engine/mod.rs`、
//! `PENDING_ACTIVATION_TIMEOUT`・`TARGET_CONNECT_TIMEOUT`は`engine/attach_runtime.rs`)はすべて
//! privateなので、`pub(crate)`化せずに見える`attach_runtime`の子モジュールに置いた(Rustの
//! privateは定義モジュールとその子孫から見える)。serverの既定値のうち定数でなく
//! `parse_args_from`内のリテラルであるもの(`--idle-timeout`・`--resume-window`の既定)は、
//! 本番の引数解析そのものを空の引数で呼んで読む。
//!
//! 本番コードの変更(リテラルの定数抽出)が要る関係(sweep周期`5s`、server keep-aliveの
//! `idle/3`)はここに無い。PRの後続作業として列挙してある。
//!
//! client側だけで閉じる関係は`resume_loop/timing_relations.rs`。

use std::time::Duration;

use isekai_transport::resume::TRANSPORT_STEP_TIMEOUT;

use super::super::{parse_args_from, HELLO_TIMEOUT, PREEMPT_WAIT_TIMEOUT};
use super::{HELLO_OUTCOME_TIMEOUT, PENDING_ACTIVATION_TIMEOUT, TARGET_CONNECT_TIMEOUT};
use crate::resume_fsm::{resume_window_for, UNKNOWN_SESSION_MIN_ELAPSED_FLOOR};

/// `isekai-pipe serve`を引数なしで起動したときの既定値(`--idle-timeout`, `--resume-window`)。
fn serve_defaults() -> (Duration, Duration) {
    let args = parse_args_from(Vec::<String>::new()).expect("`isekai-pipe serve` with no arguments must parse");
    (Duration::from_secs(args.idle_timeout), Duration::from_secs(args.resume_window))
}

fn client_quic_idle_timeout() -> Duration {
    isekai_transport::system::isekai_mux_config(true).max_idle_timeout
}

/// UnknownSessionのgive-up床は、`isekai-pipe serve --idle-timeout`の既定値の2倍以上
/// (`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`のdocがまさにこの値を指す: "`isekai-pipe serve
/// --idle-timeout`'s default (15s)" / "30s is double the idle-timeout default")。serverは旧接続の死を
/// 自分のidle timeoutでしか知れないので、それより前のRESUMEは「未park」でUnknownSessionになる。
/// serverの既定だけを伸ばしてclientの床を据え置くと、roaming中の正当な再接続をgive-upで殺す
/// (cc5fb926 → 71292e68 → dbb80d56)。
#[test]
fn unknown_session_give_up_floor_is_at_least_twice_the_server_default_idle_timeout() {
    let (server_idle, _) = serve_defaults();
    assert!(
        UNKNOWN_SESSION_MIN_ELAPSED_FLOOR >= server_idle * 2,
        "client UNKNOWN_SESSION_MIN_ELAPSED_FLOOR ({UNKNOWN_SESSION_MIN_ELAPSED_FLOOR:?}) must be >= 2 x the server's \
         default --idle-timeout ({server_idle:?}): the server only learns the old path died via its own idle timeout, \
         so a shorter floor gives up on roaming reconnects (regression class cc5fb926 -> 71292e68 -> dbb80d56)"
    );
}

/// serverがRESUMEを受けてzombie relayにpreemptを頼み、`reparked`を待つ時間
/// (`PREEMPT_WAIT_TIMEOUT`)は、clientのrequest_resume stepの上限(`TRANSPORT_STEP_TIMEOUT`)に収まる
/// (ADR §4.5(b))。収まらないと、server側でpreemptが成功しても応答前にclientがstepをtimeoutし、
/// 次の世代のRESUMEがまた同じzombieとpreemptを競う(857f6ae6 D-2のクラス。resume_fsm.rsの
/// `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`のdocも「高々2秒の遅延」を前提にしている)。
#[test]
fn server_preempt_wait_fits_inside_one_client_transport_step() {
    assert!(
        PREEMPT_WAIT_TIMEOUT < TRANSPORT_STEP_TIMEOUT,
        "server PREEMPT_WAIT_TIMEOUT ({PREEMPT_WAIT_TIMEOUT:?}) must be shorter than the client's \
         TRANSPORT_STEP_TIMEOUT ({TRANSPORT_STEP_TIMEOUT:?}), or the client abandons the RESUME step before a \
         successful preemption can answer it (zombie-relay class 857f6ae6 D-2)"
    );
}

/// serverの同一プロセス内のwire交換の上限(`HELLO_TIMEOUT`と`PENDING_ACTIVATION_TIMEOUT`、
/// どちらも`PENDING_ACTIVATION_TIMEOUT`のdocで「`HELLO_TIMEOUT`と同じ役割」)は、clientの1 stepの
/// 上限より短い(ADR §4.5(b))。長いと、clientがstepをtimeoutして再試行した時点で、serverには
/// 前の試行のHELLO待ち/PendingActivationのleaseがまだ残っており、再試行がそれと競合する。
#[test]
fn server_hello_and_pending_activation_timeouts_are_shorter_than_one_client_transport_step() {
    assert!(
        HELLO_TIMEOUT < TRANSPORT_STEP_TIMEOUT,
        "server HELLO_TIMEOUT ({HELLO_TIMEOUT:?}) must be shorter than the client's TRANSPORT_STEP_TIMEOUT \
         ({TRANSPORT_STEP_TIMEOUT:?}) so a stalled HELLO is cleaned up server-side before the client retries"
    );
    assert!(
        PENDING_ACTIVATION_TIMEOUT < TRANSPORT_STEP_TIMEOUT,
        "server PENDING_ACTIVATION_TIMEOUT ({PENDING_ACTIVATION_TIMEOUT:?}) must be shorter than the client's \
         TRANSPORT_STEP_TIMEOUT ({TRANSPORT_STEP_TIMEOUT:?}) so an un-activated lease is released before the client's \
         retry (with a new generation) arrives"
    );
}

/// 意図的な非関係(ADR rev2 N6): serverの`TARGET_CONNECT_TIMEOUT`(target TCPへのconnect)は、
/// clientの`TRANSPORT_STEP_TIMEOUT`より**長い**。`TARGET_CONNECT_TIMEOUT`のdocによれば、targetが
/// 遅いとclientが先にtimeoutするが、新しいgenerationでの再試行が`ClosingForSupersede`で自己回復する
/// ので「stuck-foreverではない」。また20秒はネットワーク越しのTCP handshakeを縛るもので、同一プロセス内の
/// wire交換を縛る`HELLO_TIMEOUT`/`PENDING_ACTIVATION_TIMEOUT`より意図的に長い。
///
/// これは「直すべき逆転」ではない。どちらかの向きに「直す」変更(例: `TARGET_CONNECT_TIMEOUT`を
/// `TRANSPORT_STEP_TIMEOUT`未満へ縮める、あるいは`TRANSPORT_STEP_TIMEOUT`を20秒超へ伸ばす)をする
/// 場合は、`TARGET_CONNECT_TIMEOUT`のdocの理由を先に見直し、このテストを書き換えること。
#[test]
fn deliberate_non_relation_target_connect_timeout_exceeds_the_client_transport_step() {
    assert!(
        TARGET_CONNECT_TIMEOUT > TRANSPORT_STEP_TIMEOUT,
        "server TARGET_CONNECT_TIMEOUT ({TARGET_CONNECT_TIMEOUT:?}) is DELIBERATELY longer than the client's \
         TRANSPORT_STEP_TIMEOUT ({TRANSPORT_STEP_TIMEOUT:?}): a client retry self-heals via ClosingForSupersede, so \
         this is not a stuck-forever bug (ADR_DETERMINISTIC_NETWORK_SIMULATION_L1.md rev2 N6). Re-read \
         TARGET_CONNECT_TIMEOUT's docs before 'fixing' either side"
    );
    assert!(
        TARGET_CONNECT_TIMEOUT > HELLO_TIMEOUT && TARGET_CONNECT_TIMEOUT > PENDING_ACTIVATION_TIMEOUT,
        "server TARGET_CONNECT_TIMEOUT ({TARGET_CONNECT_TIMEOUT:?}) is documented as deliberately longer than \
         HELLO_TIMEOUT ({HELLO_TIMEOUT:?}) / PENDING_ACTIVATION_TIMEOUT ({PENDING_ACTIVATION_TIMEOUT:?}): it bounds a \
         real TCP handshake over the network, not a same-process wire exchange"
    );
}

/// clientが「serverからgrantを学べなかった」ときのfallback窓(`resume_window_for(0)`)は、serverが
/// 引数なしで起動したときのpark失効の既定(`--resume-window`)と同じ。どちらも
/// `isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS`をそのまま使うと両側のdocが書いている
/// (`engine/mod.rs`の`resume_window`既定、`resume_fsm.rs`の`DEFAULT_RESUME_WINDOW`)。
/// clientの方が短いと、serverがまだparkしているsessionをclientが先に諦める(always-connects違反)。
/// 長いと、server失効後もclientがUnknownSessionを受け続ける。
#[test]
fn client_fallback_resume_window_matches_the_server_default_park_expiry() {
    let (_, server_park_expiry) = serve_defaults();
    let client_fallback = resume_window_for(0);
    assert_eq!(
        client_fallback, server_park_expiry,
        "client fallback resume window (resume_window_for(0) = {client_fallback:?}) must equal the server's default \
         --resume-window park expiry ({server_park_expiry:?}); both are documented to use DEFAULT_RESUME_GRACE_SECS"
    );
}

/// serverのpark失効の既定(`--resume-window`)は、「切断の検知にかかる時間 + clientの再試行予算」より
/// 十分長い(`engine/mod.rs`のsweepのコメント、実機Phase 8-4b: 両者を同じ値で共用すると、clientが
/// 切断を検知する頃にはparkが破棄済みで、reattachが必ずREJECT_UNKNOWN_SESSIONになる致命的な不具合)。
/// 検知はclient/serverのidle timeoutの長い方、再試行予算はclientがUnknownSessionを信じるまでに
/// 必ず待つ`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`で下から押さえる。
///
/// 意図的な非関係として、`--resume-window`は`--idle-timeout`と共用しない(同コメント「意図的に別の値」)。
#[test]
fn server_default_park_expiry_outlives_detection_plus_client_retry_budget() {
    let (server_idle, server_park_expiry) = serve_defaults();
    let detection = server_idle.max(client_quic_idle_timeout());
    let needed = detection + UNKNOWN_SESSION_MIN_ELAPSED_FLOOR;
    assert!(
        server_park_expiry > needed,
        "server default --resume-window ({server_park_expiry:?}) must outlive disconnect detection ({detection:?}) plus \
         the client's minimum retry span before trusting UnknownSession ({UNKNOWN_SESSION_MIN_ELAPSED_FLOOR:?}); \
         sharing one value for both made every reattach fail with REJECT_UNKNOWN_SESSION (Phase 8-4b)"
    );
    assert_ne!(
        server_park_expiry, server_idle,
        "--resume-window and --idle-timeout are deliberately separate values (engine/mod.rs sweep comment, Phase 8-4b)"
    );
}

/// `hello()`の待機上限(`HELLO_OUTCOME_TIMEOUT`、PIPE-06のbackstop)は、正当な待機のうち最長の
/// target接続(`TARGET_CONNECT_TIMEOUT`)より長い。短いと、遅いtargetへの正常な接続中に
/// ATTACHを`Target`で拒否してしまう。
#[test]
fn hello_outcome_backstop_outlives_the_target_connect_timeout() {
    assert!(
        HELLO_OUTCOME_TIMEOUT > TARGET_CONNECT_TIMEOUT,
        "HELLO_OUTCOME_TIMEOUT ({HELLO_OUTCOME_TIMEOUT:?}) must exceed TARGET_CONNECT_TIMEOUT ({TARGET_CONNECT_TIMEOUT:?}): \
         it is only a backstop and must never cut a legitimate slow target connect short"
    );
}
