#!/usr/bin/env bash
# Shared headless input harness. Source after setting temporary to a private directory.
# The X11 target is Weston's kiosk window; GPUI itself runs on its Wayland socket.
gpui_start_display() {
  local display ready=0
  mkdir -p "$temporary/runtime"
  chmod 700 "$temporary/runtime"
  export XDG_RUNTIME_DIR="$temporary/runtime"
  for display in $(seq 90 120); do
    [[ -e /tmp/.X11-unix/X$display || -e /tmp/.X$display-lock ]] && continue
    export DISPLAY=:$display
    Xvfb "$DISPLAY" -screen 0 1280x900x24 -nolisten tcp >"$temporary/xvfb.log" 2>&1 &
    xvfb_pid=$!
    for _ in {1..80}; do
      kill -0 "$xvfb_pid" 2>/dev/null || break
      if xdotool getdisplaygeometry >/dev/null 2>&1; then
        ready=1
        break
      fi
      sleep 0.1
    done
    [[ "$ready" == 1 ]] && break
    wait "$xvfb_pid" 2>/dev/null || true
    xvfb_pid=''
  done
  [[ "$ready" == 1 ]] || { echo 'No usable headless X display' >&2; return 1; }
  export WAYLAND_DISPLAY=wayland-1 LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
  weston --backend=x11-backend.so --use-pixman --shell=kiosk-shell.so \
    --width=780 --height=791 --socket="$WAYLAND_DISPLAY" --no-config --idle-time=0 \
    >"$temporary/weston.log" 2>&1 &
  weston_pid=$!
  for _ in {1..80}; do [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] && break; sleep 0.1; done
  [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] || { cat "$temporary/weston.log" >&2; return 1; }
}

gpui_wait_window() {
  local candidate pixel
  for _ in {1..120}; do
    [[ -z "${app_pid:-}" ]] || kill -0 "$app_pid" 2>/dev/null || return 1
    candidate="$(xwininfo -root -tree 2>/dev/null | awk '/Weston Compositor - screen0/ {print $1; exit}')"
    if [[ -n "$candidate" ]]; then
      # Weston maps before GPUI has painted its first Wayland surface. Its
      # kiosk window is black until the app appears; do not type into that
      # empty host window or wait an arbitrary three seconds after mapping.
      pixel="$(import -window "$candidate" -crop 1x1+390+22 txt:- 2>/dev/null | tail -n 1 || true)"
      if [[ -n "$pixel" && "$pixel" != *'#000000000000'* && "$pixel" != *'#000000'* ]]; then
        window="$candidate"
        gpui_focus && return 0
      fi
    fi
    sleep 0.1
  done
  return 1
}

gpui_focus() {
  xdotool windowfocus --sync "$window" >/dev/null 2>&1 || return 1
  [[ "$(xdotool getwindowfocus)" == "$((window))" ]]
}
gpui_key() { gpui_focus || return 1; xdotool key --clearmodifiers "$@" || return 1; sleep 0.3; }
gpui_type() { gpui_focus || return 1; xdotool type --delay 20 --clearmodifiers "$1" || return 1; sleep 0.3; }
gpui_click() { gpui_focus || return 1; xdotool mousemove --window "$window" "$1" "$2" click 1 || return 1; sleep 0.3; }

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
