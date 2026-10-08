#!/usr/bin/env bash
# Shared headless input harness. Source after setting temporary to a private directory.
# The X11 target is Weston's kiosk window; GPUI itself runs on its Wayland socket.
gpui_start_display() {
  local display
  mkdir -p "$temporary/runtime"
  chmod 700 "$temporary/runtime"
  export XDG_RUNTIME_DIR="$temporary/runtime"
  for display in $(seq 90 120); do
    [[ -e /tmp/.X11-unix/X$display || -e /tmp/.X$display-lock ]] && continue
    export DISPLAY=:$display
    Xvfb "$DISPLAY" -screen 0 1280x900x24 -nolisten tcp >"$temporary/xvfb.log" 2>&1 &
    xvfb_pid=$!
    break
  done
  [[ -n "${xvfb_pid:-}" ]] || { echo 'No free headless X display' >&2; return 1; }
  for _ in {1..80}; do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
  xdotool getdisplaygeometry >/dev/null
  export WAYLAND_DISPLAY=wayland-1 LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
  weston --backend=x11-backend.so --use-pixman --shell=kiosk-shell.so \
    --width=780 --height=791 --socket="$WAYLAND_DISPLAY" --no-config --idle-time=0 \
    >"$temporary/weston.log" 2>&1 &
  weston_pid=$!
  for _ in {1..80}; do [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] && break; sleep 0.1; done
  [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] || { cat "$temporary/weston.log" >&2; return 1; }
}

gpui_wait_window() {
  local candidate
  for _ in {1..120}; do
    [[ -z "${app_pid:-}" ]] || kill -0 "$app_pid" 2>/dev/null || return 1
    candidate="$(xwininfo -root -tree 2>/dev/null | awk '/Weston Compositor - screen0/ {print $1; exit}')"
    if [[ -n "$candidate" ]]; then
      window="$candidate"
      # Weston maps before the Wayland surface is available for input.
      sleep 3
      gpui_focus
      return 0
    fi
    sleep 0.2
  done
  return 1
}

gpui_focus() {
  xdotool windowfocus --sync "$window" >/dev/null 2>&1 || return 1
  [[ "$(xdotool getwindowfocus)" == "$((window))" ]]
}
gpui_key() { gpui_focus && xdotool key --clearmodifiers "$@"; sleep 0.3; }
gpui_type() { gpui_focus && xdotool type --delay 20 --clearmodifiers "$1"; sleep 0.3; }
gpui_click() { gpui_focus && xdotool mousemove --window "$window" "$1" "$2" click 1; sleep 0.3; }

gpui_binary() {
  local override="${1:-}"
  if [[ -n "$override" ]]; then
    binary="$override"
  else
    cargo build --locked
    binary="${CARGO_TARGET_DIR:-target}/debug/opencode-gpui"
  fi
  [[ -x "$binary" ]] || { printf 'Missing GPUI binary: %s\n' "$binary" >&2; return 1; }
}

gpui_stop_display() {
  local process
  for process in "${weston_pid:-}" "${xvfb_pid:-}"; do
    if [[ -n "$process" ]]; then kill "$process" 2>/dev/null || true; wait "$process" 2>/dev/null || true; fi
  done
}
