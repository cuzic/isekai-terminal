//! Bare hex color string parsing (`ff8800` / `#ff8800` <-> `(u8, u8, u8)`) —
//! used for `--idle-color`/`--attention-color`/`--waiting-color` CLI args and
//! their `$ISEKAI_TAB_*_COLOR` env var equivalents. Split out of the former
//! `osc-color` crate dependency (removed 2026-08 — see `tab_color.rs` for
//! where that crate's other job, terminal-kind-specific OSC generation,
//! moved to) since this part has nothing to do with any particular
//! terminal's escape sequence format.

/// Parses a bare or `#`-prefixed 6-hex-digit color (`ff0000` / `#ff0000`)
/// into `(r, g, b)`.
pub(crate) fn parse_hex_color(value: &str) -> Result<(u8, u8, u8), String> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{value:?} is not a valid color (expected 6 hex digits, e.g. ff0000 or '#ff0000')"));
    }
    let r = u8::from_str_radix(&hex[0..2], 16).expect("validated hex digits");
    let g = u8::from_str_radix(&hex[2..4], 16).expect("validated hex digits");
    let b = u8::from_str_radix(&hex[4..6], 16).expect("validated hex digits");
    Ok((r, g, b))
}

/// Formats an `(r, g, b)` triple back to bare lowercase `rrggbb` hex — no
/// `#` prefix, so the result is always safe to interpolate directly into a
/// shell command line or an `export FOO=...;` clause.
pub(crate) fn format_hex_color((r, g, b): (u8, u8, u8)) -> String {
    format!("{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_hash_prefixed() {
        assert_eq!(parse_hex_color("ff0000"), Ok((0xff, 0x00, 0x00)));
        assert_eq!(parse_hex_color("#00ff80"), Ok((0x00, 0xff, 0x80)));
    }

    #[test]
    fn accepts_uppercase() {
        assert_eq!(parse_hex_color("FF0000"), Ok((0xff, 0x00, 0x00)));
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(parse_hex_color("fff").is_err());
        assert!(parse_hex_color("ff00000").is_err());
    }

    #[test]
    fn rejects_non_hex_digits() {
        assert!(parse_hex_color("zzzzzz").is_err());
    }

    #[test]
    fn format_hex_color_round_trips() {
        assert_eq!(format_hex_color((0xff, 0x00, 0x80)), "ff0080");
        assert_eq!(parse_hex_color(&format_hex_color((0x12, 0x34, 0x56))), Ok((0x12, 0x34, 0x56)));
    }
}
