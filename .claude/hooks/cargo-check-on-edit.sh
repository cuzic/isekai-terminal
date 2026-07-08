#!/usr/bin/env bash
# PostToolUse hook (Write|Edit): after Claude edits a rust-core/**/*.rs file,
# build just the crate that file belongs to (found by walking up to the
# nearest Cargo.toml) so compile warnings/errors surface automatically,
# without paying for a full `cargo build --workspace` on every edit.
#
# Runs async (see .claude/settings.json) with asyncRewake: exit 2 wakes
# Claude with this script's stdout as feedback; exit 0 is silent.
set -u

input=$(cat)
fp=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty' 2>/dev/null)

case "$fp" in
  */rust-core/*.rs) ;;
  *) exit 0 ;;
esac

# Walk up from the edited file to the nearest Cargo.toml — that's the crate
# it belongs to (works for both workspace members like isekai-ssh/ and the
# root isekai-terminal-core crate, whose Cargo.toml is also rust-core's
# workspace manifest).
dir=$(dirname "$fp")
manifest=""
while [ "$dir" != "/" ]; do
  if [ -f "$dir/Cargo.toml" ]; then
    manifest="$dir/Cargo.toml"
    break
  fi
  dir=$(dirname "$dir")
done
if [ -z "$manifest" ]; then
  exit 0
fi

out=$(cargo build -q --manifest-path "$manifest" --message-format=short 2>&1)
rc=$?

if [ $rc -ne 0 ] || printf '%s' "$out" | grep -q 'warning:'; then
  printf '%s' "$out" | head -c 4000
  exit 2
fi
exit 0
