#!/usr/bin/env bash
# GPUI smoke test against the real 2.0.8 harness server. Run by tests/v2/e2e.sh
# inside the builder image, joined to `ocgtk-v2h-net`, never on a desktop.
#
# Needs: the built client at $GUI_BINARY, the Basic password at
# $GUI_PASSWORD_FILE, and optionally $GUI_SHOTS for the screenshot.
# Connects through a loopback forward (P1 allows plain HTTP to loopback only),
# sends one `[[scenario:text]]` prompt by keyboard and checks through the
# server API that the session got the user prompt and an assistant reply.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${GUI_IN_DBUS:-}" ]]; then
  GUI_IN_DBUS=1 exec dbus-run-session -- bash "$0" "$@"
fi

binary="${GUI_BINARY:-target/debug/opencode-gpui}"
password_file="${GUI_PASSWORD_FILE:?GUI_PASSWORD_FILE is required}"
upstream_host="${GUI_UPSTREAM_HOST:-ocgtk-v2h-server}"
workspace="${GUI_WORKSPACE:-/state/workspace}"
shots="${GUI_SHOTS:-}"
# Never 4096/4097: those are the ports of real servers on a developer host.
local_port=14096
base="http://127.0.0.1:${local_port}"
case "${GUI_CONFIG_SUFFIX:-}" in
  ''|/api) configured="${base}${GUI_CONFIG_SUFFIX:-}" ;;
  *) echo 'GUI_CONFIG_SUFFIX must be empty or /api' >&2; exit 2 ;;
esac
temporary="$(mktemp -d)"
pids=()
failures=0
app_pid='' xvfb_pid='' weston_pid='' window=''
source tests/gpui-headless.sh

cleanup() {
  for pid in "${pids[@]}"; do kill "${pid}" 2>/dev/null || true; done
  gpui_stop_display
  for pid in "${pids[@]}"; do wait "${pid}" 2>/dev/null || true; done
  rm -rf "${temporary}"
}
trap cleanup EXIT

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s: %s\n' "$1" "$2" >&2; failures=$((failures + 1)); }

python3 tests/v2/loopback.py --ready "${temporary}/forward-ready" "${local_port}" "${upstream_host}" 4096 &
forwarder=$!
pids+=("${forwarder}")
for _ in $(seq 1 50); do
  [[ -s "${temporary}/forward-ready" ]] && break
  kill -0 "${forwarder}" 2>/dev/null || break
  sleep 0.1
done
if [[ ! -s "${temporary}/forward-ready" ]]; then
  fail "forwarder" "could not listen on 127.0.0.1:${local_port}"
  exit 1
fi

gpui_start_display || exit 1

# api METHOD PATH [JSON] -- prints the JSON response; the password stays in the file.
api() {
  python3 - "${base}" "${password_file}" "$@" <<'PY'
import base64, json, sys, urllib.request
base, password_file, method, path = sys.argv[1:5]
body = sys.argv[5].encode() if len(sys.argv) > 5 else None
token = base64.b64encode(("opencode:" + open(password_file).read().strip()).encode()).decode()
request = urllib.request.Request(base + path, data=body, method=method,
                                 headers={"authorization": "Basic " + token, "content-type": "application/json"})
with urllib.request.urlopen(request, timeout=15) as response:
    raw = response.read()
print(raw.decode() if raw else "null")
PY
}

for _ in $(seq 1 50); do api GET /api/info >/dev/null 2>&1 && break; sleep 0.2; done
if version="$(api GET /api/info | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')"; then
  pass "server.reachable (${version})"
else
  fail "server.reachable" "no /api/info through the loopback forward"
  exit 1
fi

