#!/usr/bin/env bash
# Headless keyboard-shortcut check for the GPUI client against the fake v2
# server (tests/fake_opencode_server.py). Keyboard effects are asserted from
# server requests or the app lifecycle, so this verifies the shortcuts end to end
# instead of only proving that the client survives them (tests/smoke-ui.sh).
#   Docker: docker run --rm --platform linux/amd64 -v "$PWD":/repo -w /repo \
#             opencode-gpui-builder-amd64:latest bash tests/shortcut-ui.sh
# Uses a private Xvfb and a nested Weston Wayland compositor. Env knobs:
#   SHORTCUT_BINARY=path   use a built client instead of `cargo build --locked`
#   SHORTCUT_TIMEOUT=15    seconds to wait for each request-log marker
#   SHORTCUT_KEEP=1        keep the temp dir (state, request log, app log)
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${SHORTCUT_IN_DBUS:-}" ]] && command -v dbus-run-session >/dev/null; then
  SHORTCUT_IN_DBUS=1 exec dbus-run-session -- bash "$0" "$@"
fi

timeout_s="${SHORTCUT_TIMEOUT:-15}"
temporary="$(mktemp -d)"
log="${temporary}/requests.jsonl"
app_log="${temporary}/app.log"
server_pid=""
app_pid=""
xvfb_pid=""
weston_pid=""
window=""
mark=0
passes=0
failures=0

cleanup() {
  [[ -z "${app_pid}" ]] || kill "${app_pid}" 2>/dev/null || true
  [[ -z "${server_pid}" ]] || kill "${server_pid}" 2>/dev/null || true
  [[ -z "${app_pid}" ]] || wait "${app_pid}" 2>/dev/null || true
  [[ -z "${server_pid}" ]] || wait "${server_pid}" 2>/dev/null || true
  gpui_stop_display
  if [[ -n "${SHORTCUT_KEEP:-}" ]]; then
    printf 'Kept %s\n' "${temporary}" >&2
  else
    rm -rf "${temporary}"
  fi
}
trap cleanup EXIT
source tests/gpui-headless.sh

pass() { passes=$((passes + 1)); printf 'PASS %s\n' "$1"; }
fail() { failures=$((failures + 1)); printf 'FAIL %s: %s\n' "$1" "$2" >&2; }
logq() { python3 tests/fake_v2/logwait.py "$@"; }

# expect NAME EXPR [TIMEOUT] -- waits for a matching request-log record.
found=""
expect() {
  local name="$1" expr="$2" wait="${3:-${timeout_s}}"
  if found="$(logq wait "${log}" --after "${mark}" --timeout "${wait}" --expr "${expr}")"; then
    pass "${name}"
  else
    found=""
    fail "${name}" "no matching record within ${wait}s: ${expr}"
  fi
}

# expect_count NAME EXPR N -- exactly N records must match (guards double sends).
expect_count() {
  local name="$1" expr="$2" want="$3" got
  got="$(logq count "${log}" --after "${mark}" --expr "${expr}")"
  if [[ "${got}" == "${want}" ]]; then
    pass "${name}"
  else
    fail "${name}" "expected ${want} matching record(s), got ${got}: ${expr}"
  fi
}

alive() { [[ -n "${app_pid}" ]] && kill -0 "${app_pid}" 2>/dev/null; }
expect_zoom() {
  local name="$1" wanted="$2" state="${temporary}/config/opencode-gpui/state.json"
  for _ in {1..30}; do
    if python3 - "$state" "$wanted" <<'PY' 2>/dev/null
import json, sys
with open(sys.argv[1]) as file:
    actual = json.load(file)['zoom_level']
assert abs(actual - float(sys.argv[2])) < 0.001
PY
    then
      pass "$name"
      return
    fi
    sleep 0.1
  done
  fail "$name" "saved zoom did not reach $wanted"
}

gpui_start_display || exit 1

password="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
FAKE_OPENCODE_PASSWORD="${password}" python3 tests/fake_opencode_server.py \
  --address-file "${temporary}/address" \
  --log-file "${log}" \
  --step-delay-ms 1500 \
  --slow-delay-ms 1500 \
  --slow-deltas 12 \
  >"${temporary}/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do [[ -s "${temporary}/address" ]] && break; sleep 0.1; done
if [[ ! -s "${temporary}/address" ]]; then
  printf 'fake server did not start\n' >&2
  cat "${temporary}/server.log" >&2
  exit 1
fi
address="$(<"${temporary}/address")"

gpui_binary "${SHORTCUT_BINARY:-}" || exit 1

