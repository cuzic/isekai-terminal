//! `ssh(1)`-style escape sequence detection (`~.`, `~^Z`, `~~`, `~?`).
//! Extracted from `connect.rs` so the pure scanning logic is unit-testable
//! without an SSH channel or a real terminal.

/// What the escape sequence detector found in the stdin bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EscapeAction {
    /// No escape sequence was found.
    None,
    /// `~.` — disconnect the session.
    Disconnect,
    /// `~^Z` — suspend the process (Unix only).
    Suspend,
    /// `~?` — print help.
    Help,
}

/// Scans `data` for `ssh(1)`-style escape sequences (`~.`, `~^Z`, `~~`,
/// `~?`). Returns the bytes that should be sent to the remote (with escape
/// sequences stripped or decoded) and any escape action to take.
///
/// `at_line_start` and `pending_escape` are carried across calls so a `~`
/// that arrives as the last byte of one read and its command character in
/// the next read are still handled correctly.
pub(crate) fn process_stdin_bytes(
    data: &[u8],
    at_line_start: &mut bool,
    pending_escape: &mut bool,
) -> (Vec<u8>, EscapeAction) {
    let mut to_send = Vec::with_capacity(data.len());
    let mut action = EscapeAction::None;
    let mut i = 0;

    while i < data.len() {
        let b = data[i];

        if *pending_escape {
            *pending_escape = false;
            match b {
                b'.' => {
                    action = EscapeAction::Disconnect;
                    i += 1;
                    continue;
                }
                b'~' => {
                    to_send.push(b'~');
                    i += 1;
                    continue;
                }
                b'?' => {
                    action = EscapeAction::Help;
                    i += 1;
                    continue;
                }
                0x1A => {
                    // Ctrl-Z
                    action = EscapeAction::Suspend;
                    i += 1;
                    continue;
                }
                b'\r' | b'\n' => {
                    // `~` followed by a newline sends just the `~` (OpenSSH
                    // behavior: the escape character alone at the start of a
                    // line is consumed). The newline starts a new line.
                    to_send.push(b'~');
                    *at_line_start = true;
                    i += 1;
                    continue;
                }
                _ => {
                    // Not a recognized escape command — send the tilde and the
                    // character verbatim.
                    to_send.push(b'~');
                    *at_line_start = false;
                    // Fall through to send the current byte below.
                }
            }
        }

        if *at_line_start && b == b'~' {
            *pending_escape = true;
            i += 1;
            continue;
        }

        if b == b'\n' || b == b'\r' {
            *at_line_start = true;
        } else {
            *at_line_start = false;
        }
        to_send.push(b);
        i += 1;
    }

    (to_send, action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(data: &[u8]) -> (Vec<u8>, EscapeAction) {
        let (bytes, action) = process_stdin_bytes(data, &mut true, &mut false);
        (bytes, action)
    }

    fn scan_continue(data: &[u8], at_line: &mut bool, pending: &mut bool) -> (Vec<u8>, EscapeAction) {
        process_stdin_bytes(data, at_line, pending)
    }

    #[test]
    fn plain_text_passes_through() {
        let (bytes, action) = scan(b"hello\n");
        assert_eq!(bytes, b"hello\n");
        assert_eq!(action, EscapeAction::None);
    }

    #[test]
    fn tilde_dot_disconnects() {
        let (bytes, action) = scan(b"echo hi\r\n~.");
        assert_eq!(bytes, b"echo hi\r\n");
        assert_eq!(action, EscapeAction::Disconnect);
    }

    #[test]
    fn tilde_dot_after_newline_disconnects() {
        let (bytes, action) = scan(b"ls\n~.");
        assert_eq!(bytes, b"ls\n");
        assert_eq!(action, EscapeAction::Disconnect);
    }

    #[test]
    fn double_tilde_sends_literal_tilde() {
        let (bytes, action) = scan(b"text\r\n~~more");
        assert_eq!(bytes, b"text\r\n~more");
        assert_eq!(action, EscapeAction::None);
    }

    #[test]
    fn tilde_question_shows_help() {
        let (bytes, action) = scan(b"cmd\n~?");
        assert_eq!(bytes, b"cmd\n");
        assert_eq!(action, EscapeAction::Help);
    }

    #[test]
    fn tilde_ctrl_z_suspends() {
        let (bytes, action) = scan(b"\r\n~\x1A");
        assert_eq!(bytes, b"\r\n");
        assert_eq!(action, EscapeAction::Suspend);
    }

    #[test]
    fn tilde_not_at_line_start_is_literal() {
        let (bytes, action) = scan(b"hello~.world");
        assert_eq!(bytes, b"hello~.world");
        assert_eq!(action, EscapeAction::None);
    }

    #[test]
    fn tilde_alone_at_line_start_with_newline_sends_tilde() {
        let (bytes, action) = scan(b"cmd\n~\nnext");
        // `~` at line start followed by newline → sends `~` literally,
        // the newline is consumed as part of the escape sequence.
        assert_eq!(bytes, b"cmd\n~next");
        assert_eq!(action, EscapeAction::None);
    }

    #[test]
    fn tilde_with_unknown_command_sends_both() {
        let (bytes, action) = scan(b"\r\n~x");
        assert_eq!(bytes, b"\r\n~x");
        assert_eq!(action, EscapeAction::None);
    }

    #[test]
    fn pending_escape_carries_across_calls() {
        let mut at_line = true;
        let mut pending = false;

        // First call: `\r\n` makes at_line_start true, then `~` sets pending_escape
        let (bytes, action) = scan_continue(b"\r\n~", &mut at_line, &mut pending);
        assert_eq!(bytes, b"\r\n");
        assert_eq!(action, EscapeAction::None);
        assert!(pending, "pending_escape should be true after `~` at line start");

        // Second call: `.` arrives, completing the `~.` escape
        let (bytes, action) = scan_continue(b".", &mut at_line, &mut pending);
        assert!(bytes.is_empty());
        assert_eq!(action, EscapeAction::Disconnect);
        assert!(!pending, "pending_escape should be cleared");
    }

    #[test]
    fn pending_escape_with_unknown_command_in_next_call() {
        let mut at_line = true;
        let mut pending = false;

        let (bytes, action) = scan_continue(b"\r\n~", &mut at_line, &mut pending);
        assert_eq!(bytes, b"\r\n");
        assert!(pending);

        // `x` is not a valid escape command → send `~x` literally
        let (bytes, action) = scan_continue(b"x", &mut at_line, &mut pending);
        assert_eq!(bytes, b"~x");
        assert_eq!(action, EscapeAction::None);
        assert!(!pending);
    }

    #[test]
    fn carriage_return_sets_line_start() {
        let mut at_line = false;
        let mut pending = false;

        let (bytes, _) = scan_continue(b"text\r\n", &mut at_line, &mut pending);
        assert_eq!(bytes, b"text\r\n");
        assert!(at_line, "\\r or \\n should set at_line_start");
    }

    #[test]
    fn empty_input_returns_empty() {
        let (bytes, action) = scan(b"");
        assert!(bytes.is_empty());
        assert_eq!(action, EscapeAction::None);
    }
}