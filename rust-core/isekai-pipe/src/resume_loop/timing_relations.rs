//! client側の時間定数の**関係**を固定するテスト
//! (docs/adr/0021-deterministic-network-simulation-l1.md §4.5(a)・§5-1、「定数関係テスト」)。
//!
//! このファイルは値そのものではなく、定数同士の関係だけをassertする。関係はすべて、定数のdoc
//! コメントかADRに書かれた意図から導いたもの(ADR §12 R3: 意図として書かれている関係だけを守る)。
//! 片方の定数だけを変えて関係が崩れると、ここがCIで落ちる。過去にこのクラスで直し直しが起きた:
//!
//! - BUSY_OTHER_SESSIONの再試行期限: 75d08a39 → fd32ce11 → 3d5e0da5(3回)
//! - UnknownSession give-upのroaming誤爆: cc5fb926 → 71292e68 → dbb80d56(3回)
//! - backoffのjitter欠落: 857f6ae6 D-4
//!
//! 「意図的な非関係」(どちらの向きにも「直して」はいけない、docに理由が書いてある関係)も
//! `deliberate_non_relation_`で始まるテストとして固定する。そのassertが落ちたら、値を戻すか、
//! 指しているdocの理由を見直してから、このテストを書き換えること。
//!
//! serverの定数を使う関係は`engine/attach_runtime/timing_relations.rs`(serverのprivate定数が
//! 見える位置)に置いた。

use std::time::Duration;

use isekai_transport::resume::TRANSPORT_STEP_TIMEOUT;

use super::{REPLAY_WRITE_TIMEOUT, STUN_RESUME_GIVE_UP_WINDOW, WARM_STANDBY_PROBE_INTERVAL, WARM_STANDBY_SUSPEND_JUMP_FACTOR};
use crate::resume_fsm::{
    resume_window_for, BUSY_OTHER_SESSION_RETRY_WINDOW, CROSS_FAMILY_MIN_PROBE_BUDGET, CROSS_FAMILY_SWITCH_DEADLINE,
    RESUME_BACKOFF, UNKNOWN_SESSION_MIN_ELAPSED_FLOOR,
};

/// clientのQUIC idle timeout。`isekai_transport::system`の定数はprivateなので、本番が実際に
/// 使う設定(`system_quic_factory`と同じ`isekai_mux_config(true)`)から読む。
fn client_quic_idle_timeout() -> Duration {
    isekai_transport::system::isekai_mux_config(true).max_idle_timeout
}

/// `RESUME_BACKOFF`の初回待ちの最大値(jitterの上振れ込み)。
fn resume_backoff_first_delay_upper_bound() -> Duration {
    RESUME_BACKOFF.initial.mul_f64(1.0 + RESUME_BACKOFF.jitter)
}

/// UnknownSessionのgive-up床は、idle timeoutの2倍以上(`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`のdoc:
/// "30s is double the idle-timeout default")。break-before-makeのroamでは旧パスのresetがserverに
/// 届かず、serverは自分のQUIC idle timeoutで初めて旧接続の死に気づく。その前に届いたRESUMEは
/// 「まだparkされていない」ためUnknownSessionになる。床がidle timeoutより短いと、roaming中の
/// 正当な再接続をgive-upで殺す(cc5fb926 → 71292e68 → dbb80d56)。
///
/// docが指すのはserverの`--idle-timeout`既定値で、その関係はserver側のファイルで固定している。
/// ここではclient側のidle timeout(ADR §4.5(a)の形)との関係を固定する。両者は同じ15秒を
/// 意図しているので、どちらを変えてもどちらかのテストが落ちる。
#[test]
fn unknown_session_give_up_floor_is_at_least_twice_the_client_quic_idle_timeout() {
    let idle = client_quic_idle_timeout();
    assert!(
        UNKNOWN_SESSION_MIN_ELAPSED_FLOOR >= idle * 2,
        "UNKNOWN_SESSION_MIN_ELAPSED_FLOOR ({UNKNOWN_SESSION_MIN_ELAPSED_FLOOR:?}) must be >= 2 x the client QUIC idle \
         timeout ({idle:?}): a shorter floor lets 3 fast UnknownSession rejections kill a roaming reconnect before the \
         old connection is even declared dead (regression class cc5fb926 -> 71292e68 -> dbb80d56; see the constant's docs)"
    );
}

/// cross-family切替が意味を持つ最小の残り時間は「`RESUME_BACKOFF`の初回待ち(jitter込み)+
/// `TRANSPORT_STEP_TIMEOUT`1回分のconnect」(`CROSS_FAMILY_MIN_PROBE_BUDGET`のdoc)。
/// これを下回ると、残り時間でconnect stepを1回も最後まで走らせられないのに切替えてしまう。
#[test]
fn cross_family_min_probe_budget_covers_one_backoff_and_one_transport_step() {
    let needed = TRANSPORT_STEP_TIMEOUT + resume_backoff_first_delay_upper_bound();
    assert!(
        CROSS_FAMILY_MIN_PROBE_BUDGET >= needed,
        "CROSS_FAMILY_MIN_PROBE_BUDGET ({CROSS_FAMILY_MIN_PROBE_BUDGET:?}) must cover one RESUME_BACKOFF first delay \
         incl. jitter plus one TRANSPORT_STEP_TIMEOUT ({needed:?}); otherwise a switch fires with no time left for even \
         one cross-family connect step (opus review round 5 on ADR_STUN_REESTABLISH_CONTINUITY's implementation)"
    );
}

