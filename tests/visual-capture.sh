#!/usr/bin/env bash
# Run only inside the headless Linux test container, never on a live desktop.
# APP_BINARY, OUTPUT and CLIENT=(gtk|gpui) are required. WINDOW_W/H and
# CASE=(main|settings|sessions|new-session|model|level|rename) are optional.
set -euo pipefail
: "${APP_BINARY:?absolute path to preview binary}"
: "${OUTPUT:?absolute path for output PNG}"
: "${CLIENT:?gtk or gpui}"
window_w="${WINDOW_W:-790}"
window_h="${WINDOW_H:-800}"
case_name="${CASE:-main}"
export DISPLAY=:98
Xvfb :98 -screen 0 1280x900x24 -nolisten tcp >/dev/null 2>&1 &
xvfb_pid=$!
app_pid=""
compositor_pid=""
cleanup() {
  if [[ -n "$app_pid" ]]; then kill "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true; fi
  if [[ -n "$compositor_pid" ]]; then kill "$compositor_pid" 2>/dev/null || true; wait "$compositor_pid" 2>/dev/null || true; fi
  kill "$xvfb_pid" 2>/dev/null || true
  wait "$xvfb_pid" 2>/dev/null || true
}
trap cleanup EXIT
for _ in {1..80}; do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
export XDG_CONFIG_HOME="${TEST_CONFIG_DIR:-$(mktemp -d)}" XDG_DATA_HOME="$(mktemp -d)" XDG_CACHE_HOME="$(mktemp -d)"
export XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"
export GSETTINGS_BACKEND=memory GDK_BACKEND=x11 GTK_A11Y=none NO_AT_BRIDGE=1
export LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
if [[ "$CLIENT" == gpui ]]; then
  # Vulkan/X11 on Xvfb maps a window but does not present its pixels. Nested
  # Weston composites the GPUI Wayland surface into its X11 kiosk output.
  window_w=780
  window_h=791
  weston --backend=x11-backend.so --use-pixman --shell=kiosk-shell.so \
    --width="$window_w" --height="$window_h" --socket=wayland-1 \
    --no-config --log="${OUTPUT}.weston.log" >/dev/null 2>&1 & compositor_pid=$!
  export WAYLAND_DISPLAY=wayland-1
  sleep 3
fi
if [[ "${BG:-light}" == dark ]]; then
  mkdir -p "$XDG_CONFIG_HOME/gtk-4.0"
  printf '[Settings]\ngtk-application-prefer-dark-theme=1\n' > "$XDG_CONFIG_HOME/gtk-4.0/settings.ini"
fi
if [[ "${TEST_LIVE:-0}" == 1 && "$CLIENT" == gpui ]]; then
  app_args=()
elif [[ "${PREVIEW_API:-0}" == 1 && "$CLIENT" == gpui ]]; then
  app_args=(--preview-api)
else
  app_args=(--preview)
fi
if [[ "$CLIENT" == gpui && "$case_name" != main ]]; then
  app_args+=(--drawer "$case_name")
fi
"$APP_BINARY" "${app_args[@]}" >"${OUTPUT}.app.log" 2>&1 &
app_pid=$!
win=""
for _ in {1..120}; do
  if [[ "$CLIENT" == gpui ]]; then
    win="$(xwininfo -root -tree 2>/dev/null | awk '/Weston Compositor - screen0/ {print $1; exit}')"
  else
    win="$(xdotool search --onlyvisible --name 'OpenCode' 2>/dev/null | tail -1 || true)"
  fi
  [[ -n "$win" ]] && break
  sleep 0.25
done
[[ -n "$win" ]] || { tail -30 "${OUTPUT}.app.log"; exit 1; }
if [[ "$CLIENT" == gpui && -n "${INTERACTION:-}" ]]; then
  # The Weston X window exists before GPUI has created its Wayland surface.
  # Early xdotool events otherwise vanish before the first client frame.
  sleep 5
fi
if [[ "$CLIENT" == gtk ]]; then
  xdotool windowsize --sync "$win" "$window_w" "$window_h"
fi
xdotool windowfocus --sync "$win"
focused="$(xdotool getwindowfocus)"
if (( focused != win )); then
  echo "Window focus mismatch: wanted $win, got $focused" >&2
  exit 1
fi
if [[ "$CLIENT" == gtk ]]; then case "$case_name" in
  main) ;;
  settings) xdotool key ctrl+comma ;;
  settings-sessions) xdotool key ctrl+comma; sleep 1; xdotool mousemove 80 198 click 1 ;;
  sessions) xdotool key ctrl+p ;;
  new-session) xdotool mousemove 70 76 click 1 ;;
  model) xdotool mousemove 420 758 click 1 ;;
  level) xdotool mousemove 523 758 click 1 ;;
  rename) xdotool key F2 ;;
  *) echo "unknown case $case_name" >&2; exit 2 ;;
