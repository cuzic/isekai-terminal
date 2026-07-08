#!/usr/bin/env bash
# start-sshd-fixture.sh が起動したsshd fixtureを停止する。
set -euo pipefail

FIXTURE_DIR="${1:?usage: stop-sshd-fixture.sh <fixture_dir>}"

if [[ -f "${FIXTURE_DIR}/sshd.pid" ]]; then
    PID="$(cat "${FIXTURE_DIR}/sshd.pid")"
    kill "$PID" 2>/dev/null || true
    echo "stopped sshd fixture (pid $PID)"
else
    echo "no sshd.pid found under ${FIXTURE_DIR}, nothing to stop"
fi
