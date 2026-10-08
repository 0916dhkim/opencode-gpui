#!/usr/bin/env bash
# Real Wayland IME integration test: ASCII keys -> Fcitx5 Hangul preedit/commit
# -> GPUI composer -> authenticated fake-server prompt. Only run headlessly.
#
# In the builder image (extra packages are installed only in this container):
# docker run --rm --platform linux/amd64 -v "$PWD":/repo:ro -w /repo \
#   opencode-gpui-builder-amd64:latest bash -lc \
#   'apt-get update -qq && apt-get install -y -qq --no-install-recommends \
#      sway fcitx5 fcitx5-hangul dbus-x11 wayland-utils fonts-noto-cjk \
#      >/dev/null && bash tests/ime-ui.sh'
# IME_BINARY overrides target/debug/opencode-gpui (build it separately).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# wlroots/Sway refuses to run as root. Drop privileges before creating state or
# opening any displays; Docker's read-only repository mount remains readable.
if [[ "$(id -u)" == 0 ]]; then
  command -v runuser >/dev/null || { echo 'FAIL missing runuser' >&2; exit 1; }
  exec runuser -u nobody -- bash "$0" "$@"
fi

# Even when the caller has a bus, isolate the IME and the client on a new one.
if [[ -z "${IME_PRIVATE_BUS:-}" ]]; then
  command -v dbus-run-session >/dev/null \
    || { echo 'FAIL missing dbus-run-session (see Docker command above)' >&2; exit 1; }
  IME_TEST_TEMP="$(mktemp -d)"
  mkdir -p "$IME_TEST_TEMP"/{home,config,data,cache,runtime}
  chmod 700 "$IME_TEST_TEMP/runtime"
  export IME_TEST_TEMP HOME="$IME_TEST_TEMP/home"
  export XDG_CONFIG_HOME="$IME_TEST_TEMP/config" XDG_DATA_HOME="$IME_TEST_TEMP/data"
  export XDG_CACHE_HOME="$IME_TEST_TEMP/cache" XDG_RUNTIME_DIR="$IME_TEST_TEMP/runtime"
  export GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1
  trap 'rm -rf -- "$IME_TEST_TEMP"' EXIT
  if IME_PRIVATE_BUS=1 dbus-run-session -- bash "$0" "$@" \
      2>"$IME_TEST_TEMP/session.stderr"; then
    exit 0
  else
    status=$?
    # D-Bus prints command lines on activation; show only this script's
    # deliberately non-secret failure labels, never its raw daemon log.
    grep '^FAIL ' "$IME_TEST_TEMP/session.stderr" >&2 \
      || echo 'FAIL IME test exited unexpectedly' >&2
    exit "$status"
  fi
fi

fail() { printf 'FAIL %s\n' "$*" >&2; exit 1; }
for tool in Xvfb sway wayland-info fcitx5 fcitx5-remote dbus-send xdotool xwininfo \
            python3 timeout sha256sum; do
  command -v "$tool" >/dev/null || fail "missing $tool (see Docker command above)"
done

binary="${IME_BINARY:-target/debug/opencode-gpui}"
[[ -x "$binary" ]] || fail "missing executable GPUI binary (build it or set IME_BINARY)"
temporary="${IME_TEST_TEMP:?private test directory was not initialized}"
app_pid='' server_pid='' fcitx_pid='' sway_pid='' xvfb_pid=''
cleanup() {
  local pid
  for pid in "$app_pid" "$fcitx_pid" "$server_pid" "$sway_pid" "$xvfb_pid"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
}
trap cleanup EXIT

export LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
export XMODIFIERS=@im=fcitx GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx
# Sway names the socket wayland-1 inside this unique private runtime dir.
export WAYLAND_DISPLAY=wayland-1

# Pin the executable for this run, avoiding races with a concurrent cargo build.
cp -- "$binary" "$temporary/client"
chmod +x "$temporary/client"
binary_hash="$(sha256sum "$temporary/client" | cut -c1-16)"

for display in $(seq 121 180); do
  [[ -e /tmp/.X11-unix/X$display || -e /tmp/.X$display-lock ]] && continue
  export DISPLAY=:$display
  Xvfb "$DISPLAY" -screen 0 1280x900x24 -nolisten tcp >"$temporary/xvfb.log" 2>&1 &
  xvfb_pid=$!
  for _ in $(seq 1 50); do
    if xdotool getdisplaygeometry >/dev/null 2>&1; then break; fi
    kill -0 "$xvfb_pid" 2>/dev/null || break
    sleep 0.1
  done
  if xdotool getdisplaygeometry >/dev/null 2>&1; then break; fi
  kill "$xvfb_pid" 2>/dev/null || true
  wait "$xvfb_pid" 2>/dev/null || true
  xvfb_pid=''
