//! In-memory KV store backing `CtlMessage::SetVar`/`GetVarRequest` (task
//! #16). Deliberately the simplest thing that could work: a
//! `Mutex<HashMap<String, String>>`, no disk persistence, no eviction beyond
//! "the whole store disappears when the process/session that owns it does".
//! The task explicitly green-lit this ("セッション終了で消えるプロセスメモリ
//! 方式がシンプルで妥当な初期実装") rather than a bigger spike into
//! cross-process/disk-backed sharing.
//!
//! `VarScope` (`ctl.rs`) selects *which* `CtlVarStore` instance a caller
//! reads/writes, not a partition within one store — so the actual
//! tab/session/global semantics are a property of how many `CtlVarStore`
//! instances the receiving process keeps around and shares between tabs, not
//! of this type itself:
//!
//! - `isekai-ssh`'s Unix `ssh(1)`-wrapper path (`ctl_forward.rs`): one
//!   process per tab, so a single `CtlVarStore` serves `Tab`/`Session`/
//!   `Global` alike — `Global` does **not** currently span multiple
//!   `isekai-ssh` invocations (that would need cross-process, likely
//!   disk-backed, sharing — left as documented future work, not implemented
//!   here).
//! - The isekai-terminal Android app's in-process listener
//!   (`src/transport/ssh_handler.rs`): one `CtlVarStore` per tab for
//!   `Tab`/`Session`, plus one process-wide `CtlVarStore` shared by every tab
//!   for `Global`.
//!
//! Pure in-memory, no I/O — lives in this crate (rather than
//! `isekai-pipe-core`, which `isekai-terminal-core`/the Android app cannot
//! depend on without a dependency-graph change) purely so both receiving
//! implementations can share one implementation, the same reason
//! `CtlMessage` itself lives here.

use std::collections::HashMap;
use std::sync::Mutex;

/// A single scope's worth of shared variables. Cheap to construct
/// (`CtlVarStore::default()`); callers hold it behind whatever lifetime is
/// appropriate for the scope it backs (see module docs).
#[derive(Debug, Default)]
pub struct CtlVarStore {
    vars: Mutex<HashMap<String, String>>,
}

impl CtlVarStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `value` under `key`, overwriting any previous value.
    pub fn set(&self, key: impl Into<String>, value: impl Into<String>) {
        let mut guard = self.vars.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(key.into(), value.into());
    }

    /// Returns the current value for `key`, or `None` if it was never set
    /// (or has since been removed).
    pub fn get(&self, key: &str) -> Option<String> {
        let guard = self.vars.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(key).cloned()
    }

    /// Number of keys currently stored. Test/diagnostic use.
    pub fn len(&self) -> usize {
        self.vars.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_on_unset_key_is_none() {
        let store = CtlVarStore::new();
        assert_eq!(store.get("missing"), None);
    }

    #[test]
    fn set_then_get_round_trips() {
        let store = CtlVarStore::new();
        store.set("last_build_status", "ok");
        assert_eq!(store.get("last_build_status"), Some("ok".to_string()));
    }

    #[test]
    fn set_overwrites_previous_value() {
        let store = CtlVarStore::new();
        store.set("k", "first");
        store.set("k", "second");
        assert_eq!(store.get("k"), Some("second".to_string()));
    }

    #[test]
    fn distinct_keys_do_not_collide() {
        let store = CtlVarStore::new();
        store.set("a", "1");
        store.set("b", "2");
        assert_eq!(store.get("a"), Some("1".to_string()));
        assert_eq!(store.get("b"), Some("2".to_string()));
    }

    #[test]
    fn len_and_is_empty_reflect_contents() {
        let store = CtlVarStore::new();
        assert!(store.is_empty());
        store.set("a", "1");
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
    }
}
