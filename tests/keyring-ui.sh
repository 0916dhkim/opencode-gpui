#!/usr/bin/env bash
# Headless check that the OpenCode password saved in Settings lives in a real Secret Service
# (gnome-keyring) and survives app and keyring-daemon restarts, and that the client still starts
# and connects when no Secret Service is running. Run it only headless (never on a live desktop);
# the GPUI builder image includes gnome-keyring-daemon and secret-tool:
#   docker run --rm --platform linux/amd64 -v "$PWD":/app -w /app \
#     opencode-gpui-builder-amd64:latest bash tests/keyring-ui.sh
# The script starts isolated Weston/Xvfb; without a session bus it re-runs under
# dbus-run-session. KEYRING_BINARY=path skips the build; KEYRING_TIMEOUT=15 bounds each wait.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${KEYRING_IN_DBUS:-}" ]] && command -v dbus-run-session >/dev/null; then
  KEYRING_IN_DBUS=1 exec dbus-run-session -- bash "$0" "$@"
fi

for tool in gnome-keyring-daemon secret-tool xdotool; do
  command -v "${tool}" >/dev/null || { printf 'SKIP: %s is not installed (see the header)\n' "${tool}" >&2; exit 77; }
done

timeout_s="${KEYRING_TIMEOUT:-15}"
temporary="$(mktemp -d)"
log="${temporary}/requests.jsonl"
app_log="${temporary}/app.log"
state="${temporary}/config/opencode-gpui/state.json"
server_pid=""
app_pid=""
xvfb_pid=""
weston_pid=""
window=""
mark=0
failures=()
passes=()

cleanup() {
  [[ -z "${app_pid}" ]] || kill "${app_pid}" 2>/dev/null || true
  [[ -z "${server_pid}" ]] || kill "${server_pid}" 2>/dev/null || true
  [[ -z "${app_pid}" ]] || wait "${app_pid}" 2>/dev/null || true
  [[ -z "${server_pid}" ]] || wait "${server_pid}" 2>/dev/null || true
  stop_keyring
  gpui_stop_display
  rm -rf "${temporary}"
}
trap cleanup EXIT
source tests/gpui-headless.sh

logq() { python3 tests/fake_v2/logwait.py "$@"; }
mark_now() { mark="$(logq seq "${log}")"; }
pass() { passes+=("$1"); printf 'PASS %s\n' "$1"; }
fail() { failures+=("$1"); printf 'FAIL %s: %s\n' "$1" "$2" >&2; }
app_alive() { [[ -n "${app_pid}" ]] && kill -0 "${app_pid}" 2>/dev/null; }
check() { if eval "$2"; then pass "$1"; else fail "$1" "$2"; fi; }

expect() {
  local name="$1" expr="$2"
  app_alive || { fail "${name}" "client is not running"; return 1; }
  if logq wait "${log}" --after "${mark}" --timeout "${timeout_s}" --expr "${expr}" >/dev/null; then
    pass "${name}"
  else
    fail "${name}" "no matching request within ${timeout_s}s: ${expr}"
  fi
}

expect_none_since_mark() {
  local name="$1" expr="$2" offending
  if offending="$(logq none "${log}" --after "${mark}" --expr "${expr}")"; then
    pass "${name}"
  else
    fail "${name}" "unexpected requests:"$'\n'"${offending}"
  fi
}

key() { gpui_key "$@"; }
type_text() { gpui_type "$1"; }

keyring_home="${temporary}/keyring-home"
start_keyring() {
  mkdir -p "${keyring_home}"
  # --unlock creates (or opens) the login keyring with this password and makes it the default.
  eval "$(printf 'headless' | HOME="${keyring_home}" XDG_DATA_HOME="${keyring_home}/data" \
    gnome-keyring-daemon --unlock --components=secrets)"
  for _ in $(seq 1 50); do
    secret-tool search --all probe none >/dev/null 2>&1 && return 0
    sleep 0.1
  done
}
stop_keyring() {
  if [[ -n "${GNOME_KEYRING_PID:-}" ]]; then
    kill "$GNOME_KEYRING_PID" 2>/dev/null || true
    unset GNOME_KEYRING_PID
  fi
  for _ in $(seq 1 50); do
    pgrep -u "$(id -u)" -f gnome-keyring-daemon >/dev/null || return 0
    sleep 0.1
  done
}

write_state() {
  mkdir -p "$(dirname "${state}")"
  python3 - "${address}" "${state}" "$1" <<'PY'
import json, sys
server, path, flag = sys.argv[1:]
state = {
    "connection": {"server": server, "username": "opencode", "cloudflare_access": False,
                   "basic_auth_in_keyring": flag == "true"},
    "servers": {},
    "zoom_level": 1.0,
}
with open(path, "w", encoding="utf-8") as stream:
    json.dump(state, stream)
PY
}

state_flag() {
  python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["connection"].get("basic_auth_in_keyring")).lower())' "${state}"
}

