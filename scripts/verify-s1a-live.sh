#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: verify-s1a-live.sh --installed | --dmg /absolute/path/to/file.dmg" >&2
  exit 2
}

fail() {
  echo "S1A live verification failed: $1" >&2
  exit 1
}

mode=""
dmg=""
case "$#:${1:-}" in
  1:--installed) mode="installed" ;;
  2:--dmg)
    mode="dmg"
    dmg=$2
    [[ "$dmg" == /* && "$dmg" == *.dmg ]] || usage
    ;;
  *) usage ;;
esac

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
runtime_root="$HOME/Library/Application Support/PersonalComputerAgent"
bundle_verifier="$repository_root/scripts/verify-s1a-bundle.sh"
install_polls=301
health_polls=6
poll_seconds=1

if [[ "${PCA_S1A_LIVE_TEST_MODE:-0}" == "1" ]]; then
  runtime_root=${PCA_S1A_LIVE_TEST_ROOT:?PCA_S1A_LIVE_TEST_ROOT is required in test mode}
  bundle_verifier=${PCA_S1A_LIVE_TEST_BUNDLE_VERIFIER:-$bundle_verifier}
  install_polls=${PCA_S1A_LIVE_TEST_INSTALL_POLLS:-1}
  health_polls=${PCA_S1A_LIVE_TEST_HEALTH_POLLS:-1}
  poll_seconds=${PCA_S1A_LIVE_TEST_POLL_SECONDS:-0}
fi

[[ "$runtime_root" == /* && "$runtime_root" != "/" ]] || fail "runtime root must be an absolute non-root path"
[[ "$install_polls" =~ ^[1-9][0-9]*$ && "$health_polls" =~ ^[1-9][0-9]*$ ]] \
  || fail "invalid bounded poll configuration"
[[ "$poll_seconds" =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "invalid poll interval"

for tool in id launchctl plutil ps python3 sqlite3; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required read-only tool: $tool"
done
if [[ "$mode" == "dmg" ]]; then
  command -v open >/dev/null 2>&1 || fail "missing required tool: open"
  [[ -f "$dmg" && ! -L "$dmg" ]] || fail "DMG must be a regular non-symbolic-link file"
  team_id=${PCA_TEAM_ID:-}
  [[ "$team_id" =~ ^[A-Z0-9]{10}$ ]] \
    || fail "PCA_TEAM_ID must name the 10-character signing Team ID used for this DMG"
  [[ -x "$bundle_verifier" ]] || fail "bundle verifier is unavailable"
  "$bundle_verifier" --team-id "$team_id" "$dmg" \
    || fail "read-only DMG bundle verification failed"
  open "$dmg" || fail "could not open DMG"
  echo "DMG verified and opened. Complete Gatekeeper and graphical install decisions manually."
fi

app_directory="$runtime_root/App"
data_directory="$runtime_root/Data"
run_directory="$runtime_root/Run"
app="$app_directory/PersonalComputerAgent.app"
info="$app/Contents/Info.plist"
agent="$app/Contents/Resources/bin/pca-agentd"
bridge="$app/Contents/Resources/bin/PCAPlatformBridge"
launch_agent="$app/Contents/Library/LaunchAgents/com.pca.agentd.plist"
status_file="$run_directory/runtime-status.json"
socket_file="$run_directory/bridge.sock"
database_file="$data_directory/agent.sqlite3"

installed=0
for ((attempt = 1; attempt <= install_polls; attempt++)); do
  if [[ -d "$app" && -x "$agent" && -x "$bridge" ]]; then
    installed=1
    break
  fi
  if (( attempt < install_polls )); then sleep "$poll_seconds"; fi
done
[[ "$installed" -eq 1 ]] || fail "installed app did not appear at the exact expected path"

expected_uid=$(id -u)
[[ "$expected_uid" =~ ^[0-9]+$ && "$expected_uid" -ne 0 ]] \
  || fail "live verification must run as the non-root installed user"

layout_error=$(python3 - "$runtime_root" "$app_directory" "$data_directory" "$run_directory" \
  "$app" "$info" "$agent" "$bridge" "$launch_agent" "$status_file" "$socket_file" \
  "$database_file" "$expected_uid" <<'PY' || true
import os
import stat
import sys
from pathlib import Path

(
    root_text, app_dir_text, data_text, run_text, app_text, info_text,
    agent_text, bridge_text, launch_agent_text, status_text, socket_text,
    database_text, uid_text,
) = sys.argv[1:]
uid = int(uid_text)
root = Path(root_text)
app_dir = Path(app_dir_text)
data = Path(data_text)
run = Path(run_text)
app = Path(app_text)

def reject(message: str) -> None:
    print(message)
    raise SystemExit(1)

if root.resolve(strict=True) != root:
    reject("runtime root resolves through a symbolic link")
expected_children = {app_dir: "App", data: "Data", run: "Run"}
for path, name in expected_children.items():
    if path.parent != root or path.name != name:
        reject("App/Data/Run are not exact direct children")
for path in (root, app_dir, data, run, app):
    try:
        metadata = path.lstat()
    except OSError:
        reject(f"missing directory: {path}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        reject(f"unsafe directory: {path}")
    if metadata.st_uid != uid:
        reject(f"wrong directory owner: {path}")
for path in (root, app_dir, data, run):
    if stat.S_IMODE(path.lstat().st_mode) != 0o700:
        reject(f"directory is not mode 0700: {path}")

required_regular = tuple(Path(value) for value in (info_text, agent_text, bridge_text, launch_agent_text, status_text, database_text))
for path in required_regular:
    try:
        metadata = path.lstat()
    except OSError:
        reject(f"missing required file: {path}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        reject(f"required path is not a regular file: {path}")
    if metadata.st_uid != uid:
        reject(f"wrong file owner: {path}")

socket_path = Path(socket_text)
try:
    socket_metadata = socket_path.lstat()
except OSError:
    reject(f"missing socket: {socket_path}")
if stat.S_ISLNK(socket_metadata.st_mode) or not stat.S_ISSOCK(socket_metadata.st_mode):
    reject(f"runtime socket is not a Unix socket: {socket_path}")
if socket_metadata.st_uid != uid:
    reject(f"wrong socket owner: {socket_path}")
for path in (Path(status_text), socket_path, Path(database_text)):
    if stat.S_IMODE(path.lstat().st_mode) != 0o600:
        reject(f"runtime file is not mode 0600: {path}")

for current, directories, files in os.walk(app, followlinks=False):
    current_path = Path(current)
    for name in (*directories, *files):
        candidate = current_path / name
        if candidate.is_symlink():
            reject(f"symbolic link inside installed app: {candidate}")
    if current_path != app and current_path.name in {"Data", "Run"}:
        reject(f"writable runtime directory inside installed app: {current_path}")
PY
)
[[ -z "$layout_error" ]] || fail "unsafe App/Data/Run layout: $layout_error"

[[ "$(plutil -extract CFBundleIdentifier raw -o - "$info" 2>/dev/null || true)" == \
  "com.pca.PersonalComputerAgent" ]] || fail "installed app has the wrong bundle identifier"
app_version=$(plutil -extract CFBundleShortVersionString raw -o - "$info" 2>/dev/null || true)
[[ "$app_version" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] || fail "installed app version is invalid"
[[ "$(plutil -extract Label raw -o - "$launch_agent" 2>/dev/null || true)" == "com.pca.agentd" ]] \
  || fail "embedded LaunchAgent label is invalid"
[[ "$(plutil -extract BundleProgram raw -o - "$launch_agent" 2>/dev/null || true)" == \
  "Contents/Resources/bin/pca-agentd" ]] || fail "embedded LaunchAgent program is invalid"

status_pid=""
for ((attempt = 1; attempt <= health_polls; attempt++)); do
  status_pid=$(python3 - "$status_file" "$app_version" <<'PY' 2>/dev/null || true
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

path = Path(sys.argv[1])
expected_version = sys.argv[2]
expected_keys = {
    "agent_status", "bridge_status", "local_healthy", "heartbeat_at",
    "process_id", "app_version", "schema_version",
}
value = json.loads(path.read_text(encoding="utf-8"))
if not isinstance(value, dict) or set(value) != expected_keys:
    raise SystemExit(1)
heartbeat_text = value["heartbeat_at"]
if not isinstance(heartbeat_text, str):
    raise SystemExit(1)
heartbeat = datetime.fromisoformat(heartbeat_text.replace("Z", "+00:00"))
if heartbeat.tzinfo is None:
    raise SystemExit(1)
age = (datetime.now(timezone.utc) - heartbeat.astimezone(timezone.utc)).total_seconds()
modified_age = datetime.now(timezone.utc).timestamp() - path.stat().st_mtime
pid = value["process_id"]
valid = (
    value["agent_status"] in {"unpaired", "running"}
    and value["bridge_status"] == "ready"
    and value["local_healthy"] is True
    and -5 <= age <= 5
    and -5 <= modified_age <= 5
    and isinstance(pid, int) and not isinstance(pid, bool) and pid > 0
    and value["app_version"] == expected_version
    and value["schema_version"] == 1
)
if not valid:
    raise SystemExit(1)
print(pid)
PY
)
  if [[ "$status_pid" =~ ^[1-9][0-9]*$ ]]; then break; fi
  if (( attempt < health_polls )); then sleep "$poll_seconds"; fi
done
[[ "$status_pid" =~ ^[1-9][0-9]*$ ]] \
  || fail "healthy runtime status was not observed within five seconds"

job=$(launchctl print "gui/$expected_uid/com.pca.agentd" 2>/dev/null) \
  || fail "expected user-level launchd job is not registered"
grep -Fq "state = running" <<<"$job" || fail "expected launchd job is not running"
if ! grep -Fq "program = $agent" <<<"$job" \
  && ! grep -Fq "program = Contents/Resources/bin/pca-agentd" <<<"$job"; then
  fail "launchd job does not resolve to the exact installed agent"
fi
grep -Eq "pid = *$status_pid([[:space:]]|$)" <<<"$job" \
  || fail "launchd job PID does not match runtime status"

process_table=$(ps -axo pid=,ppid=,uid=,comm=) || fail "could not inspect processes"
process_ids=$(python3 -c '
import sys

agent, bridge, uid_text, status_pid_text = sys.argv[1:]
uid = int(uid_text)
status_pid = int(status_pid_text)
rows = []
for line in sys.stdin:
    fields = line.strip().split(maxsplit=3)
    if len(fields) != 4:
        continue
    try:
        pid, parent, owner = (int(fields[0]), int(fields[1]), int(fields[2]))
    except ValueError:
        continue
    rows.append((pid, parent, owner, fields[3]))
agents = [row for row in rows if row[3] == agent]
bridges = [row for row in rows if row[3] == bridge]
if len(agents) != 1:
    print("expected exactly one agent process")
    raise SystemExit(1)
if len(bridges) != 1:
    print("expected exactly one Bridge process")
    raise SystemExit(1)
agent_row = agents[0]
bridge_row = bridges[0]
if agent_row[2] != uid or bridge_row[2] != uid or uid == 0:
    print("agent and Bridge must run as the current user, never root")
    raise SystemExit(1)
if agent_row[0] != status_pid:
    print("agent PID does not match runtime status")
    raise SystemExit(1)
if bridge_row[1] != agent_row[0]:
    print("Bridge is not a child of the exact agent")
    raise SystemExit(1)
print(agent_row[0], bridge_row[0])
' "$agent" "$bridge" "$expected_uid" "$status_pid" <<<"$process_table" 2>&1) \
  || fail "$process_ids"
read -r agent_pid bridge_pid <<<"$process_ids"
[[ "$agent_pid" =~ ^[1-9][0-9]*$ && "$bridge_pid" =~ ^[1-9][0-9]*$ ]] \
  || fail "process inspection returned invalid PIDs"

agent_arguments=$(ps -p "$agent_pid" -o args=) || fail "could not inspect agent arguments"
bridge_arguments=$(ps -p "$bridge_pid" -o args=) || fail "could not inspect Bridge arguments"
[[ "$agent_arguments" == "$agent run" || "$agent_arguments" == "pca-agentd run" ]] \
  || fail "agent arguments are ambiguous"
[[ "$bridge_arguments" == "$bridge --socket $socket_file" \
  || "$bridge_arguments" == "PCAPlatformBridge --socket $socket_file" ]] \
  || fail "Bridge is not bound to the exact runtime socket"

database_check=$(sqlite3 -readonly "$database_file" <<'SQL' 2>/dev/null || true
PRAGMA query_only=ON;
PRAGMA integrity_check;
SELECT id || ':' || status FROM schema_migrations ORDER BY id;
SQL
)
[[ "$database_check" == $'ok\n0000:completed\n0001:completed' ]] \
  || fail "SQLite integrity or exact S1A migration ledger is invalid"

echo "S1A LIVE VERIFIED: version=$app_version agent_pid=$agent_pid bridge_pid=$bridge_pid uid=$expected_uid"