mkdir -p "${temporary}/config" "${temporary}/data" "${temporary}/cache" "${temporary}/runtime"
chmod 700 "${temporary}/runtime"
XDG_CONFIG_HOME="${temporary}/config" \
XDG_DATA_HOME="${temporary}/data" \
XDG_CACHE_HOME="${temporary}/cache" \
XDG_RUNTIME_DIR="${temporary}/runtime" \
GSETTINGS_BACKEND=memory \
NO_AT_BRIDGE=1 OPENCODE_SERVER_PASSWORD="${password}" \
"${binary}" --server "${address}" --username opencode >"${app_log}" 2>&1 &
app_pid=$!

if ! gpui_wait_window; then
  cat "${app_log}" >&2
  printf 'no GPUI window\n' >&2
  exit 1
fi

key() { gpui_key "$@"; }
type_text() { gpui_type "$1"; }
mark_now() { mark="$(logq seq "${log}")"; }

# Wait for the initial transcript and model selection, not merely the first
# session-list request (which can precede the completed GPUI bootstrap).
logq wait "$log" --after 0 --timeout 30 --expr "http and route == 'message.list'" >/dev/null || {
  cat "$app_log" >&2; echo 'bootstrap did not load a session' >&2; exit 1;
}
logq wait "$log" --after 0 --timeout 30 --expr "http and route == 'model.list'" >/dev/null || {
  cat "$app_log" >&2; echo 'bootstrap did not load models' >&2; exit 1;
}
sleep 1
gpui_click 410 680

# ---------------------------------------------------------------- Ctrl+T
# Opens GPUI's new-session palette; Enter takes the first location and Ctrl+G
# then leaves the caret in the composer.
mark_now
key ctrl+t
key Return
expect "ctrl-t.create" "http and route == 'session.create' and r['status'] == 200"
sleep 1

# -------------------------------------------------- Enter sends a prompt
mark_now
key ctrl+g
gpui_click 410 680
type_text "shortcut note one"
key Return
expect "enter.prompt" \
  "http and route == 'session.prompt' and 'shortcut note one' in (b.get('text') or '') and 'delivery' not in b['keys'] and r['status'] == 200"
expect_count "enter.prompt-once" \
  "http and route == 'session.prompt' and 'shortcut note one' in (b.get('text') or '')" 1

# --------------------------- Enter steers and Ctrl+Enter queues while running
# The fake run is slow (--step-delay-ms 1500), so the session is still busy.
# Neither message re-focuses the composer: typing here also checks that the
# caret survives the tray filling up while the run streams.
mark_now
type_text "shortcut steer two"
key Return
expect "enter.steer" \
  "http and route == 'session.prompt' and 'shortcut steer two' in (b.get('text') or '') and 'delivery' not in b['keys']"
expect_count "enter.steer-once" \
  "http and route == 'session.prompt' and 'shortcut steer two' in (b.get('text') or '')" 1

mark_now
type_text "shortcut queue three"
key ctrl+Return
expect "ctrl-enter.queue" \
  "http and route == 'session.prompt' and 'shortcut queue three' in (b.get('text') or '') and b.get('delivery') == 'queue' and r['status'] == 200"
expect_count "ctrl-enter.queue-once" \
  "http and route == 'session.prompt' and 'shortcut queue three' in (b.get('text') or '')" 1

# The smoke suite exercises overlay shortcuts; this suite keeps the assertions
# that observe actual create/send/steer/queue requests instead of repeating
# alive-only input sequences.
if grep -qi panicked "${app_log}"; then
  cat "${app_log}" >&2
  fail "no-panic" "client logged a panic"
else
  pass "no-panic"
fi

expect_count "no.unexpected-prompts" \
  "http and route == 'session.prompt' and 'shortcut' not in (b.get('text') or '')" 0

key ctrl+equal
expect_zoom "ctrl-equal.zoom-in" 1.1
key ctrl+minus
expect_zoom "ctrl-minus.zoom-out" 1.0
key ctrl+equal
expect_zoom "ctrl-equal.zoom-again" 1.1
key ctrl+0
expect_zoom "ctrl-0.zoom-reset" 1.0

key ctrl+q
for _ in {1..30}; do
  alive || break
  sleep 0.1
done
if alive; then fail "ctrl-q.quit" "application remained running"; else pass "ctrl-q.quit"; fi

printf '\n%s passed, %s failed\n' "${passes}" "${failures}"
if ((failures)); then
  tail -n 40 "$app_log" >&2
  if [[ -n "${SHORTCUT_SHOTS:-}" ]]; then import -window "$window" "$SHORTCUT_SHOTS"; fi
fi
[[ "${failures}" -eq 0 ]]
