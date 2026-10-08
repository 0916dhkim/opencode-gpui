#!/usr/bin/env bash
# Inspect the GPUI Wayland application's platform AT-SPI tree in an isolated
# session bus and nested Weston/Xvfb, without touching the host desktop.
# The GPUI builder image includes at-spi2-core and python3-pyatspi:
#   ACCESSIBILITY_BINARY=target/debug/opencode-gpui bash tests/accessibility-ui.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if [[ -x /usr/local/cargo/bin/cargo ]]; then export PATH="/usr/local/cargo/bin:$PATH"; fi
for tool in dbus-run-session gdbus /usr/bin/python3; do
  command -v "$tool" >/dev/null || { echo "Missing $tool (see Docker prerequisites above)" >&2; exit 1; }
done
/usr/bin/python3 -c 'import pyatspi' 2>/dev/null || {
  echo 'Missing python3-pyatspi (see Docker prerequisites above)' >&2; exit 1;
}

if [[ -z "${ACCESSIBILITY_IN_DBUS:-}" ]]; then
  ACCESSIBILITY_IN_DBUS=1 exec dbus-run-session -- bash "$0" "$@"
fi

temporary="$(mktemp -d)"
app_pid='' xvfb_pid='' weston_pid=''
source tests/gpui-headless.sh
cleanup() {
  if [[ -n "$app_pid" ]]; then kill "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true; fi
  gpui_stop_display
  if [[ -n "${ACCESSIBILITY_KEEP:-}" ]]; then echo "Kept $temporary" >&2; else rm -rf "$temporary"; fi
}
trap cleanup EXIT

mkdir -p "$temporary"/{home,config,data,cache}
export HOME="$temporary/home" XDG_CONFIG_HOME="$temporary/config"
export XDG_DATA_HOME="$temporary/data" XDG_CACHE_HOME="$temporary/cache"
export GSETTINGS_BACKEND=memory
unset NO_AT_BRIDGE
gpui_start_display
gpui_binary "${ACCESSIBILITY_BINARY:-}"

# AccessKit's Unix adapter stays inactive unless the session's AT-SPI status
# is enabled. Start the bus explicitly and enable it before launching GPUI.
gdbus call --session --dest org.a11y.Bus --object-path /org/a11y/bus \
  --method org.a11y.Bus.GetAddress >"$temporary/a11y-address"
gdbus call --session --dest org.a11y.Bus --object-path /org/a11y/bus \
  --method org.freedesktop.DBus.Properties.Set org.a11y.Status IsEnabled '<true>' >/dev/null
"$binary" --preview >"$temporary/app.log" 2>&1 &
app_pid=$!
gpui_wait_window || { cat "$temporary/app.log" >&2; exit 1; }

/usr/bin/python3 - <<'PY'
import sys
import time
import pyatspi

def describe(obj):
    try:
        return (f"{obj.getRoleName()!r} name={obj.name!r} children={obj.childCount} "
                f"interfaces={obj.get_interfaces()} states={obj.getState().getStates()}")
    except Exception as exc:
        return f"<inaccessible: {exc}>"

def walk(obj, depth=0):
    print('  ' * depth + describe(obj), flush=True)
    if depth < 6:
        try:
            for child in obj:
                walk(child, depth + 1)
        except Exception as exc:
            print('  ' * (depth + 1) + f'<traversal failed: {exc}>', flush=True)

for attempt in range(60):
    desktop = pyatspi.Registry.getDesktop(0)
    apps = list(desktop)
    matches = [app for app in apps if app.name == 'opencode-gpui']
    frames = ([child for child in matches[0]
               if child.getRoleName() == 'frame' and child.name == 'OpenCode']
              if len(matches) == 1 else [])
    entries = ([child for child in frames[0] if child.getRoleName() == 'entry']
               if len(frames) == 1 else [])
    session_buttons = ([child for child in frames[0]
                        if child.getRoleName() == 'push button'
                        and child.name.startswith('Open session: ')]
                       if len(frames) == 1 else [])
    if (len(entries) == 1 and entries[0].name == 'Ask OpenCode anything…'
            and len(session_buttons) >= 5):
        break
    time.sleep(0.3)
else:
    print('Timed out waiting for the named GPUI composer and session buttons in AT-SPI',
          file=sys.stderr)
    for app in apps:
        walk(app)
    sys.exit(1)

print(f'AT-SPI desktop applications: {len(apps)}')
for app in apps:
    walk(app)
assert len(matches) == 1, 'GPUI application was not registered exactly once'
assert len(frames) == 1, 'OpenCode frame was not exposed'
assert len(entries) == 1, 'Composer entry was not exposed'
assert pyatspi.STATE_EDITABLE in entries[0].getState().getStates(), 'Composer is not editable'
session_buttons = [child for child in frames[0]
                   if child.getRoleName() == 'push button' and child.name.startswith('Open session: ')]
assert session_buttons, 'No named session buttons were exposed'
for button in session_buttons:
    assert 'Action' in button.get_interfaces(), f'Session button lacks Action: {button.name}'
    assert pyatspi.STATE_FOCUSABLE in button.getState().getStates(), (
        f'Session button is not keyboard-focusable: {button.name}')
    children = list(button)
    for prefix in ('Rename session: ', 'Close tab: '):
        assert any(child.getRoleName() == 'push button'
                    and child.name.startswith(prefix) and 'Action' in child.get_interfaces()
                    and pyatspi.STATE_FOCUSABLE in child.getState().getStates()
                    for child in children), f'{button.name} lacks actionable {prefix}'
for interface in ('Text', 'EditableText'):
    try:
        getattr(entries[0], 'query' + interface)()
        print(f'Composer supports {interface} interface')
    except NotImplementedError:
        print(f'Composer does not support {interface} interface')
print(f'PASS GPUI application, window, named composer, and {len(session_buttons)} '
      'session buttons with rename/close actions are exposed through AT-SPI')
PY
