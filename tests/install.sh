#!/usr/bin/env bash
# Exercise the install layout without touching the user's home or compiling a release build.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT
mkdir -p "$temporary/mock-bin" "$temporary/home"
cat >"$temporary/mock-bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == build && " $* " == *' --release --locked '* ]]
install -Dm755 /bin/true "$CARGO_TARGET_DIR/release/opencode-gpui"
MOCK
chmod +x "$temporary/mock-bin/cargo"
(
  cd "$temporary"
  export PATH="$temporary/mock-bin:$PATH"
  export HOME="$temporary/home" CARGO_TARGET_DIR=custom-target
  export CARGO_INSTALL_ROOT="$temporary/install" XDG_DATA_HOME="$temporary/data"
  bash "$root/install.sh"
)
test -x "$temporary/install/bin/opencode-gpui"
desktop="$temporary/data/applications/ai.opencode.Gpui.desktop"
test -f "$desktop"
grep -Fxq "Exec=$temporary/install/bin/opencode-gpui" "$desktop"
test ! -e "$temporary/install/bin/opencode-cosmic"
test ! -e "$temporary/data/applications/ai.opencode.Cosmic.desktop"
(
  cd "$temporary"
  export PATH="$temporary/mock-bin:$PATH"
  export HOME="$temporary/home" CARGO_TARGET_DIR="$temporary/absolute-target"
  export CARGO_INSTALL_ROOT="$temporary/absolute-install" XDG_DATA_HOME="$temporary/absolute-data"
  bash "$root/install.sh"
)
test -x "$temporary/absolute-install/bin/opencode-gpui"
grep -Fxq "Exec=$temporary/absolute-install/bin/opencode-gpui" \
  "$temporary/absolute-data/applications/ai.opencode.Gpui.desktop"
printf '%s\n' 'PASS GPUI install layout and relative/absolute Cargo target isolation'
