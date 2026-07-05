//! Current Unix time as `i64`, shared by every place in this crate (and
//! `isekai-ssh`) that needs to stamp or compare against a token's
//! `expires_at`. `i64` (not `u64`) matches `TokenSet::expires_at`'s type —
//! chosen there so a comparison against `expires_at - REFRESH_SKEW_SECS`
//! never has to worry about unsigned underflow.

/// Current time as a Unix timestamp (seconds). Saturates to `0` if the
/// system clock is somehow set before the epoch, rather than panicking —
/// this value is only ever used for expiry comparisons/bookkeeping, not
/// anything security-critical enough to warrant failing closed.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
