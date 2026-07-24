//! タスク#14: Androidプロセスkillからの黙示的セッション再アタッチ。
//!
//! ## 何を実装し、何を実装しなかったか(設計判断)
//!
//! `.claude/rules/always-connects.md`の「常に接続できる」原則は、ネットワーク経路の
//! 切断・ローミング(`resume_client::ReattachableStream`のRESUMEフレームによる、
//! **同一プロセス内**でのQUIC/isekai-pipe接続の裏での張り替え)をカバーしている。
//! 一方、Androidがバックグラウンドでアプリ**プロセス自体**をkillした場合は、
//! `ReattachableStream`が保持していたSSHクライアント(russh)の暗号状態(セッション鍵・
//! MACシーケンス番号)ごとメモリから消え去る。
//!
//! `ReattachableStream`のRESUME(`session_id`+`c2h_sent_offset`+`h2c_client_delivered_offset`)
//! は、isekai-pipe serve側から見て「TCP:22への生バイト列パイプが一時的に途切れただけ」
//! という**QUIC/中継層のみ**の再接続契約であり(`ISEKAI_PIPE_DESIGN.md` §6.3
//! 「data stream: SSHの生バイト列(HELLO/ACK後はフレーミング無し)」)、SSHプロトコル
//! そのものの継続性は一切保証しない。プロセスがkillされた後に新しいrussh
//! クライアントで同じ`session_id`を使ってこのRESUMEを試みても、新しいrusshは
//! 必ずゼロから`SSH_MSG_KEXINIT`(平文のプロトコルバージョン交換)を送出しようとする。
//! これは相手(isekai-pipe serveの先の実sshd)にとっては、既に鍵交換済みの
//! セッションの暗号化アプリケーションデータの**途中**に紛れ込む不正バイト列にしか
//! 見えず、ほぼ確実にMAC検証失敗等でsshd側から即座に切断される——「サイレントに
//! 元のシェルへ戻る」ことは原理的に不可能(SSHは対話プロトコルレベルでのmid-session
//! resumeを想定していない。mosh等が独自プロトコルを持つのはまさにこの制約のため)。
//! 加えて、`ReplayBuffer`(C→S再送用バッファ)自体もプロセスのメモリ上にしか無いため、
//! プロセス再起動後は「まだhelperにackされていない送信済みバイト」を再送する手段も
//! 失われている。
//!
//! そのため本モジュールは、`resume_client::SessionId`のワイヤーレベルRESUMEを
//! プロセス再起動後に再利用することは行わない(行うと『成功したように見えて実際は
//! 即座に失敗する接続の空撃ち』を毎回発生させるだけで、既存のRESUMEリトライ
//! (`REATTACH_MAX_RETRIES`)を巻き込んで無駄な遅延を生みかねない)。
//!
//! 代わりに実装するのは、**「直近アクティブだったセッションを、ユーザー操作なしに
//! 通常の新規接続で自動的に復元してよいか」の判断ポリシー**である。実際の
//! 新規接続(新しい`SessionId`での通常ATTACH)は既存の`SessionOrchestrator::connect*`
//! がそのまま担い、サーバー側に残っている古いparkセッションは`ISEKAI_PIPE_DESIGN.md`
//! §8 Epic N-4の`hello_with_parked_preemption`が新規ATTACH時に自動的に立ち退かせる
//! (このモジュールが明示的に何かを解放する必要はない)。
//!
//! 実際の永続化(どのプロファイルのタブが開いていたか)・profile lookup・自動
//! `openTab()`呼び出しはAndroid側(Kotlin、`TerminalTabsViewModel`/
//! `ReattachStateStore`)が担う——ここに永続化されるのはKotlin側が生成した
//! ローカルな記録(タブID・プロファイルID・保存時刻)であり、isekai-pipeの
//! ワイヤーレベル`SessionId`そのものではない(`resume_client::SessionId`は
//! `pub(crate)`のままRust内部に閉じており、UniFFI境界を越えて公開されていない
//! ことも参照)。「いつまでを黙示的な自動再接続の対象とみなすか」という
//! ポリシー判断だけは、他の状態機械判断と同じく`.claude/rules/rust-ssot.md`に
//! 従いRust側に一元化し、Kotlin側では複製しない。

/// アプリプロセスがkillされてから、次回起動時に「黙示的に自動再接続してよい」と
/// 判断する猶予期間(秒)。isekai-pipe serve側の`--resume-window`(既定10日間、
/// `ISEKAI_PIPE_DESIGN.md` §6.4)とは意図的に無関係な値である——本モジュールは
/// 前述の通りサーバー側のparkセッションを再利用しないため、サーバー側の猶予期間を
/// 知る必要も合わせる必要も無い。ここでの猶予期間は純粋にクライアント側のUXポリシー
/// (「数分〜数十分前まで開いていたタブなら、プロセスkillからの復帰時にユーザー操作
/// 無しで再接続してよい」)であり、OSのLMK(Low Memory Killer)がバックグラウンド
/// プロセスをkillしてからユーザーがアプリを再度前面に出すまでの典型的な間隔を
/// カバーすることを狙った値。
pub const AUTO_REATTACH_GRACE_SECS: u64 = 30 * 60;

/// [`AUTO_REATTACH_GRACE_SECS`]をUniFFI経由でKotlin/Swift側に公開する。値そのものを
/// Kotlin側にハードコードで複製させないための単純なgetter。
#[uniffi::export]
pub fn reattach_grace_window_secs() -> u64 {
    AUTO_REATTACH_GRACE_SECS
}

/// 永続化された「直近アクティブだったセッション」記録が、黙示的な自動再接続を
/// 試みるにあたってまだ新鮮かどうかを判定する。`saved_at_unix_secs`は記録時刻、
/// `now_unix_secs`は判定時刻(いずれもUnix epoch秒)。
///
/// `now_unix_secs`が`saved_at_unix_secs`より前(端末の時計調整等で稀に起こりうる)の
/// 場合は`saturating_sub`により経過時間0として扱い、freshと判定する——「保存した
/// 直後なのに古いと誤判定される」という直感に反する挙動を避けるための意図的な選択。
#[uniffi::export]
pub fn reattach_record_is_fresh(saved_at_unix_secs: u64, now_unix_secs: u64) -> bool {
    now_unix_secs.saturating_sub(saved_at_unix_secs) <= AUTO_REATTACH_GRACE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_exactly_at_window_boundary_counts_as_fresh() {
        assert!(reattach_record_is_fresh(
            1_000,
            1_000 + AUTO_REATTACH_GRACE_SECS
        ));
    }

    #[test]
    fn stale_just_past_window_boundary_is_rejected() {
        assert!(!reattach_record_is_fresh(
            1_000,
            1_000 + AUTO_REATTACH_GRACE_SECS + 1
        ));
    }

    #[test]
    fn well_within_window_is_fresh() {
        assert!(reattach_record_is_fresh(1_000, 1_060));
    }

    #[test]
    fn far_in_the_past_is_stale() {
        assert!(!reattach_record_is_fresh(0, 1_000_000));
    }

    #[test]
    fn clock_skew_where_now_precedes_saved_at_is_treated_as_fresh() {
        // saturating_sub により経過時間0として扱われる(ドキュメント参照)。
        assert!(reattach_record_is_fresh(2_000, 1_000));
    }

    #[test]
    fn grace_window_getter_matches_constant() {
        assert_eq!(reattach_grace_window_secs(), AUTO_REATTACH_GRACE_SECS);
    }
}
