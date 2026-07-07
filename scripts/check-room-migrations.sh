#!/usr/bin/env bash
# Room migration(AppDatabase)の版数整合性をチェックする。CI(room-migration-check.yml)と
# ローカルの両方から実行できる。
#
# 検証内容:
#  1. AppDatabase.kt の @Database(version = N) と android/migration_registry.toml の
#     current が一致していること。
#  2. AppDatabase.kt 内の Migration(X, Y) が 1 -> N まで、欠番・重複無く連続していること
#     (各マイグレーションは必ず Y = X + 1 であること前提)。
#  3. android/migration_registry.toml の [[reserved]] に current 以下の版が残っていないこと
#     (マージ後の削除し忘れの検出)。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB_FILE="$ROOT/android/src/main/kotlin/tools/isekai/terminal/data/AppDatabase.kt"
REGISTRY="$ROOT/android/migration_registry.toml"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

[ -f "$DB_FILE" ] || fail "not found: $DB_FILE"
[ -f "$REGISTRY" ] || fail "not found: $REGISTRY"

DB_VERSION=$(grep -E '^\s*version = [0-9]+,' "$DB_FILE" | head -1 | sed -E 's/[^0-9]*([0-9]+).*/\1/')
REGISTRY_CURRENT=$(grep -E '^current = ' "$REGISTRY" | head -1 | sed -E 's/^current = ([0-9]+).*/\1/')

[ -n "$DB_VERSION" ] || fail "could not parse @Database(version = ...) from $DB_FILE"
[ -n "$REGISTRY_CURRENT" ] || fail "could not parse 'current = ...' from $REGISTRY"

echo "AppDatabase.kt version       = $DB_VERSION"
echo "migration_registry.toml current = $REGISTRY_CURRENT"

if [ "$DB_VERSION" != "$REGISTRY_CURRENT" ]; then
  fail "AppDatabase.kt version ($DB_VERSION) != migration_registry.toml current ($REGISTRY_CURRENT). \
Update migration_registry.toml's 'current' after merging a new migration (see file header for the workflow)."
fi

# Migration(X, Y) の全ペアを抽出し、各ペアが Y = X + 1 であることを確認しつつ、
# X の集合が 1..DB_VERSION-1 と過不足なく一致するか(欠番・重複が無いか)を確認する。
PAIRS=$(grep -oE 'Migration\([0-9]+, *[0-9]+\)' "$DB_FILE" | sed -E 's/Migration\(([0-9]+), *([0-9]+)\)/\1 \2/')

FROM_VERSIONS=""
while read -r x y; do
  [ -z "$x" ] && continue
  if [ "$y" != "$((x + 1))" ]; then
    fail "Migration($x, $y) in AppDatabase.kt is not X -> X+1 (found $x -> $y)."
  fi
  FROM_VERSIONS="$FROM_VERSIONS$x"$'\n'
done <<< "$PAIRS"

FROM_SORTED=$(echo -n "$FROM_VERSIONS" | grep -v '^$' | sort -n)
EXPECTED=$(seq 1 $((DB_VERSION - 1)))

if [ "$FROM_SORTED" != "$EXPECTED" ]; then
  echo "--- found MIGRATION from-versions ---" >&2
  echo "$FROM_SORTED" >&2
  echo "--- expected 1..$((DB_VERSION - 1)) ---" >&2
  echo "$EXPECTED" >&2
  fail "Migration(X, Y) chain in AppDatabase.kt is not a contiguous 1..$((DB_VERSION - 1)) sequence with no gaps/duplicates."
fi

# reserved の中に current 以下(=既にマージ済みのはずの版)が残っていないか確認する。
STALE=$(grep -E '^version = ' "$REGISTRY" | sed -E 's/^version = ([0-9]+).*/\1/' \
  | awk -v cur="$REGISTRY_CURRENT" '$1 <= cur' || true)
if [ -n "$STALE" ]; then
  echo "$STALE" >&2
  fail "android/migration_registry.toml has [[reserved]] entries <= current ($REGISTRY_CURRENT); \
remove them after merging (see file header)."
fi

echo "OK: Room migration chain and migration_registry.toml are consistent."
