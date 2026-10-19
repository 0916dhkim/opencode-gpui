#!/usr/bin/env bash
# Run only inside the headless Linux test container, never on a live desktop.
# APP_BINARY, OUTPUT and CLIENT=(gtk|gpui) are required. WINDOW_W/H and
# CASE=(main|settings|sessions|new-session|model|level|rename) are optional.
# Static GPUI comparisons omit its extra image fixture by default to match GTK;
# PREVIEW_IMAGE=1 deliberately includes that extra turn for image-rendering tests.
set -euo pipefail
: "${APP_BINARY:?absolute path to preview binary}"
: "${OUTPUT:?absolute path for output PNG}"
: "${CLIENT:?gtk or gpui}"
window_w="${WINDOW_W:-790}"
window_h="${WINDOW_H:-800}"
case_name="${CASE:-main}"
export DISPLAY=:98
Xvfb :98 -screen 0 "${SCREEN_W:-1280}x${SCREEN_H:-900}x24" -nolisten tcp >/dev/null 2>&1 &
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
export XDG_CONFIG_HOME="${TEST_CONFIG_DIR:-$(mktemp -d)}" XDG_DATA_HOME="${TEST_DATA_DIR:-$(mktemp -d)}" XDG_CACHE_HOME="$(mktemp -d)"
if [[ "$CLIENT" == gpui && "${TEST_INVALID_STATE:-0}" == 1 ]]; then
  mkdir -p "$XDG_CONFIG_HOME/opencode-gpui"
  printf '{"connection":{"server":"not-a-url","username":"opencode"}}\n' > "$XDG_CONFIG_HOME/opencode-gpui/state.json"
fi
export XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"
export GSETTINGS_BACKEND=memory GDK_BACKEND=x11 GTK_A11Y=none NO_AT_BRIDGE=1
export LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=vulkan
if [[ "$CLIENT" == gpui ]]; then
  # Vulkan/X11 on Xvfb maps a window but does not present its pixels. Nested
  # Weston composites the GPUI Wayland surface into its X11 output.
  window_w="${WINDOW_W:-780}"
  window_h="${WINDOW_H:-791}"
  weston --backend=x11-backend.so --use-pixman --shell="${WESTON_SHELL:-kiosk-shell.so}" \
    --width="$window_w" --height="$window_h" --socket=wayland-1 \
    --no-config --log="${OUTPUT}.weston.log" >/dev/null 2>&1 & compositor_pid=$!
  export WAYLAND_DISPLAY=wayland-1
  sleep 3
fi
if [[ "${BG:-light}" == dark ]]; then
  mkdir -p "$XDG_CONFIG_HOME/gtk-4.0"
  printf '[Settings]\ngtk-application-prefer-dark-theme=1\n' > "$XDG_CONFIG_HOME/gtk-4.0/settings.ini"
fi
if [[ "$CLIENT" == gpui ]]; then
  export OPENCODE_GPUI_THEME="${BG:-light}"
  if [[ "${TEST_LIVE:-0}" != 1 && "${PREVIEW_API:-0}" != 1 && "${PREVIEW_IMAGE:-0}" != 1 ]]; then
    export OPENCODE_PREVIEW_MATCH_GTK=1
  fi
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
if [[ "$CLIENT" == gpui ]]; then
  # Weston maps its X window before GPUI's Wayland surface is ready. A fixed
  # delay sometimes captured a completely black release build; wait for an
  # actual painted frame before sending input or accepting a screenshot.
  ready=0
  for _ in {1..40}; do
    kill -0 "$app_pid" 2>/dev/null || break
    if import -window "$win" "$OUTPUT" 2>/dev/null; then
      frame_mean="$(convert "$OUTPUT" -format '%[fx:mean]' info:)"
      if awk -v mean="$frame_mean" 'BEGIN { exit !(mean > 0.01) }'; then
        ready=1
        break
      fi
    fi
    sleep 0.5
  done
  if (( ! ready )); then
    echo 'GPUI did not paint a visible frame before the capture deadline' >&2
    tail -30 "${OUTPUT}.app.log" >&2
    exit 1
  fi
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
  settings-sessions) xdotool key ctrl+comma; sleep 1; xdotool mousemove --window "$win" 80 184 click 1; sleep 0.2; xdotool mousemove --window "$win" 80 192 click 1 ;;
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
if [[ "${INTERACTION:-}" == toggle-sidebar ]]; then
  if [[ "$CLIENT" == gpui ]]; then
    xdotool mousemove --window "$win" 410 680 click 1
    sleep 0.2
  fi
  xdotool key ctrl+b
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == zoom-in ]]; then
  if [[ "$case_name" != model && "$case_name" != level ]]; then
    xdotool mousemove --window "$win" 410 680 click 1
  fi
  xdotool key ctrl+equal
  sleep 0.3
  xdotool key ctrl+equal
