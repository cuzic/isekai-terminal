#!/bin/sh
# Default tab-progress OSC generator for claude-hookd (embedded into the
# binary via `include_str!` in src/tab_progress.rs — this file is not run
# directly by anything else). Mirrors default-tab-color.sh's terminal-kind
# detection and default policy byte-for-byte (see that file's own comment
# for the rationale) — only the emitted OSC convention differs.
#
# argv[1]: ProgressState wire value (`isekai-protocol::ctl::ProgressState`
#   as u8: 0=none, 1=normal, 2=error, 3=indeterminate, 4=warning).
# argv[2]: progress, 0-100 (meaningful only when argv[1] is "1"/normal).
# stdout: the raw, unwrapped OSC escape sequence for the resolved terminal
#   kind. Empty output means "send nothing".
#
# Terminal kind resolution (identical to default-tab-color.sh):
#   1. $ISEKAI_TERMINAL_KIND, if it is exactly "iterm2" or "windows-terminal"
#   2. else $TERM_PROGRAM = "iTerm.app" -> iterm2
#   3. else windows-terminal (the default/fallback — same reasoning as
#      default-tab-color.sh: defaulting to the Windows Terminal convention
#      is safe because it's a no-op on terminals that don't recognize
#      OSC 9;4, and over SSH the real terminal's env vars often never reach
#      the remote shell at all, so "unknown" must not mean "silent").
#
# iTerm2 does NOT support OSC 9;4 as a progress convention — it treats a
# bare OSC 9 as "post a system notification", so blindly emitting OSC 9;4
# there would pop a spurious notification showing the literal
# "4;<state>;<progress>" text. Unlike tab-color (which has a real iTerm2-
# native OSC 6 sequence to fall back to), there is no iTerm2-native progress
# convention, so iTerm2 gets empty output instead — matching
# `isekai-ssh::ctl_forward::osc_sequence_for`'s own `TerminalKind::ITerm2 =>
# None` decision for `SetProgress`.

state="$1"
progress="$2"

kind="$ISEKAI_TERMINAL_KIND"
if [ "$kind" != "iterm2" ] && [ "$kind" != "windows-terminal" ]; then
    if [ "$TERM_PROGRAM" = "iTerm.app" ]; then
        kind="iterm2"
    else
        kind="windows-terminal"
    fi
fi

if [ "$kind" = "iterm2" ]; then
    :
else
    printf '\033]9;4;%s;%s\007' "$state" "$progress"
fi
