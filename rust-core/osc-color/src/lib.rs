//! Pure, dependency-free OSC color escape sequence synthesis, shared by
//! `isekai-ssh` (`ctl_forward.rs`, the ctl-socket → real-terminal OSC bridge)
//! and `claude-hookd` (the standalone Claude Code state indicator, which
//! needs the identical terminal-color mapping when it emits OSC directly
//! rather than going through `isekai-ssh`'s ctl-socket — see
//! `claude-hookd`'s `delivery` module). Deliberately has zero dependencies
//! beyond `std`: both callers care about keeping their own dependency
//! footprint small (`isekai-ssh` for build times, `claude-hookd` because its
//! entire point is being usable outside the isekai-terminal ecosystem).
//!
//! Extracted 2026-08 from `isekai-ssh::ctl_forward` when `claude-hookd` was
//! split into its own standalone crate — see that crate's module docs for
//! why the split happened.

/// Which real terminal emulator is on the other end of the OSC this process
/// is about to emit, for OSC variants that differ per terminal (currently
/// only tab color — see [`tab_color_sequence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    /// Default/fallback — also what every Windows-native call site (in
    /// `isekai-ssh`) passes unconditionally, since iTerm2 doesn't exist on
    /// Windows and there is nothing to detect there.
    WindowsTerminal,
    ITerm2,
}

impl TerminalKind {
    /// Reads `$ISEKAI_TERMINAL_KIND` (explicit override) and falls back to
    /// auto-detecting from `$TERM_PROGRAM` (`iTerm.app` on iTerm2; anything
    /// else, including unset, defaults to Windows Terminal-compatible
    /// behavior).
    ///
    /// Known limitation (undetectable, not a bug to "fix"): if the calling
    /// process runs inside a LOCAL tmux, tmux 3.2+ overwrites
    /// `$TERM_PROGRAM=tmux` for everything running inside it, so the real
    /// outer terminal can't be auto-detected — use the explicit override in
    /// that case. This is also why the OSC this maps to doesn't reach the
    /// real terminal at all when a *local* tmux is in the way unless it's
    /// wrapped via [`wrap_for_tmux_passthrough`] (and `allow-passthrough` is
    /// enabled on that tmux): a local tmux swallows raw OSC 4/OSC 6 exactly
    /// like a remote one does.
    pub fn resolve() -> Self {
        Self::resolve_from(std::env::var("ISEKAI_TERMINAL_KIND").ok().as_deref(), std::env::var("TERM_PROGRAM").ok().as_deref())
    }

    pub fn resolve_from(override_val: Option<&str>, term_program: Option<&str>) -> Self {
        match override_val {
            Some("iterm2") => return Self::ITerm2,
            Some("windows-terminal") => return Self::WindowsTerminal,
            _ => {} // unset, or an unrecognized value — fall through to auto-detect
        }
        match term_program {
            Some("iTerm.app") => Self::ITerm2,
            _ => Self::WindowsTerminal,
        }
    }
}

