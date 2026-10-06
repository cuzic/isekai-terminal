//! `Millis`: reducerへ時刻を渡すための値型(ADR_FUNCTIONAL_CORE_EFFECTS.md §2.2、Q1)。
//!
//! shell が持つエポック(プロセス/shell起動時の `tokio::time::Instant`)からの経過ミリ秒。
//! **wire(プロトコルフレーム)に載せないこと。異なるshell/プロセスの値同士を比較しないこと**
//! (エポックがshellごとに異なるので無意味)。serde導出は §2.6 の replay 記録のためだけにある。
//!
//! このcrateは本来wire型のcrateだが、`isekai-terminal-core`・`isekai-pipe`・`isekai-ssh`・
//! `isekai-transport`のすべてが既に依存している純粋crateなので、ここに置く(ADR §2.2)。

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Millis(pub u64);

impl Millis {
    /// `self - earlier`。時刻が逆行している(`self < earlier`)場合は0(=「未経過」)。
    ///
    /// `Millis`同士の差はこの関数でのみ計算する(素の`-`は禁止: releaseビルドでは
    /// overflow checkが既定で無効なのでwrapする)。reducerは`now`が非単調に届くことを
    /// 前提にしているので、逆行を0に潰すことで「期限切れ」の誤発火を防ぐ。
    pub fn saturating_sub(self, earlier: Millis) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_sub_is_the_forward_difference() {
        assert_eq!(Millis(1_500).saturating_sub(Millis(500)), Duration::from_millis(1_000));
        assert_eq!(Millis(7).saturating_sub(Millis(7)), Duration::ZERO);
    }

    #[test]
    fn saturating_sub_clamps_time_going_backwards_to_zero() {
        assert_eq!(Millis(500).saturating_sub(Millis(1_500)), Duration::ZERO);
        assert_eq!(Millis(0).saturating_sub(Millis(u64::MAX)), Duration::ZERO);
    }

    #[test]
    fn serde_roundtrip_is_a_bare_number() {
        let json = serde_json::to_string(&Millis(42)).unwrap();
        assert_eq!(json, "42");
        assert_eq!(serde_json::from_str::<Millis>(&json).unwrap(), Millis(42));
    }
}
