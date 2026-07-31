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
timeout_runner="$repository_root/scripts/run-with-timeout.py"
snapshot_helper="$repository_root/scripts/snapshot-s1a-dmg.py"
process_inspector="$repository_root/scripts/inspect-s1a-processes.py"
[[ -x "$timeout_runner" ]] || fail "bounded command runner is unavailable"
[[ -x "$snapshot_helper" ]] || fail "DMG snapshot helper is unavailable"
[[ -x "$process_inspector" ]] || fail "macOS process inspector is unavailable"

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
  snapshot_parent=${PCA_S1A_LIVE_TEST_SNAPSHOT_PARENT:?PCA_S1A_LIVE_TEST_SNAPSHOT_PARENT is required in test mode}
  process_inspector=${PCA_S1A_LIVE_TEST_PROCESS_INSPECTOR:-$process_inspector}
fi

[[ "$runtime_root" == /* && "$runtime_root" != "/" ]] || fail "runtime root must be an absolute non-root path"
[[ "$install_polls" =~ ^[1-9][0-9]*$ && "$health_polls" =~ ^[1-9][0-9]*$ ]] \
  || fail "invalid bounded poll configuration"
[[ "$poll_seconds" =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "invalid poll interval"

for tool in codesign id launchctl plutil python3 sqlite3; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required tool: $tool"
done

new_deadline() {
  local seconds=$1 output rc
  set +e
  output=$("$timeout_runner" --timeout 1 -- python3 -c \
    'import sys,time; print(f"{time.monotonic()+float(sys.argv[1]):.9f}")' "$seconds" 2>&1)
  rc=$?
  set -e
  [[ "$rc" -eq 0 && "$output" =~ ^[0-9]+[.][0-9]+$ ]] || fail "could not start bounded verification phase"
  deadline=$output
}

snapshot_identity=""
snapshot_cleanup_needed=0
cleanup_snapshot_on_exit() {
  local original_status=$? cleanup_output cleanup_status
  trap - EXIT
  if [[ "$snapshot_cleanup_needed" -eq 1 ]]; then
    set +e
    cleanup_output=$("$timeout_runner" --timeout 3 -- "$snapshot_helper" cleanup --identity-json "$snapshot_identity" 2>&1)
    cleanup_status=$?
    set -e
    if [[ "$cleanup_status" -ne 0 ]]; then
      echo "S1A live verification failed: private DMG snapshot cleanup failed visibly: $cleanup_output" >&2
      exit 1
    fi
  fi
  exit "$original_status"
}
trap cleanup_snapshot_on_exit EXIT

capture() {
  local variable=$1 output rc
  shift
  set +e
  output=$("$timeout_runner" --deadline "$deadline" -- "$@" 2>&1)
  rc=$?
  set -e
  printf -v "$variable" '%s' "$output"
  return "$rc"
}

app_directory="$runtime_root/App"
data_directory="$runtime_root/Data"
run_directory="$runtime_root/Run"
app="$app_directory/PersonalComputerAgent.app"
info="$app/Contents/Info.plist"
main="$app/Contents/MacOS/PersonalComputerAgent"
agent="$app/Contents/Resources/bin/pca-agentd"
bridge="$app/Contents/Resources/bin/PCAPlatformBridge"
launch_agent="$app/Contents/Library/LaunchAgents/com.pca.agentd.plist"
status_file="$run_directory/runtime-status.json"
socket_file="$run_directory/bridge.sock"
database_file="$data_directory/agent.sqlite3"

snapshot_install() {
  capture "$1" python3 - "$app" "$info" "$main" <<'PY'
import os, plistlib, sys, time
from pathlib import Path
app, info, main = map(Path, sys.argv[1:])
if not app.is_dir() or not info.is_file() or not main.is_file():
    raise SystemExit(3)
app_metadata = app.lstat()
main_metadata = main.lstat()
with info.open("rb") as source:
    version = plistlib.load(source).get("CFBundleShortVersionString")
if not isinstance(version, str):
    raise SystemExit(4)
print(f"version={version} app_dev={app_metadata.st_dev} app_ino={app_metadata.st_ino} main_dev={main_metadata.st_dev} main_ino={main_metadata.st_ino} observed_mono={time.monotonic():.9f} observed_wall={time.time():.9f}")
PY
}

candidate_version=""
candidate_team=""
candidate_app_cdhash=""
candidate_main_cdhash=""
candidate_agent_cdhash=""
candidate_bridge_cdhash=""
pre_open_snapshot="<not-installed>"

if [[ "$mode" == "dmg" ]]; then
  command -v open >/dev/null 2>&1 || fail "missing required tool: open"
  team_id=${PCA_TEAM_ID:-}
  [[ "$team_id" =~ ^[A-Z0-9]{10}$ ]] \
    || fail "PCA_TEAM_ID must name the 10-character signing Team ID used for this DMG"
  [[ -x "$bundle_verifier" ]] || fail "bundle verifier is unavailable"

  new_deadline 300
  if [[ "${PCA_S1A_LIVE_TEST_MODE:-0}" == "1" ]]; then
    if ! capture snapshot_identity "$snapshot_helper" create --source "$dmg" --parent "$snapshot_parent"; then
      fail "could not create private DMG snapshot: $snapshot_identity"
    fi
  else
    if ! capture snapshot_identity "$snapshot_helper" create --source "$dmg"; then
      fail "could not create private DMG snapshot: $snapshot_identity"
    fi
  fi
  snapshot_cleanup_needed=1
  if ! capture snapshot_dmg python3 -c 'import json,sys; print(json.loads(sys.argv[1])["file_path"])' "$snapshot_identity"; then
    fail "could not parse private DMG snapshot identity"
  fi
  if ! capture snapshot_check "$snapshot_helper" validate --identity-json "$snapshot_identity"; then
    fail "private DMG snapshot identity validation failed: $snapshot_check"
  fi
  if snapshot_install pre_open_snapshot; then
    if ! capture activation_bounds python3 -c 'import sys; d=dict(x.split("=",1) for x in sys.argv[1].split()); print(d["observed_mono"], d["observed_wall"])' "$pre_open_snapshot"; then
      fail "could not parse old-install observation bounds"
    fi
  elif capture absent_check python3 -c 'import os,sys,time; absent=not os.path.lexists(sys.argv[1]) and not os.path.lexists(sys.argv[2]); print(f"{time.monotonic():.9f} {time.time():.9f}") if absent else None; raise SystemExit(0 if absent else 1)' "$app" "$main"; then
    pre_open_snapshot="<not-installed>"
    activation_bounds=$absent_check
  else
    fail "pre-open App/main state is partial or unsafe"
  fi
  read -r activation_monotonic activation_wall <<<"$activation_bounds"
  if ! capture bundle_output "$bundle_verifier" --team-id "$team_id" "$snapshot_dmg"; then
    fail "read-only DMG bundle verification failed: $bundle_output"
  fi
  metadata_line=""
  metadata_count=0
  while IFS= read -r line; do
    if [[ "$line" == S1A_BUNDLE_METADATA\ * ]]; then
      metadata_line=$line
      ((metadata_count += 1))
    fi
  done <<<"$bundle_output"
  [[ "$metadata_count" -eq 1 ]] || fail "bundle verifier did not return exactly one candidate identity"
  if [[ "$metadata_line" =~ ^S1A_BUNDLE_METADATA\ version=([0-9]+\.[0-9]+\.[0-9]+)\ team_id=([A-Z0-9]{10})\ app_cdhash=([0-9A-Fa-f]{40})\ main_cdhash=([0-9A-Fa-f]{40})\ agent_cdhash=([0-9A-Fa-f]{40})\ bridge_cdhash=([0-9A-Fa-f]{40})$ ]]; then
    candidate_version=${BASH_REMATCH[1]}
    candidate_team=${BASH_REMATCH[2]}
    candidate_app_cdhash=${BASH_REMATCH[3]}
    candidate_main_cdhash=${BASH_REMATCH[4]}
    candidate_agent_cdhash=${BASH_REMATCH[5]}
    candidate_bridge_cdhash=${BASH_REMATCH[6]}
  else
    fail "bundle verifier returned malformed candidate identity"
  fi
  [[ "$candidate_team" == "$team_id" ]] || fail "candidate TeamIdentifier does not match requested team"
  if ! capture snapshot_check "$snapshot_helper" validate --identity-json "$snapshot_identity"; then
    fail "private DMG snapshot changed between verification and open: $snapshot_check"
  fi
  if ! capture open_output open "$snapshot_dmg"; then fail "could not open DMG: $open_output"; fi
  immediate_snapshot=""
  if snapshot_install immediate_snapshot && [[ "$pre_open_snapshot" != "<not-installed>" ]]; then
    if capture immediate_old_bounds python3 -c '
import sys
def parse(value): return dict(field.split("=", 1) for field in value.split())
before, after = parse(sys.argv[1]), parse(sys.argv[2]); keys=("app_dev","app_ino","main_dev","main_ino")
if all(before[k] == after[k] for k in keys): print(after["observed_mono"], after["observed_wall"])
else: raise SystemExit(1)
' "$pre_open_snapshot" "$immediate_snapshot"; then
      read -r activation_monotonic activation_wall <<<"$immediate_old_bounds"
    fi
  elif [[ "$pre_open_snapshot" == "<not-installed>" ]] && capture immediate_absent python3 -c 'import os,sys,time; absent=not os.path.lexists(sys.argv[1]) and not os.path.lexists(sys.argv[2]); print(f"{time.monotonic():.9f} {time.time():.9f}") if absent else None; raise SystemExit(0 if absent else 1)' "$app" "$main"; then
    read -r activation_monotonic activation_wall <<<"$immediate_absent"
  fi
  if ! capture snapshot_check "$snapshot_helper" validate --identity-json "$snapshot_identity"; then
    fail "private DMG snapshot changed while being opened: $snapshot_check"
  fi
  echo "DMG verified and opened. Complete Gatekeeper and graphical install decisions manually."

  candidate_activated=0
  for ((attempt = 1; attempt <= install_polls; attempt++)); do
    current_snapshot=""
    if snapshot_install current_snapshot; then
      if [[ "$pre_open_snapshot" == "<not-installed>" ]]; then
        candidate_activated=1
        break
      fi
      if capture transition_check python3 -c '
import sys
def parse(value): return dict(field.split("=", 1) for field in value.split())
before, after = parse(sys.argv[1]), parse(sys.argv[2])
app_changed = (before["app_dev"], before["app_ino"]) != (after["app_dev"], after["app_ino"])
main_changed = (before["main_dev"], before["main_ino"]) != (after["main_dev"], after["main_ino"])
raise SystemExit(0 if app_changed and main_changed else 1)
' "$pre_open_snapshot" "$current_snapshot"; then
        candidate_activated=1
        break
      fi
      if capture old_bounds python3 -c '
import sys
def parse(value): return dict(field.split("=", 1) for field in value.split())
before, after = parse(sys.argv[1]), parse(sys.argv[2])
keys=("app_dev","app_ino","main_dev","main_ino")
if all(before[k] == after[k] for k in keys): print(after["observed_mono"], after["observed_wall"])
else: raise SystemExit(1)
' "$pre_open_snapshot" "$current_snapshot"; then
        read -r activation_monotonic activation_wall <<<"$old_bounds"
      fi
    elif [[ "$pre_open_snapshot" == "<not-installed>" ]]; then
      if capture absent_check python3 -c 'import os,sys,time; absent=not os.path.lexists(sys.argv[1]) and not os.path.lexists(sys.argv[2]); print(f"{time.monotonic():.9f} {time.time():.9f}") if absent else None; raise SystemExit(0 if absent else 1)' "$app" "$main"; then
        read -r activation_monotonic activation_wall <<<"$absent_check"
      fi
    fi
    if (( attempt < install_polls )); then
      if ! capture sleep_output sleep "$poll_seconds"; then fail "candidate install wait exceeded 300 seconds"; fi
    fi
  done
  [[ "$candidate_activated" -eq 1 ]] \
    || fail "verified candidate did not replace or create the installed app within 300 seconds"
else
  activation_monotonic=0
  activation_wall=0
  new_deadline 30
fi

installed=0
for ((attempt = 1; attempt <= install_polls; attempt++)); do
  if [[ -d "$app" && -x "$main" && -x "$agent" && -x "$bridge" ]]; then installed=1; break; fi
  if (( attempt < install_polls )); then
    if ! capture sleep_output sleep "$poll_seconds"; then fail "installed app wait exceeded its bounded phase"; fi
  fi
done
[[ "$installed" -eq 1 ]] || fail "installed app did not appear at the exact expected path"

if ! capture expected_uid id -u; then fail "could not identify installed user"; fi
[[ "$expected_uid" =~ ^[0-9]+$ && "$expected_uid" -ne 0 ]] \
  || fail "live verification must run as the non-root installed user"

if ! capture layout_error python3 - "$runtime_root" "$app_directory" "$data_directory" "$run_directory" \
  "$app" "$info" "$agent" "$bridge" "$launch_agent" "$status_file" "$socket_file" \
  "$database_file" "$expected_uid" <<'PY'
import os, stat, sys
from pathlib import Path

(root_text, app_dir_text, data_text, run_text, app_text, info_text, agent_text,
 bridge_text, launch_agent_text, status_text, socket_text, database_text, uid_text) = sys.argv[1:]
uid = int(uid_text)
root, app_dir, data, run, app = map(Path, (root_text, app_dir_text, data_text, run_text, app_text))
def reject(message):
    print(message)
    raise SystemExit(1)
try:
    if root.resolve(strict=True) != root:
        reject("runtime root resolves through a symbolic link")
except OSError as error:
    reject(f"cannot resolve runtime root: {error}")
for path, name in ((app_dir, "App"), (data, "Data"), (run, "Run")):
    if path.parent != root or path.name != name:
        reject("App/Data/Run are not exact direct children")
for path in (root, app_dir, data, run, app):
    try: metadata = path.lstat()
    except OSError as error: reject(f"missing directory: {path}: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode): reject(f"unsafe directory: {path}")
    if metadata.st_uid != uid: reject(f"wrong directory owner: {path}")
for path in (root, app_dir, data, run):
    if stat.S_IMODE(path.lstat().st_mode) != 0o700: reject(f"directory is not mode 0700: {path}")
for path in map(Path, (info_text, agent_text, bridge_text, launch_agent_text)):
    try: metadata = path.lstat()
    except OSError as error: reject(f"missing required file: {path}: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode): reject(f"required path is not a regular file: {path}")
    if metadata.st_uid != uid: reject(f"wrong file owner: {path}")
for path in map(Path, (status_text, database_text)):
    if os.path.lexists(path):
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode): reject(f"runtime file is not regular: {path}")
        if metadata.st_uid != uid: reject(f"wrong runtime file owner: {path}")
socket_path = Path(socket_text)
if os.path.lexists(socket_path):
    metadata = socket_path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISSOCK(metadata.st_mode): reject(f"runtime socket is invalid: {socket_path}")
    if metadata.st_uid != uid: reject(f"wrong runtime socket owner: {socket_path}")
walk_errors = []
def onerror(error): walk_errors.append(str(error))
for current, directories, files in os.walk(app, followlinks=False, onerror=onerror):
    current_path = Path(current)
    for name in (*directories, *files):
        candidate = current_path / name
        if candidate.is_symlink(): reject(f"symbolic link inside installed app: {candidate}")
    if current_path != app and current_path.name in {"Data", "Run"}: reject(f"writable runtime directory inside installed app: {current_path}")
if walk_errors: reject("could not enumerate installed app: " + "; ".join(walk_errors))
PY
then
  fail "unsafe App/Data/Run layout: $layout_error"
fi
[[ -z "$layout_error" ]] || fail "layout helper returned unexpected output: $layout_error"

plist_value() {
  local destination=$1 key=$2 path=$3
  if ! capture "$destination" plutil -extract "$key" raw -o - "$path"; then
    fail "could not read $key from $(basename "$path")"
  fi
}
plist_value bundle_identifier CFBundleIdentifier "$info"
[[ "$bundle_identifier" == "com.pca.PersonalComputerAgent" ]] || fail "installed app has the wrong bundle identifier"
plist_value app_version CFBundleShortVersionString "$info"
[[ "$app_version" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] || fail "installed app version is invalid"
if [[ "$mode" == "dmg" && "$app_version" != "$candidate_version" ]]; then fail "installed version does not match verified candidate"; fi
plist_value launch_label Label "$launch_agent"
[[ "$launch_label" == "com.pca.agentd" ]] || fail "embedded LaunchAgent label is invalid"
plist_value launch_program BundleProgram "$launch_agent"
[[ "$launch_program" == "Contents/Resources/bin/pca-agentd" ]] || fail "embedded LaunchAgent program is invalid"

signature_identity() {
  local destination=$1 target=$2 details team="" cdhash="" line
  if ! capture signature_verify codesign --verify --strict --verbose=2 "$target"; then
    fail "signature verification failed for $(basename "$target"): $signature_verify"
  fi
  if ! capture details codesign -d --verbose=4 "$target"; then
    fail "could not inspect signature for $(basename "$target"): $details"
  fi
  while IFS= read -r line; do
    case "$line" in
      TeamIdentifier=*) [[ -z "$team" ]] || fail "duplicate TeamIdentifier for $(basename "$target")"; team=${line#TeamIdentifier=} ;;
      CDHash=*) [[ -z "$cdhash" ]] || fail "duplicate CDHash for $(basename "$target")"; cdhash=${line#CDHash=} ;;
    esac
  done <<<"$details"
  [[ "$team" =~ ^[A-Z0-9]{10}$ ]] || fail "invalid TeamIdentifier for $(basename "$target")"
  [[ "$cdhash" =~ ^[0-9A-Fa-f]{40}$ ]] || fail "invalid CDHash for $(basename "$target")"
  printf -v "$destination" '%s %s' "$team" "$cdhash"
}
signature_identity app_identity "$app"
signature_identity main_identity "$main"
signature_identity agent_identity "$agent"
signature_identity bridge_identity "$bridge"
read -r installed_team app_cdhash <<<"$app_identity"
read -r main_team main_cdhash <<<"$main_identity"
read -r agent_team agent_cdhash <<<"$agent_identity"
read -r bridge_team bridge_cdhash <<<"$bridge_identity"
[[ "$main_team" == "$installed_team" && "$agent_team" == "$installed_team" && "$bridge_team" == "$installed_team" ]] \
  || fail "TeamIdentifier mismatch between installed app, main, agent, or Bridge"
if [[ "$mode" == "dmg" ]]; then
  [[ "$installed_team" == "$candidate_team" && "$app_cdhash" == "$candidate_app_cdhash" \
    && "$main_cdhash" == "$candidate_main_cdhash" && "$agent_cdhash" == "$candidate_agent_cdhash" \
    && "$bridge_cdhash" == "$candidate_bridge_cdhash" ]] \
    || fail "installed signatures do not match the verified candidate"
fi

if [[ "$mode" == "dmg" ]]; then
  if ! capture health_deadline python3 -c 'import sys; print(f"{float(sys.argv[1])+5.0:.9f}")' "$activation_monotonic"; then
    fail "candidate activation health deadline already expired"
  fi
  deadline=$health_deadline
else
  new_deadline 5
fi
status_pid=""
for ((attempt = 1; attempt <= health_polls; attempt++)); do
  candidate_pid=""
  if capture candidate_pid python3 - "$status_file" "$app_version" "$socket_file" "$database_file" "$expected_uid" "$activation_wall" <<'PY'
import json, stat, sys
from datetime import datetime, timezone
from pathlib import Path
path, expected_version, socket_path, database_path = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]), Path(sys.argv[4])
expected_uid = int(sys.argv[5])
activation_lower_bound = float(sys.argv[6])
for runtime_path, expected_type in ((path, stat.S_ISREG), (database_path, stat.S_ISREG), (socket_path, stat.S_ISSOCK)):
    metadata = runtime_path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not expected_type(metadata.st_mode): raise SystemExit(1)
    if metadata.st_uid != expected_uid or stat.S_IMODE(metadata.st_mode) != 0o600: raise SystemExit(1)
expected_keys = {"agent_status", "bridge_status", "local_healthy", "heartbeat_at", "process_id", "app_version", "schema_version"}
value = json.loads(path.read_text(encoding="utf-8"))
if not isinstance(value, dict) or set(value) != expected_keys: raise SystemExit(1)
heartbeat = datetime.fromisoformat(value["heartbeat_at"].replace("Z", "+00:00"))
if heartbeat.tzinfo is None: raise SystemExit(1)
age = (datetime.now(timezone.utc) - heartbeat.astimezone(timezone.utc)).total_seconds()
modified_age = datetime.now(timezone.utc).timestamp() - path.stat().st_mtime
pid = value["process_id"]
valid = (value["agent_status"] in {"unpaired", "running"} and value["bridge_status"] == "ready"
         and value["local_healthy"] is True and -5 <= age <= 5 and -5 <= modified_age <= 5
         and heartbeat.timestamp() >= activation_lower_bound
         and path.stat().st_mtime >= activation_lower_bound
         and isinstance(pid, int) and not isinstance(pid, bool) and pid > 0
         and value["app_version"] == expected_version and value["schema_version"] == 2)
if not valid: raise SystemExit(1)
print(pid)
PY
  then
    if [[ "$candidate_pid" =~ ^[1-9][0-9]*$ ]]; then status_pid=$candidate_pid; break; fi
  fi
  if (( attempt < health_polls )); then
    if ! capture sleep_output sleep "$poll_seconds"; then fail "health wait exceeded five seconds"; fi
  fi
done
[[ "$status_pid" =~ ^[1-9][0-9]*$ ]] \
  || fail "healthy runtime status with required owner and mode 0600 newer than candidate activation was not observed within five seconds"

if ! capture job launchctl print "gui/$expected_uid/com.pca.agentd"; then fail "expected user-level launchd job is not registered"; fi
[[ "$job" == *"state = running"* ]] || fail "expected launchd job is not running"
[[ "$job" == *"program = $agent"* \
  || "$job" == *"program = Contents/Resources/bin/pca-agentd"* \
  || ( "$job" == *"program identifier = Contents/Resources/bin/pca-agentd (mode:"* \
    && "$job" == *"parent bundle identifier = com.pca.PersonalComputerAgent"* ) ]] \
  || fail "launchd job does not resolve to the exact installed agent"
[[ "$job" =~ pid\ =\ *$status_pid([^0-9]|$) ]] || fail "launchd job PID does not match runtime status"

if ! capture process_document "$process_inspector" --agent-pid "$status_pid" --uid "$expected_uid" \
  --agent-path "$agent" --bridge-path "$bridge"; then
  fail "reliable macOS process inspection failed: $process_document"
fi
if ! capture process_ids python3 -c '
import json, sys
document = json.loads(sys.argv[1])
agent_path, bridge_path, uid_text, status_pid_text, lower_text = sys.argv[2:]
uid, status_pid, lower = int(uid_text), int(status_pid_text), float(lower_text)
if not isinstance(document, dict) or set(document) != {"agent", "bridge"}: raise SystemExit(1)
a, b = document["agent"], document["bridge"]
keys = {"pid", "ppid", "uid", "path", "start_time"}
if not isinstance(a, dict) or not isinstance(b, dict) or set(a) != keys or set(b) != keys: raise SystemExit(1)
for item in (a, b):
    for key in ("pid", "ppid", "uid"):
        if not isinstance(item[key], int) or isinstance(item[key], bool) or item[key] < 0: raise SystemExit(1)
if a["pid"] != status_pid or a["uid"] != uid or uid == 0 or a["path"] != agent_path: raise SystemExit(1)
if b["pid"] <= 0 or b["uid"] != uid or b["path"] != bridge_path or b["ppid"] != a["pid"]: raise SystemExit(1)
if not isinstance(a["start_time"], (int, float)) or isinstance(a["start_time"], bool): raise SystemExit(1)
if not isinstance(b["start_time"], (int, float)) or isinstance(b["start_time"], bool): raise SystemExit(1)
if a["start_time"] < lower or b["start_time"] < lower or b["start_time"] < a["start_time"]: raise SystemExit(1)
print(a["pid"], b["pid"])
' "$process_document" "$agent" "$bridge" "$expected_uid" "$status_pid" "$activation_wall"; then
  fail "Agent/Bridge current user process identity predates or does not match candidate activation"
fi
read -r agent_pid bridge_pid <<<"$process_ids"
[[ "$agent_pid" =~ ^[1-9][0-9]*$ && "$bridge_pid" =~ ^[1-9][0-9]*$ ]] || fail "process inspection returned invalid PIDs"

if ! capture database_check sqlite3 -readonly "$database_file" <<'SQL'
PRAGMA query_only=ON;
PRAGMA integrity_check;
SELECT id || ':' || status FROM schema_migrations ORDER BY id;
SQL
then
  fail "SQLite helper failed: $database_check"
fi
[[ "$database_check" == $'ok\n0000:completed\n0001:completed' ]] || fail "SQLite integrity or exact S1A migration ledger is invalid"

echo "S1A LIVE VERIFIED: version=$app_version agent_pid=$agent_pid bridge_pid=$bridge_pid uid=$expected_uid"
