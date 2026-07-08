#!/usr/bin/env bash
# 新しいRoom migration(AppDatabase)のバージョン番号を予約し、
# android/migration_registry.toml に [[reserved]] エントリとして記録する。
#
# 背景・使い方の詳細は android/migration_registry.toml のコメントを参照。
#
# 使い方:
#   scripts/reserve-room-migration.sh <owner-slug>
#   例: scripts/reserve-room-migration.sh phase12-relay-credential-vault
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <owner-slug>" >&2
  exit 1
fi

OWNER="$1"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY="$ROOT/android/migration_registry.toml"
BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
TODAY="$(date +%Y-%m-%d)"

CURRENT=$(grep -E '^current = ' "$REGISTRY" | head -1 | sed -E 's/^current = ([0-9]+).*/\1/')
RESERVED_MAX=$(grep -E '^version = ' "$REGISTRY" | sed -E 's/^version = ([0-9]+).*/\1/' | sort -n | tail -1 || true)

NEXT=$((CURRENT + 1))
if [ -n "${RESERVED_MAX:-}" ] && [ "$RESERVED_MAX" -ge "$NEXT" ]; then
  NEXT=$((RESERVED_MAX + 1))
fi

cat >> "$REGISTRY" <<EOF

[[reserved]]
version = $NEXT
owner = "$OWNER"
branch = "$BRANCH"
reserved_at = "$TODAY"
EOF

echo "Reserved Room migration version $NEXT for '$OWNER' (branch: $BRANCH)."
echo
echo "Next steps:"
echo "  1. In AppDatabase.kt, add:"
echo "       internal val MIGRATION_${CURRENT}_${NEXT} = object : Migration($CURRENT, $NEXT) { ... }"
echo "     and add it to the .addMigrations(...) chain."
echo "  2. Bump @Database(version = $NEXT, ...)."
echo "  3. After merging, delete this [[reserved]] entry from android/migration_registry.toml"
echo "     and update 'current' to $NEXT."