state_zoom() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["zoom_level"])' "${state}"
}

wait_state_flag() {
  local expected="$1" attempt
  # A fresh ApiHandle can authenticate before the synchronous Settings Apply
  # callback finishes writing state. Never kill that client immediately on
  # the first successful server request.
  for ((attempt = 0; attempt < timeout_s * 10; attempt++)); do
    if [[ -s "${state}" ]] && [[ "$(state_flag)" == "${expected}" ]]; then return 0; fi
    sleep 0.1
  done
  return 1
}

wait_keyring_removed() {
  local attempt
  for ((attempt = 0; attempt < timeout_s * 10; attempt++)); do
    [[ -z "$(stored_entry)" ]] && return 0
    sleep 0.1
  done
  return 1
}

# launch [ENV=VALUE...] -- starts the client (no password unless given) and waits for its window.
launch() {
  mark_now
  env XDG_CONFIG_HOME="${temporary}/config" \
    XDG_DATA_HOME="${temporary}/data" \
    XDG_CACHE_HOME="${temporary}/cache" \
    GSETTINGS_BACKEND=memory \
    NO_AT_BRIDGE=1 \
    "$@" \
    "${binary}" >>"${app_log}" 2>&1 &
  app_pid=$!
  gpui_wait_window || { fail "window" "no GPUI window"; cat "${app_log}" >&2; exit 1; }
}

# State is written on Apply; each launch is a new process in the same private home.
quit() {
  kill "${app_pid}" 2>/dev/null || true
  wait "${app_pid}" 2>/dev/null || true
  app_pid=""
}

open_password_field() {
  gpui_click 410 680
  key ctrl+comma
  sleep 0.4
  gpui_click 440 367
}

stored_entry() {
  local stored
  stored="$(secret-tool lookup service ai.opencode.Gpui.basic-auth username "${account}" 2>/dev/null)"
  [[ "$stored" == '{"version":1,"removed":true}' ]] || printf '%s' "$stored"
}

# ------------------------------------------------------------ environment

gpui_start_display || exit 1

password="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
FAKE_OPENCODE_PASSWORD="${password}" python3 tests/fake_opencode_server.py \
  --address-file "${temporary}/address" \
  --log-file "${log}" \
  --heartbeat-s 2 \
  >"${temporary}/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do [[ -s "${temporary}/address" ]] && break; sleep 0.1; done
[[ -s "${temporary}/address" ]] || { printf 'fake server did not start\n' >&2; cat "${temporary}/server.log" >&2; exit 1; }
address="$(<"${temporary}/address")"
account="http://opencode@${address#http://}"
account="${account%/}"

gpui_binary "${KEYRING_BINARY:-}" || exit 1

auth_route="http and route == 'server.info'"

# ------------------------------------------------------------ 1. no Secret Service: fall back
#
# gnome-keyring is D-Bus activatable once installed, so this run gets no session bus at all.

stop_keyring
write_state true
launch OPENCODE_SERVER_URL="${address}" DBUS_SESSION_BUS_ADDRESS="unix:path=${temporary}/no-bus"
expect "fallback.connects-without-keyring" "${auth_route} and r['auth'] == 'missing'"
sleep 1
check "fallback.alive" app_alive
quit
check "fallback.state-flag-kept" '[[ "$(state_flag)" == true ]]'

# ------------------------------------------------------------ 2. save the password in Settings

start_keyring
write_state false
launch OPENCODE_SERVER_URL="${address}"
expect "save.unauthenticated-before" "${auth_route} and r['auth'] == 'missing'"
open_password_field
mark_now
type_text "${password}"
gpui_click 760 727
expect "save.connects" "http and r['auth'] == 'ok'"
check "save.state-flag" 'wait_state_flag true'
check "save.keyring-entry" '[[ "$(stored_entry)" == "{\"version\":1,\"password\":\"${password}\"}" ]]'
quit

# ------------------------------------------------------------ 3. restart app and keyring daemon

stop_keyring
start_keyring
launch OPENCODE_SERVER_URL="${address}"
expect "restart.connects-with-stored-password" "${auth_route} and r['auth'] == 'ok'"
sleep 1
expect_none_since_mark "restart.no-unauthenticated-requests" "http and r['auth'] != 'ok'"

# ------------------------------------------------------------ 4. uncheck Remember to forget it

open_password_field
gpui_click 310 404
mark_now
gpui_click 760 727
sleep 1
check "forget.keyring-entry-removed" 'wait_keyring_removed'
check "forget.state-flag" 'wait_state_flag false'
check "forget.still-connected" app_alive
quit
launch OPENCODE_SERVER_URL="${address}"
expect "forget.restart-has-no-password" "${auth_route} and r['auth'] == 'missing'"
quit

# ------------------------------------------------------------ 5. an environment password is not saved

