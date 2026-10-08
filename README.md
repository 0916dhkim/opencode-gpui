# OpenCode Desktop (GPUI rebuild)

An in-progress native Linux desktop client for [OpenCode v2](https://opencode.ai/v2/docs/) using GPUI Kit. This `rebuild/gpui` branch replaces the old libcosmic UI while retaining the v2 transport and state model. It is **not ready to install as a replacement**; the last verified COSMIC client remains on `main` until behavior and visual parity are complete.

The prototype currently renders a session sidebar, transcript, composer, model and effort pickers, waiting tray, and several modals. Basic v2 bootstrap, streaming events, message send, session creation/rename, model selection, editable connection settings and per-server tab persistence are wired. Queue/switch/cancel/resume and several shortcuts and picker-keyboard paths have been exercised against the in-process preview API. Not all UI paths have been verified against a live server. Permission cards, robust Markdown/virtualized transcript behavior, comprehensive shortcuts, dark-theme parity, and interaction/accessibility testing still need work. Do not treat a successful preview capture as proof of those features.

## Building & Running

### Dependencies

Requires Rust 1.93+ (Rust 2024 edition) and Linux graphics development libraries for GPUI Kit (Wayland/X11, Vulkan and fontconfig). The repository Dockerfile contains an amd64 development/test environment; it is not a minimal runtime image.

Build for Linux:

```bash
cargo build --release
```

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
