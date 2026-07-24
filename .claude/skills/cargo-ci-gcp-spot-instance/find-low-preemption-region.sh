#!/usr/bin/env bash
# 指定したマシンタイプについて、複数リージョンのSpot VM preemption率(過去30日平均)と
# 現在のcapacity obtainability scoreを比較し、避けるべき/選ぶべきリージョンを判断する。
#
# 前提: `gcloud beta compute advice capacity`/`capacity-history` サブコマンドが必要。
# 2026-07-24時点、このリポジトリのサンドボックスに入っている google-cloud-cli(548.0.0)には
# まだ入っておらず、577.0.0では確認できた。system側をsudoで更新できない場合、
# 以下でユーザーローカルに新しいgcloudを展開して使う(既存の認証設定はそのまま使い回せる):
#
#   mkdir -p ~/.local/gcloud-sdk
#   curl -sL https://dl.google.com/dl/cloudsdk/channels/rapid/downloads/google-cloud-cli-linux-x86_64.tar.gz \
#     | tar -xz -C ~/.local/gcloud-sdk --strip-components=1
#   CLOUDSDK_CONFIG=~/.config/gcloud ~/.local/gcloud-sdk/bin/gcloud components install beta --quiet
#   GCLOUD_BIN=~/.local/gcloud-sdk/bin/gcloud ./find-low-preemption-region.sh ...
#
# Usage:
#   ./find-low-preemption-region.sh <PROJECT_ID> <MACHINE_TYPE> <ACCOUNT> [REGION...]
#
# Example:
#   ./find-low-preemption-region.sh isekai-terminal-cargo-ci n1-highcpu-32 \
#     tomoya.kaw@gmail.com us-central1 us-east1 us-east4 us-west1

set -euo pipefail

PROJECT_ID="${1:?PROJECT_ID required}"
MACHINE_TYPE="${2:?MACHINE_TYPE required}"
ACCOUNT="${3:?ACCOUNT required}"
shift 3
REGIONS=("$@")
if [ "${#REGIONS[@]}" -eq 0 ]; then
  # 深く考えず候補にする定番リージョン(必要に応じて増減する)
  REGIONS=(us-central1 us-east1 us-east4 us-west1 us-west4 europe-west1 asia-northeast1)
fi

GCLOUD_BIN="${GCLOUD_BIN:-gcloud}"

echo "machine_type=$MACHINE_TYPE project=$PROJECT_ID"
printf "%-20s %10s %6s %s\n" "region" "avg_preempt" "obtain" "recommended_zone"

for region in "${REGIONS[@]}"; do
  avg=$("$GCLOUD_BIN" beta compute advice capacity-history \
    --provisioning-model=SPOT --machine-type="$MACHINE_TYPE" --types=PREEMPTION \
    --region="$region" --project="$PROJECT_ID" --account="$ACCOUNT" 2>/dev/null \
    | grep preemptionRate | awk -F': ' '{sum+=$2; n++} END {if(n>0) printf "%.3f", sum/n; else print "n/a"}')

  read -r obtain zone <<<"$("$GCLOUD_BIN" beta compute advice capacity \
    --provisioning-model=SPOT --instance-selection-machine-types="$MACHINE_TYPE" \
    --target-distribution-shape=ANY --size=1 --region="$region" \
    --project="$PROJECT_ID" --account="$ACCOUNT" 2>/dev/null \
    | python3 -c "
import sys, yaml
try:
    d = yaml.safe_load(sys.stdin)
    r = d['recommendations'][0]
    print(r['scores']['obtainability'], r['shards'][0]['zone'].rsplit('/', 1)[-1])
except Exception:
    print('n/a n/a')
")"

  printf "%-20s %10s %6s %s\n" "$region" "$avg" "$obtain" "$zone"
done

echo
echo "avg_preemptが低く、obtainが高い(0.9目安)組み合わせのregion/zoneを選ぶ。"
echo "2026-07-24にn1-highcpu-32で実測した例: us-central1(avg=0.36, 実際に26分で実際にpreemptされた)"
echo "                                      us-east4(avg=0.14)/us-east1(avg=0.30)の方が低かった"
echo "                                      us-west1(avg=0.91)は明確に避けるべきだった"
