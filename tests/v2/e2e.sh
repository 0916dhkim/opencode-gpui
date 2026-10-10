#!/usr/bin/env bash
# End-to-end run of the client against a real, isolated OpenCode 2.0.8 server.
#
#   tests/v2/e2e.sh --state DIR [--shots DIR] [--build] [--skip-gui]
#
# 1. Brings up the harness (tests/v2/harness.sh; `--build` rebuilds its image).
# 2. API-level live test: `api::live_tests` (ignored by default) in the builder
#    image, joined to `ocgtk-v2h-net`, through a loopback forward.
# 3. GUI smoke on a fresh server: GPUI under nested Weston/Xvfb in the builder
#    (tests/v2/gui_smoke.sh), with a screenshot in --shots (default DIR/shots).
# 4. Always takes the harness down again.
#
# DIR must be outside the repository; it only holds the per-run password.
# Env knobs: E2E_BUILDER_IMAGE, E2E_UI_IMAGE, E2E_CARGO_VOLUME, E2E_TARGET_VOLUME,
# E2E_GUI_BINARY (an already built absolute path inside the UI image),
# E2E_GUI_CONFIG_SUFFIX (/api to test canonical tab-state migration), and
# OCGTK_V2H_PREFIX (harness container/network names; see harness.sh).
set -uo pipefail

export PATH="/usr/local/bin:$PATH"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BUILDER="${E2E_BUILDER_IMAGE:-opencode-gpui-builder-amd64:latest}"
UI_IMAGE="${E2E_UI_IMAGE:-opencode-gpui-builder-amd64:latest}"
CARGO_VOLUME="${E2E_CARGO_VOLUME:-opencode-gpui-e2e-cargo}"
TARGET_VOLUME="${E2E_TARGET_VOLUME:-opencode-gpui-e2e-target}"
PREFIX="${OCGTK_V2H_PREFIX:-ocgtk-v2h}"
NET="$PREFIX-net"
UPSTREAM="$PREFIX-server"

state="" shots="" build="" skip_gui=""
while [ $# -gt 0 ]; do
  case "$1" in
    --state) state="$2"; shift 2 ;;
    --shots) shots="$2"; shift 2 ;;
    --build) build=1; shift ;;
    --skip-gui) skip_gui=1; shift ;;
    *) echo "e2e: unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$state" ] || { echo "e2e: --state DIR is required" >&2; exit 2; }
mkdir -p "$state"
state="$(cd "$state" && pwd)"
shots="${shots:-$state/shots}"
mkdir -p "$shots"

results=()
record() { results+=("$1 $2"); printf '== %s: %s\n' "$2" "$1"; }

teardown() { "$HERE/harness.sh" down --state "$state" >/dev/null 2>&1 || true; }
trap teardown EXIT

if [ -n "$build" ] || ! docker image inspect ocgtk-v2h-server:2.0.8 >/dev/null 2>&1; then
  "$HERE/harness.sh" build || { echo "e2e: harness build failed" >&2; exit 1; }
fi
teardown
"$HERE/harness.sh" up --state "$state" || { echo "e2e: harness did not come up" >&2; exit 1; }

# ---------------------------------------------------------------- API live test

docker run --rm --platform linux/amd64 \
  -v "$REPO":/app -w /app \
  -v "$CARGO_VOLUME":/usr/local/cargo/registry \
  -v "$TARGET_VOLUME":/app/target \
  "$BUILDER" cargo test --locked --no-run >/dev/null 2>&1 \
  || { echo "e2e: test build failed" >&2; exit 1; }

if docker run --rm --platform linux/amd64 --network "$NET" \
  -v "$REPO":/app -w /app \
  -v "$CARGO_VOLUME":/usr/local/cargo/registry \
  -v "$TARGET_VOLUME":/app/target \
  -v "$state/password":/run/ocgtk/password:ro \
  -e OCGTK_V2H_HARNESS=1 \
  -e OCGTK_LIVE_URL=http://127.0.0.1:14096 \
  -e OCGTK_LIVE_PASSWORD_FILE=/run/ocgtk/password \
  -e OCGTK_V2H_UPSTREAM="$UPSTREAM" \
  "$BUILDER" bash -c '
    set -e
    python3 tests/v2/loopback.py --ready /tmp/forward-ready 14096 "$OCGTK_V2H_UPSTREAM" 4096 &
    forwarder=$!
    for _ in $(seq 1 50); do
      [ -s /tmp/forward-ready ] && break
      kill -0 "$forwarder" 2>/dev/null || break
      sleep 0.1
    done
    [ -s /tmp/forward-ready ] || { echo "e2e: loopback forwarder could not listen on 127.0.0.1:14096" >&2; exit 1; }
    cargo test --offline --locked live_server_end_to_end -- --ignored --nocapture --test-threads 1'; then
  record PASS api-live
else
  record FAIL api-live
fi

# ---------------------------------------------------------------- GUI smoke

if [ -z "$skip_gui" ]; then
  gui_binary="${E2E_GUI_BINARY:-/app/target/debug/opencode-gpui}"
  if [ -z "${E2E_GUI_BINARY:-}" ]; then
    # The UI image builds into the repository's own target/, like remote-flow-ui.sh.
    docker run --rm --platform linux/amd64 -v "$REPO":/app -w /app "$UI_IMAGE" \
      cargo build --locked >/dev/null 2>&1 || { echo "e2e: GUI build failed" >&2; exit 1; }
  fi
  # A fresh server, so nothing the API test left pending covers the composer.
  teardown
  "$HERE/harness.sh" up --state "$state" >/dev/null || { echo "e2e: harness did not come up again" >&2; exit 1; }
  if docker run --rm --platform linux/amd64 --network "$NET" \
    -v "$REPO":/app -w /app \
    -v "$state/password":/run/ocgtk/password:ro \
    -v "$shots":/shots \
    -e GUI_PASSWORD_FILE=/run/ocgtk/password \
    -e GUI_BINARY="$gui_binary" \
    -e GUI_CONFIG_SUFFIX="${E2E_GUI_CONFIG_SUFFIX:-}" \
    -e GUI_SLOW_FLOW="${E2E_GUI_SLOW_FLOW:-0}" \
    -e GUI_SETTINGS_SWITCH="${E2E_GUI_SETTINGS_SWITCH:-0}" \
    -e GUI_SHOTS=/shots \
    -e GUI_UPSTREAM_HOST="$UPSTREAM" \
    "$UI_IMAGE" bash tests/v2/gui_smoke.sh; then
    record PASS gui-smoke
  else
    record FAIL gui-smoke
  fi
fi

printf '\n'
printf '%s\n' "${results[@]}"
for result in "${results[@]}"; do
  case "$result" in FAIL*) exit 1 ;; esac
done
