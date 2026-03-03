#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS (uses 'sample')."
  exit 1
fi

if ! command -v sample >/dev/null 2>&1; then
  echo "'sample' command not found."
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_BIN="$REPO_ROOT/target/debug/acui"
MOCK_BIN="$REPO_ROOT/target/debug/acui_mock_agent"

PROFILE_DURATION_SECONDS="${PROFILE_DURATION_SECONDS:-20}"
PROFILE_INTERVAL_MILLIS="${PROFILE_INTERVAL_MILLIS:-1}"
ACUI_MOCK_PROFILE_ITERATIONS="${ACUI_MOCK_PROFILE_ITERATIONS:-140}"
PROFILE_ROOT="${PROFILE_ROOT:-$REPO_ROOT/target/non_headless_profile}"

if [[ ! -x "$APP_BIN" || ! -x "$MOCK_BIN" ]]; then
  echo "Building acui + acui_mock_agent..."
  (cd "$REPO_ROOT" && cargo build --bin acui --bin acui_mock_agent)
fi

rm -rf "$PROFILE_ROOT"
mkdir -p "$PROFILE_ROOT/workspace/src" "$PROFILE_ROOT/data/workspaces" "$PROFILE_ROOT/data/threads"

for file in state.rs chat.rs client.rs; do
  if [[ -f "$REPO_ROOT/src/$file" ]]; then
    cp "$REPO_ROOT/src/$file" "$PROFILE_ROOT/workspace/src/$file"
  fi
done

export PROFILE_ROOT
python3 <<'PY'
import json
import os
import uuid
from datetime import datetime, timezone
from pathlib import Path

root = Path(os.environ["PROFILE_ROOT"])
workspace_id = str(uuid.uuid4())
thread_id = str(uuid.uuid4())
message_id = str(uuid.uuid4())
timestamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

workspace_record = {
    "id": workspace_id,
    "name": "Profile Workspace",
    "path": str((root / "workspace").resolve()),
    "created_at": timestamp,
    "session_listed_agents": [],
    "thread_ids": [thread_id],
}

thread_record = {
    "id": thread_id,
    "workspace_id": workspace_id,
    "name": "Profile Thread",
    "user_renamed": False,
    "agent_name": "mock-agent",
    "session_id": "mock-profile-session",
    "messages": [
        {
            "id": message_id,
            "role": "user",
            "content": {"type": "text", "data": "seed"},
            "timestamp": timestamp,
        }
    ],
    "created_at": timestamp,
    "updated_at": timestamp,
}

(root / "data" / "workspaces" / f"{workspace_id}.json").write_text(
    json.dumps(workspace_record, indent=2),
    encoding="utf-8",
)
(root / "data" / "threads" / f"{thread_id}.json").write_text(
    json.dumps(thread_record, indent=2),
    encoding="utf-8",
)
PY

cat >"$PROFILE_ROOT/acui.toml" <<EOF
data_dir = "$PROFILE_ROOT/data"
enable_mock_agent = false
log_file = "$PROFILE_ROOT/messages.log"

[[agent]]
name = "mock-agent"
command = "$MOCK_BIN"
cwd = "$PROFILE_ROOT/workspace"
EOF

APP_LOG="$PROFILE_ROOT/acui.stdout.log"
SAMPLE_OUT="$PROFILE_ROOT/sample.txt"

echo "Launching non-headless acui profile app..."
(
  cd "$PROFILE_ROOT"
  ACUI_MOCK_PROFILE_LONG_THREAD=1 \
  ACUI_MOCK_PROFILE_ITERATIONS="$ACUI_MOCK_PROFILE_ITERATIONS" \
  "$APP_BIN" >"$APP_LOG" 2>&1
) &
APP_PID=$!

cleanup() {
  if kill -0 "$APP_PID" >/dev/null 2>&1; then
    kill "$APP_PID" >/dev/null 2>&1 || true
    wait "$APP_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

sleep 4

echo "App PID: $APP_PID"
echo "Bring acui to front and scroll the large thread now."
echo "Sampling for ${PROFILE_DURATION_SECONDS}s at ${PROFILE_INTERVAL_MILLIS}ms intervals..."

sample "$APP_PID" "$PROFILE_DURATION_SECONDS" "$PROFILE_INTERVAL_MILLIS" -file "$SAMPLE_OUT"

echo "Sample saved: $SAMPLE_OUT"
echo "App stdout log: $APP_LOG"