session="$(api POST /api/session "{\"location\":{\"directory\":\"${workspace}\"},\"title\":\"GUI smoke\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["id"])')"
[[ -n "${session}" ]] && pass "session.created ${session}" || { fail "session.created" "no session"; exit 1; }

mkdir -p "${temporary}/config/opencode-gpui"
python3 - "${configured}" "${session}" "${workspace}" "${temporary}/config/opencode-gpui/state.json" <<'PY'
import json, sys
server, session, workspace, path = sys.argv[1:]
json.dump({
    "connection": {"server": server, "username": "opencode", "cloudflare_access": False},
    "servers": {server: {"tabs": [{"id": session, "directory": workspace, "title": "GUI smoke"}],
                         "active": session}},
    "zoom_level": 1.0,
}, open(path, "w"))
PY

OPENCODE_SERVER_PASSWORD="$(cat "${password_file}")" \
XDG_CONFIG_HOME="${temporary}/config" \
XDG_DATA_HOME="${temporary}/data" \
XDG_CACHE_HOME="${temporary}/cache" \
GSETTINGS_BACKEND=memory \
NO_AT_BRIDGE=1 \
"${binary}" --server "${configured}" --username opencode >"${temporary}/app.log" 2>&1 &
app=$!
app_pid=$app
pids+=("${app}")

if gpui_wait_window; then pass "ui.window"; else fail "ui.window" "no GPUI window"; tail -20 "${temporary}/app.log" >&2; exit 1; fi

# Let bootstrap, history and the model catalog load before typing.
sleep 4
# The first live bootstrap can still move focus after the window maps. Click
# the visible composer rather than depending on an earlier focus shortcut.
gpui_click 410 680
gpui_type "Hello from the GUI smoke test [[scenario:text]]"
gpui_key Return

check_messages() {
  api GET "/api/session/$1/message?limit=20" | python3 -c '
import json, sys
entries = json.load(sys.stdin)["data"]
user = [e for e in entries if e["type"] == "user" and sys.argv[1] in e.get("text", "")]
assistant = [e for e in entries if e["type"] == "assistant"
              and any(c.get("type") == "text" and c.get("text") for c in e.get("content", []))]
sys.exit(0 if user and assistant else 1)' "$2"
}
ok=""
for _ in $(seq 1 60); do
  if check_messages "${session}" "GUI smoke test"; then ok=1; break; fi
  sleep 0.5
done
if [[ -n "${ok}" ]]; then
  pass "prompt.round-trip (user + assistant entries on the server)"
else
  fail "prompt.round-trip" "no user+assistant entries within 30s"
fi

sleep 1.5
if [[ -n "${shots}" ]]; then
  mkdir -p "${shots}"
  if import -window "${window}" "${shots}/e2e-gui-real-server.png" 2>/dev/null; then
    pass "screenshot ${shots}/e2e-gui-real-server.png"
  else
    fail "screenshot" "import failed"
  fi
fi

if [[ "${GUI_SLOW_FLOW:-0}" == 1 ]]; then
  # A real v2 server with a deliberately slow provider, not a fabricated UI
  # event. Capture the optimistic/streaming phases for native visual review.
  gpui_click 410 680
  gpui_type "Slow GUI turn [[scenario:slow]]"
  gpui_key Return
  if [[ -n "${shots}" ]]; then
    import -window "${window}" "${shots}/e2e-gui-slow-submitted.png" 2>/dev/null \
      && pass "slow.submitted-frame" || fail "slow.submitted-frame" "capture failed"
  fi
  slow_piece() {
    api GET "/api/session/${session}/message?limit=20" | python3 -c '
import json,sys
entries=json.load(sys.stdin)["data"]
sys.exit(0 if any(e["type"]=="assistant" and any("slow-0" in c.get("text","")
  for c in e.get("content",[]) if c.get("type")=="text") for e in entries) else 1)'
  }
  slow_complete() {
    api GET "/api/session/${session}/message?limit=20" | python3 -c '
import json,sys
entries=json.load(sys.stdin)["data"]
users=[e for e in entries if e["type"]=="user" and "Slow GUI turn" in e.get("text","")]
done=any(e["type"]=="assistant" and any("slow-19" in c.get("text","")
  for c in e.get("content",[]) if c.get("type")=="text") for e in entries)
sys.exit(0 if len(users)==1 and done else 1)'
  }
  streaming=''
  for _ in $(seq 1 40); do
    if slow_piece; then streaming=1; break; fi
    sleep 0.25
  done
  if [[ -n "${streaming}" ]]; then
    pass "slow.streaming-on-real-server"
    if [[ -n "${shots}" ]]; then
      import -window "${window}" "${shots}/e2e-gui-slow-streaming.png" 2>/dev/null \
        && pass "slow.streaming-frame" || fail "slow.streaming-frame" "capture failed"
    fi
  else
    fail "slow.streaming-on-real-server" "no first slow text delta within 10s"
  fi
  complete=''
  for _ in $(seq 1 60); do
    if slow_complete; then complete=1; break; fi
    sleep 0.5
  done
  if [[ -n "${complete}" ]]; then
    pass "slow.one-user-and-complete-reply"
  else
    fail "slow.one-user-and-complete-reply" "no single delivered user and final reply within 30s"
  fi
  is_active() {
    api GET /api/session/active | python3 -c '
import json,sys
sys.exit(0 if sys.argv[1] in json.load(sys.stdin).get("data",{}) else 1)' "${session}"
  }
  for _ in $(seq 1 30); do
    if ! is_active; then break; fi
    sleep 0.2
  done
  gpui_click 410 680
  gpui_type "GUI Stop turn [[scenario:slow]]"
  gpui_key Return
  running=''
  for _ in $(seq 1 30); do
    if is_active; then running=1; break; fi
    sleep 0.2
  done
  if [[ -n "${running}" ]]; then
    pass "stop.real-run-became-active"
    gpui_click 681 750
    stopped=''
    for _ in $(seq 1 25); do
      if ! is_active; then stopped=1; break; fi
      sleep 0.2
    done
    if [[ -n "${stopped}" ]]; then
      pass "stop.gui-interrupted-before-slow-run-finished"
    else
      fail "stop.gui-interrupted-before-slow-run-finished" "session stayed active after Stop"
    fi
    if [[ -n "${shots}" ]]; then
      import -window "${window}" "${shots}/e2e-gui-slow-stopped.png" 2>/dev/null \
        && pass "stop.stopped-frame" || fail "stop.stopped-frame" "capture failed"
    fi
  else
    fail "stop.real-run-became-active" "slow follow-up never became active"
  fi
fi

if [[ "${GUI_SETTINGS_SWITCH:-0}" == 1 ]]; then
  # Apply the same live server through its alternate /api URL. This exercises
  # the Settings UI, reconnect behavior, canonical server identity and an
  # actual prompt after the transition without using developer credentials.
  if [[ "${configured}" == "${base}/api" ]]; then next_url="${base}"; else next_url="${base}/api"; fi
  gpui_click 410 680
  gpui_key ctrl+comma
  sleep 0.5
  gpui_click 430 219
  gpui_key ctrl+a
  gpui_type "${next_url}"
  gpui_click 758 726
  persisted=''
  for _ in $(seq 1 50); do
    if python3 - "${temporary}/config/opencode-gpui/state.json" "${next_url}" <<'PY'
import json,sys
try:
    state=json.load(open(sys.argv[1]))
    assert state["connection"]["server"] == sys.argv[2]
except (OSError, ValueError, KeyError, AssertionError):
    sys.exit(1)
PY
    then persisted=1; break; fi
    sleep 0.2
  done
  if [[ -n "${persisted}" ]]; then
    pass "settings.server-url-persisted"
  else
    fail "settings.server-url-persisted" "Apply did not save ${next_url}"
  fi
  sleep 1
  gpui_click 410 680
  gpui_type "After settings reconnect [[scenario:text]]"
  gpui_key Return
  check_after_settings() {
    api GET "/api/session/${session}/message?limit=20" | python3 -c '
import json,sys
entries=json.load(sys.stdin)["data"]
users=[e for e in entries if e["type"]=="user" and "After settings reconnect" in e.get("text","")]
last=max((e.get("time",{}).get("created",0) for e in users),default=0)
reply=any(e["type"]=="assistant" and e.get("time",{}).get("created",0)>=last
  and any(c.get("type")=="text" and c.get("text") for c in e.get("content",[])) for e in entries)
sys.exit(0 if len(users)==1 and reply else 1)'
  }
  reconnected=''
  for _ in $(seq 1 60); do
    if check_after_settings; then reconnected=1; break; fi
    sleep 0.5
  done
  if [[ -n "${reconnected}" ]]; then
    pass "settings.gui-prompt-after-url-switch"
  else
    fail "settings.gui-prompt-after-url-switch" "no reply after applying alternate URL"
  fi
fi

# Exercise a second real-server GUI path, not just an API-created tab: create
# through the project picker and verify the server and persisted active tab.
state_file="${temporary}/config/opencode-gpui/state.json"
active_tab() {
  python3 - "${state_file}" "${base}" <<'PY'
import json, sys
try:
    state = json.load(open(sys.argv[1]))
    print(state["servers"][sys.argv[2]]["active"] or "")
except (FileNotFoundError, KeyError, ValueError):
    print("")
PY
}
gpui_key ctrl+t
gpui_key Return
new_session=''
for _ in $(seq 1 50); do
  candidate="$(active_tab)"
  if [[ -n "${candidate}" && "${candidate}" != "${session}" ]]; then
    new_session="${candidate}"
    break
  fi
  sleep 0.2
done
if [[ -n "${new_session}" ]] && api GET "/api/session/${new_session}" |
    python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("data", {}).get("id") else 1)'; then
  pass "new-session.gui-created-and-persisted"