launch OPENCODE_SERVER_URL="${address}" OPENCODE_SERVER_PASSWORD="${password}"
expect "env.connects" "${auth_route} and r['auth'] == 'ok'"
open_password_field
gpui_click 760 727
sleep 1
quit
check "env.not-saved" '[[ -z "$(stored_entry)" ]]'
check "env.state-flag" '[[ "$(state_flag)" == false ]]'

# ------------------------------------------------------------ 6. copy legacy state and password

# This is a separate first-launch migration fixture, not a forgotten GPUI
# credential from the preceding scenario.
secret-tool clear service ai.opencode.Gpui.basic-auth username "${account}" >/dev/null
legacy_state="${temporary}/config/opencode-cosmic/state.json"
gpui_state="${state}"
state="${legacy_state}"
write_state true
state="${gpui_state}"
legacy_hash="$(sha256sum "${legacy_state}" | cut -d' ' -f1)"
rm -f "${gpui_state}"
printf '%s' "{\"version\":1,\"password\":\"${password}\"}" |
  secret-tool store --label='Legacy OpenCode Basic password' \
    service ai.opencode.Cosmic.basic-auth username "${account}"
launch OPENCODE_SERVER_URL="${address}"
expect "migration.connects-with-legacy-password" "${auth_route} and r['auth'] == 'ok'"
check "migration.state-copied" 'wait_state_flag true'
check "migration.new-keyring-entry" '[[ "$(stored_entry)" == "{\"version\":1,\"password\":\"${password}\"}" ]]'
check "migration.old-keyring-entry-retained" '[[ "$(secret-tool lookup service ai.opencode.Cosmic.basic-auth username "${account}")" == "$(stored_entry)" ]]'
check "migration.old-state-untouched" '[[ "$(sha256sum "${legacy_state}" | cut -d" " -f1)" == "${legacy_hash}" ]]'
open_password_field
gpui_click 310 404
gpui_click 760 727
check "migration.forget-state-flag" 'wait_state_flag false'
check "migration.forget-marker" '[[ "$(secret-tool lookup service ai.opencode.Gpui.basic-auth username "${account}")" == "{\"version\":1,\"removed\":true}" ]]'
check "migration.forget-retains-old-entry" '[[ "$(secret-tool lookup service ai.opencode.Cosmic.basic-auth username "${account}")" == "{\"version\":1,\"password\":\"${password}\"}" ]]'
quit

# ------------------------------------------------------------ 7. newer GTK state and keyring

secret-tool clear service ai.opencode.Gpui.basic-auth username "${account}" >/dev/null
printf '%s' '{"version":1,"password":"obsolete-cosmic-password"}' |
  secret-tool store --label='Older Cosmic password' \
    service ai.opencode.Cosmic.basic-auth username "${account}"
gtk_state="${temporary}/config/opencode-gtk/state.json"
state="${gtk_state}"
write_state true
python3 - "${gtk_state}" <<'PY'
import json, sys
path = sys.argv[1]
with open(path, encoding='utf-8') as stream: state = json.load(stream)
state['zoom_level'] = 1.15
with open(path, 'w', encoding='utf-8') as stream: json.dump(state, stream)
PY
state="${gpui_state}"
gtk_hash="$(sha256sum "${gtk_state}" | cut -d' ' -f1)"
rm -f "${gpui_state}"
printf '%s' "{\"version\":1,\"password\":\"${password}\"}" |
  secret-tool store --label='GTK OpenCode Basic password' \
    service ai.opencode.Gtk.basic-auth username "${account}"
launch OPENCODE_SERVER_URL="${address}"
expect "gtk-migration.connects-with-gtk-password" "${auth_route} and r['auth'] == 'ok'"
check "gtk-migration.newer-state-copied" '[[ "$(state_zoom)" == 1.15 ]]'
check "gtk-migration.gpui-keyring-entry" '[[ "$(stored_entry)" == "{\"version\":1,\"password\":\"${password}\"}" ]]'
check "gtk-migration.gtk-keyring-retained" '[[ "$(secret-tool lookup service ai.opencode.Gtk.basic-auth username "${account}")" == "$(stored_entry)" ]]'
check "gtk-migration.gtk-state-untouched" '[[ "$(sha256sum "${gtk_state}" | cut -d" " -f1)" == "${gtk_hash}" ]]'
quit

# ------------------------------------------------------------ secrets stay out of files and logs

check "secret.not-in-state" '! grep -qF "${password}" "${state}"'
check "secret.not-in-app-log" '! grep -qF "${password}" "${app_log}"'
check "secret.not-in-request-log" '! grep -qF "${password}" "${log}"'
check "no-panic" '! grep -qi panicked "${app_log}"'

printf '\n%d passed, %d failed\n' "${#passes[@]}" "${#failures[@]}"
if ((${#failures[@]})); then
  printf 'App log:\n' >&2
  tail -n 40 "${app_log}" >&2
  exit 1
fi