fi
if [[ "$CLIENT" == gpui && ( "${INTERACTION:-}" == zoom-max || "${INTERACTION:-}" == zoom-max-type ) ]]; then
  if [[ "$case_name" != model && "$case_name" != level ]]; then
    xdotool mousemove --window "$win" 410 680 click 1
  fi
  for _ in {1..6}; do xdotool key ctrl+equal; sleep 0.15; done
  if [[ "${INTERACTION:-}" == zoom-max-type ]]; then
    xdotool type --clearmodifiers --delay 40 'Ping from the GPUI client'
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == titlebar-maximize ]]; then
  xdotool mousemove --window "$win" "$((window_w - 51))" 22 click 1
  sleep 0.3
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == titlebar-drag ]]; then
  xdotool mousemove --window "$win" "$((window_w / 2))" 22 mousedown 1
  xdotool mousemove --window "$win" "$((window_w / 2 + 12))" 30
  sleep 0.3
  xdotool mouseup 1
fi
if [[ "$CLIENT" == gpui && "$case_name" == main && -z "${INTERACTION:-}" && "${PREVIEW_API:-0}" != 1 ]]; then
  # GTK's reference main screenshot has an empty, focused composer. Match its
  # input state instead of misreporting GPUI's unfocused placeholder as a UI gap.
  xdotool mousemove --window "$win" 410 680 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == complex-top ]]; then
  xdotool mousemove --window "$win" 550 350 click --repeat 30 --delay 20 4
  sleep 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == transcript-scrollbar-top ]]; then
  # Manual inspection aid for a long already-loaded transcript, not a GUI test.
  # Wake an auto-hidden native scrollbar before attempting to grab its thumb.
  xdotool mousemove --window "$win" 550 350 click 4
  sleep "${TRANSCRIPT_STEP_WAIT:-0.3}"
  xdotool mousemove --window "$win" "${TRANSCRIPT_THUMB_X:-773}" "${TRANSCRIPT_THUMB_Y:-560}"
  sleep "${TRANSCRIPT_STEP_WAIT:-0.3}"
  xdotool mousedown 1
  sleep "${TRANSCRIPT_HOLD_WAIT:-0.4}"
  xdotool mousemove --sync --window "$win" "${TRANSCRIPT_THUMB_X:-773}" 48
  sleep "${TRANSCRIPT_STEP_WAIT:-0.3}"
  xdotool mouseup 1
  sleep "${TRANSCRIPT_SETTLE_WAIT:-1}"
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == sticky-mid ]]; then
  if (( ${STICKY_ZOOM_STEPS:-0} > 0 )); then
    xdotool mousemove --window "$win" 410 680 click 1
  fi
  for ((i = 0; i < ${STICKY_ZOOM_STEPS:-0}; i++)); do xdotool key ctrl+equal; sleep 0.15; done
  sleep 0.3
  xdotool mousemove --window "$win" 550 350 click --repeat "${STICKY_SCROLL_TICKS:-2}" --delay 40 5
  sleep 0.5
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
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == composer-lines ]]; then
  # Agent-directed multiline/overflow inspection, not an assertion suite.
  xdotool mousemove --window "$win" "$((window_w / 2))" "$((window_h - 120))" click 1
  for ((i = 0; i < ${COMPOSER_ZOOM_STEPS:-0}; i++)); do xdotool key ctrl+equal; sleep 0.15; done
  for ((i = 1; i <= ${COMPOSER_LINES:-6}; i++)); do
    if (( i > 1 )); then xdotool key shift+Return; fi
    xdotool type --clearmodifiers --delay 20 "$i"
  done
  sleep 0.5
  if [[ "${COMPOSER_POSITION:-end}" == start ]]; then xdotool key ctrl+Home; fi
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
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == middle-close ]]; then
  xdotool mousemove --window "$win" 110 148 click 2
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == working-pill ]]; then
  xdotool mousemove --window "$win" 100 227 click 1
  sleep 0.5