done
[[ -n "$xvfb_pid" ]] || fail 'Xvfb did not start on a private display'

mkdir -p "$XDG_CONFIG_HOME/sway" "$XDG_CONFIG_HOME/fcitx5"
cat >"$XDG_CONFIG_HOME/sway/config" <<'CFG'
xwayland disable
output * resolution 780x791
input * xkb_layout us
focus_follows_mouse yes
CFG
cat >"$XDG_CONFIG_HOME/fcitx5/profile" <<'CFG'
[Groups/0]
Name=Default
Default Layout=us
DefaultIM=hangul

[Groups/0/Items/0]
Name=keyboard-us
Layout=

[Groups/0/Items/1]
Name=hangul
Layout=

[GroupOrder]
0=Default
CFG

WLR_BACKENDS=x11 WLR_RENDERER=pixman WLR_X11_OUTPUTS=1 \
  sway -c "$XDG_CONFIG_HOME/sway/config" >"$temporary/sway.log" 2>&1 &
sway_pid=$!
for _ in $(seq 1 150); do
  [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] && break
  kill -0 "$sway_pid" 2>/dev/null || fail 'Sway exited before creating its Wayland socket'
  sleep 0.1
done
[[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] || fail 'Sway socket timed out'
timeout 5 wayland-info >"$temporary/globals" 2>/dev/null || fail 'could not inspect Sway globals'
grep -q "interface: 'zwp_text_input_manager_v3'" "$temporary/globals" || fail 'Sway lacks text-input-v3'
grep -q "interface: 'zwp_input_method_manager_v2'" "$temporary/globals" || fail 'Sway lacks input-method-v2'
echo 'PASS private Sway advertises text-input-v3 and input-method-v2'

WAYLAND_DEBUG=client fcitx5 >"$temporary/fcitx.log" 2>&1 &
fcitx_pid=$!
for _ in $(seq 1 100); do
  # fcitx5-remote auto-starts its own daemon when called too early. Inspect
  # ownership through the bus itself instead, avoiding a second IME process.
  if dbus-send --session --print-reply --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner \
      string:org.fcitx.Fcitx5 2>/dev/null | grep -q 'boolean true'; then break; fi
  kill -0 "$fcitx_pid" 2>/dev/null || fail 'Fcitx5 exited during startup'
  sleep 0.1
done
kill -0 "$fcitx_pid" 2>/dev/null || fail 'Fcitx5 lost its D-Bus name'
dbus-send --session --print-reply --dest=org.freedesktop.DBus \
  /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner \
  string:org.fcitx.Fcitx5 2>/dev/null | grep -q 'boolean true' \
  || fail 'Fcitx5 D-Bus registration timed out'
password="$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
FAKE_OPENCODE_PASSWORD="$password" python3 tests/fake_opencode_server.py \
  --address-file "$temporary/address" --log-file "$temporary/requests.jsonl" \
  >"$temporary/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  [[ -s "$temporary/address" ]] && break
  kill -0 "$server_pid" 2>/dev/null || fail 'fake server exited during startup'
  sleep 0.1
done
[[ -s "$temporary/address" ]] || fail 'fake server address timed out'
address="$(<"$temporary/address")"
OPENCODE_SERVER_PASSWORD="$password" WAYLAND_DEBUG=client \
  "$temporary/client" --server "$address" --username opencode \
  >"$temporary/app.log" 2>&1 &
app_pid=$!
unset password

python3 tests/fake_v2/logwait.py wait "$temporary/requests.jsonl" --timeout 30 \
  --expr "http and route == 'message.list' and r['status'] == 200" >/dev/null \
  || fail 'GPUI did not load the fake session'
python3 tests/fake_v2/logwait.py wait "$temporary/requests.jsonl" --timeout 30 \
  --expr "http and route == 'model.list' and r['status'] == 200" >/dev/null \
  || fail 'GPUI did not load models'

window=''
for _ in $(seq 1 120); do
  window="$(xwininfo -root -tree 2>/dev/null | awk '/780x791[+]/{print $1; exit}')"
  [[ -n "$window" ]] && break
  kill -0 "$app_pid" 2>/dev/null || fail 'GPUI exited before Sway window was ready'
  sleep 0.2
done
[[ -n "$window" ]] || fail 'Sway X window timed out'
timeout 5 xdotool windowfocus --sync "$window" >/dev/null || fail 'could not focus nested Sway window'
[[ "$(xdotool getwindowfocus)" == "$((window))" ]] || fail 'nested Sway window was not focused'

# The X11 keys below are ASCII only. Actual Hangul must come from the IME's
# Wayland preedit/commit events, never from an injected Unicode key or paste.
sleep 2  # X window can exist before the GPUI surface is ready for input.
line_before="$(wc -l <"$temporary/app.log")"
xdotool mousemove --window "$window" 410 680 click 1
for _ in $(seq 1 50); do
  if tail -n "+$((line_before + 1))" "$temporary/app.log" \
      | grep -Eq 'zwp_text_input_v3@[0-9]+\.enable\('; then break; fi
  kill -0 "$app_pid" 2>/dev/null || fail 'GPUI exited after composer click'
  sleep 0.1
done
tail -n "+$((line_before + 1))" "$temporary/app.log" \
  | grep -Eq 'zwp_text_input_v3@[0-9]+\.enable\(' || fail 'composer did not enable text-input-v3'
for _ in $(seq 1 60); do
  timeout 5 fcitx5-remote -s hangul >/dev/null 2>&1 || true
  timeout 5 fcitx5-remote -o >/dev/null 2>&1 || true
  if [[ "$(timeout 5 fcitx5-remote -n 2>/dev/null || true)" == hangul \
     && "$(timeout 5 fcitx5-remote 2>/dev/null || true)" == 2 ]]; then break; fi
  kill -0 "$fcitx_pid" 2>/dev/null || fail 'Fcitx5 exited after composer focus'
  sleep 0.1
done
[[ "$(timeout 5 fcitx5-remote -n)" == hangul && "$(timeout 5 fcitx5-remote)" == 2 ]] \
  || fail 'Hangul IME was not active at composer focus'

timeout 10 xdotool type --clearmodifiers --delay 150 dkssud || fail 'ASCII key injection failed'
for _ in $(seq 1 60); do
  grep -Eq 'zwp_text_input_v3@[0-9]+\.preedit_string\("녕"' "$temporary/app.log" && break
  sleep 0.1
done
grep -Eq 'zwp_text_input_v3@[0-9]+\.preedit_string\("녕"' "$temporary/app.log" \
  || fail 'no real text-input-v3 Hangul preedit'
xdotool key space  # Commit the final Hangul syllable, then send the prompt.
for _ in $(seq 1 60); do
  grep -Eq 'zwp_text_input_v3@[0-9]+\.commit_string\("녕"\)' "$temporary/app.log" && break
  sleep 0.1
done
grep -Eq 'zwp_text_input_v3@[0-9]+\.commit_string\("안"\)' "$temporary/app.log" \
  || fail 'missing text-input-v3 commit for first syllable'
grep -Eq 'zwp_text_input_v3@[0-9]+\.commit_string\("녕"\)' "$temporary/app.log" \
  || fail 'missing text-input-v3 commit for second syllable'
grep -Eq 'zwp_input_method_v2@[0-9]+\.set_preedit_string\("녕"' "$temporary/fcitx.log" \
  || fail 'Fcitx5 did not produce Hangul preedit through input-method-v2'
echo 'PASS Fcitx5 preedit and GPUI text-input-v3 preedit+commit: 안녕'

xdotool key Return
python3 tests/fake_v2/logwait.py wait "$temporary/requests.jsonl" --timeout 20 \
  --expr "http and route == 'session.prompt' and r['status'] == 200 and b.get('text') == '안녕 '" \
  >/dev/null || fail "fake server did not receive HTTP 200 prompt text '안녕 '"
[[ "$(python3 tests/fake_v2/logwait.py count "$temporary/requests.jsonl" \
  --expr "http and route == 'session.prompt'")" == 1 ]] || fail 'unexpected extra prompt'
kill -0 "$app_pid" 2>/dev/null || fail 'GPUI exited after IME send'
echo "PASS HTTP 200 session.prompt text='안녕 ' (one request; binary sha256 $binary_hash…)"