/// Maps an RGB tab-color request to the OSC escape sequence for `terminal`:
/// - `TerminalKind::WindowsTerminal` → OSC 4 palette-index 264, Windows
///   Terminal's private convention for the tab background color
///   (`microsoft/terminal` PR #13058, which closed the original feature
///   request #6574). A harmless no-op on terminals that don't recognize that
///   index.
/// - `TerminalKind::ITerm2` → iTerm2's proprietary
///   `OSC 6;1;bg;<channel>;brightness;<0-255>`, one sequence per RGB channel
///   (see iTerm2's "Proprietary Escape Codes" documentation).
pub fn tab_color_sequence(terminal: TerminalKind, r: u8, g: u8, b: u8) -> String {
    match terminal {
        TerminalKind::WindowsTerminal => format!("\x1b]4;264;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\"),
        TerminalKind::ITerm2 => format!(
            "\x1b]6;1;bg;red;brightness;{r}\x07\x1b]6;1;bg;green;brightness;{g}\x07\x1b]6;1;bg;blue;brightness;{b}\x07"
        ),
    }
}

/// Wraps an arbitrary escape sequence in tmux's passthrough DCS
/// (`\ePtmux;<payload with every ESC doubled>\e\\`) so a tmux server with
/// `allow-passthrough on` forwards it to the real outer terminal instead of
/// swallowing it. Every real `ESC` (`0x1b`) byte inside `inner` must be
/// doubled per tmux's own escaping rule for this wrapper — this function
/// does that doubling, callers pass the plain unwrapped sequence.
///
/// This is `claude-hookd`'s `TmuxPassthrough` delivery mode's actual
/// mechanism (see that crate's `delivery` module) — deliberately NOT what
/// this project's `isekai-ssh` ctl-socket path uses (`ISEKAI_PIPE_DESIGN.md`
/// explicitly rejected relying on `allow-passthrough`, since it requires a
/// server-wide opt-in most users won't have set and is version-sensitive).
/// It's offered here as an independent, `isekai-ssh`-free option for
/// contexts (like a bare `claude-hookd` install with no isekai-terminal
/// infrastructure at all) where the ctl-socket path isn't available.
pub fn wrap_for_tmux_passthrough(inner: &str) -> String {
    let escaped = inner.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Parses a bare or `#`-prefixed 6-hex-digit color (`ff0000` / `#ff0000`)
/// into `(r, g, b)`.
pub fn parse_hex_color(value: &str) -> Result<(u8, u8, u8), String> {
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
pub fn format_hex_color((r, g, b): (u8, u8, u8)) -> String {
    format!("{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_color_on_windows_terminal_is_osc_4_264() {
        assert_eq!(tab_color_sequence(TerminalKind::WindowsTerminal, 0xff, 0x00, 0x00), "\x1b]4;264;rgb:ff/00/00\x1b\\");
    }

    #[test]
    fn tab_color_on_iterm2_is_osc_6() {
        assert_eq!(
            tab_color_sequence(TerminalKind::ITerm2, 0xff, 0x88, 0x00),
            "\x1b]6;1;bg;red;brightness;255\x07\x1b]6;1;bg;green;brightness;136\x07\x1b]6;1;bg;blue;brightness;0\x07"
        );
    }

    #[test]
    fn terminal_kind_resolve_from_defaults_to_windows_terminal_when_unset() {
        assert_eq!(TerminalKind::resolve_from(None, None), TerminalKind::WindowsTerminal);
    }

    #[test]
    fn terminal_kind_resolve_from_auto_detects_iterm2_via_term_program() {
        assert_eq!(TerminalKind::resolve_from(None, Some("iTerm.app")), TerminalKind::ITerm2);
    }

    #[test]
    fn terminal_kind_resolve_from_ignores_unrelated_term_program_values() {
        // tmux 3.2+ overwrites $TERM_PROGRAM=tmux for anything running inside it —
        // this must NOT be mistaken for iTerm2 (see `TerminalKind::resolve` doc).
        assert_eq!(TerminalKind::resolve_from(None, Some("tmux")), TerminalKind::WindowsTerminal);
        assert_eq!(TerminalKind::resolve_from(None, Some("vscode")), TerminalKind::WindowsTerminal);
    }

    #[test]
    fn terminal_kind_resolve_from_explicit_override_wins_over_auto_detection() {
        assert_eq!(TerminalKind::resolve_from(Some("iterm2"), Some("tmux")), TerminalKind::ITerm2);
        assert_eq!(TerminalKind::resolve_from(Some("windows-terminal"), Some("iTerm.app")), TerminalKind::WindowsTerminal);
    }

    #[test]
    fn terminal_kind_resolve_from_unrecognized_override_falls_back_to_auto_detect() {
        assert_eq!(TerminalKind::resolve_from(Some("bogus"), Some("iTerm.app")), TerminalKind::ITerm2);
    }

    #[test]
    fn tmux_passthrough_wraps_and_doubles_inner_escapes() {
        let inner = "\x1b]4;264;rgb:ff/00/00\x1b\\";
        let wrapped = wrap_for_tmux_passthrough(inner);
        assert_eq!(wrapped, "\x1bPtmux;\x1b\x1b]4;264;rgb:ff/00/00\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn tmux_passthrough_of_plain_text_has_no_doubling() {
        assert_eq!(wrap_for_tmux_passthrough("hello"), "\x1bPtmux;hello\x1b\\");
    }

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