else
  fail "new-session.gui-created-and-persisted" "no new server session became the active tab"
  exit 1
fi
sleep 1
gpui_key ctrl+1
restored=''
for _ in $(seq 1 50); do
  if [[ "$(active_tab)" == "${session}" ]]; then restored=1; break; fi
  sleep 0.2
done
if [[ -n "${restored}" ]]; then
  pass "tab-switch.first-session-persisted"
else
  fail "tab-switch.first-session-persisted" "Ctrl+1 did not restore the first tab"
fi

# A new process must restore both tabs from this private state, then send in
# the second session without borrowing the first session's composer draft.
kill "${app}"
wait "${app}" 2>/dev/null || true
pids=("${forwarder}")
OPENCODE_SERVER_PASSWORD="$(cat "${password_file}")" \
XDG_CONFIG_HOME="${temporary}/config" XDG_DATA_HOME="${temporary}/data" \
XDG_CACHE_HOME="${temporary}/cache" GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1 \
"${binary}" --server "${configured}" --username opencode >>"${temporary}/app.log" 2>&1 &
app=$!
app_pid=$app
pids+=("${app}")
if gpui_wait_window; then pass "restart.ui.window"; else fail "restart.ui.window" "no restored GPUI window"; fi
sleep 3
gpui_key ctrl+2
restored=''
for _ in $(seq 1 50); do
  if [[ "$(active_tab)" == "${new_session}" ]]; then restored=1; break; fi
  sleep 0.2
