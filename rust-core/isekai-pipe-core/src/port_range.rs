//! Shared `<START>-<END>` UDP/QUIC port range parsing.
//!
//! Previously reimplemented identically in three places — `isekai-ssh`'s
//! `#@isekai remote-bind-port-range` directive (`wrapper.rs`), `isekai-pipe
//! connect`'s `--bind-port-range` flag (`main.rs`), and `isekai-pipe
//! serve`'s own `--bind-port-range` flag (`engine/mod.rs`) — which risked
//! the three call sites' validation drifting apart. Each caller wraps the
//! bare error message returned here with its own flag-name/subsystem
//! context.

/// Parses `<START>-<END>` into an inclusive `(start, end)` `u16` pair,
/// rejecting `start > end`.
pub fn parse_port_range(value: &str) -> Result<(u16, u16), String> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| format!("invalid port range {value:?} (expected <START>-<END>)"))?;
    let start: u16 = start.parse().map_err(|_| format!("invalid port range start {start:?}"))?;
    let end: u16 = end.parse().map_err(|_| format!("invalid port range end {end:?}"))?;
    if start > end {
        return Err(format!("invalid port range {value:?}: start must be <= end"));
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_range() {
        assert_eq!(parse_port_range("40000-40100").unwrap(), (40000, 40100));
    }

    #[test]
    fn accepts_a_single_port_range() {
        assert_eq!(parse_port_range("40000-40000").unwrap(), (40000, 40000));
    }

    #[test]
    fn rejects_a_range_with_start_after_end() {
        assert!(parse_port_range("40100-40000").is_err());
    }

    #[test]
    fn rejects_a_single_port() {
        assert!(parse_port_range("40000").is_err());
    }

    #[test]
    fn rejects_non_numeric_bounds() {
        assert!(parse_port_range("abc-def").is_err());
    }
}