/// 意図的な非関係: `CROSS_FAMILY_MIN_PROBE_BUDGET`は`reconnect_and_resume`全体の最悪値
/// (connect stepとrequest_resume stepがそれぞれ`TRANSPORT_STEP_TIMEOUT`、計約30秒)を要求しない
/// (同docの"Deliberately *not* the ~30s worst case")。31秒を要求すると、中程度の
/// `resume-grace`帯で切替を抑止してしまい、到達できない`cached_relay_addr`の判定(connect stepだけで
/// 決まる)が届かなくなる。「直す」つもりで2 step分に広げる変更はここで止まる。
#[test]
fn deliberate_non_relation_cross_family_min_probe_budget_is_not_the_two_step_worst_case() {
    assert!(
        CROSS_FAMILY_MIN_PROBE_BUDGET < TRANSPORT_STEP_TIMEOUT * 2,
        "CROSS_FAMILY_MIN_PROBE_BUDGET ({CROSS_FAMILY_MIN_PROBE_BUDGET:?}) is deliberately below the two-step worst case \
         of reconnect_and_resume (2 x TRANSPORT_STEP_TIMEOUT); widening it suppresses the cross-family switch in the \
         moderate resume-grace band. Re-read the constant's 'Deliberately *not*' docs before changing this"
    );
}

/// cross-family先へ切替えた後の有界な初回再試行の窓(`CROSS_FAMILY_SWITCH_DEADLINE`)は、
/// 少なくとも1回のprobeが収まる長さ(`CROSS_FAMILY_MIN_PROBE_BUDGET`、「切替が何かを得られる
/// 最小の残り時間」)を持つ。短いと切替先を1回も試さないまま期限切れでgive-upする。
#[test]
fn cross_family_switch_deadline_leaves_room_for_at_least_one_probe() {
    assert!(
        CROSS_FAMILY_SWITCH_DEADLINE >= CROSS_FAMILY_MIN_PROBE_BUDGET,
        "CROSS_FAMILY_SWITCH_DEADLINE ({CROSS_FAMILY_SWITCH_DEADLINE:?}) must be >= CROSS_FAMILY_MIN_PROBE_BUDGET \
         ({CROSS_FAMILY_MIN_PROBE_BUDGET:?}): the bounded post-switch window must fit at least one cross-family probe \
         (docs/adr/0006-stun-reestablish-continuity.md §3.2 task 4's 'short bounded retry')"
    );
}

/// STUNのbare-redial resumeの窓(`STUN_RESUME_GIVE_UP_WINDOW`)は、期限ベースのcross-family切替
/// (`should_switch_to_cross_family`: 残り時間が`CROSS_FAMILY_SWITCH_DEADLINE`以下で切替)より長い。
/// 同じか短いと、切断直後の最初の失敗で即座に切替わり、元のSTUN peerへの再試行
/// (`STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`回)が一度も行われなくなる。
#[test]
fn stun_resume_window_is_longer_than_the_deadline_aware_cross_family_switch() {
    assert!(
        STUN_RESUME_GIVE_UP_WINDOW > CROSS_FAMILY_SWITCH_DEADLINE,
        "STUN_RESUME_GIVE_UP_WINDOW ({STUN_RESUME_GIVE_UP_WINDOW:?}) must exceed CROSS_FAMILY_SWITCH_DEADLINE \
         ({CROSS_FAMILY_SWITCH_DEADLINE:?}); otherwise the deadline-aware half of should_switch_to_cross_family fires on \
         the very first failure and the original STUN peer is never retried"
    );
}

/// `REPLAY_WRITE_TIMEOUT`はdocで「`TRANSPORT_STEP_TIMEOUT`と同じ理由・同じ大きさ」と定めている
/// (docの「private」という記述は古く、今は`pub`)。resume直後のreplay書き込みも1ネットワーク
/// 往復と同じ上限で縛る、という意図(3205178f「connect/RESUMEにtimeoutが無い」と同じクラス)。
#[test]
fn replay_write_timeout_matches_the_transport_step_timeout() {
    assert_eq!(
        REPLAY_WRITE_TIMEOUT, TRANSPORT_STEP_TIMEOUT,
        "REPLAY_WRITE_TIMEOUT is documented as 'same rationale and magnitude' as isekai_transport's \
         TRANSPORT_STEP_TIMEOUT; change both together (or derive it from TRANSPORT_STEP_TIMEOUT, which is now pub)"
    );
}

