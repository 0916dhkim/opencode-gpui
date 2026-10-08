#!/usr/bin/env bash
# GPUI end-to-end flow against the local fake v2 server, in nested Weston/Xvfb.
# docker run --rm --platform linux/amd64 -v "$PWD":/repo -w /repo \
#   opencode-gpui-builder-amd64:latest bash tests/remote-flow-ui.sh
# FLOW_BINARY skips the build; FLOW_TIMEOUT bounds each marker; FLOW_KEEP retains logs.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
temporary="$(mktemp -d)"
source tests/gpui-headless.sh
log="$temporary/requests.jsonl"
app_log="$temporary/app.log"
app_pid='' server_pid='' weston_pid='' xvfb_pid='' watchdog_pid='' window=''
mark=0
timeout_s="${FLOW_TIMEOUT:-20}"
passes=() failures=()
main=ses_f90000000001ffeIntegration
other=ses_f90000000002ffeSecondSessn
cleanup() {
  for pid in "$app_pid" "$server_pid" "$watchdog_pid"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  gpui_stop_display
  if [[ -n "${FLOW_KEEP:-}" ]]; then echo "Kept $temporary" >&2; else rm -rf "$temporary"; fi
}
trap cleanup EXIT
(sleep "${FLOW_DEADLINE:-600}"; echo 'FLOW_DEADLINE reached' >&2; kill -TERM $$) & watchdog_pid=$!
pass() { passes+=("$1"); printf 'PASS %s\n' "$1"; }
fail() { failures+=("$1"); printf 'FAIL %s: %s\n' "$1" "$2" >&2; }
logq() { python3 tests/fake_v2/logwait.py "$@"; }
mark_now() { mark="$(logq seq "$log")"; }
alive() { [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; }
found=''
expect() {
  local name="$1" expr="$2" wait="${3:-$timeout_s}"
  if found="$(logq wait "$log" --after "$mark" --timeout "$wait" --expr "$expr")"; then
    pass "$name"
    return 0
  fi
  found=''
  fail "$name" "no record within ${wait}s: $expr"
  return 1
}
expect_none() {
  local name="$1" expr="$2" offending
  if offending="$(logq none "$log" --expr "$expr")"; then pass "$name"; else fail "$name" "$offending"; fi
}
field() { python3 -c 'import json,sys; r=json.loads(sys.argv[1]); print(eval(sys.argv[2], {}, {"r": r}))' "$1" "$2"; }
control() {
  python3 - "$address" "$1" <<'PY'
import json, sys, urllib.request
request = urllib.request.Request(sys.argv[1] + '/__control', data=sys.argv[2].encode(), method='POST', headers={'Content-Type':'application/json'})
with urllib.request.urlopen(request, timeout=10) as response: json.load(response)
PY
}

gpui_start_display || exit 1
password="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
FAKE_OPENCODE_PASSWORD="$password" python3 tests/fake_opencode_server.py \
  --address-file "$temporary/address" --log-file "$log" \
  --workspace /state/workspace --other-directory /state/other --cwd /state/home \
  --history-turns 120 --extra-sessions 120 --boot-permission --background-jobs \
  --heartbeat-s 2 --slow-delay-ms 700 --slow-deltas 40 --models-empty-once \
  --delay-bootstrap-ms 600 --race session.list:rename \
  >"$temporary/server.log" 2>&1 & server_pid=$!
for _ in {1..100}; do [[ -s "$temporary/address" ]] && break; sleep 0.1; done
[[ -s "$temporary/address" ]] || { cat "$temporary/server.log" >&2; exit 1; }
address="$(<"$temporary/address")"

# Include one stale tab to check migration/repair of persisted state.
mkdir -p "$temporary/config/opencode-gpui" "$temporary/data" "$temporary/cache"
python3 - "$address" "$temporary/config/opencode-gpui/state.json" <<'PY'
import json, sys
server, path = sys.argv[1:]
state = {'connection': {'server': server, 'username': 'opencode'}, 'servers': {
    server: {'tabs': [
        {'id': 'ses_f90000000001ffeIntegration', 'directory': '/state/workspace', 'title': 'Integration session'},
        {'id': 'ses_f90000000002ffeSecondSessn', 'directory': '/state/other', 'title': 'Second session'},
        {'id': 'ses_0000000000v1StaleTab000001', 'directory': '/state/workspace', 'title': 'Old v1 tab'}],
        'active': 'ses_f90000000001ffeIntegration'}}}
with open(path, 'w', encoding='utf-8') as stream: json.dump(state, stream)
PY
gpui_binary "${FLOW_BINARY:-}" || exit 1
XDG_CONFIG_HOME="$temporary/config" XDG_DATA_HOME="$temporary/data" XDG_CACHE_HOME="$temporary/cache" \
  GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1 OPENCODE_SERVER_PASSWORD="$password" \
  "$binary" --server "$address" --username opencode >"$app_log" 2>&1 & app_pid=$!
if gpui_wait_window; then pass ui.window; else fail ui.window 'no GPUI window'; tail -40 "$app_log" >&2; exit 1; fi

expect bootstrap.info 'http and route == "server.info" and r["status"] == 200'
expect bootstrap.projects 'http and route == "project.list"'
expect bootstrap.sessions 'http and route == "session.list" and q.get("parentID") == "null" and "cursor" not in q'
expect bootstrap.paged 'http and route == "session.list" and "cursor" in q and "limit" in q'
expect bootstrap.active 'http and route == "session.active"'
expect bootstrap.permissions 'http and route == "permission.request.list" and "location[directory]" in q'
expect bootstrap.forms 'http and route == "form.list" and "location[directory]" in q'
if expect bootstrap.models 'http and route == "model.list" and "location[directory]" in q'; then
  first_models="$(field "$found" 'r["seq"]')"
  mark="$first_models"
  expect models.refetch-after-empty 'http and route == "model.list" and "location[directory]" in q'
  mark=0
fi
expect bootstrap.history "http and route == 'message.list' and p.get('sessionID') == '$main' and 'cursor' not in q"
expect bootstrap.jobs "http and route == 'shell.list' and q.get('location[directory]') == '/state/other'"

# The permission card is rendered over the composer on the active session.
if [[ -n "${FLOW_SHOTS:-}" ]]; then import -window "$window" "${FLOW_SHOTS%.png}-permission.png"; fi
mark_now
gpui_click 696 738
expect permission.once "http and route == 'session.permission.reply' and p.get('sessionID') == '$main' and b.get('decision') == 'once'"

# The earlier-history button is at the top of the main transcript.
mark_now
gpui_click 400 76
expect history.cursor "http and route == 'message.list' and p.get('sessionID') == '$main' and 'cursor' in q and 'order' not in q"

# The pencil in the main tab opens the GPUI rename modal.
gpui_click 229 110
if [[ -n "${FLOW_SHOTS:-}" ]]; then import -window "$window" "${FLOW_SHOTS%.png}-rename.png"; fi
gpui_type 'Renamed integration session'
gpui_click 510 490
expect rename.session "http and route == 'session.update' and p.get('sessionID') == '$main' and b.get('title') == 'Renamed integration session' and r['status'] == 204"

# Focus a GPUI input before routing application-level keyboard shortcuts.
gpui_click 410 680
mark_now
gpui_key ctrl+t
gpui_key Return
new_session=''
if expect create.session "http and route == 'session.create' and b.get('location', {}).get('directory') in ('/state/workspace', '/state/other') and 'agent' not in b['keys'] and 'model' not in b['keys']"; then
  if expect create.event 'ev == "session.created"'; then
    new_session="$(field "$found" 'r["sessionID"]')"
  fi
fi

if [[ -n "$new_session" ]]; then
  gpui_click 410 680
  mark_now
  gpui_type 'Need input [[scenario:form]]'
  gpui_key Return
  if expect form.created "ev == 'form.created'"; then
    sleep 1
    if [[ -n "${FLOW_SHOTS:-}" ]]; then import -window "$window" "${FLOW_SHOTS%.png}-form.png"; fi
    mark_now
    gpui_click 716 600
    expect form.cancel "http and route == 'session.form.cancel' and p.get('sessionID') in ('$new_session', 'global') and r['status'] in (204, 409)"
  fi
  mark_now
  gpui_type 'Flow prompt [[scenario:slow]]'
  gpui_key Return
  expect prompt.send "http and route == 'session.prompt' and p.get('sessionID') == '$new_session' and 'Flow prompt' in (b.get('text') or '') and 'delivery' not in b['keys'] and str(b.get('id') or '').startswith('msg_')"
  expect prompt.stream "ev == 'session.text.delta' and r.get('sessionID') == '$new_session'"
  mark_now
  gpui_type 'Flow queued [[scenario:text]]'
  gpui_key ctrl+Return
  expect prompt.queue "http and route == 'session.prompt' and p.get('sessionID') == '$new_session' and 'Flow queued' in (b.get('text') or '') and b.get('delivery') == 'queue'"
  if [[ -n "${FLOW_SHOTS:-}" ]]; then import -window "$window" "${FLOW_SHOTS%.png}-queue.png"; fi
  mark_now
  gpui_click 680 607
  expect tray.steer "http and route == 'session.inbox.update' and b.get('delivery') == 'steer' and r['status'] == 204"
  mark_now
  gpui_click 680 607
  expect tray.queue "http and route == 'session.inbox.update' and b.get('delivery') == 'queue' and r['status'] == 204"
  # GPUI's stop button is in the footer; the server must see an interrupt.
  mark_now
  gpui_click 690 750
  expect prompt.interrupt "http and route == 'session.interrupt' and p.get('sessionID') == '$new_session' and 'resume' not in q"
  if [[ -n "${FLOW_SHOTS:-}" ]]; then import -window "$window" "${FLOW_SHOTS%.png}-parked.png"; fi
  mark_now
  gpui_click 706 554
  expect tray.resume "http and route == 'session.inbox.update' and b.get('delivery') == 'steer' and r['status'] == 204"
  expect tray.delivered "ev == 'session.inbox.delivered' and r.get('sessionID') == '$new_session'" 35
fi

mark_now
control '{"action":"drop_sse"}'
if expect sse.reconnect 'r["kind"] == "sse.open" and r["n"] >= 2' 30; then
  mark="$(field "$found" 'r["seq"]')"
  expect sse.resync 'http and route in ("session.list", "server.info", "project.list")' 30
fi
mark_now
control '{"action":"restart","down_ms":1500}'
if expect restart.reconnect 'r["kind"] == "sse.open"' 40; then
  mark="$(field "$found" 'r["seq"]')"
  expect restart.resync 'http and route in ("session.list", "server.info", "project.list")' 30
fi

expect_none no.unknown-routes 'http and route in (None, "method.not_allowed")'
expect_none no.bad-auth 'http and r["auth"] != "ok"'
expect_none no.agent 'http and route in ("session.prompt", "session.create") and any(k in b["keys"] for k in ("agent", "agents"))'
expect_none no.order-with-cursor 'http and route == "message.list" and "cursor" in q and "order" in q'
expect_none no.plain-directory 'http and route in ("model.list", "model.default", "permission.request.list", "form.list", "shell.list") and "directory" in q'
expect_none no.form-answer 'http and route == "session.form.reply"'
expect_none no.server-errors 'http and r["status"] >= 500'
expect_none no.prompt-resume 'http and route == "session.prompt" and "resume" in b["keys"]'
expect_none no.interrupt-resume 'http and route == "session.interrupt" and q.get("resume") == "true"'
expect_none no.steer-delivery 'http and route == "session.prompt" and b.get("delivery") not in (None, "queue")'

if python3 - "$temporary/config/opencode-gpui/state.json" "$address" "$new_session" <<'PY'
import json, sys
path, server, new = sys.argv[1:]
state = json.load(open(path, encoding='utf-8'))
tabs = {tab['id']: tab for tab in state['servers'][server]['tabs']}
assert 'ses_0000000000v1StaleTab000001' not in tabs, 'stale v1 tab not removed'
assert tabs['ses_f90000000001ffeIntegration']['title'] == 'Renamed integration session', 'rename not persisted'
assert tabs['ses_f90000000002ffeSecondSessn']['title'] == 'Raced title', 'bootstrap race lost'
assert new and new in tabs, 'created tab not persisted'
PY
then pass state.persisted; else fail state.persisted 'stale, race, or created tab discrepancy'; fi
alive && pass ui.alive || fail ui.alive 'client exited'
if grep -qi panicked "$app_log"; then fail ui.no-panic 'panic in log'; else pass ui.no-panic; fi
printf '\n%d passed, %d failed\n' "${#passes[@]}" "${#failures[@]}"
if ((${#failures[@]})); then
  tail -n 40 "$app_log" >&2
  tail -n 20 "$log" >&2
  exit 1
fi
