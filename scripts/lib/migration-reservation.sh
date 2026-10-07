#!/usr/bin/env bash
# reserve-room-migration.sh / reserve-grdb-migration.sh 共通の予約ロジック。
# source して `reserve_migration_version <registry-relpath> <owner> <kind-label>` を呼ぶ。
#
# 背景(2026-09-29レビューCI-M6): 以前の予約スクリプトは自分のworktreeの
# migration_registry.toml だけを読んで次の番号を決めていたため、並列worktree同士が
# 同時に予約すると同じ番号を取ってしまい、予約台帳の目的(番号の奪い合い防止)を
# 果たせていなかった。ここでは次の全てを見て、既知の最大値+1を予約する:
#   1. このworktreeのレジストリ(current と [[reserved]])
#   2. origin/main のレジストリ(best-effortで `git fetch origin main` した後)
#   3. `git worktree list` に登録されている他の全worktreeのレジストリ(未コミットの予約も含む)
# ネットワークに触れたくない場合は MIGRATION_RESERVE_OFFLINE=1 で 2 の fetch を省略する。
#
# それでも「別マシンで並行して予約したが、まだどちらもmainに入っていない」ケースは
# 検出できない。予約したら予約コミットだけを先にmainへ入れること(レジストリのコメント
# 参照)。check-*-migrations.sh は [[reserved]] の重複版数も検出する。

# レジストリの本文(stdin)から current と [[reserved]] の version を数値で列挙する。
migration_registry_versions() {
  awk '
    /^current = [0-9]+/ { sub(/^current = /, ""); print $1 + 0 }
    /^version = [0-9]+/ { sub(/^version = /, ""); print $1 + 0 }
  '
}

# レジストリの本文(stdin)から current だけを取り出す。
migration_registry_current() {
  awk '/^current = [0-9]+/ { sub(/^current = /, ""); print $1 + 0; exit }'
}

# usage: reserve_migration_version <root> <registry-relpath> <owner> <kind-label>
# 予約した番号を RESERVED_VERSION、ローカルの current を RESERVED_CURRENT に設定する。
reserve_migration_version() {
  local root="$1" rel="$2" owner="$3" kind="$4"
  local registry="$root/$rel"
  local branch today current known_max main_text main_current wt v
  branch="$(git -C "$root" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
  today="$(date +%Y-%m-%d)"

  [ -f "$registry" ] || { echo "ERROR: not found: $registry" >&2; return 1; }
  current="$(migration_registry_current < "$registry")"
  [ -n "$current" ] || { echo "ERROR: could not parse 'current = ...' from $registry" >&2; return 1; }

  known_max="$current"
  # "<version> <source>" の一覧(他者の予約の出所を表示するため)。
  local sources=""
  while read -r v; do
    [ -n "$v" ] || continue
    if [ "$v" -gt "$known_max" ]; then known_max="$v"; fi
  done < <(migration_registry_versions < "$registry")

  if [ "${MIGRATION_RESERVE_OFFLINE:-0}" != "1" ]; then
    if ! timeout 30 git -C "$root" fetch --quiet origin main 2>/dev/null; then
      echo "warning: 'git fetch origin main' failed; using the local origin/main ref as-is." >&2
    fi
  fi
  if main_text="$(git -C "$root" show "origin/main:$rel" 2>/dev/null)"; then
    main_current="$(migration_registry_current <<< "$main_text")"
    if [ -n "$main_current" ] && [ "$main_current" -gt "$current" ]; then
      echo "warning: origin/main already has current = $main_current (> this branch's $current)." >&2
      echo "         Rebase/merge origin/main before implementing the migration." >&2
    fi
    while read -r v; do
      [ -n "$v" ] || continue
      if [ "$v" -gt "$known_max" ]; then known_max="$v"; fi
      if [ "$v" -gt "$current" ]; then sources+="$v origin/main"$'\n'; fi
    done < <(migration_registry_versions <<< "$main_text")
  fi

  while read -r wt; do
    [ -n "$wt" ] || continue
    if [ "$(cd "$wt" 2>/dev/null && pwd -P)" = "$(cd "$root" && pwd -P)" ]; then continue; fi
    if [ ! -f "$wt/$rel" ]; then continue; fi
    while read -r v; do
      [ -n "$v" ] || continue
      if [ "$v" -gt "$known_max" ]; then known_max="$v"; fi
      if [ "$v" -gt "$current" ]; then sources+="$v worktree:$wt"$'\n'; fi
    done < <(migration_registry_versions < "$wt/$rel")
  done < <(git -C "$root" worktree list --porcelain 2>/dev/null | awk '/^worktree / { sub(/^worktree /, ""); print }')

  RESERVED_CURRENT="$current"
  RESERVED_VERSION=$((known_max + 1))

  cat >> "$registry" <<EOF

[[reserved]]
version = $RESERVED_VERSION
owner = "$owner"
branch = "$branch"
reserved_at = "$today"
EOF

  echo "Reserved $kind migration version $RESERVED_VERSION for '$owner' (branch: $branch)."
  if [ "$RESERVED_VERSION" -gt $((current + 1)) ]; then
    echo
    echo "NOTE: versions $((current + 1))..$((RESERVED_VERSION - 1)) are already current/reserved elsewhere:"
    if [ -n "$sources" ]; then
      printf '%s' "$sources" | sort -n -u | sed 's/^/  - v/'
    else
      echo "  - (in this worktree's own [[reserved]] entries)"
    fi
    echo "  Your migration can only merge after those land on main (the chain must stay contiguous);"
    echo "  rebase onto origin/main once they do."
  fi
  echo
  echo "Commit this reservation and get it onto main right away (before the implementation),"
  echo "so that other worktrees/machines see it."
}