fi
if [[ "$CLIENT" == gpui && "$case_name" == many-projects && "${INTERACTION:-}" == picker-last ]]; then
  xdotool key --repeat 8 --delay 50 Down
  sleep 0.4
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == alt-hold ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool keydown Alt_L
  sleep 0.3
  import -window "$win" "${OUTPUT%.png}-held.png"
  xdotool keyup Alt_L
  sleep 0.3
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == transcript-autoscroll ]]; then
  if (( ${AUTOSCROLL_ZOOM_STEPS:-0} > 0 )); then
    xdotool mousemove --window "$win" 410 680 click 1
    for ((i = 0; i < AUTOSCROLL_ZOOM_STEPS; i++)); do xdotool key ctrl+equal; sleep 0.15; done
  fi
  xdotool mousemove --window "$win" 550 350 click 2
  sleep 0.2
  import -window "$win" "${OUTPUT%.png}-marker.png"
  xdotool mousemove --window "$win" 550 200
  sleep 1.2
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == drag-reorder ]]; then
  xdotool mousemove --window "$win" 105 110 mousedown 1
  xdotool mousemove --window "$win" 105 135
  xdotool mousemove --window "$win" 105 185
  sleep 0.4
  import -window "$win" "${OUTPUT%.png}-cue.png"
  xdotool mouseup 1
  sleep 0.06
  import -window "$win" "${OUTPUT%.png}-shrinking.png"
fi
if [[ "${INTERACTION:-}" == permission || "${INTERACTION:-}" == permission-deny || "${INTERACTION:-}" == permission-once || "${INTERACTION:-}" == permission-tab-deny ]]; then
  if [[ "$CLIENT" == gpui ]]; then
    if [[ "${INTERACTION:-}" == permission-tab-deny ]]; then xdotool mousemove --window "$win" 410 680 click 1; fi
    xdotool mousemove --window "$win" 92 148 click 1
    sleep 0.4
    if [[ "${INTERACTION:-}" == permission-deny ]]; then xdotool mousemove --window "$win" 492 737 click 1; fi
    if [[ "${INTERACTION:-}" == permission-once ]]; then xdotool mousemove --window "$win" 578 737 click 1; fi
    if [[ "${INTERACTION:-}" == permission-tab-deny ]]; then sleep 0.5; xdotool key Tab shift+Tab space; fi
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
if [[ "$CLIENT" == gpui && ( "${INTERACTION:-}" == draft-switch || "${INTERACTION:-}" == draft-return ) ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'SESSION_A_MARKER'
  xdotool mousemove --window "$win" 92 187 click 1
  sleep 0.5
  if [[ "${INTERACTION:-}" == draft-return ]]; then
    xdotool mousemove --window "$win" 92 110 click 1
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == draft-return-after-send ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'SESSION_A_MARKER'
  xdotool mousemove --window "$win" 92 187 click 1
  sleep 0.5
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'SESSION_B_MARKER'
  xdotool key ctrl+Return
  sleep 1
  xdotool mousemove --window "$win" 92 110 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == load-earlier ]]; then
  xdotool mousemove --window "$win" 400 76 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == history-race ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'RACE_MARKER_KEEP_THIS'
  xdotool key Return
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == repair-server ]]; then
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool key ctrl+comma
  sleep 0.4
  xdotool mousemove --window "$win" 390 222 click 1 key ctrl+a
  xdotool type --clearmodifiers --delay 25 'http://127.0.0.1:4096'
  xdotool key ctrl+Return
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
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == idle ]]; then
  xdotool mousemove --window "$win" 92 265 click 1
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == parked-type ]]; then
  xdotool mousemove --window "$win" 92 224 click 1
  sleep 0.5
  xdotool mousemove --window "$win" 410 680 click 1
  xdotool type --clearmodifiers --delay 30 'Next turn'
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
  xdotool key ctrl+Return
