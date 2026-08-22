//! ADR D-6 / §3.10.2-②のtmuxウィンドウclaimガード。
//!
//! プロセス内の`Mutex<HashMap>`なので、プロセス再起動(jetsam/force-quit/クラッシュ)
//! を跨いだstale claimは原理的に発生しない。`reset_for_test()`単体では並行実行下の
//! テスト独立性を保証しないため、テストは`profile_identity`をテストごとに一意にする。

use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::Mutex;

/// profile_identity → 現在のclaim owner_id。
static TMUX_WINDOW_CLAIMS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[uniffi::export]
pub fn try_claim_tmux_window(profile_identity: String, owner_id: String) -> bool {
    let mut claims = TMUX_WINDOW_CLAIMS.lock();
    match claims.get(&profile_identity) {
        Some(existing) if existing == &owner_id => true,
        Some(_) => false,
        None => {
            claims.insert(profile_identity, owner_id);
            true
        }
    }
}

#[uniffi::export]
pub fn release_tmux_window_claim(profile_identity: String, owner_id: String) {
    let mut claims = TMUX_WINDOW_CLAIMS.lock();
    if claims.get(&profile_identity) == Some(&owner_id) {
        claims.remove(&profile_identity);
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    TMUX_WINDOW_CLAIMS.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::{release_tmux_window_claim, reset_for_test, try_claim_tmux_window};

    #[test]
    fn try_claim_by_new_owner_succeeds() {
        let profile_identity = "try_claim_by_new_owner_succeeds".to_string();

        assert!(try_claim_tmux_window(profile_identity, "owner-a".to_string()));
    }

    #[test]
    fn try_claim_by_same_owner_is_idempotent() {
        let profile_identity = "try_claim_by_same_owner_is_idempotent".to_string();
        assert!(try_claim_tmux_window(profile_identity.clone(), "owner-a".to_string()));

        assert!(try_claim_tmux_window(profile_identity, "owner-a".to_string()));
    }

    #[test]
    fn try_claim_by_different_owner_fails() {
        let profile_identity = "try_claim_by_different_owner_fails".to_string();
        assert!(try_claim_tmux_window(profile_identity.clone(), "owner-a".to_string()));

        assert!(!try_claim_tmux_window(profile_identity, "owner-b".to_string()));
    }

    #[test]
    fn release_by_wrong_owner_is_ignored() {
        let profile_identity = "release_by_wrong_owner_is_ignored".to_string();
        assert!(try_claim_tmux_window(profile_identity.clone(), "owner-a".to_string()));

        release_tmux_window_claim(profile_identity.clone(), "owner-b".to_string());

        assert!(!try_claim_tmux_window(profile_identity, "owner-b".to_string()));
    }

    #[test]
    fn release_by_correct_owner_frees_the_slot() {
        let profile_identity = "release_by_correct_owner_frees_the_slot".to_string();
        assert!(try_claim_tmux_window(profile_identity.clone(), "owner-a".to_string()));

        release_tmux_window_claim(profile_identity.clone(), "owner-a".to_string());

        assert!(try_claim_tmux_window(profile_identity, "owner-b".to_string()));
    }

    #[test]
    fn release_when_not_claimed_is_a_safe_noop() {
        release_tmux_window_claim(
            "release_when_not_claimed_is_a_safe_noop".to_string(),
            "owner-a".to_string(),
        );
    }

    #[test]
    fn reset_for_test_clears_all_claims() {
        let profile_identity = "reset_for_test_clears_all_claims".to_string();
        assert!(try_claim_tmux_window(profile_identity.clone(), "owner-a".to_string()));

        reset_for_test();

        assert!(try_claim_tmux_window(profile_identity, "owner-b".to_string()));
    }
}