done
if [[ -n "${restored}" ]]; then
  pass "restart.second-session-restored"
else
  fail "restart.second-session-restored" "Ctrl+2 did not select the persisted second tab"
fi
gpui_click 410 680
gpui_type "Second session after restart [[scenario:text]]"
gpui_key Return
ok=''
for _ in $(seq 1 60); do
  if check_messages "${new_session}" "after restart"; then ok=1; break; fi
  sleep 0.5
done
if [[ -n "${ok}" ]]; then
  pass "restart.second-session-prompt-round-trip"
else
  fail "restart.second-session-prompt-round-trip" "no reply in the restored second session"
fi
if [[ -n "${shots}" ]]; then
  import -window "${window}" "${shots}/e2e-gui-restored-session.png" 2>/dev/null \
    && pass "screenshot ${shots}/e2e-gui-restored-session.png" \
    || fail "screenshot" "restored-session capture failed"
fi

kill -0 "${app}" 2>/dev/null && pass "ui.still-running" || fail "ui.still-running" "client exited"
if grep -qi "panicked" "${temporary}/app.log"; then fail "ui.no-panic" "$(grep -i -m1 panicked "${temporary}/app.log")"; fi

if ((failures)); then
  printf -- '--- client log (tail) ---\n' >&2
  tail -n 30 "${temporary}/app.log" >&2
  exit 1
fi
printf 'GUI smoke: all checks passed\n'
