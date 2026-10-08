#!/usr/bin/env bash
# Session-isolated attachment drafts, picker/chips and overlapping prompt sends.
# Run only inside the amd64 headless builder container. Requires python3-dbus,
# python3-gi and dbus-run-session; ATTACHMENT_BINARY skips a build. Example:
# docker run --rm --platform linux/amd64 -v "$PWD":/repo:ro -v "$CACHE":/cache \
#   -w /repo -e ATTACHMENT_BINARY=/cache/target/debug/opencode-gpui \
#   opencode-gpui-builder-amd64:latest bash -lc \
#   'apt-get update -qq && apt-get install -y -qq --no-install-recommends \
#    python3-dbus python3-gi >/dev/null && bash tests/attachment-ui.sh'
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if [[ -z "${ATTACHMENT_IN_DBUS:-}" ]]; then
  exec env ATTACHMENT_IN_DBUS=1 dbus-run-session -- bash "$0" "$@"
fi
/usr/bin/python3 -c 'import dbus, gi' || { echo 'Install python3-dbus python3-gi in the container' >&2; exit 1; }
temporary="$(mktemp -d)"
source tests/gpui-headless.sh
app_pid='' server_pid='' portal_pid='' weston_pid='' xvfb_pid='' window=''
cleanup() {
  for pid in "$app_pid" "$server_pid" "$portal_pid"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  gpui_stop_display
  if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then echo "Kept $temporary"; else rm -rf "$temporary"; fi
}
trap cleanup EXIT
fail() {
  echo "FAIL $*" >&2
  if [[ -n "$window" && -n "${ATTACHMENT_KEEP:-}" ]]; then
    import -window "$window" "$temporary/fail.png" 2>/dev/null || true
  fi
  tail -35 "$temporary/app.log" >&2 || true
  exit 1
}
logq() { python3 tests/fake_v2/logwait.py "$@"; }
wait_http() { logq wait "$temporary/requests.jsonl" --after "${mark:-0}" --timeout 25 --expr "$1" >/dev/null || fail "$2"; }
portal_calls() { [[ -f "$temporary/portal.jsonl" ]] && awk 'END { print NR }' "$temporary/portal.jsonl" || echo 0; }
wait_picker() {
  local expected="$1"
  for _ in {1..100}; do [[ "$(portal_calls)" -ge "$expected" ]] && { sleep 0.8; return; }; sleep 0.1; done
  fail "picker call $expected did not arrive"
}
wait_picker_call() {
  local expected="$1"
  for _ in {1..100}; do [[ "$(portal_calls)" -ge "$expected" ]] && return; sleep 0.1; done
  fail "picker call $expected did not arrive"
}
select_tab() {
  local y="$1" red
  for _ in {1..8}; do
    gpui_click 92 "$y"
    sleep 0.4
    import -window "$window" "$temporary/tab-check.png"
    red="$(convert "$temporary/tab-check.png" -format "%[fx:p{12,$y}.r*255]" info:)"
    if awk -v red="$red" 'BEGIN { exit !(red < 235) }'; then return; fi
  done
  fail "session tab at y=$y did not become active"
}
main=ses_f90000000001ffeIntegration
other=ses_f90000000002ffeSecondSessn
mkdir -p "$temporary"/{home,config/opencode-gpui,data,cache,files}
export HOME="$temporary/home" XDG_CONFIG_HOME="$temporary/config" XDG_DATA_HOME="$temporary/data"
export XDG_CACHE_HOME="$temporary/cache" GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1
export XDG_CURRENT_DESKTOP=TEST
printf 'alpha attachment marker\n' > "$temporary/files/alpha.txt"
printf 'beta attachment marker\n' > "$temporary/files/beta.txt"
python3 - "$temporary" <<'PY'
import json, sys
from pathlib import Path
root = Path(sys.argv[1])
a, b = (str(root / 'files' / name) for name in ('alpha.txt', 'beta.txt'))
(root / 'selections.json').write_text(json.dumps([[a], {'paths': [a], 'delay_ms': 1800}, [b], [a]]), encoding='utf-8')
PY
gpui_start_display
password="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
FAKE_OPENCODE_PASSWORD="$password" python3 tests/fake_opencode_server.py \
  --address-file "$temporary/address" --log-file "$temporary/requests.jsonl" \
  --workspace /state/workspace --other-directory /state/other --cwd /state/home \
  --delay session.prompt=4000 >"$temporary/server.log" 2>&1 & server_pid=$!
for _ in {1..100}; do [[ -s "$temporary/address" ]] && break; sleep 0.1; done
[[ -s "$temporary/address" ]] || fail 'fake server startup'
address="$(<"$temporary/address")"
/usr/bin/python3 tests/fake_file_portal.py "$temporary/selections.json" \
  "$temporary/portal.jsonl" "$temporary/portal.ready" >"$temporary/portal.log" 2>&1 & portal_pid=$!
