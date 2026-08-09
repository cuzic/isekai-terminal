#!/bin/sh
# Default tab-color OSC generator for claude-hookd (embedded into the binary
# via `include_str!` in src/tab_color.rs — this file is not run directly by
# anything else). Faithfully reproduces the former `osc-color` crate's
# `TerminalKind::resolve()` + `tab_color_sequence()` behavior byte-for-byte,
# so existing zero-config deployments see no change: only the *mechanism*
# (shell instead of compiled Rust) changed, not the defaults.
#
# argv[1]: bare 6-hex-digit color, e.g. "ff8800" (no leading "#").
# stdout: the raw, unwrapped OSC escape sequence for the resolved terminal
#   kind. Empty output means "send nothing" (this script never does that on
#   its own, but a user's override script at ~/.config/claude-hookd/hooks/
#   tab-color is free to).
#
# Terminal kind resolution (same as the removed osc-color crate):
#   1. $ISEKAI_TERMINAL_KIND, if it is exactly "iterm2" or "windows-terminal"
#   2. else $TERM_PROGRAM = "iTerm.app" -> iterm2
#   3. else windows-terminal (the default/fallback — see osc-color's git
#      history for why: OSC 4;264 is Windows Terminal's own private
#      tab-color convention and a harmless no-op on terminals that don't
#      recognize that palette index, so defaulting to it is safe even when
#      the real terminal can't be determined, e.g. over SSH where the local
#      terminal's own env vars never reach the remote shell at all).

hex="$1"
# POSIX parameter expansion instead of `cut` (which was here originally) —
# avoids forking 3 external processes per color update just to slice a
# 6-char string, and removes a $PATH-resolved external command from this
# hot path entirely (found live, 2026-08-09 adversarial review).
r=${hex%????}
t=${hex#??}
g=${t%??}
b=${hex#????}

kind="$ISEKAI_TERMINAL_KIND"
if [ "$kind" != "iterm2" ] && [ "$kind" != "windows-terminal" ]; then
    if [ "$TERM_PROGRAM" = "iTerm.app" ]; then
        kind="iterm2"
    else
        kind="windows-terminal"
    fi
fi

if [ "$kind" = "iterm2" ]; then
    # `$((16#$r))` (bash/ksh hex arithmetic syntax) is NOT POSIX and fails on
    # dash's /bin/sh ("arithmetic expression: expecting EOF: 16#ff") — found
    # live, this script must stay POSIX sh. `printf`'s own numeric-argument
    # parsing (unlike shell arithmetic) does accept a C-style `0x`-prefixed
    # hex literal, which is portable.
    rdec=$(printf '%d' "0x$r")
    gdec=$(printf '%d' "0x$g")
    bdec=$(printf '%d' "0x$b")
    printf '\033]6;1;bg;red;brightness;%d\007\033]6;1;bg;green;brightness;%d\007\033]6;1;bg;blue;brightness;%d\007' "$rdec" "$gdec" "$bdec"
else
    printf '\033]4;264;rgb:%s/%s/%s\033\\' "$r" "$g" "$b"
fi
