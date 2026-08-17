//! 項目2(OEMバッテリー最適化への案内UI): 「バックグラウンド動作の案内ダイアログを
//! 出すべきか」の判断ポリシー。
//!
//! ## 背景
//!
//! Android端末(特にXiaomi/OPPO/Vivo等の一部OEM)は、標準のDoze/App Standby以外に
//! 独自のバックグラウンドプロセスkillを行うことがある。`.claude/rules/always-connects.md`
//! のタスク#14(黙示的セッション再アタッチ)は「killされても自動的に復旧する」ことを
//! 保証するが、それでも接続が一瞬途切れることに変わりはなく、頻発するならユーザーに
//! OEMのバッテリー最適化設定を見直すよう案内したい。
//!
//! `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`権限は追加しない(Playのacceptable use cases
//! にSSHクライアントは該当せず、誤用はアプリ停止措置の対象になりうる)。標準API
//! (`PowerManager.isIgnoringBatteryOptimizations` / `Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`、
//! 共に権限不要)のみを使う前提のポリシーである。
//!
//! ## 何を「予期しないkill」とみなすか
//!
//! 接続の切断回数はトリガーにしない(ネットワーク起因の切断とOEMによるプロセスkillは
//! 別事象で、切断回数はノイズが多すぎる)。Kotlin側(`TerminalSessionService`)が
//! `onDestroy`(サービスが正規のライフサイクル経由で終了する唯一の経路)でのみ書く
//! 「正常終了マーカー」の有無と、`ReattachStateStore`の
//! 「新鮮なreattachレコード」の有無を突き合わせ、「新鮮なレコードあり && マーカー無し」を
//! 1回の予期しないkillとしてKotlin側が数える。このモジュールはその生の事実(kill回数・
//! 前回案内時刻・免除状態・オプトアウトフラグ)を受け取って判断するだけで、kill検出
//! ロジック自体はKotlin側に留まる(`rust-ssot.md`が対象にしているのはセッション/接続の
//! 状態機械であり、この判断はプラットフォームAPIの生の観測結果を集約するだけの単純な
//! 閾値判定だが、「いつ案内するか」という一貫したポリシーをKotlin側のif文に分散させず
//! ここに一元化しておくことで、将来ロジックが複雑化しても`reattach_persistence.rs`と
//! 同じ場所を見れば良い状態を保つ)。
//!
//! この案内は予防策(「案内すれば頻度が減るかもしれない」)に過ぎず、実際の復旧保証は
//! タスク#14の黙示的自動再アタッチが既に担っている——このモジュールが無くても
//! `.claude/rules/always-connects.md`の原則は満たされたままである、という位置づけ。

/// 「予期しないkill」がこの回数に達したら案内対象の候補にする。1回だけでは
/// (アプリの手動スワイプkill・OSの通常のメモリ回収など)偶発的な事象と区別が
/// つかないため、2回以上の再現性を要求する。
pub const UNEXPECTED_KILL_THRESHOLD: u32 = 2;

/// 前回案内してから、再度案内するまでに空けるべき最短期間(秒)。1回案内した直後に
/// 何度もダイアログを出すと単なる迷惑通知になるため、14日間のクールダウンを設ける。
pub const GUIDANCE_COOLDOWN_SECS: u64 = 14 * 24 * 60 * 60;

/// [`decide_battery_guidance`]への入力となる生の事実。判断ロジックを持たず、
/// Kotlin側が観測した値をそのまま渡すだけのデータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BackgroundKillFacts {
    /// 「新鮮なreattachレコードあり && clean-shutdownマーカー無し」で起動した回数の
    /// 累積(Kotlin側が単調増加でカウントし永続化する)。
    pub unexpected_kill_count: u32,
    /// 前回この案内ダイアログを表示した時刻(Unix epoch秒)。一度も表示したことが
    /// 無ければ`None`。
    pub last_shown_unix_secs: Option<u64>,
    /// 判定時刻(Unix epoch秒)。
    pub now_unix_secs: u64,
    /// `PowerManager.isIgnoringBatteryOptimizations()`の結果。既にOSのバッテリー
    /// 最適化対象外になっている(=免除済み)なら、これ以上案内する意味が無い。
    pub is_ignoring_battery_optimizations: bool,
    /// ユーザーが案内ダイアログの「今後表示しない」トグルを選んでいるか。
    pub user_opted_out: bool,
}