for _ in {1..100}; do [[ -s "$temporary/portal.ready" ]] && break; sleep 0.1; done
[[ -s "$temporary/portal.ready" ]] || fail 'fake portal startup'
python3 - "$address" "$temporary/config/opencode-gpui/state.json" "$main" "$other" <<'PY'
import json, sys
address, path, main, other = sys.argv[1:]
json.dump({'connection': {'server': address, 'username': 'opencode'},
           'servers': {address: {'tabs': [
               {'id': main, 'directory': '/state/workspace', 'title': 'Integration session'},
               {'id': other, 'directory': '/state/other', 'title': 'Second session'}],
               'active': main}}}, open(path, 'w', encoding='utf-8'))
PY
gpui_binary "${ATTACHMENT_BINARY:-}" || fail 'client build'
OPENCODE_SERVER_PASSWORD="$password" "$binary" --server "$address" --username opencode \
  >"$temporary/app.log" 2>&1 & app_pid=$!
gpui_wait_window || fail 'GPUI window startup'
mark=0
wait_http "http and route == 'message.list' and p.get('sessionID') == '$main' and r['status'] == 200" 'main history'
wait_http "http and route == 'model.list' and r['status'] == 200" 'model catalog'

# Add one file, select it again, and assert the duplicate is removed by the
# client (only one file appears in the eventual POST). No seeded attachments
# are used here: both selection and chip state are exercised through input.
gpui_click 308 747
wait_picker 1
if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then import -window "$window" "$temporary/main-chip.png"; fi
gpui_click 308 747
wait_picker_call 2
echo 'PASS file picker opened twice on the main session'
[[ "$(portal_calls)" == 2 ]] || fail 'unexpected picker calls'

# The second alpha selection resolves *after* switching sessions. The picker
# must retain its originating session, and the second session gets beta only.
select_tab 149
gpui_click 308 747
wait_picker 3
if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then import -window "$window" "$temporary/other-chip.png"; fi
sleep 1.5
python3 - "$temporary/portal.jsonl" <<'PY' || fail 'picker request shape or selection order'
import json, sys
records = [json.loads(line) for line in open(sys.argv[1], encoding='utf-8')]
assert len(records) == 3 and all(record['multiple'] for record in records)
assert [record['paths'][0].rsplit('/', 1)[-1] for record in records] == [
    'alpha.txt', 'alpha.txt', 'beta.txt']
PY
echo 'PASS second-session picker selection'
select_tab 110
gpui_click 410 650
gpui_type 'ALPHA_SEND'
mark="$(logq seq "$temporary/requests.jsonl")"
gpui_key Return
select_tab 149
gpui_click 410 650
gpui_type 'BETA_SEND'
if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then import -window "$window" "$temporary/beta-before-send.png"; fi
logq none "$temporary/requests.jsonl" --after "$mark" \
  --expr "http and route == 'session.prompt' and p.get('sessionID') == '$main' and r['status'] == 200" \
  >/dev/null || fail 'main prompt settled before overlapping second send'
gpui_key Return
if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then import -window "$window" "$temporary/beta-after-send.png"; fi
wait_http "http and route == 'session.prompt' and p.get('sessionID') == '$main' and b.get('text') == 'ALPHA_SEND' and len(b.get('files', [])) == 1 and b['files'][0]['name'] == 'alpha.txt' and b['files'][0]['bytes'] == 24 and b['files'][0]['canonical'] and r['status'] == 200" 'main isolated prompt'
wait_http "http and route == 'session.prompt' and p.get('sessionID') == '$other' and b.get('text') == 'BETA_SEND' and len(b.get('files', [])) == 1 and b['files'][0]['name'] == 'beta.txt' and b['files'][0]['canonical'] and r['status'] == 200" 'other isolated prompt'
echo 'PASS overlapping sends preserve session/text/file ownership'
[[ "$(logq count "$temporary/requests.jsonl" --expr "http and route == 'session.prompt'")" == 2 ]] || fail 'extra prompt request'
sleep 1  # The HTTP log precedes the response reaching the client.
select_tab 110
gpui_click 410 650
gpui_key Return
select_tab 149
gpui_click 410 650
gpui_key Return
sleep 2
[[ "$(logq count "$temporary/requests.jsonl" --expr "http and route == 'session.prompt'")" == 2 ]] \
  || fail 'accepted draft remained after switching sessions'
echo 'PASS accepted chips and text cleared in both sessions'

# Reattach, remove the visible chip, and send text alone. A stale draft path
# would be visible to the server as an unexpected `files` property.
select_tab 110
gpui_click 308 747
wait_picker 4
if [[ -n "${ATTACHMENT_KEEP:-}" ]]; then import -window "$window" "$temporary/remove-chip.png"; fi
gpui_click 374 714
gpui_click 410 650
gpui_type 'REMOVED_CHIP'
mark="$(logq seq "$temporary/requests.jsonl")"
gpui_key Return
wait_http "http and route == 'session.prompt' and p.get('sessionID') == '$main' and b.get('text') == 'REMOVED_CHIP' and 'files' not in b['keys'] and r['status'] == 200" 'removable attachment chip'
echo 'PASS removed chip is excluded from the prompt payload'
logq none "$temporary/requests.jsonl" --expr "http and r['status'] >= 400" \
  >/dev/null || fail 'fake server recorded an HTTP error'
kill -0 "$app_pid" || fail 'client exited'
echo 'PASS client alive; two isolated sends and one text-only send'
