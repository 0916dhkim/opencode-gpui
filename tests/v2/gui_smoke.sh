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
python3 - "${base}" "${session}" "${workspace}" "${temporary}/config/opencode-gpui/state.json" <<'PY'
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
"${binary}" --server "${base}" --username opencode >"${temporary}/app.log" 2>&1 &
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
  api GET "/api/session/${session}/message?limit=20" | python3 -c '
import json, sys
entries = json.load(sys.stdin)["data"]
user = [e for e in entries if e["type"] == "user" and "GUI smoke test" in e.get("text", "")]
assistant = [e for e in entries if e["type"] == "assistant"
             and any(c.get("type") == "text" and c.get("text") for c in e.get("content", []))]
sys.exit(0 if user and assistant else 1)'
}
ok=""
for _ in $(seq 1 60); do
  if check_messages; then ok=1; break; fi
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

kill -0 "${app}" 2>/dev/null && pass "ui.still-running" || fail "ui.still-running" "client exited"
if grep -qi "panicked" "${temporary}/app.log"; then fail "ui.no-panic" "$(grep -i -m1 panicked "${temporary}/app.log")"; fi

if ((failures)); then
  printf -- '--- client log (tail) ---\n' >&2
  tail -n 30 "${temporary}/app.log" >&2
  exit 1
fi
printf 'GUI smoke: all checks passed\n'
