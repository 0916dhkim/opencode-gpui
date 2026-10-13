# OpenCode Desktop (GPUI rebuild)

An in-progress native Linux desktop client for [OpenCode v2](https://opencode.ai/v2/docs/) using GPUI Kit. This GPUI prototype is now on `main` while the last verified COSMIC client remains recoverable in Git history. It is **not ready to install as a replacement** until behavior and visual parity are accepted.

The prototype currently renders a session sidebar, transcript, composer, model and effort pickers, waiting tray, and several modals. Basic v2 bootstrap, streaming events, message send, session creation/rename, model selection, editable connection settings and per-server tab persistence are wired. Queue/switch/cancel/resume and several shortcuts and picker-keyboard paths have been exercised against the in-process preview API. Not all UI paths have been verified against a live server. Permission cards, robust Markdown/virtualized transcript behavior, comprehensive shortcuts, dark-theme parity, and interaction/accessibility testing still need work. Do not treat a successful preview capture as proof of those features.

## Building & Running

### Dependencies

Requires Rust 1.93+ (Rust 2024 edition) and Linux graphics development libraries for GPUI Kit (Wayland/X11, Vulkan and fontconfig). The repository Dockerfile contains an amd64 development/test environment; it is not a minimal runtime image.

Completion sounds use the optional `canberra-gtk-play` executable (provided by `libcanberra` on Arch Linux, with its optional `gtk3` runtime for this command). Notifications still work without it, but the sound will not play.

Build for Linux:

```bash
cargo build --release
```

On a Mac with the amd64 Docker builder image, use `./build-linux-release.sh` instead. It builds from the current checkout with a per-checkout Linux-native Cargo target volume and a shared persistent registry volume, then exports the completed binary to `target/release/opencode-gpui`. This avoids false Cargo cache misses caused by rounded mtimes on Docker Desktop's macOS bind mount. Set `CARGO_BUILD_JOBS=1` if the builder runs out of memory.

Run the deterministic offline screenshot fixture, or exercise the real command/event UI against its in-process preview server:

```bash
cargo run -- --preview
cargo run -- --preview-api
```

### CLI Flags

```
--server <URL>               OpenCode server URL
--username <USER>            HTTP Basic Auth username (default: opencode)
--password <PASS>            HTTP Basic Auth password
--cf-access-client-id <ID>   Cloudflare Access client ID
--cf-access-client-secret    Cloudflare Access client secret
--preview                    Render deterministic offline preview data
--preview-api                Exercise the v2 command/event UI with the fixture
--drawer <NAME>              Open a preview modal (settings, sessions, new-session, rename, model, level)
```

The headless visual harness in `tests/visual-capture.sh` captures a GPUI Wayland surface through nested Weston on Xvfb. `tests/stitch-visual.sh` places a GTK reference capture on the left and a GPUI capture on the right as a native 1568×791 comparison. Neither script touches the user's live desktop. The progress checklist and annotated screenshots live in the shared Obsidian note `opencode-gpui-rebuild.md`.

### Validation policy

Automated headless GUI cases and scenario runners have been removed. Do not add or run a headless regression suite on each UI iteration. `cargo test --all-targets --locked` retains core/API and pure logic tests without constructing GPUI test windows; the isolated notification-portal test and fake-server protocol tests also remain.

For UI changes, agents should build once and use targeted visual checks for the affected states. Keep using `--preview`, `--preview-api`, `tests/visual-capture.sh`, and `tests/stitch-visual.sh`; `tests/gpui-headless.sh` retains reusable private-display and input helpers, and the fake server/file portal remain available for manual checks. These are inspection tools, not an automatically enumerated acceptance suite. Never use the user's live desktop for validation.
