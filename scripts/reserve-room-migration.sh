#!/usr/bin/env bash
# 新しいRoom migration(AppDatabase)のバージョン番号を予約し、
# android/migration_registry.toml に [[reserved]] エントリとして記録する。
#
# 並行作業間での版数の奪い合いを防ぐための「予約」側スクリプト。予約した番号どおりに
# 実装・マージされたか(および予約エントリの削除し忘れ)の「検証」側は
# scripts/check-room-migrations.sh(CI: .github/workflows/room-migration-check.yml)が担当する。
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
# shellcheck source=lib/migration-reservation.sh
source "$ROOT/scripts/lib/migration-reservation.sh"

# 自worktreeだけでなく origin/main と他の全worktreeの予約も見て番号を決める
# (lib/migration-reservation.sh 参照)。
reserve_migration_version "$ROOT" "android/migration_registry.toml" "$OWNER" "Room"
NEXT="$RESERVED_VERSION"
PREV=$((NEXT - 1))

echo
echo "Next steps:"
echo "  1. In AppDatabase.kt, add:"
echo "       internal val MIGRATION_${PREV}_${NEXT} = object : Migration($PREV, $NEXT) { ... }"
echo "     and add it to the .addMigrations(...) chain."
echo "  2. Bump @Database(version = $NEXT, ...)."
echo "  3. After merging, delete this [[reserved]] entry from android/migration_registry.toml"
echo "     and update 'current' to $NEXT."
