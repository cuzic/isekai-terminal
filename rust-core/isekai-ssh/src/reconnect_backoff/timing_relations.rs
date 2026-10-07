//! `isekai-ssh`の再接続方針の時間定数の**関係**を固定するテスト
//! (ADR_DETERMINISTIC_NETWORK_SIMULATION_L1.md §4.5(a)、「定数関係テスト」)。
//!
//! 関係はすべて定数のdocに書かれた意図から導いた(値そのものは固定しない)。
//!
//! `isekai-pipe`は`[[bin]]`のみのcrateで、`isekai-ssh`から定数をimportできない。そこで
//! `isekai-pipe`側の定数(`BUSY_OTHER_SESSION_RETRY_WINDOW`)は、その定義行をテスト時に
//! `include_str!`で読んで取り出す。定義の形が変わって読めなくなったら、このテストが
//! 「読めない」と言って落ちる(黙って通らない)。
//!
//! 意図的な非関係(assertしない): `REDEPLOY_BACKOFF.initial`(60秒)は、かつて
//! `RECONNECT_STABLE_THRESHOLD`と数値を合わせていたが、後者は別の理由で200秒に伸び、
//! 前者の理由(再デプロイはSSHログイン2回分の費用)は単独で成り立つ(`REDEPLOY_BACKOFF`のdoc)。
//! 両者を再び揃える必要は無い。

use std::time::Duration;

use super::{RECONNECT_BACKOFF, RECONNECT_STABLE_THRESHOLD};

/// `isekai-pipe`の`resume_fsm.rs`の`BUSY_OTHER_SESSION_RETRY_WINDOW`を、定義行から読む。
fn isekai_pipe_busy_other_session_retry_window() -> Duration {
    const SRC: &str = include_str!("../../../isekai-pipe/src/resume_fsm.rs");
    const PREFIX: &str = "pub(crate) const BUSY_OTHER_SESSION_RETRY_WINDOW: Duration = Duration::from_secs(";
    let start = SRC.find(PREFIX).map(|i| i + PREFIX.len()).unwrap_or_else(|| {
        panic!(
            "could not find `{PREFIX}..)` in isekai-pipe/src/resume_fsm.rs; if BUSY_OTHER_SESSION_RETRY_WINDOW's definition \
             changed shape, update this reader rather than deleting the relation it guards"
        )
    });
    let digits: String = SRC[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '_').filter(|c| *c != '_').collect();
    let secs: u64 = digits
        .parse()
        .unwrap_or_else(|e| panic!("BUSY_OTHER_SESSION_RETRY_WINDOW is not a plain `from_secs(<integer>)` literal ({e}); update this reader"));
    Duration::from_secs(secs)
}

/// 1回の`isekai-pipe connect`は、BUSY_OTHER_SESSIONの間、内部で最大
/// `BUSY_OTHER_SESSION_RETRY_WINDOW`まで再試行してから失敗を返す。`RECONNECT_STABLE_THRESHOLD`が
/// それ以下だと、その内部再試行の天井に当たって遅く失敗しただけの試行が「安定していた」と数えられ、
/// 実際の障害中に失敗のたびにredeploy gateと軽量再試行の予算が新品に戻る
/// (`RECONNECT_STABLE_THRESHOLD`のdoc、`isekai-ssh` PR #115 round 2で60秒→200秒に修正)。
/// 片方のcrateだけで定数を変えるとこの関係は黙って壊れる(BUSY_OTHER_SESSION期限のクラス、
/// 75d08a39 → fd32ce11 → 3d5e0da5)。
#[test]
fn stable_threshold_exceeds_isekai_pipe_busy_other_session_retry_window() {
    let busy_window = isekai_pipe_busy_other_session_retry_window();
    assert!(
        RECONNECT_STABLE_THRESHOLD > busy_window,
        "RECONNECT_STABLE_THRESHOLD ({RECONNECT_STABLE_THRESHOLD:?}) must exceed isekai-pipe's \
         BUSY_OTHER_SESSION_RETRY_WINDOW ({busy_window:?}): an attempt that merely hit isekai-pipe's internal BUSY retry \
         ceiling would otherwise count as 'stable' and reset the redeploy gate / retry budget on every failure of a \
         real outage (isekai-ssh PR #115 round 2; see the constant's docs)"
    );
}

/// `RECONNECT_STABLE_THRESHOLD`は`RECONNECT_BACKOFF.max`を「十分に上回る」(同doc)。下回ると、
/// 失敗→最大backoff待ち→失敗という純粋な連続失敗だけで予算がリセットされる。
#[test]
fn stable_threshold_exceeds_the_backoff_cap() {
    assert!(
        RECONNECT_STABLE_THRESHOLD > RECONNECT_BACKOFF.max,
        "RECONNECT_STABLE_THRESHOLD ({RECONNECT_STABLE_THRESHOLD:?}) must exceed RECONNECT_BACKOFF.max ({:?}), or \
         back-to-back failed attempts alone reset the budget meant to bound them",
        RECONNECT_BACKOFF.max
    );
}

/// `RECONNECT_BACKOFF`にはjitterが必要(857f6ae6 D-4。jitter欠落は`isekai-pipe`と`isekai-ssh`の
/// 2つのコピーで別々に直す必要があった)。jitterが0だと、共有イベント(スリープ復帰・roaming)の後に
/// 全タブが同じ予定で再接続する。
#[test]
fn reconnect_backoff_has_jitter() {
    assert!(
        RECONNECT_BACKOFF.jitter > 0.0 && RECONNECT_BACKOFF.jitter <= 1.0,
        "RECONNECT_BACKOFF.jitter ({}) must be in (0, 1]: without jitter every tab reconnects in lockstep after a shared \
         event (857f6ae6 D-4)",
        RECONNECT_BACKOFF.jitter
    );
}
