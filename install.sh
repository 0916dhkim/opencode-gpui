#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_ROOT="${CARGO_INSTALL_ROOT:-${PREFIX:-${HOME}/.local}}"
BIN_DIR="${INSTALL_ROOT}/bin"
APPLICATION_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/applications"

if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' 'cargo is required. Install Rust from https://rustup.rs and try again.' >&2
  exit 1
fi

cargo build --manifest-path "${ROOT}/Cargo.toml" --release --locked
install -Dm755 "${ROOT}/target/release/opencode-gpui" "${BIN_DIR}/opencode-gpui"
install -d "${APPLICATION_DIR}"
sed "s|@EXEC@|${BIN_DIR}/opencode-gpui|g" \
  "${ROOT}/data/ai.opencode.Gpui.desktop" \
  > "${APPLICATION_DIR}/ai.opencode.Gpui.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${APPLICATION_DIR}"
fi

printf 'Installed OpenCode GPUI to %s\n' "${BIN_DIR}/opencode-gpui"