/// BUSY_OTHER_SESSIONの再試行窓は、少なくとも「1回の接続step + backoffの上限待ち」の後にもう1回
/// 試せる長さを持つ。docによれば、preemption修正入りのserveへの正当な再試行は「ほぼ即座に成功する」
/// ので、その1回の再試行が窓に収まらないと、窓が存在する意味が無くなる。
#[test]
fn busy_other_session_window_fits_at_least_one_full_retry_cycle() {
    let one_cycle = TRANSPORT_STEP_TIMEOUT + RESUME_BACKOFF.max;
    assert!(
        BUSY_OTHER_SESSION_RETRY_WINDOW >= one_cycle,
        "BUSY_OTHER_SESSION_RETRY_WINDOW ({BUSY_OTHER_SESSION_RETRY_WINDOW:?}) must fit one TRANSPORT_STEP_TIMEOUT plus \
         one capped RESUME_BACKOFF wait ({one_cycle:?}), or not even one retry against a preempting serve fits \
         (BUSY_OTHER_SESSION deadline class 75d08a39 -> fd32ce11 -> 3d5e0da5)"
    );
}

/// 意図的な非関係: `BUSY_OTHER_SESSION_RETRY_WINDOW`はresume grace(既定
/// `DEFAULT_RESUME_GRACE_SECS`、10日)から導出しない、固定の短い窓(3d5e0da5で切り離した。
/// 同定数のdoc「Deliberately **not** derived from resume_window_for/resume-grace」)。graceに
/// 合わせて伸ばすと、preemptionを持たない古いserveに対して「複数日の無言のhang」に戻る。
/// 上限の1時間は「分のオーダーで、時間/日のオーダーではない」(doc: "a fast, visible failure")を
/// 検査可能にした境界で、値そのものの固定ではない。
#[test]
fn deliberate_non_relation_busy_other_session_window_is_not_derived_from_resume_grace() {
    let default_grace_window = resume_window_for(0);
    assert!(
        BUSY_OTHER_SESSION_RETRY_WINDOW < default_grace_window,
        "BUSY_OTHER_SESSION_RETRY_WINDOW ({BUSY_OTHER_SESSION_RETRY_WINDOW:?}) must stay a fixed short window, not track \
         the default resume grace ({default_grace_window:?}) — decoupled in 3d5e0da5; see the constant's docs"
    );
    assert!(
        BUSY_OTHER_SESSION_RETRY_WINDOW <= Duration::from_secs(60 * 60),
        "BUSY_OTHER_SESSION_RETRY_WINDOW ({BUSY_OTHER_SESSION_RETRY_WINDOW:?}) is meant to turn an old serve's \
         un-preempting multi-day hang into a fast, visible failure (minutes, not hours); see the constant's docs"
    );
}

/// `RESUME_BACKOFF`にはjitterが必要(857f6ae6 D-4)。jitterが0だと、スリープ復帰や瞬断を同時に
/// 見た全タブが同じグリッドで再試行し、serverのresume競合(D-2)を確実に踏む。
#[test]
fn resume_backoff_has_jitter() {
    assert!(
        RESUME_BACKOFF.jitter > 0.0 && RESUME_BACKOFF.jitter <= 1.0,
        "RESUME_BACKOFF.jitter ({}) must be in (0, 1]: without jitter every tab retries in lockstep after a shared \
         sleep/roam event and reliably hits the server-side resume race (857f6ae6 D-4)",
        RESUME_BACKOFF.jitter
    );
    assert!(
        RESUME_BACKOFF.initial <= RESUME_BACKOFF.max,
        "RESUME_BACKOFF.initial ({:?}) must not exceed RESUME_BACKOFF.max ({:?})",
        RESUME_BACKOFF.initial,
        RESUME_BACKOFF.max
    );
}

/// warm-standbyのsuspend検出(壁時計の間隔 > `WARM_STANDBY_PROBE_INTERVAL` ×
/// `WARM_STANDBY_SUSPEND_JUMP_FACTOR`)は、通常のtick + 1回のprobeのtimeout
/// (`isekai_transport::warm_standby::PROBE_TIMEOUT`)を「十分に上回る」
/// (`WARM_STANDBY_SUSPEND_JUMP_FACTOR`のdoc)。下回ると、probeが1回timeoutしただけで
/// suspendと誤認してstandbyを捨てる。
#[test]
fn warm_standby_suspend_threshold_exceeds_one_tick_plus_one_probe_timeout() {
    let threshold = WARM_STANDBY_PROBE_INTERVAL * WARM_STANDBY_SUSPEND_JUMP_FACTOR;
    let ordinary_gap = WARM_STANDBY_PROBE_INTERVAL + isekai_transport::warm_standby::PROBE_TIMEOUT;
    assert!(
        threshold > ordinary_gap,
        "warm-standby suspend threshold ({threshold:?}) must exceed one tick plus one probe timeout ({ordinary_gap:?}), \
         or a single slow probe is misread as a host suspend (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-3)"
    );
}