/// [`decide_battery_guidance`]の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BatteryGuidanceDecision {
    /// コールドスタート時に案内ダイアログを表示すべきか。
    pub should_show: bool,
}

/// 「予期しないkillが2回以上」かつ「前回案内から14日以上(または未表示)」かつ
/// 「既に免除済みでない」かつ「ユーザーがオプトアウトしていない」場合のみ`true`を返す
/// 純関数。
///
/// クールダウン判定は`now_unix_secs`が`last_shown_unix_secs`より前(端末の時計調整等で
/// 稀に起こりうる)の場合、`saturating_sub`により経過時間0として扱う——つまり
/// 「直近案内したばかり」と同じ扱いになり、`should_show`はfalse側に倒れる
/// (`reattach_record_is_fresh`とは異なり、クロックスキュー時は「案内すべき」ではなく
/// 「案内を控える」安全側に倒す設計。案内を出しすぎることの実害[迷惑通知]の方が、
/// 出し足りないことの実害[案内が遅れるだけ]より大きいと判断したため)。
#[uniffi::export]
pub fn decide_battery_guidance(facts: BackgroundKillFacts) -> BatteryGuidanceDecision {
    let meets_kill_threshold = facts.unexpected_kill_count >= UNEXPECTED_KILL_THRESHOLD;
    let cooldown_elapsed = match facts.last_shown_unix_secs {
        None => true,
        Some(last_shown) => facts.now_unix_secs.saturating_sub(last_shown) >= GUIDANCE_COOLDOWN_SECS,
    };

    let should_show = meets_kill_threshold
        && cooldown_elapsed
        && !facts.is_ignoring_battery_optimizations
        && !facts.user_opted_out;

    BatteryGuidanceDecision { should_show }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_facts() -> BackgroundKillFacts {
        BackgroundKillFacts {
            unexpected_kill_count: UNEXPECTED_KILL_THRESHOLD,
            last_shown_unix_secs: None,
            now_unix_secs: 1_000_000,
            is_ignoring_battery_optimizations: false,
            user_opted_out: false,
        }
    }

    #[test]
    fn shows_when_all_conditions_met_and_never_shown_before() {
        let decision = decide_battery_guidance(base_facts());
        assert!(decision.should_show);
    }

    #[test]
    fn does_not_show_below_kill_threshold() {
        let facts = BackgroundKillFacts {
            unexpected_kill_count: UNEXPECTED_KILL_THRESHOLD - 1,
            ..base_facts()
        };
        assert!(!decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn shows_when_kill_count_exceeds_threshold() {
        let facts = BackgroundKillFacts {
            unexpected_kill_count: UNEXPECTED_KILL_THRESHOLD + 5,
            ..base_facts()
        };
        assert!(decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn does_not_show_when_already_exempted() {
        let facts = BackgroundKillFacts {
            is_ignoring_battery_optimizations: true,
            ..base_facts()
        };
        assert!(!decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn does_not_show_when_user_opted_out() {
        let facts = BackgroundKillFacts {
            user_opted_out: true,
            ..base_facts()
        };
        assert!(!decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn does_not_show_within_cooldown_window() {
        let facts = BackgroundKillFacts {
            last_shown_unix_secs: Some(1_000_000 - (GUIDANCE_COOLDOWN_SECS - 1)),
            ..base_facts()
        };
        assert!(!decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn shows_exactly_at_cooldown_boundary() {
        let facts = BackgroundKillFacts {
            last_shown_unix_secs: Some(1_000_000 - GUIDANCE_COOLDOWN_SECS),
            ..base_facts()
        };
        assert!(decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn shows_well_past_cooldown_window() {
        let facts = BackgroundKillFacts {
            last_shown_unix_secs: Some(0),
            now_unix_secs: GUIDANCE_COOLDOWN_SECS * 3,
            ..base_facts()
        };
        assert!(decide_battery_guidance(facts).should_show);
    }

    #[test]
    fn clock_skew_where_now_precedes_last_shown_is_treated_as_within_cooldown() {
        // saturating_sub により経過時間0として扱われ、cooldown未経過(=表示しない)側に
        // 倒れる(ドキュメント参照: クロックスキュー時は安全側=案内を控える方に倒す)。
        let facts = BackgroundKillFacts {
            last_shown_unix_secs: Some(2_000),
            now_unix_secs: 1_000,
            ..base_facts()
        };
        assert!(!decide_battery_guidance(facts).should_show);
    }
}
