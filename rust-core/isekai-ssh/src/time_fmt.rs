//! SHA-256 hex encoding and a minimal hand-rolled RFC 3339 UTC formatter,
//! shared by `init.rs` (writing a fresh `PersistentProfile`'s
//! `trusted_helper_sha256`/`trusted_at`) and `wrapper.rs` (the same fields
//! on automatic re-bootstrap). Previously duplicated identically in both
//! modules.

use sha2::{Digest, Sha256};

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Current UTC time formatted as RFC 3339 (`trusted_at`/`last_seen_at` are
/// purely informational per `isekai-trust`'s schema docs, so a hand-rolled
/// formatter — rather than pulling in a full datetime crate for this alone —
/// is enough).
pub(crate) fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_rfc3339_utc(secs)
}

/// Minimal civil-calendar conversion from a Unix timestamp to
/// `YYYY-MM-DDTHH:MM:SSZ`, good for any date this project will ever run at
/// (proleptic Gregorian, UTC only — exactly what `trusted_at`/`last_seen_at`
/// need and nothing more).
pub(crate) fn format_rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);

    // Civil-from-days algorithm (Howard Hinnant's `civil_from_days`),
    // proleptic Gregorian, days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_sha256_matches_known_vector() {
        // sha256("") — a standard test vector.
        assert_eq!(hex_sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn format_rfc3339_utc_matches_a_known_timestamp() {
        // 2026-07-04T00:00:00Z, matching the fixtures used across
        // isekai-trust's own tests.
        let unix_secs = 1_783_123_200u64;
        assert_eq!(format_rfc3339_utc(unix_secs), "2026-07-04T00:00:00Z");
    }
}