fi
xdotool mousemove 1100 850
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == hover-close ]]; then
  xdotool mousemove --window "$win" 245 110
fi
sleep 6
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == latency-* ]]; then
  # Manual agent inspection of the real button/shortcut/export, not a GUI suite.
  xdotool mousemove --window "$win" 65 "$((window_h - 90))" click 1
  sleep 0.4
  xdotool mousemove --window "$win" "$((window_w / 2))" "$((window_h - 125))" click 1
  xdotool type --clearmodifiers --delay 50 'latency sample'
  xdotool key ctrl+p
  sleep 0.2
  xdotool key Escape
  xdotool mousemove --window "$win" "$((window_w - 60))" 350 click --repeat 3 --delay 60 4
  sleep 1
  if [[ "$INTERACTION" == latency-report ]]; then
    xdotool key ctrl+shift+l
    sleep 2
    xdotool mousemove --window "$win" 65 "$((window_h - 90))" click 1
  fi
  xdotool mousemove --window "$win" 140 400
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == background-* ]]; then
  # Agent-directed inspection only. The foreground fixture is opt-in through
  # OPENCODE_PREVIEW_BACKGROUND=1 and PREVIEW_API=1; no GUI suite is added.
  background_y="${BACKGROUND_Y:-$((window_h - 240))}"
  case "$INTERACTION" in
    background-hover)
      xdotool mousemove --window "$win" 250 "$background_y"
      sleep 1
      ;;
    background-click|background-pending|background-error)
      xdotool mousemove --window "$win" 250 "$background_y" click 1
      xdotool mousemove --window "$win" 140 400
      sleep 0.8
      ;;
    background-shortcut-hidden)
      xdotool mousemove --window "$win" 410 680 click 1
      xdotool key ctrl+b
      xdotool key ctrl+shift+b
      sleep 0.8
      ;;
  esac
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == composer-lines ]]; then
  # Scroll after settling so a transient native scrollbar is visible in the
  # capture. Optional coordinates also allow a targeted thumb-drag inspection.
  if (( ${COMPOSER_SCROLL_TICKS:-0} > 0 )); then
    xdotool mousemove --window "$win" "$((window_w - 90))" "$((window_h - 130))" \
      click --repeat "$COMPOSER_SCROLL_TICKS" --delay 50 4
    sleep 0.3
  fi
  if [[ -n "${COMPOSER_THUMB_FROM_Y:-}" ]]; then
    xdotool mousemove --window "$win" "${COMPOSER_THUMB_X:?thumb x required}" "$COMPOSER_THUMB_FROM_Y"
    sleep 0.1
    xdotool mousedown 1
    sleep 0.1
    xdotool mousemove --window "$win" "$COMPOSER_THUMB_X" "${COMPOSER_THUMB_TO_Y:?thumb destination required}"
    sleep 0.1
    xdotool mouseup 1
    sleep 0.3
  fi
fi
if [[ "$CLIENT" == gpui && "${INTERACTION:-}" == history-race ]]; then
  xdotool mousemove --window "$win" 710 350 click --repeat 30 --delay 20 5
  sleep 1
fi
if [[ "$CLIENT" == gpui ]]; then
  import -window "$win" "$OUTPUT"
  if [[ -n "${SECOND_OUTPUT:-}" ]]; then
    sleep 0.27
    import -window "$win" "$SECOND_OUTPUT"
  fi
else
  import -window "$win" "$OUTPUT"
  if [[ "$case_name" == model || "$case_name" == level ]]; then
    import -window root "${OUTPUT}.root.png"
  fi
fi
printf '%s %s %s\n' "$CLIENT" "$case_name" "$OUTPUT"