esac
fi
if [[ "$CLIENT" == gtk && "$case_name" == main && "${SCROLL_BOTTOM:-0}" == 1 ]]; then
  xdotool mousemove 710 350 click --repeat 30 --delay 20 5
fi
if [[ "$CLIENT" == gpui && ( "${INTERACTION:-}" == send || "${INTERACTION:-}" == type ) ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  sleep 0.2
  xdotool type --clearmodifiers --delay 40 'Ping from the GPUI client'
  if [[ "${INTERACTION:-}" == send ]]; then
    xdotool key Return
    sleep 1
    xdotool mousemove 710 350 click --repeat 20 --delay 20 5
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == open-settings ]]; then
  xdotool mousemove --window "$win" 72 764 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == settings-sessions ]]; then
  xdotool mousemove --window "$win" 72 764 click 1
  sleep 0.5
  xdotool mousemove --window "$win" 80 198 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == close-tab ]]; then
  xdotool mousemove --window "$win" 247 116 click 1
fi
if [[ "${INTERACTION:-}" == permission || "${INTERACTION:-}" == permission-deny || "${INTERACTION:-}" == permission-once ]]; then
  if [[ "$CLIENT" == gpui ]]; then
    xdotool mousemove --window "$win" 92 148 click 1
    sleep 0.4
    if [[ "${INTERACTION:-}" == permission-deny ]]; then xdotool mousemove --window "$win" 492 737 click 1; fi
    if [[ "${INTERACTION:-}" == permission-once ]]; then xdotool mousemove --window "$win" 578 737 click 1; fi
  else
    xdotool mousemove 92 148 click 1
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == close-last-reopen ]]; then
  for _ in {1..5}; do
    xdotool mousemove --window "$win" 247 116 click 1
    sleep 0.25
  done
  xdotool mousemove --window "$win" 72 764 click 1
  sleep 0.4
  xdotool mousemove --window "$win" 80 198 click 1
  sleep 0.4
  xdotool mousemove --window "$win" 440 252 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == copy-code ]]; then
  xdotool mousemove --window "$win" 720 392 click 1
  sleep 0.2
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+v
fi
if [[ "${INTERACTION:-}" == running || "${INTERACTION:-}" == parked ]]; then
  if [[ "${INTERACTION:-}" == running ]]; then row_y=187; else row_y=224; fi
  if [[ "$CLIENT" == gpui ]]; then
    xdotool mousemove --window "$win" 92 "$row_y" click 1
  else
    xdotool mousemove 92 "$row_y" click 1
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == queue ]]; then
  xdotool mousemove --window "$win" 92 187 click 1
  sleep 0.8
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'Another queued prompt'
  xdotool key ctrl+Return
fi
if [[ "$CLIENT" == gpui && ( "${INTERACTION:-}" == cancel-waiting || "${INTERACTION:-}" == switch-waiting ) ]]; then
  xdotool mousemove --window "$win" 92 187 click 1
  sleep 0.8
  if [[ "${INTERACTION:-}" == cancel-waiting ]]; then action_x=735; else action_x=675; fi
  xdotool mousemove --window "$win" "$action_x" 503 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == resume ]]; then
  xdotool mousemove --window "$win" 92 225 click 1
  sleep 0.8
  xdotool mousemove --window "$win" 716 452 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == shortcut-settings ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+comma
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == shortcut-new-session ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+t
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == shortcut-next-tab ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+Tab
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == shortcut-escape ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+p
  sleep 0.2
  xdotool key Escape
fi
if [[ "${INTERACTION:-}" == model-picker || "${INTERACTION:-}" == level-picker ]]; then
  if [[ "${INTERACTION:-}" == model-picker ]]; then picker_x=387; else picker_x=491; fi
  if [[ "$CLIENT" == gpui ]]; then
    xdotool mousemove --window "$win" "$picker_x" 748 click 1
  else
    xdotool mousemove "$picker_x" 748 click 1
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == keyboard-model-choice ]]; then
  xdotool mousemove --window "$win" 387 748 click 1
  sleep 0.3
  xdotool key Up Return
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == keyboard-level-choice ]]; then
  xdotool mousemove --window "$win" 491 748 click 1
  sleep 0.3
  xdotool key Down Return
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == apply-settings ]]; then
  xdotool mousemove --window "$win" 72 764 click 1
  sleep 0.5
  xdotool mousemove --window "$win" 430 219 click 1
  xdotool key ctrl+a
  xdotool type --clearmodifiers --delay 30 'http://127.0.0.1:4555'
  xdotool mousemove --window "$win" 758 726 click 1
fi
xdotool mousemove 1100 850
sleep 6
if [[ "$CLIENT" == gpui ]]; then
  import -window "$win" "$OUTPUT"
else
  import -window "$win" "$OUTPUT"
  if [[ "$case_name" == model || "$case_name" == level ]]; then
    import -window root "${OUTPUT}.root.png"
  fi
fi
printf '%s %s %s\n' "$CLIENT" "$case_name" "$OUTPUT"
