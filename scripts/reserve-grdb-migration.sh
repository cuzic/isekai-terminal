#!/usr/bin/env bash
# 新しいGRDB migration(ProfileDatabase)のバージョン番号を予約し、
# ios/migration_registry.toml に [[reserved]] エントリとして記録する。
#
# 並行作業間での版数の奪い合いを防ぐための「予約」側スクリプト。予約した番号どおりに
# 実装・マージされたか(および予約エントリの削除し忘れ)の「検証」側は
# scripts/check-grdb-migrations.sh(CI: .github/workflows/grdb-migration-check.yml)が担当する。
# Android版`scripts/reserve-room-migration.sh`の1:1移植(`docs/adr/0001-ios-parity-implementation.md` §3.11(b))。
#
# 背景・使い方の詳細は ios/migration_registry.toml のコメントを参照。
#
# 使い方:
#   scripts/reserve-grdb-migration.sh <owner-slug>
#   例: scripts/reserve-grdb-migration.sh y-p2c-hostkey-autotrust
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
reserve_migration_version "$ROOT" "ios/migration_registry.toml" "$OWNER" "GRDB"
NEXT="$RESERVED_VERSION"

echo
echo "Next steps:"
echo "  1. In ProfileDatabase.swift's migrator, add:"
echo "       migrator.registerMigration(\"v${NEXT}_<description>\") { db in ... }"
echo "  2. After merging, delete this [[reserved]] entry from ios/migration_registry.toml"
echo "     and update 'current' to $NEXT."
