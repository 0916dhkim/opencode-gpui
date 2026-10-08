#!/usr/bin/env bash
# Run inside the amd64 GUI builder, never against the user's desktop or server.
set -euo pipefail
tmp="$(mktemp -d)"
server_pid='' weston_pid='' app_pid='' xvfb_pid=''
cleanup() {
  for pid in "$app_pid" "$weston_pid" "$server_pid" "$xvfb_pid"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  if [[ -n "${KEEP_RESYNC_TEST:-}" ]]; then
    printf 'Retained test logs: %s\n' "$tmp"
  else
    rm -rf "$tmp"
  fi
}
trap cleanup EXIT
export DISPLAY=:98 XDG_RUNTIME_DIR="$tmp/runtime" XDG_CONFIG_HOME="$tmp/config"
export XDG_DATA_HOME="$tmp/data" XDG_CACHE_HOME="$tmp/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/opencode-gpui"
chmod 700 "$XDG_RUNTIME_DIR"
Xvfb :98 -screen 0 1280x900x24 -nolisten tcp >"$tmp/xvfb.log" 2>&1 & xvfb_pid=$!
for _ in {1..60}; do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
weston --backend=x11-backend.so --use-pixman --shell=kiosk-shell.so \
  --width=780 --height=791 --socket=wayland-1 --idle-time=0 >"$tmp/weston.log" 2>&1 & weston_pid=$!
for _ in {1..60}; do [[ -S "$XDG_RUNTIME_DIR/wayland-1" ]] && break; sleep 0.1; done
[[ -S "$XDG_RUNTIME_DIR/wayland-1" ]]
export WAYLAND_DISPLAY=wayland-1 LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
export NO_AT_BRIDGE=1 GTK_A11Y=none GSETTINGS_BACKEND=memory
export FAKE_OPENCODE_PASSWORD=resync-test-password OPENCODE_SERVER_PASSWORD=resync-test-password
python3 tests/fake_opencode_server.py --address-file "$tmp/address" --log-file "$tmp/requests.jsonl" \
  --workspace /state/workspace --other-directory /state/other --cwd /state/home \
  --history-turns 2 --heartbeat-s 2 >"$tmp/server.log" 2>&1 & server_pid=$!
for _ in {1..100}; do [[ -s "$tmp/address" ]] && break; sleep 0.1; done
[[ -s "$tmp/address" ]] || { printf 'Fake server failed to start\n' >&2; exit 1; }
address="$(<"$tmp/address")"
control() {
  python3 - "$address" "$1" <<'PY'
import json, sys, urllib.request
request = urllib.request.Request(sys.argv[1] + '/__control', data=sys.argv[2].encode(), method='POST', headers={'Content-Type':'application/json'})
with urllib.request.urlopen(request, timeout=10) as response:
    json.load(response)
PY
}
wait_log() { python3 tests/fake_v2/logwait.py wait "$tmp/requests.jsonl" --after "$1" --expr "$2" --timeout 25 >/dev/null; }
control '{"action":"fail_route","route":"server.info","count":1}'
python3 - "$address" "$XDG_CONFIG_HOME/opencode-gpui/state.json" <<'PY'
import json, sys
address, path = sys.argv[1:]
main = 'ses_f90000000001ffeIntegration'
other = 'ses_f90000000002ffeSecondSessn'
state = {'connection': {'server': address, 'username': 'opencode'}, 'servers': {
    address: {'tabs': [
        {'id': main, 'directory': '/state/workspace', 'title': 'Integration session'},
        {'id': other, 'directory': '/state/other', 'title': 'Second session'}],
        'active': main}}}
with open(path, 'w', encoding='utf-8') as stream: json.dump(state, stream)
PY
target/debug/opencode-gpui --server "$address" --username opencode >"$tmp/app.log" 2>&1 & app_pid=$!
wait_log 0 "http and route == 'server.info' and r.get('status') == 503"
wait_log 0 "http and route == 'server.info' and r.get('status') == 200"
wait_log 0 "http and route == 'message.list' and p.get('sessionID') == 'ses_f90000000001ffeIntegration'"
printf 'PASS retry after bootstrap error with SSE connected\n'
mark="$(python3 tests/fake_v2/logwait.py seq "$tmp/requests.jsonl")"
control '{"action":"fail_route","route":"project.list","count":1}'
control '{"action":"drop_sse"}'
wait_log "$mark" 'r["kind"] == "sse.open" and r["n"] >= 2'
wait_log "$mark" "http and route == 'project.list' and r.get('status') == 503"
wait_log "$mark" "http and route == 'message.list' and p.get('sessionID') == 'ses_f90000000001ffeIntegration'"
wait_log "$mark" "http and route == 'message.list' and p.get('sessionID') == 'ses_f90000000002ffeSecondSessn'"
wait_log "$mark" "http and route == 'project.list' and r.get('status') == 200"
kill -0 "$app_pid"
printf 'PASS reconnect reloads both tabs; partial refresh retries independently of SSE\n'
