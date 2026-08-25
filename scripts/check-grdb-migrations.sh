#!/usr/bin/env bash
# GRDB migration(ProfileDatabase)の版数整合性をチェックする。CI(grdb-migration-check.yml)と
# ローカルの両方から実行できる。版数を事前に確保する「予約」側は
# scripts/reserve-grdb-migration.sh が担当し、こちらはその結果の「検証」側にあたる。
# Android版`scripts/check-room-migrations.sh`の1:1移植(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.11(b))。
# 背景・運用手順の詳細は ios/migration_registry.toml のコメントを参照。
#
# 検証内容:
#  1. ProfileDatabase.swiftに登録済みの`registerMigration("vN_...")`の最大Nと、
#     ios/migration_registry.tomlのcurrentが一致していること。
#  2. `v1`〜current までの番号が欠番・重複無く連続していること
#     (RoomのMigration(X, Y)と違いGRDBは`from`版数を取らない文字列名の連番方式)。
#  3. ios/migration_registry.tomlの[[reserved]]にcurrent以下の版
#     (＝マージ後の削除し忘れ)が残っていないこと。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB_FILE="$ROOT/ios/Sources/IsekaiTerminalCore/ProfileDatabase.swift"
REGISTRY="$ROOT/ios/migration_registry.toml"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

[ -f "$DB_FILE" ] || fail "not found: $DB_FILE"
[ -f "$REGISTRY" ] || fail "not found: $REGISTRY"

REGISTRY_CURRENT=$(grep -E '^current = ' "$REGISTRY" | head -1 | sed -E 's/^current = ([0-9]+).*/\1/')
[ -n "$REGISTRY_CURRENT" ] || fail "could not parse 'current = ...' from $REGISTRY"

# registerMigration("vN_...") からNを全て抽出する。
VERSIONS=$(grep -oE 'registerMigration\("v[0-9]+_' "$DB_FILE" | sed -E 's/registerMigration\("v([0-9]+)_/\1/' | sort -n)

[ -n "$VERSIONS" ] || fail "no registerMigration(\"vN_...\") calls found in $DB_FILE"

DB_MAX=$(echo "$VERSIONS" | tail -1)

echo "ProfileDatabase.swift max migration = v$DB_MAX"
echo "migration_registry.toml current     = $REGISTRY_CURRENT"

if [ "$DB_MAX" != "$REGISTRY_CURRENT" ]; then
  fail "ProfileDatabase.swift's highest registered migration (v$DB_MAX) != migration_registry.toml current ($REGISTRY_CURRENT). \
Update migration_registry.toml's 'current' after merging a new migration (see file header for the workflow)."
fi

EXPECTED=$(seq 1 "$DB_MAX")
if [ "$VERSIONS" != "$EXPECTED" ]; then
  echo "--- found registerMigration versions ---" >&2
  echo "$VERSIONS" >&2
  echo "--- expected 1..$DB_MAX ---" >&2
  echo "$EXPECTED" >&2
  fail "registerMigration(\"vN_...\") chain in ProfileDatabase.swift is not a contiguous 1..$DB_MAX sequence with no gaps/duplicates."
fi

# reserved の中に current 以下(=既にマージ済みのはずの版)が残っていないか確認する。
STALE=$(grep -E '^version = ' "$REGISTRY" | sed -E 's/^version = ([0-9]+).*/\1/' \
  | awk -v cur="$REGISTRY_CURRENT" '$1 <= cur' || true)
if [ -n "$STALE" ]; then
  echo "$STALE" >&2
  fail "ios/migration_registry.toml has [[reserved]] entries <= current ($REGISTRY_CURRENT); \
remove them after merging (see file header)."
fi

echo "OK: GRDB migration chain and migration_registry.toml are consistent."
