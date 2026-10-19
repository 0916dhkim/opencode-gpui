# OpenCode Desktop (GPUI rebuild)

An in-progress native Linux desktop client for [OpenCode v2](https://opencode.ai/v2/docs/) using GPUI Kit. This GPUI prototype is now on `main` while the last verified COSMIC client remains recoverable in Git history. It is **not ready to install as a replacement** until behavior and visual parity are accepted.

The prototype currently renders a session sidebar, transcript, composer, model and effort pickers, waiting tray, and several modals. Basic v2 bootstrap, streaming events, message send, session creation/rename, model selection, editable connection settings and per-server tab persistence are wired. Queue/switch/cancel/resume and several shortcuts and picker-keyboard paths have been exercised against the in-process preview API. Not all UI paths have been verified against a live server. Complex transcript behavior, comprehensive shortcuts, dark-theme parity, and interaction/accessibility testing still need work. Do not treat a successful preview capture as proof of those features.

## Building & Running

### Transcript scrolling and history

The transcript is a normal GPUI overflow-scrolling column containing **every loaded row**. It has no virtual list, detached row measurement, height cache, estimated spacers or custom layout/scroll-correction engine. GPUI handles natural layout and clipping; a plain `ScrollHandle` supplies bottom scrolling, item positions and the scrollbar.

Opening a session still requests only its newest history page. **Load earlier messages** requests one older page on demand, with the existing loading, duplicate-request and retry handling. Rendering loaded content does not trigger further history requests, and no loaded-row cap or automatic eviction is imposed.

While following the tail, updates use GPUI's native bottom-scroll request; manual scrolling yields that follow mode. Loading an older page keeps the same message in view by asking GPUI to align that row to the top. It does not retain an exact pixel displacement inside that message. The sticky user-context header is selected from GPUI's first visible child rather than calculated row positions. Resize/zoom reflows naturally without semantic/pixel anchor correction.

The latency report's legacy `measured_rows` field is always zero with this renderer; it does **not** mean that no rows are rendered. The old detached `transcript.row_measure` operation no longer runs.

`--preview --drawer long-transcript` supplies 1,200 already-loaded user/Markdown rows for manual top/bottom, scroll, scrollbar and resize inspection. It neither changes the normal page size nor fetches server history. Use `--preview-api` with `OPENCODE_PREVIEW_HISTORY_PAGES=1` to inspect actual on-demand page loading against the in-process fixture.

### Send foreground tasks to background

The shallow-detour icon beside **BACKGROUND** offers **Continue without waiting** when the active session has a running foreground shell/subagent invocation. It backgrounds all eligible tools blocking that session; the processes and child sessions keep running, the job count is unchanged, and the agent can continue. Already-backgrounded jobs, inbox delivery modes and the composer are not changed. The public running-tool predicate is approximate, so a task that finishes or has not yet entered its blocking wait can produce a harmless no-op.

**Ctrl+Shift+B** invokes the same action with the sidebar hidden. The icon is keyboard-focusable with Enter/Space activation, has a tooltip, and becomes a disabled spinner while the request is pending. Errors appear in the session's sidebar panel and leave the task states untouched for retry. The button disappears when canonical events/history no longer show eligible tools, not just because HTTP returned 204.

For an isolated interactive preview, use `OPENCODE_PREVIEW_BACKGROUND=1 cargo run -- --preview-api`. The optional preview-only `OPENCODE_PREVIEW_BACKGROUND_DELAY_MS` and `OPENCODE_PREVIEW_BACKGROUND_FAIL=1` expose pending/error states for targeted visual inspection.

### Collect a latency report

The normal client has a **Record lag** button above Tabs. **Ctrl+Shift+L** also starts/stops collection, including with the sidebar hidden or an in-app modal open.

1. Click **Record lag** and use the client normally while the delay occurs.
2. Click **Stop lag capture** (or press Ctrl+Shift+L).
3. Wait for **Lag report saved locally**, then use **Copy report path**. Do not close the client before saving finishes. A failed save keeps the bounded capture available through **Retry saving lag report**.
4. Provide that path when asking for analysis. The report can then be copied/read from that machine without launching the client or running a benchmark there.

Reports are private `lag-*.jsonl` files under the local data directory's `opencode-gpui/diagnostics/` (normally `~/.local/share/opencode-gpui/diagnostics/` on Linux). Nothing is uploaded automatically. They contain categorized input/dispatch and edit markers, main-executor scheduling delay, paired GPUI draw/platform-call timings, slow named app-operation spans and technical counts/context. Keys, message contents, images, server URLs, credentials and user paths are not recorded in report contents. A source fingerprint identifies the instrumented build.

The recorder keeps a bounded rolling buffer (32,768 events) and reports overwritten events, unmatched/dropped inputs and GPUI journal gaps. It preserves individual slow spans rather than only percentile summaries. Saving/serialization happens off the UI thread. Custom collection/probes are off by default; the compiled GPUI profiler still performs its own bounded journal bookkeeping. The single-flight scheduling probe does not notify, refresh, or request frames.

All durations are elapsed time, not exclusive CPU execution. Input receipt is not physical key-press time; edit notifications have independent IDs rather than guessed key causality. A correlated frame is the first paired draw starting after an edit/post-dispatch marker, not proof that the change became visible. The platform draw-call endpoint is **not GPU completion, compositor presentation or scanout**, and does not guarantee successful submission. Synchronous inline-image preparation is timed; GPUI's asynchronous image decode is not separately timed. Read the report's header semantics and gap/drop fields before interpreting it.

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

For targeted long-label inspection, set `OPENCODE_PREVIEW_MODEL_NAME="GPT-5.6 Long Context Preview"` on a preview run. The composer shows only the model name with a flexible width; the dropdown retains provider details. This override never affects a normal server catalog.

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
