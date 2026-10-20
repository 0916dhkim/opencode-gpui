# OpenCode Desktop (GPUI rebuild)

An in-progress native Linux desktop client for [OpenCode v2](https://opencode.ai/v2/docs/) using GPUI Kit. This GPUI prototype is now on `main` while the last verified COSMIC client remains recoverable in Git history. It is **not ready to install as a replacement** until behavior and visual parity are accepted.

The prototype currently renders a session sidebar, transcript, composer, model and effort pickers, waiting tray, and several modals. Basic v2 bootstrap, streaming events, message send, session creation/rename, model selection, editable connection settings and per-server tab persistence are wired. Queue/switch/cancel/resume and several shortcuts and picker-keyboard paths have been exercised against the in-process preview API. Not all UI paths have been verified against a live server. Complex transcript behavior, comprehensive shortcuts, dark-theme parity, and interaction/accessibility testing still need work. Do not treat a successful preview capture as proof of those features.

## Building & Running

### Typography

The client embeds **Manrope** for interface/transcript text and **Monaspace Neon v1.400** for code, tool output and technical fields. No separate installation of these fonts is required. Existing font sizes and app zoom scaling are unchanged; system fonts still supply fallback glyphs not covered by the bundled faces.

`assets/fonts/` contains Manrope regular/medium/semibold/bold and Neon regular/medium/semibold/bold/italic/bold italic, with each upstream SIL Open Font License. Run `opencode-gpui --font-licenses` to read the full notices from the standalone executable without opening the app.

Manrope uses the static Google Fonts faces from the approved preview (WOFF containers unpacked to TTF without changing outlines). Their legacy internal family is `Manrope ExtraLight` despite their correct 400/500/600/700 weights; this is a lookup name, not the chosen weight. Neon uses the unmodified [v1.400 static release](https://github.com/githubnext/monaspace/releases/tag/v1.400). Manrope's [upstream license](https://github.com/google/fonts/blob/main/ofl/manrope/OFL.txt) and Neon's release license are preserved alongside the fonts and embedded in `--font-licenses`.

### Transcript scrolling and history

The transcript is a normal GPUI overflow-scrolling container containing **every loaded row**. It has no virtual list, detached row measurement, height cache, estimated spacers or custom layout/scroll-correction engine. GPUI handles natural layout and clipping; a plain `ScrollHandle` supplies bottom scrolling, item positions and the scrollbar. Non-flexing vertical row/header stacks use block flow, and rows receive the already-known viewport width instead of repeatedly resolving percentage widths during intrinsic sizing. Their heights still come entirely from natural layout.

Opening a session still requests only its newest history page. **Load earlier messages** requests one older page on demand, with the existing loading, duplicate-request and retry handling. Rendering loaded content does not trigger further history requests, and no loaded-row cap or automatic eviction is imposed.

While following the tail, updates use GPUI's native bottom-scroll request; manual scrolling yields that follow mode. Loading an older page keeps the same message in view by asking GPUI to align that row to the top. It does not retain an exact pixel displacement inside that message. The sticky user-context header is selected from GPUI's first visible child rather than calculated row positions. Resize/zoom reflows naturally without semantic/pixel anchor correction.

The latency report's legacy `measured_rows` field is always zero with this renderer; it does **not** mean that no rows are rendered. The old detached `transcript.row_measure` operation no longer runs.

`--preview --drawer long-transcript` supplies 1,200 already-loaded user/Markdown rows for manual top/bottom, scroll, scrollbar and resize inspection. It neither changes the normal page size nor fetches server history. Use `--preview-api` with `OPENCODE_PREVIEW_HISTORY_PAGES=1` to inspect actual on-demand page loading against the in-process fixture.

`--preview --drawer layout-profile` supplies a fixed 102-row, 357-rich-paragraph document with inline code, links, code blocks and tables, with busy jobs/animations disabled. This uses the real renderer. The manual capture helper's `INTERACTION=layout-profile` runs normal Record lag collection around a repeatable wheel sequence; compare scroll-input-paired draws separately from the post-scroll scrollbar fade. Software-container timings are not native desktop FPS or GPU measurements.

### View ownership and redraw

Repeating gear and pending-background spinner animations have persistent, keyed view entities; their frame requests notify the icon rather than the application controller. Definite-size transcript, sidebar, editor and modal-layer views use GPUI's native retained-view API. Message rows, attachment strips, notices and other naturally sized content do not have application geometry/height caches.

The transcript scrollbar is an independent sibling view of the retained content. Its native fade/hover/idle notifications repaint the thumb without rebuilding the document. An observer forwards actual native offset changes (track clicks and thumb dragging) to the controller, preserving sticky context and scroll-follow behavior. Wheel scrolling continues to use the ordinary scroll container; this does not promise layout-free active scrolling.

Press and hold the middle mouse button in the transcript for autoscroll; move above or below the marker to control direction and speed. Release the middle button to stop, including when releasing over the sidebar or another child. Speed remains linear and uncapped beyond the zoom-scaled dead zone. Escape, another mouse press, wheel input, session changes and modals retain their existing cancellation behavior.

The controller still owns application state. Transcript presentation inputs cover active session/connection, projection revision (including same-row-count streaming updates), history cursor/loading, native scrolling/follow state and code-copy feedback. The editor retains its existing input entity and tracks session/focus transitions. Sidebar and modal layers conservatively observe controller notifications. Native descendant notifications still invalidate cached ancestors for parsing, image readiness, selection, scrolling and cursor blinking. Changed inputs render immediately through GPUI's uncached path before returning to retained rendering; no arbitrary state hash or pixel-height estimate is used. Cheap application chrome and layout-changing animations remain live.

Latency reports include `transcript_build`, `sidebar_build` and `editor_build` operation totals to distinguish root frames from actual pane reconstruction. These are CPU-side inclusive build spans, not GPU timings or a native performance guarantee. `ui_build` now covers the live shell rather than all pane construction. A native cache miss can construct/layout a subtree during root prepaint, so compare whole draw timings—not old versus new root-phase percentages—as a performance measure. This is subtree construction/layout reuse, not GPU dirty-rectangle rendering: an animated icon still schedules window frames, and actual transcript scrolling still needs layout of the loaded natural-height content.

### Link destinations

Right-click a rendered Markdown link to inspect its underlying URL, including its query and fragment, then choose **Open link** or **Copy link**. Long destinations wrap in a bounded, scrollable menu header rather than being abbreviated. Labels retain native text wrapping/selection and the existing opening policy. Destination-on-hover is intentionally replaced by this explicit menu; ordinary button tooltips remain.

Normal clicks and **Open link** support HTTP, HTTPS, mailto and Obsidian (`obsidian://`) destinations through the operating system's registered handler. The original URL, including percent-encoded vault/file names, query and fragment, is passed unchanged. Obsidian must be installed and registered on that computer. Other schemes remain blocked; **Copy link** is available regardless of scheme.

GPUI Kit **0.8.0**, Base and Component are published Cargo packages, pinned by the lockfile with no local source overrides or vendored toolkit copies. They use the standard **gpui-pre 0.3.9** backend; the optional GPUI Fast backend is not enabled. The app owns its raw GPUI window root and client frame rather than mounting Component's default window-border plugin.

The small app-owned host supplies selection lifecycle, keyboard Copy/Tab traversal and input context-menu presentation. Component's native editor, focus, paste handling, themes, title bar and scrolling widgets remain in use through public APIs. The composer uses stock editor insets, not a private padding extension. Input edit menus explicitly route actions to their original editor and keep masked/read-only capabilities. The app does not use Component's root-dependent dialog/sheet APIs.

Use `--preview --drawer link-tooltips` (the retained fixture name) for URL/label, wrapped-label, long-destination and custom-scheme inspection. The visual-capture helper's `INTERACTION=link-hover` with `LINK_CONTEXT_MENU=1` right-clicks at `LINK_HOVER_X/Y`; it is inspection tooling, not an automated GUI test suite.

### Send foreground tasks to background

The shallow-detour icon beside **BACKGROUND** offers **Continue without waiting** when the active session has a running foreground shell/subagent invocation. It backgrounds all eligible tools blocking that session; the processes and child sessions keep running, the job count is unchanged, and the agent can continue. Already-backgrounded jobs, inbox delivery modes and the composer are not changed. The public running-tool predicate is approximate, so a task that finishes or has not yet entered its blocking wait can produce a harmless no-op.

**Ctrl+Shift+B** invokes the same action with the sidebar hidden. The icon is keyboard-focusable with Enter/Space activation, has a tooltip, and becomes a disabled spinner while the request is pending. Errors appear in the session's sidebar panel and leave the task states untouched for retry. The button disappears when canonical events/history no longer show eligible tools, not just because HTTP returned 204.

For an isolated interactive preview, use `OPENCODE_PREVIEW_BACKGROUND=1 cargo run -- --preview-api`. The optional preview-only `OPENCODE_PREVIEW_BACKGROUND_DELAY_MS` and `OPENCODE_PREVIEW_BACKGROUND_FAIL=1` expose pending/error states for targeted visual inspection.

### Collect a latency report

The normal client has a **Record lag** button above Tabs. **Ctrl+Shift+L** also starts/stops collection, including with the sidebar hidden or an in-app modal open.

1. Select the session to investigate, click **Record lag**, and wait for **CPU stacks: recording**. Reproduce the delay for about 10–15 seconds.
2. Click **Stop lag capture** (or press Ctrl+Shift+L).
3. Wait for **Lag report saved locally**, then use **Copy report path**. Do not close the client before saving finishes. A failed save keeps the bounded capture available through **Retry saving lag report**.
4. Provide that path when asking for analysis. The report can then be copied/read from that machine without launching the client or running a benchmark there.

For a slow/fast comparison, make **two separate captures**, one per session, at the same window size/zoom and with similar interactions. Identify which path is slow and which is fast. Do not switch sessions or resize during either capture. The report includes anonymous capture-local view ordinals, selected-session busy state and transcript byte/row counts, never session IDs/titles or text. Aggregated CPU stacks cannot be split reliably by a later session switch.

Linux x86-64 GNU builds also sample native function stacks at a requested 99Hz, with a **30-second sampling limit**. The timeline continues after that limit; stop/save and start a fresh capture if needed. Sampling initialization, snapshot, teardown, symbolization and export run on a dedicated worker. The saved JSONL contains the sampling status and actual start/snapshot/stop offsets, UI versus other-thread observed stacks, missing/native-depth/export-truncation metadata, and root layout-request, layout-gap, prepaint and paint intervals with wall and Linux current-thread CPU time. The layout gap includes surrounding framework work, not just the layout solver. Sampler unavailability or missing UI symbols is explicit; the timeline is still saved.

CPU sampling uses pinned `pprof-rs`/framehop, requires no sudo or installed profiler, and refuses conflicting SIGPROF handlers/timers. It is opt-in and adds overhead; no numerical overhead claim is made. Its fixed buffers can spill to a private temporary file, and it has no complete sampling-loss counter or strict temporary-storage quota. Only exported stacks are capped (512 stacks, 96 names per stack and 4MiB of stack JSON). Function names are exported; thread names/IDs, source/module paths and raw addresses are not. Unresolved frames may be removed by the upstream symbolizer, so the first resolved function is not guaranteed to be the real leaf. Samples are process-CPU-triggered observations, not exact per-thread CPU utilization, an off-CPU wait profile or timestamped stacks for individual stalls.

The optimized release executable now **retains symbols and line tables** so deployed profiles can resolve function names. It is larger than the old stripped executable; do not strip the diagnostic binary. No conversation content is stored in those symbols. CPU sampling stops on Stop, the sampling deadline or window close; the upstream singleton retains allocated buffers/an empty private temporary file after first use. No active sampling worker/timer is started before recording.

Reports are private `lag-*.jsonl` files under the local data directory's `opencode-gpui/diagnostics/` (normally `~/.local/share/opencode-gpui/diagnostics/` on Linux). Nothing is uploaded automatically. They contain categorized input/dispatch and edit markers, main-executor scheduling delay, paired GPUI draw/platform-call timings, slow named app-operation spans and technical counts/context. Keys, message contents, images, server URLs, credentials and user paths are not recorded in report contents. A source fingerprint identifies the instrumented build.

The recorder keeps a bounded rolling buffer (32,768 events) and reports overwritten events, unmatched/dropped inputs and GPUI journal gaps. It preserves individual slow spans rather than only percentile summaries. Saving/serialization happens off the UI thread. Custom collection/probes are off by default; the compiled GPUI profiler still performs its own bounded journal bookkeeping. The single-flight scheduling probe does not notify, refresh, or request frames.

Legacy operation/draw/submission durations are elapsed wall time, not exclusive CPU execution. The new phase `cpu_us` values instead measure inclusive current-thread CPU time. Sampler start/snapshot/stop offsets bracket worker lifecycle boundaries, not exact first/last sample times. Input receipt is not physical key-press time; edit notifications have independent IDs rather than guessed key causality. A correlated frame is the first paired draw starting after an edit/post-dispatch marker, not proof that the change became visible. The platform draw-call endpoint is **not GPU completion, compositor presentation or scanout**, and does not guarantee successful submission. Synchronous inline-image preparation is timed; GPUI's asynchronous image decode is not separately timed. Read the report's header semantics and gap/drop fields before interpreting it.

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
--font-licenses              Print bundled font notices and exit (no GUI)
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
