//! `rrggbb` hex color parsing, shared by `isekai-pipe ctl tab-color`,
//! `#@isekai tab-idle-color`/`tab-attention-color` directive validation, and
//! `claude-hookd`'s color env-var resolution (`ISEKAI_PIPE_DESIGN.md` §8
//! Epic Q). Centralized here (rather than duplicated per crate) because the
//! `#@isekai` directive path feeds a value straight into the shell command
//! line `isekai-ssh` execs on connect (`ctl_forward.rs::remote_command_arg`)
//! — an unvalidated or under-validated color string there is a shell
//! injection / connection-killing bug, not just a cosmetic one, so this
//! function is the single place that decides what a "valid color" is.

/// Parses a `rrggbb` (optionally `#`-prefixed) color argument into its three
/// channel bytes. Rejects anything that isn't exactly 6 ASCII hex digits
/// after stripping an optional leading `#`, so a caller that embeds the
/// result directly into a shell command line (rather than re-serializing
/// through JSON like the `ctl` wire protocol does) can do so safely without
/// its own quoting logic.
pub fn parse_hex_color(value: &str) -> Result<(u8, u8, u8), String> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{value:?} is not a valid color (expected 6 hex digits, e.g. ff0000 or '#ff0000')"
        ));
    }
    let r = u8::from_str_radix(&hex[0..2], 16).expect("validated hex digits");
    let g = u8::from_str_radix(&hex[2..4], 16).expect("validated hex digits");
    let b = u8::from_str_radix(&hex[4..6], 16).expect("validated hex digits");
    Ok((r, g, b))
}

/// Formats an `(r, g, b)` triple back to bare lowercase `rrggbb` hex — no
/// `#` prefix, so the result is always safe to interpolate directly into a
/// shell command line or an `export FOO=...;` clause.
pub fn format_hex_color((r, g, b): (u8, u8, u8)) -> String {
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
        assert!(parse_hex_color("").is_err());
    }

    #[test]
    fn rejects_non_hex_digits() {
        assert!(parse_hex_color("zzzzzz").is_err());
    }

    #[test]
    fn rejects_shell_metacharacters_disguised_as_color() {
        // The concrete failure mode this function exists to prevent: a
        // config-file value that would otherwise be interpolated verbatim
        // into a shell command line.
        assert!(parse_hex_color("$(id)").is_err());
        assert!(parse_hex_color("#$(id)").is_err());
    }

    #[test]
    fn format_round_trips_through_parse() {
        let formatted = format_hex_color((0xff, 0x00, 0x80));
        assert_eq!(formatted, "ff0080");
        assert_eq!(parse_hex_color(&formatted), Ok((0xff, 0x00, 0x80)));
    }
}
