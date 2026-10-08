#!/usr/bin/env bash
# Headless GPUI smoke check in the amd64 builder image. Never use a live display.
# docker run --rm --platform linux/amd64 -v "$PWD":/repo -w /repo \
#   opencode-gpui-builder-amd64:latest bash tests/smoke-ui.sh
# SMOKE_BINARY skips the build; SMOKE_SHOTS saves Weston screenshots.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
temporary="$(mktemp -d)"
app_pid='' xvfb_pid='' weston_pid=''
source tests/gpui-headless.sh
cleanup() {
  if [[ -n "$app_pid" ]]; then kill "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true; fi
  gpui_stop_display
  rm -rf "$temporary"
}
trap cleanup EXIT
gpui_start_display
gpui_binary "${SMOKE_BINARY:-}"

run() {
  local name="$1" combo
  shift
  mkdir -p "$temporary/$name"/{config,data,cache}
  XDG_CONFIG_HOME="$temporary/$name/config" XDG_DATA_HOME="$temporary/$name/data" \
    XDG_CACHE_HOME="$temporary/$name/cache" GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1 \
    "$binary" "$@" >"$temporary/$name.log" 2>&1 &
  app_pid=$!
  gpui_wait_window || { cat "$temporary/$name.log" >&2; echo "$name: no GPUI window" >&2; return 1; }
  for combo in ctrl+t Escape ctrl+p Escape ctrl+comma Escape ctrl+g; do gpui_key "$combo"; done
  gpui_type 'smoke draft'
  kill -0 "$app_pid"
  if grep -qi panicked "$temporary/$name.log"; then cat "$temporary/$name.log" >&2; return 1; fi
  if [[ -n "${SMOKE_SHOTS:-}" ]]; then
    mkdir -p "$SMOKE_SHOTS"
    import -window "$window" "$SMOKE_SHOTS/smoke-$name.png"
  fi
  kill "$app_pid"
  wait "$app_pid" 2>/dev/null || true
  app_pid=''
  printf 'PASS %s: window, shortcuts, draft, alive, no panic\n' "$name"
}

run preview --preview
run unreachable --server http://127.0.0.1:9 --username smoke-test
