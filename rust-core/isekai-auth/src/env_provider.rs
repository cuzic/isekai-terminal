//! `TokenProvider` backed by an environment variable
//! (`ISEKAI_SSH_DESIGN.md` フェーズ表 S-0c-1: "`ISEKAI_RELAY_JWT`環境変数からの
//! トークン取得が動くこと").

use crate::{AuthError, TokenProvider};

/// Default environment variable name used by `EnvTokenProvider::new`.
pub const RELAY_JWT_ENV_VAR: &str = "ISEKAI_RELAY_JWT";

/// Reads the relay JWT from an environment variable (`ISEKAI_RELAY_JWT` by
/// default).
///
/// The variable name is configurable (`with_var_name`) so that tests don't
/// need to mutate the process-global `ISEKAI_RELAY_JWT` name shared with
/// every other concurrently-running test in the process — each test can use
/// its own uniquely-named variable instead, avoiding the
/// `std::env::set_var` race condition that a shared name would create
/// (`isekai-trust::store::config_dir_from_home` splits `HOME` handling the
/// same way for the same reason).
pub struct EnvTokenProvider {
    var_name: String,
}

impl EnvTokenProvider {
    /// Reads from the default `ISEKAI_RELAY_JWT` env var.
    pub fn new() -> Self {
        Self { var_name: RELAY_JWT_ENV_VAR.to_string() }
    }

    /// Reads from a custom env var name. Primarily useful for tests; see
    /// the struct docs for why.
    pub fn with_var_name(var_name: impl Into<String>) -> Self {
        Self { var_name: var_name.into() }
    }
}

impl Default for EnvTokenProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenProvider for EnvTokenProvider {
    fn get_relay_jwt(&self) -> Result<String, AuthError> {
        token_from_lookup(&self.var_name, |name| std::env::var(name).ok())
    }
}

/// Pure logic split out from the actual `std::env::var` call so it can be
/// unit-tested with an injected lookup function, without touching any real
/// process env var at all.
fn token_from_lookup(
    var_name: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, AuthError> {
    let raw = lookup(var_name).ok_or_else(|| AuthError::EnvVarMissing { var_name: var_name.to_string() })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AuthError::EnvVarEmpty { var_name: var_name.to_string() });
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Guards the handful of tests below that exercise the real
    // `std::env::var`/`std::env::set_var` (via a uniquely-named variable per
    // test, but still process-global state) so they can't interleave with
    // each other even if the test harness ever runs them concurrently.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pure_lookup_returns_trimmed_value() {
        let mut vars = HashMap::new();
        vars.insert("SOME_VAR".to_string(), "  a-jwt-value  ".to_string());
        let result = token_from_lookup("SOME_VAR", |name| vars.get(name).cloned());
        assert_eq!(result.unwrap(), "a-jwt-value");
    }

    #[test]
    fn pure_lookup_missing_var_is_an_error() {
        let err = token_from_lookup("MISSING_VAR", |_| None).unwrap_err();
        assert!(matches!(err, AuthError::EnvVarMissing { var_name } if var_name == "MISSING_VAR"));
    }

    #[test]
    fn pure_lookup_empty_var_is_an_error() {
        let err = token_from_lookup("EMPTY_VAR", |_| Some("   ".to_string())).unwrap_err();
        assert!(matches!(err, AuthError::EnvVarEmpty { var_name } if var_name == "EMPTY_VAR"));
    }

    #[test]
    fn get_relay_jwt_reads_from_a_custom_var_name() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let var_name = "ISEKAI_AUTH_TEST_CUSTOM_VAR_NAME";
        std::env::set_var(var_name, "custom-token");
        let result = EnvTokenProvider::with_var_name(var_name).get_relay_jwt();
        std::env::remove_var(var_name);
        assert_eq!(result.unwrap(), "custom-token");
    }

    #[test]
    fn get_relay_jwt_reads_from_the_default_isekai_relay_jwt_var() {
        // End-to-end coverage of the acceptance criterion "TokenProvider
        // traitを介してISEKAI_RELAY_JWT環境変数からトークンが取得できる":
        // exercises `EnvTokenProvider::new()` (the real default var name),
        // guarded by `ENV_TEST_LOCK` since it mutates real process state
        // that every other test in this module could otherwise observe.
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let previous = std::env::var(RELAY_JWT_ENV_VAR).ok();

        std::env::set_var(RELAY_JWT_ENV_VAR, "default-var-token");
        let result = EnvTokenProvider::new().get_relay_jwt();

        match previous {
            Some(value) => std::env::set_var(RELAY_JWT_ENV_VAR, value),
            None => std::env::remove_var(RELAY_JWT_ENV_VAR),
        }

        assert_eq!(result.unwrap(), "default-var-token");
    }

    #[test]
    fn get_relay_jwt_missing_var_is_an_error() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let var_name = "ISEKAI_AUTH_TEST_DEFINITELY_UNSET_VAR";
        std::env::remove_var(var_name);
        let err = EnvTokenProvider::with_var_name(var_name).get_relay_jwt().unwrap_err();
        assert!(matches!(err, AuthError::EnvVarMissing { .. }));
    }
}
