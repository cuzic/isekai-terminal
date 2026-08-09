//! RFC 3339 timestamp formatting for this crate's own `trusted_at`/
//! `last_seen_at` fields ([`crate::HelperTrust`]/[`crate::SshHostKeyTrust`]).
//!
//! Also the shared home for what used to be up to three verbatim copies of
//! this same ~25-line Hinnant civil-calendar algorithm: `isekai-ssh`'s
//! `wrapper.rs` carried one (`init.rs` used to have its own too, before it
//! was folded into `wrapper.rs`'s), and this crate's own
//! `host_key_verifier.rs` carried another. Consolidated here because this
//! crate already owns the timestamp fields these values are written into,
//! and both `isekai-ssh` and `isekai-bootstrap` already depend on
//! `isekai-trust` — never the other way around — so re-exporting this from
//! here cannot introduce a dependency cycle.

/// Current UTC time formatted as RFC 3339 (e.g. `"2026-07-04T00:00:00Z"`).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_rfc3339_utc(secs)
}

/// Formats a Unix timestamp (seconds since the epoch) as RFC 3339 UTC.
pub fn format_rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm (public domain,
/// <http://howardhinnant.github.io/date_algorithms.html>) — converts a day
/// count since the Unix epoch into a proleptic-Gregorian (year, month, day).
/// No `chrono`/`time` dependency needed for a value this codebase only ever
/// displays, never parses back arithmetically.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 is day 0 by definition.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-17, cross-checked against Python's
        // `(date(2026,7,17) - date(1970,1,1)).days` = 20651.
        assert_eq!(civil_from_days(20_651), (2026, 7, 17));
    }

    #[test]
    fn rfc3339_formats_a_known_timestamp() {
        // 2026-07-04T00:00:00Z, matching the fixtures used across this
        // crate's own tests (and, formerly, isekai-ssh's duplicate of this
        // same test).
        let unix_secs = 1_783_123_200u64;
        assert_eq!(format_rfc3339_utc(unix_secs), "2026-07-04T00:00:00Z");
    }
}
