//! GPUI Kit shell backed by the v2 transport, or its deterministic preview fixture.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use gpui_kit::base::{
    Button as BaseButton, Checkbox, CheckboxIndicator, CheckboxState, FocusTrapElement,
};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::text::TextView;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable, TITLE_BAR_HEIGHT, TitleBar};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use gpui_kit::{Animation, Transformation, percentage};
use opencode_gpui::{
    api::{ApiConfig, ApiHandle, Command, InboxRequest, MessageLoadError, ServerEnvelope, UiEvent},
    background,
    credentials::{self, CloudflareAccessCredentials, PasswordTarget, SystemKeyring},
    fonts::{self, MONO_FONT_FAMILY, UI_FONT_FAMILY},
    jobs::{self, JobKind, JobRow, Jobs},
    latency::{self, Action as TraceAction, Operation as TraceOperation},
    model::{
        self, Conversation, ModelCatalog, ModelSelection, Project, RunStatus, Session,
        SessionModel, TranscriptRow, TranscriptRowKey, TranscriptRowKind,
    },
    notifications::{DesktopNotifications, notification_id, play_session_ready_sound},
    pending::{self, Forms, PendingRequest},
    persist::{self, ConnectionSettings, PersistedState, PersistedTab},
    preview, protocol, tray,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Deserialize;

use crate::Args;

fn tab_number_key(key: &str) -> Option<usize> {
    match key {
        "1" | "numpad1" | "kp_1" => Some(0),
        "2" | "numpad2" | "kp_2" => Some(1),
        "3" | "numpad3" | "kp_3" => Some(2),
        "4" | "numpad4" | "kp_4" => Some(3),
        "5" | "numpad5" | "kp_5" => Some(4),
        "6" | "numpad6" | "kp_6" => Some(5),
        "7" | "numpad7" | "kp_7" => Some(6),
        "8" | "numpad8" | "kp_8" => Some(7),
        "9" | "numpad9" | "kp_9" => Some(8),
        _ => None,
    }
}

const ZOOM_STEPS: [f64; 9] = [0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.5, 1.75];

fn normalized_zoom(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(ZOOM_STEPS[0], *ZOOM_STEPS.last().unwrap())
    } else {
        1.0
    }
}

fn stepped_zoom(current: f64, direction: i8) -> f64 {
    let current = normalized_zoom(current);
    match direction {
        1 => ZOOM_STEPS
            .into_iter()
            .find(|step| *step > current + 0.04)
            .unwrap_or(*ZOOM_STEPS.last().unwrap()),
        -1 => ZOOM_STEPS
            .into_iter()
            .rev()
            .find(|step| *step < current - 0.04)
            .unwrap_or(ZOOM_STEPS[0]),
        _ => 1.0,
    }
}

/// Old prototype builds wrote tabs under the configured spelling, while the
/// transport restores them under its normalized mount-root key. Copy rather
/// than remove that entry so the older client/state remains recoverable.
fn ensure_canonical_server_state(state: &mut PersistedState, configured: &str, key: &str) {
    if !state.servers.contains_key(key)
        && let Some(saved) = state.servers.get(configured.trim_end_matches('/')).cloned()
    {
        state.servers.insert(key.to_owned(), saved);
    }
}

fn unread_on_server_switch(
    state: &mut PersistedState,
    previous: Option<(&str, &HashSet<String>)>,
    next_key: &str,
) -> HashSet<String> {
    if let Some((previous_key, unread)) = previous {
        state
            .servers
            .entry(previous_key.trim_end_matches('/').to_owned())
            .or_default()
            .unread = unread.clone();
    }
    state
        .servers
        .get(next_key)
        .map(|saved| saved.unread.clone())
        .unwrap_or_default()
}

fn apply_missed_idle(
    offline_busy: &mut HashSet<String>,
    statuses: &HashMap<String, RunStatus>,
    open_tabs: &[String],
    unread: &mut HashSet<String>,
    complete: bool,
) {
    for id in std::mem::take(offline_busy) {
        if statuses.get(&id).is_some_and(RunStatus::is_busy)
            || (!complete && !statuses.contains_key(&id))
        {
            offline_busy.insert(id);
        } else if open_tabs.contains(&id) {
            unread.insert(id);
        }
    }
}

fn model_button_presentation(
    catalog: &ModelCatalog,
    selected: Option<&ModelSelection>,
) -> (String, Option<u64>) {
    if catalog.models.is_empty() {
        return ("No models available".into(), None);
    }
    match selected {
        Some(selection) => catalog.find(selection).map_or_else(
            || (selection.model_id.clone(), None),
            |option| {
                // Catalog/dropdown labels retain the routing provider. Only
                // the collapsed composer control shows the model name alone.
                let name = option
                    .label
                    .strip_prefix(option.provider_id.as_str())
                    .and_then(|suffix| suffix.strip_prefix(" / "))
                    .unwrap_or(&option.label);
                (name.to_owned(), option.context_limit)
            },
        ),
        None => ("Choose model".into(), None),
    }
}

fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q_lower: Vec<char> = query.to_lowercase().chars().collect();
    let t_lower: Vec<char> = target.to_lowercase().chars().collect();
    let original: Vec<char> = target.chars().collect();
    let mut q_idx = 0;
    let mut score = 0;
    let mut previous = None;
    let mut first = None;
    for (index, &character) in t_lower.iter().enumerate() {
        if q_idx < q_lower.len() && character == q_lower[q_idx] {
            first.get_or_insert(index);
            score += 10;
            if previous.is_some_and(|last| last + 1 == index) {
                score += 15;
            }
            if index == 0 {
                score += 30;
            } else {
                let prior = original[index - 1];
                if matches!(prior, ' ' | '-' | '_' | '/' | '.' | ':') {
                    score += 25;
                } else if prior.is_lowercase() && original[index].is_uppercase() {
                    score += 20;
                }
            }
            previous = Some(index);
            q_idx += 1;
        }
    }
    if q_idx < q_lower.len() {
        return None;
    }
    let lowered = target.to_lowercase();
    let query_lowered = query.to_lowercase();
    if let Some(index) = lowered.find(&query_lowered) {
        score += 50;
        if index == 0 {
            score += 25;
        }
    }
    if let (Some(first), Some(last)) = (first, previous) {
        score -= last.saturating_sub(first) as i64;
    }
    score -= t_lower.len() as i64 / 4;
    Some(score)
}

const SESSION_PICKER_LIMIT: usize = 200;

fn filter_tab_sessions<'a>(
    sessions: &'a [Session],
    open_tabs: &[String],
    query: &str,
) -> Vec<&'a Session> {
    let query = query.trim();
    let mut scored: Vec<_> = open_tabs
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let session = sessions.iter().find(|session| &session.id == id)?;
            let score = if query.is_empty() {
                Some(0)
            } else {
                fuzzy_score(query, &session.title).max(fuzzy_score(query, &session.directory))
            };
            score.map(|score| (score, index, session))
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    scored
        .into_iter()
        .take(SESSION_PICKER_LIMIT)
        .map(|(_, _, session)| session)
        .collect()
}

fn filter_all_sessions<'a>(sessions: &'a [Session], query: &str) -> Vec<&'a Session> {
    let query = query.trim();
    let mut scored: Vec<_> = sessions
        .iter()
        .filter_map(|session| {
            let score = if query.is_empty() {
                Some(0)
            } else {
                match (
                    fuzzy_score(query, &session.title),
                    fuzzy_score(query, &session.directory),
                ) {
                    (Some(title), Some(directory)) => Some(title.max(directory)),
                    (title, directory) => title.or(directory),
                }
            };
            score.map(|score| (score, session.time.updated, session))
        })
        .collect();
    scored.sort_by(|a, b| {
        if query.is_empty() {
            b.1.cmp(&a.1)
        } else {
            b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1))
        }
    });
    scored
        .into_iter()
        .take(SESSION_PICKER_LIMIT)
        .map(|(_, _, session)| session)
        .collect()
}

fn needs_new_connection(current: &ApiConfig, candidate: &ApiConfig, connected: bool) -> bool {
    !connected || current != candidate
}

fn safe_connection_error(error: Option<&str>) -> &'static str {
    let Some(error) = error else {
        return "Connection lost";
    };
    let lower = error.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("unauthorized") {
        "Authentication failed (401)"
    } else if lower.contains("403") || lower.contains("forbidden") {
        "Access denied (403)"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "Connection timed out"
    } else if lower.contains("refused") {
        "Connection refused"
    } else {
        "Connection failed"
    }
}

fn filter_models<'a>(models: &'a [model::ModelOption], query: &str) -> Vec<&'a model::ModelOption> {
    let query = query.trim();
    let mut scored: Vec<_> = models
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let score = if query.is_empty() {
                Some(0)
            } else {
                fuzzy_score(query, &option.label).max(fuzzy_score(
                    query,
                    &format!("{} / {}", option.provider_id, option.model_id),
                ))
            };
            score.map(|score| (score, index, option))
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.label.cmp(&b.2.label)));
    }
    scored.into_iter().map(|(_, _, option)| option).collect()
}

fn filter_levels(variants: &[String], query: &str) -> Vec<Option<String>> {
    let query = query.trim();
    let mut scored: Vec<_> = std::iter::once(None)
        .chain(variants.iter().cloned().map(Some))
        .enumerate()
        .filter_map(|(index, variant)| {
            let label = variant.as_deref().unwrap_or("Default");
            let score = if query.is_empty() {
                Some(0)
            } else {
                fuzzy_score(query, label)
            };
            score.map(|score| (score, index, variant))
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                a.2.as_deref()
                    .unwrap_or("Default")
                    .cmp(b.2.as_deref().unwrap_or("Default"))
            })
        });
    }
    scored.into_iter().map(|(_, _, variant)| variant).collect()
}

fn filter_new_session_projects(
    projects: &[Project],
    sessions: &[Session],
    active_directory: Option<&str>,
    query: &str,
) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let query = query.trim();
    let mut scored: Vec<_> = projects
        .iter()
        .map(|project| &project.worktree)
        .chain(sessions.iter().map(|session| &session.directory))
        .filter(|path| !path.is_empty() && seen.insert((*path).clone()))
        .filter_map(|path| {
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_owned();
            let score = if query.is_empty() {
                Some(if active_directory == Some(path.as_str()) {
                    1000
                } else {
                    0
                })
            } else {
                match (fuzzy_score(query, &name), fuzzy_score(query, path)) {
                    (Some(name), Some(path)) => Some(name.max(path)),
                    (name, path) => name.or(path),
                }
            };
            score.map(|score| (score, name, path.clone()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .map(|(_, name, path)| (name, path))
        .collect()
}

fn new_session_choice(choices: &[(String, String)], index: usize, query: &str) -> Option<String> {
    choices
        .get(index)
        .or_else(|| choices.first())
        .map(|(_, path)| path.clone())
        .or_else(|| (!query.trim().is_empty()).then(|| query.trim().to_owned()))
}

fn picker_list_height(model: bool, count: usize) -> f32 {
    let (row, minimum, maximum) = if model {
        (52., 100., 280.)
    } else {
        (33., 80., 240.)
    };
    let gap = if model { 8. } else { 0. };
    (count as f32 * row + count.saturating_sub(1) as f32 * gap).clamp(minimum, maximum)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabAttention {
    Busy,
    Unread,
    Read,
}

/// Activity shape and attention color are independent: a background job can
/// keep the gear visible while an idle, read tab stays gray.
fn tab_indicator(busy: bool, unread: bool, has_jobs: bool) -> (bool, TabAttention) {
    let attention = if busy {
        TabAttention::Busy
    } else if unread {
        TabAttention::Unread
    } else {
        TabAttention::Read
    };
    (busy || has_jobs, attention)
}

/// Foreground work and background jobs both rotate, regardless of attention color.
fn status_gear(color: Rgba, id: String) -> AnyElement {
    Icon::default()
        .data(include_bytes!("icons/settings.svg"))
        .with_size(px(14.))
        .text_color(color)
        .with_animation(
            id,
            Animation::new(Duration::from_secs(1)).repeat(),
            |gear, phase| gear.transform(Transformation::rotate(percentage(phase))),
        )
        .into_any_element()
}

#[derive(Clone)]
struct TabDrag(String);

struct TabDragPreview {
    title: String,
    dark: bool,
}

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(34.))
            .min_w(px(180.))
            .px(px(12.))
            .flex()
            .items_center()
            .rounded(px(6.))
            .bg(rgb(if self.dark { 0x22262a } else { 0xe3e0da }))
            .text_color(rgb(if self.dark { 0xe8e5df } else { 0x555b5c }))
            .child(self.title.clone())
    }
}

/// Insert relative to the target's midpoint, accounting for removal of the source.
fn reorder_tab_ids(tabs: &mut Vec<String>, source: &str, target: &str, after: bool) -> bool {
    if source == target || !tabs.iter().any(|id| id == target) {
        return false;
    }
    let Some(source_index) = tabs.iter().position(|id| id == source) else {
        return false;
    };
    let moved = tabs.remove(source_index);
    let target_index = tabs
        .iter()
        .position(|id| id == target)
        .expect("target checked");
    let destination = target_index + usize::from(after);
    if destination == source_index {
        tabs.insert(source_index, moved);
        return false;
    }
    tabs.insert(destination, moved);
    true
}

#[derive(Clone)]
struct LinkMenu {
    url: String,
    position: Point<Pixels>,
}

#[derive(Clone)]
struct TranscriptAutoscroll {
    session: String,
    origin: Point<Pixels>,
    pointer: Point<Pixels>,
    token: u64,
}

fn autoscroll_step(distance: Pixels, zoom: f32) -> Pixels {
    let distance = distance.as_f32();
    let beyond_dead_zone = (distance.abs() - 16. * zoom).max(0.);
    if beyond_dead_zone == 0. {
        px(0.)
    } else {
        // Linear from zero outside the dead zone, without a speed floor or cap.
        let speed = beyond_dead_zone / 8.;
        px(-speed.copysign(distance))
    }
}

fn wall_clock_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn may_open_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https" | "mailto"))
}

#[derive(Default)]
struct LatencyCapture {
    collector: Option<Rc<RefCell<profiler::journal::ForegroundJournalCollector>>>,
    task: Option<Task<()>>,
    window_id: Option<WindowId>,
    saving: bool,
    report: Option<PathBuf>,
    failed: Option<latency::Report>,
    copied: bool,
}

impl Drop for LatencyCapture {
    fn drop(&mut self) {
        self.task.take();
        if self.collector.is_some() {
            // Deliberate Stop is needed for export; never leave probes enabled.
            latency::global().stop();
        }
    }
}

fn collect_latency(
    collector: &mut profiler::journal::ForegroundJournalCollector,
    window: WindowId,
) {
    let _latency = latency::global().span(TraceOperation::TraceCollect, 1);
    use profiler::journal::{
        ForegroundEvent as E, ForegroundJournalEntry as J, IntervalBoundary as B,
    };
    let trace = latency::global();
    for entry in collector.collect_unseen().entries {
        match entry {
            J::Discontinuity { lost } => trace.gap(lost),
            J::Event(E::Input(input)) => {
                let action = match input.kind {
                    "key_down" => TraceAction::Key,
                    "mouse_up" => TraceAction::Pointer,
                    "scroll_wheel" => TraceAction::Scroll,
                    _ => continue,
                };
                trace.dispatch(input.start, input.end, action, input.caused_invalidation);
            }
            J::Event(E::Draw(frame)) if frame.window_id == window => {
                trace.draw(
                    frame.draw_start,
                    frame.draw_end,
                    frame.invalidations,
                    frame.dirty_at,
                );
            }
            J::Boundary(B::Presented(frame)) if frame.frame.window_id == window => {
                trace.submitted(
                    frame.frame.draw_start,
                    frame.frame.draw_end,
                    frame.presentation.present_start,
                    frame.presentation.present_end,
                );
            }
            _ => {}
        }
    }
}

fn trace_input(action: TraceAction, cx: &mut App) {
    if let Some(token) = latency::global().input(action) {
        cx.defer(move |_| latency::global().dispatched(token));
    }
}

struct Client {
    latency: LatencyCapture,
    dark: bool,
    api: Option<ApiHandle>,
    preview_api: bool,
    connection_generation: u64,
    bootstrap_in_flight: bool,
    bootstrap_after_load: bool,
    bootstrap_retry_scheduled: bool,
    bootstrap_retry_count: u8,
    bootstrap_events: Vec<ServerEnvelope>,
    bootstrap_directories: Vec<String>,
    refresh_open_tabs: bool,
    settings: SettingsFields,
    saved_active: Option<String>,
    restored_server_state: bool,
    connection_status: String,
    disconnected: bool,
    sse_connected_once: bool,
    server_version: Option<String>,
    conversations: HashMap<String, Conversation>,
    loading_messages: HashMap<String, Option<String>>,
    message_load_errors: HashMap<String, Option<String>>,
    /// Locally accepted IDs while an older newest-history request was in
    /// flight. Its stale response cannot prove those sends are absent.
    skip_prune_for_load: HashMap<String, HashSet<String>>,
    message_events_during_load: HashMap<String, Vec<protocol::Event>>,
    reload_after_load: HashSet<String>,
    follow_bottom: bool,
    transcript_pointer_down: bool,
    catalogs: HashMap<String, ModelCatalog>,
    model_retry_count: HashMap<String, u8>,
    model_retry_scheduled: HashSet<String>,
    running_jobs: Jobs,
    jobs_elapsed_refresh_active: bool,
    sessions: Vec<Session>,
    open_tabs: Vec<String>,
    sidebar_visible: bool,
    sidebar_auto_hidden: bool,
    sidebar_zoom_initialized: bool,
    sidebar_zoom_width: Pixels,
    sidebar_toggle_focus: FocusHandle,
    tab_focus: HashMap<String, [FocusHandle; 3]>,
    tab_shortcut_hint: bool,
    tab_drop_target: Option<(String, bool)>,
    tab_drop_outgoing: HashMap<(String, bool), u64>,
    next_tab_drop_token: u64,
    tab_drop_anchor: Option<(String, Pixels)>,
    bootstrapped: bool,
    projects: Vec<Project>,
    active: String,
    transcript: HashMap<String, Vec<TranscriptRow>>,
    transcript_scrolls: HashMap<String, ScrollHandle>,
    transcript_spans: HashMap<String, Vec<MessageSpan>>,
    attachments: ImageCache,
    catalog: ModelCatalog,
    composer: Entity<TextareaState>,
    composer_action_focus: [FocusHandle; 5],
    composer_placeholder_focused: bool,
    composer_session: String,
    composers: HashMap<String, Entity<TextareaState>>,
    attachments_draft: Vec<PathBuf>,
    attachment_drafts: HashMap<String, Vec<PathBuf>>,
    owned_pastes: HashMap<PathBuf, Arc<tempfile::NamedTempFile>>,
    overlay: Option<String>,
    copy_feedback: Option<CopyFeedback>,
    next_copy_feedback_token: u64,
    link_menu: Option<LinkMenu>,
    link_menu_focus: [FocusHandle; 2],
    autoscroll: Option<TranscriptAutoscroll>,
    next_autoscroll_token: u64,
    scroll: ScrollHandle,
    transcript_content_width: Pixels,
    history_focus: FocusHandle,
    retry_history_focus: FocusHandle,
    sessions_picker_scroll: ScrollHandle,
    picker_list_scroll: ScrollHandle,
    projects_picker_scroll: ScrollHandle,
    picker_choice_focus: HashMap<String, FocusHandle>,
    unread: HashSet<String>,
    /// Busy tabs remembered while offline, until a complete status snapshot
    /// can distinguish a still-running turn from a missed completion.
    offline_busy: HashSet<String>,
    statuses: HashMap<String, RunStatus>,
    jobs: Vec<JobRow>,
    background_requests: background::Requests,
    background_focus: FocusHandle,
    terminal_shells: HashMap<String, protocol::ShellExited>,
    desktop_notifications: Option<DesktopNotifications>,
    notification_targets: HashMap<String, String>,
    forms: Forms,
    form_cancel_focus: FocusHandle,
    form_cancel_presented: Option<String>,
    permissions: Vec<PendingPermission>,
    permission_in_flight: HashSet<String>,
    permission_container_focus: FocusHandle,
    permission_focus: [FocusHandle; 3],
    permission_presented: Option<String>,
    settings_tab_focus: [FocusHandle; 2],
    settings_action_focus: [FocusHandle; 2],
    settings_session_focus: HashMap<String, FocusHandle>,
    settings_sessions_scroll: ScrollHandle,
    settings_highlight: Option<usize>,
    child_parents: HashMap<String, String>,
    next_prompt_request_id: u64,
    next_session_request_id: u64,
    focus_composer_pending: bool,
    next_model_request_id: u64,
    model_switches: HashMap<String, PendingModelPick>,
    pending_prompts: HashMap<String, PendingPromptSend>,
    failed_prompt_drafts: HashMap<String, FailedPromptDraft>,
    confirmed_prompt_residue: HashMap<String, FailedPromptDraft>,
    tray_in_flight: HashSet<String>,
    draft_actions: Vec<DraftAction>,
    composer_edit_generation: HashMap<String, u64>,
    local_busy: HashSet<String>,
    local_run_message_ids: HashMap<String, String>,
    deferred_abort: HashSet<String>,
    abort_timeout_tokens: HashMap<String, u64>,
    next_abort_timeout_token: u64,
    modal: Option<Modal>,
    picker_control_centers: Rc<RefCell<[Option<Pixels>; 2]>>,
    modal_container_focus: FocusHandle,
    rename_target: Option<String>,
    rename_action_focus: [FocusHandle; 3],
    rename_pending: Option<u64>,
    rename_error: Option<String>,
    picker_highlight: Option<usize>,
    search: Entity<InputState>,
    rename: Entity<InputState>,
}

#[derive(Clone)]
struct PendingPermission {
    request: protocol::PermissionRequest,
    directory: Option<String>,
}

struct CopyFeedback {
    key: String,
    label: &'static str,
    token: u64,
}

struct SettingsFields {
    persisted: PersistedState,
    current: ApiConfig,
    server: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    session_search: Entity<InputState>,
    tab: SettingsTab,
    remember_password: bool,
    error: Option<String>,
    warning: Option<String>,
}

impl SettingsFields {
    fn reset_draft(&mut self, window: &mut Window, cx: &mut Context<Client>) {
        let server = self.current.base_url.clone();
        let username = self.current.username.clone();
        let client_id = self
            .current
            .cloudflare_access
            .as_ref()
            .map_or(String::new(), |token| token.client_id.clone());
        let password_hint = if self.persisted.connection.basic_auth_in_keyring {
            "Stored in the system keyring"
        } else if self.current.password.is_some() {
            "Leave blank to keep the current password"
        } else {
            "Required by OpenCode 2.x"
        };
        let secret_hint = if self.current.cloudflare_access.is_some() {
            "Stored in the system keyring"
        } else {
            "Optional"
        };
        self.server
            .update(cx, |input, cx| input.set_value(server, window, cx));
        self.username
            .update(cx, |input, cx| input.set_value(username, window, cx));
        self.client_id
            .update(cx, |input, cx| input.set_value(client_id, window, cx));
        self.password.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(password_hint, window, cx);
        });
        self.client_secret.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(secret_hint, window, cx);
        });
        self.remember_password = true;
        self.error = None;
    }

    fn new(
        window: &mut Window,
        cx: &mut Context<Client>,
        persisted: PersistedState,
        current: ApiConfig,
    ) -> Self {
        let remember_password = true;
        let password_hint = if persisted.connection.basic_auth_in_keyring {
            "Stored in the system keyring"
        } else if current.password.is_some() {
            "Leave blank to keep the current password"
        } else {
            "Required by OpenCode 2.x"
        };
        let (id, secret) = current.cloudflare_access.as_ref().map_or_else(
            || (String::new(), "Optional"),
            |token| (token.client_id.clone(), "Stored in the system keyring"),
        );
        Self {
            server: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(current.base_url.clone())
                    .placeholder("https://opencode.example.com")
            }),
            username: cx
                .new(|cx| InputState::new(window, cx).default_value(current.username.clone())),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(password_hint)
            }),
            client_id: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(id)
                    .placeholder("Optional")
            }),
            client_secret: cx
                .new(|cx| InputState::new(window, cx).masked(true).placeholder(secret)),
            session_search: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Search sessions by title or path...")
            }),
            tab: SettingsTab::Connection,
            remember_password,
            persisted,
            current,
            error: None,
            warning: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Modal {
    Settings,
    Sessions,
    NewSession,
    Rename,
    Model,
    Level,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Connection,
    Sessions,
}

#[derive(Debug, PartialEq, Eq)]
enum MarkdownBlock {
    Heading(u8, String),
    Paragraph(String),
    List(Vec<String>),
    Code(String, String),
    NestedCode {
        language: String,
        content: String,
        list_depth: usize,
        quote_depth: usize,
    },
    Structured {
        content: String,
        marker: Option<String>,
        list_depth: usize,
        quote_depth: usize,
        heading: Option<u8>,
    },
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Rule,
}

const COMPLEX_MARKDOWN_PREVIEW: &str = "> A quoted explanation of the clip alignment.\n\n| Part | Size |\n| --- | ---: |\n| paperclip | 22px |\n| send | 22px |\n\n1. Keep the inner wire visible\n   - Check at both theme settings\n2. Match the composer actions\n\n- [x] Measure the icon\n- [ ] Verify the layout\n\n```rust\npaperclip_icon(COMPOSER_ICON_PX)\n```";
const LONG_BULLET_OVERFLOW_PREVIEW: &str = "- The parent session (`opencode-gpui`) is waiting on an explore subagent, \"Audit idle send queue flash\" (`ses_ece0a1bdfffes1R6C9nEGp5Z77`).\n\n- The subagent's last model request started at 17:13:47 and never got a single token back. [Open log](https://example.com/log).";
const LONG_ORDERED_LIST_PREVIEW: &str = r#"A long answer with rich list items and linked source locations. The remaining differences are:

1. **User-row entry edge is missing.** GTK gives every `.user-message` a 1px *top* border as well as its bottom border. See [GPUI row rendering](/test/worktrees/really-long-reproduction-project/source/tree/src/gpui_ui.rs#L6576) versus `reference:src/style.css`.
2. **Sidebar footer has extra, full-width rules.** GPUI always renders a top border even without jobs, while GTK hides the section. See [GPUI sidebar](/test/worktrees/really-long-reproduction-project/source/tree/src/gpui_ui.rs#L6320).
3. **Sticky user prompt is styled differently.** The reference applies `.message-row.user-message.sticky-message`; the GPUI row has a muted role color. See [sticky row](/test/worktrees/really-long-reproduction-project/source/tree/src/gpui_ui.rs#L7271).
4. **Header and tab separators differ slightly.** Long lines use nearby but different theme colors. See [session header](/test/worktrees/really-long-reproduction-project/source/tree/src/gpui_ui.rs#L6251)."#;
const SIDEBAR_WIDTH: f32 = 270.;
const TRANSCRIPT_SCROLLBAR_GUTTER: f32 = 14.;

/// One entry for every message, including undelivered prompts with zero rows.
/// The epoch prevents an unrelated replacement snapshot with colliding IDs
/// and revision counters from reusing an old projection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageSpan {
    id: String,
    revision: u64,
    in_tray: bool,
    row_count: usize,
    epoch: u64,
}

impl MessageSpan {
    fn matches(&self, message: &model::ChatMessage, epoch: u64) -> bool {
        #[cfg(test)]
        SPAN_MATCHES.with(|count| count.set(count.get() + 1));
        self.id == message.id
            && self.revision == message.render_revision()
            && self.in_tray == message.in_tray()
            && self.epoch == epoch
    }
}

#[cfg(test)]
thread_local! {
    static SPAN_MATCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct ProjectionChange {
    range: std::ops::Range<usize>,
    removed_images: Vec<(TranscriptRowKey, usize)>,
}

/// Keep the unchanged message prefix and suffix intact. Their row indices may
/// shift after a prepend, but row identity and decoded image ownership do not.
fn splice_transcript(
    conversation: &Conversation,
    rows: &mut Vec<TranscriptRow>,
    spans: &mut Vec<MessageSpan>,
) -> Option<ProjectionChange> {
    splice_transcript_with_tail_hint(conversation, rows, spans, None)
}

fn splice_transcript_with_tail_hint(
    conversation: &Conversation,
    rows: &mut Vec<TranscriptRow>,
    spans: &mut Vec<MessageSpan>,
    tail_delta: Option<&str>,
) -> Option<ProjectionChange> {
    let messages = &conversation.messages;
    let epoch = conversation.cache_epoch();
    // Text/reasoning deltas only mutate their named assistant message. When
    // that message is still the tail, the preceding spans were already
    // projected and need not be compared again for every streamed token.
    let tail_only = tail_delta.is_some_and(|id| {
        spans.len() == messages.len()
            && spans
                .last()
                .is_some_and(|span| span.id == id && span.epoch == epoch)
            && messages.last().is_some_and(|message| message.id == id)
    });
    let prefix = if tail_only {
        spans.len() - 1
    } else {
        spans
            .iter()
            .zip(messages)
            .take_while(|(span, message)| span.matches(message, epoch))
            .count()
    };
    let suffix = if tail_only {
        0
    } else {
        spans[prefix..]
            .iter()
            .rev()
            .zip(messages[prefix..].iter().rev())
            .take_while(|(span, message)| span.matches(message, epoch))
            .count()
    };
    if prefix == spans.len() && prefix == messages.len() {
        return None;
    }
    let start: usize = if tail_only {
        rows.len() - spans.last().unwrap().row_count
    } else {
        spans[..prefix].iter().map(|span| span.row_count).sum()
    };
    let end = rows.len()
        - spans[spans.len() - suffix..]
            .iter()
            .map(|span| span.row_count)
            .sum::<usize>();
    let mut replacements = Vec::new();
    let new_spans = messages[prefix..messages.len() - suffix]
        .iter()
        .map(|message| {
            let in_tray = message.in_tray();
            let projected = if in_tray { Vec::new() } else { message.rows() };
            let row_count = projected.len();
            replacements.extend(projected);
            MessageSpan {
                id: message.id.clone(),
                revision: message.render_revision(),
                in_tray,
                row_count,
                epoch,
            }
        })
        .collect::<Vec<_>>();
    // The model revision belongs to the whole assistant message. A streaming
    // tail edit must not invalidate every unchanged tool/reasoning row
    // in that message. Reuse a row revision only when its complete visible
    // presentation and its snapshot epoch are unchanged.
    if spans[prefix..spans.len() - suffix]
        .iter()
        .all(|span| span.epoch == epoch)
    {
        let previous: HashMap<_, _> = rows[start..end].iter().map(|row| (&row.key, row)).collect();
        for row in &mut replacements {
            if let Some(old) = previous.get(&row.key).filter(|old| {
                old.role == row.role
                    && old.kind == row.kind
                    && old.body == row.body
                    && old.images == row.images
                    && old.time == row.time
            }) {
                row.render_revision = old.render_revision();
            }
        }
    }
    let new_end = start + replacements.len();
    let removed_images = rows[start..end]
        .iter()
        .flat_map(|row| (0..row.images.len()).map(|index| (row.key.clone(), index)))
        .collect();
    rows.splice(start..end, replacements);
    spans.splice(prefix..spans.len() - suffix, new_spans);
    Some(ProjectionChange {
        range: start..new_end,
        removed_images,
    })
}

struct CachedImage {
    url: String,
    image: OnceLock<Option<Arc<Image>>>,
}

#[derive(Default)]
struct ImageCache {
    entries: HashMap<(String, TranscriptRowKey, usize), CachedImage>,
    #[cfg(test)]
    decodes: std::cell::Cell<usize>,
}

impl ImageCache {
    fn get(&self, session: &str, row: &TranscriptRow, index: usize) -> Option<&Arc<Image>> {
        self.entries
            .get(&(session.to_owned(), row.key.clone(), index))
            .filter(|entry| row.images.get(index) == Some(&entry.url))
            .and_then(|entry| {
                entry
                    .image
                    .get_or_init(|| {
                        #[cfg(test)]
                        self.decodes.set(self.decodes.get() + 1);
                        inline_image(&entry.url)
                    })
                    .as_ref()
            })
    }

    fn update(&mut self, session: &str, rows: &[TranscriptRow], change: &ProjectionChange) {
        let present: HashSet<_> = rows[change.range.clone()]
            .iter()
            .flat_map(|row| (0..row.images.len()).map(|index| (&row.key, index)))
            .collect();
        for (key, index) in &change.removed_images {
            if !present.contains(&(key, *index)) {
                self.entries
                    .remove(&(session.to_owned(), key.clone(), *index));
            }
        }
        for row in &rows[change.range.clone()] {
            for (index, url) in row.images.iter().enumerate() {
                let key = (session.to_owned(), row.key.clone(), index);
                if self
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.url == *url)
                {
                    continue;
                }
                self.entries.insert(
                    key,
                    CachedImage {
                        url: url.clone(),
                        image: OnceLock::new(),
                    },
                );
            }
        }
    }

    fn remove_session(&mut self, session: &str) {
        self.entries.retain(|(id, _, _), _| id != session);
    }
}

fn update_session_projection(
    session: &str,
    conversation: &Conversation,
    rows: &mut Vec<TranscriptRow>,
    spans: &mut Vec<MessageSpan>,
    images: &mut ImageCache,
    tail_delta: Option<&str>,
) -> Option<ProjectionChange> {
    if spans
        .first()
        .is_some_and(|span| span.epoch != conversation.cache_epoch())
    {
        images.remove_session(session);
    }
    let change = if tail_delta.is_some() {
        splice_transcript_with_tail_hint(conversation, rows, spans, tail_delta)
    } else {
        splice_transcript(conversation, rows, spans)
    };
    change.inspect(|change| {
        images.update(session, rows, change);
    })
}

struct PendingModelPick {
    request_id: u64,
    selection: ModelSelection,
}

#[derive(Clone)]
struct PendingPromptSend {
    request_id: u64,
    message_id: String,
    text: String,
    attachments: Vec<PathBuf>,
    delivery: Option<protocol::Delivery>,
    edit_generation: u64,
}

#[derive(Clone)]
struct FailedPromptDraft {
    message_id: String,
    text: String,
    restored_text: String,
    attachments: Vec<PathBuf>,
    edit_generation: u64,
}

enum DraftAction {
    Clear {
        session: String,
        pending: PendingPromptSend,
    },
    Restore {
        session: String,
        pending: PendingPromptSend,
    },
    ClearConfirmed {
        session: String,
        failed: FailedPromptDraft,
    },
}

fn should_clear_submitted(
    current: &str,
    submitted: &str,
    current_generation: u64,
    sent_generation: u64,
) -> bool {
    current == submitted && current_generation == sent_generation
}

fn has_restored_prompt_prefix(current: &str, submitted: &str) -> bool {
    !submitted.is_empty()
        && current
            .strip_prefix(submitted)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('\n'))
}

fn restored_failed_text(current: &str, submitted: &str, original_unchanged: bool) -> String {
    if original_unchanged {
        current.to_owned()
    } else if current.is_empty() {
        submitted.to_owned()
    } else if submitted.is_empty() {
        current.to_owned()
    } else {
        format!("{submitted}\n{current}")
    }
}

fn image_display_size(width: usize, height: usize, available_width: f32) -> (f32, f32) {
    let (width, height) = if width == 0 || height == 0 {
        (200, 120)
    } else {
        (width, height)
    };
    // GTK displays images at their aspect ratio, up to 640px wide and 720px
    // high, subject to the row's actual available width.
    let scale = (available_width.clamp(1., 640.) / width as f32)
        .min(720. / height as f32)
        .min(1.);
    (width as f32 * scale, height as f32 * scale)
}

fn timestamp_in_zone(time: u64, zone: jiff::tz::TimeZone) -> String {
    // The v2 API normally uses milliseconds, but older records may contain
    // seconds. Match GTK's unset-time guard rather than showing the epoch.
    let seconds = if time >= 1_000_000_000_000 {
        time / 1000
    } else {
        time
    };
    if seconds < 1_000_000_000 {
        return String::new();
    }
    jiff::Timestamp::from_second(seconds as i64)
        .map(|timestamp| {
            timestamp
                .to_zoned(zone)
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn timestamp(time: u64) -> String {
    timestamp_in_zone(time, jiff::tz::TimeZone::system())
}

fn connected_status(version: Option<&str>, partial: bool, settings_warning: bool) -> String {
    let mut text = "Connected".to_owned();
    if settings_warning {
        text.push_str(" · Settings warning");
    }
    if let Some(version) = version {
        text.push_str(" · ");
        text.push_str(version);
    }
    if partial {
        text.push_str(" · Partial refresh");
    }
    text
}

fn sticky_user_index(
    rows: &[TranscriptRow],
    top_child: usize,
    has_load: bool,
    at_item_start: bool,
) -> Option<usize> {
    let index = top_child.checked_sub(usize::from(has_load))?;
    let row = rows.get(index)?;
    if row.role.label() == "YOU" {
        return (!at_item_start).then_some(index);
    }
    rows[..=index]
        .iter()
        .rposition(|row| row.role.label() == "YOU")
}

fn permission_detail(text: String, dark: bool) -> AnyElement {
    div()
        .p(px(8.))
        .rounded(px(5.))
        .bg(rgb(if dark { 0x15191c } else { 0xefede8 }))
        .text_color(rgb(if dark { 0xd2cec7 } else { 0x34312d }))
        .font_family(MONO_FONT_FAMILY)
        .text_size(px(11.))
        .child(text)
        .into_any_element()
}

fn permission_metadata(text: String, dark: bool) -> AnyElement {
    div()
        .p(px(8.))
        .rounded(px(5.))
        .bg(rgb(if dark { 0x15191c } else { 0xefede8 }))
        .text_color(rgb(if dark { 0x9da4aa } else { 0x626764 }))
        .font_family(MONO_FONT_FAMILY)
        .text_size(px(11.))
        .child(text)
        .into_any_element()
}

fn markdown_blocks(source: &str) -> Vec<MarkdownBlock> {
    let _latency = latency::global().span(TraceOperation::MarkdownParse, source.len());
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut list_depth = 0;
    let complex = source.lines().any(|line| {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        line.trim_start().starts_with("~~~")
            || (indent > 0 && indent <= 3 && trimmed.starts_with("```"))
    }) || Parser::new_ext(source, options).any(|event| match event {
        Event::Start(Tag::List(start)) => {
            list_depth += 1;
            start.is_some() || list_depth > 1
        }
        Event::End(TagEnd::List(_)) => {
            list_depth -= 1;
            false
        }
        Event::Start(Tag::BlockQuote(_) | Tag::Table(_))
        | Event::Start(Tag::Image { .. })
        | Event::Start(Tag::CodeBlock(CodeBlockKind::Indented))
        | Event::TaskListMarker(_)
        | Event::Rule => true,
        Event::Start(Tag::CodeBlock(_)) if list_depth > 0 => true,
        _ => false,
    });
    if complex {
        return complex_markdown_blocks(source, options);
    }
    simple_markdown_blocks(source)
}

struct InlineMarkdownBlock {
    content: String,
    marker: Option<String>,
    list_depth: usize,
    quote_depth: usize,
    heading: Option<u8>,
}

#[derive(Default)]
struct MarkdownTableBuilder {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
}

fn flush_inline(blocks: &mut Vec<MarkdownBlock>, current: &mut Option<InlineMarkdownBlock>) {
    if let Some(block) = current.take().filter(|block| !block.content.is_empty()) {
        blocks.push(MarkdownBlock::Structured {
            content: block.content,
            marker: block.marker,
            list_depth: block.list_depth,
            quote_depth: block.quote_depth,
            heading: block.heading,
        });
    }
}

fn complex_markdown_blocks(source: &str, options: Options) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let mut current: Option<InlineMarkdownBlock> = None;
    let mut code: Option<(String, String, usize, usize)> = None;
    let mut table: Option<MarkdownTableBuilder> = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut marker: Option<String> = None;
    let mut quote_depth = 0;
    let mut links: Vec<String> = Vec::new();
    for event in Parser::new_ext(source, options) {
        if let Some((_, content, _, _)) = code.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let (language, content, list_depth, quote_depth) = code.take().unwrap();
                    let content = content.trim_end_matches('\n').to_owned();
                    blocks.push(if list_depth > 0 || quote_depth > 0 {
                        MarkdownBlock::NestedCode {
                            language,
                            content,
                            list_depth,
                            quote_depth,
                        }
                    } else {
                        MarkdownBlock::Code(language, content)
                    });
                }
                Event::Text(text) | Event::Code(text) => content.push_str(&text),
                Event::SoftBreak | Event::HardBreak => content.push('\n'),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::BlockQuote(_)) => {
                flush_inline(&mut blocks, &mut current);
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_inline(&mut blocks, &mut current);
                quote_depth -= 1;
            }
            Event::Start(Tag::List(start)) => {
                flush_inline(&mut blocks, &mut current);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush_inline(&mut blocks, &mut current);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush_inline(&mut blocks, &mut current);
                marker = Some(match lists.last_mut() {
                    Some(Some(number)) => {
                        let text = format!("{number}.");
                        *number += 1;
                        text
                    }
                    _ => "•".into(),
                });
            }
            Event::End(TagEnd::Item) => {
                flush_inline(&mut blocks, &mut current);
                marker = None;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_inline(&mut blocks, &mut current);
                let language = match kind {
                    CodeBlockKind::Fenced(language) => language
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .into(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((language, String::new(), lists.len(), quote_depth));
            }
            Event::Start(Tag::Table(_)) => {
                flush_inline(&mut blocks, &mut current);
                table = Some(MarkdownTableBuilder::default());
            }
            Event::Start(Tag::TableCell) => {
                current = Some(InlineMarkdownBlock {
                    content: String::new(),
                    marker: None,
                    list_depth: 0,
                    quote_depth: 0,
                    heading: None,
                });
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = table.as_mut() {
                    table
                        .current_row
                        .push(current.take().map_or_else(String::new, |cell| cell.content));
                }
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                if let Some(table) = table.as_mut() {
                    if table.header.is_empty() {
                        table.header = std::mem::take(&mut table.current_row);
                    } else {
                        table.rows.push(std::mem::take(&mut table.current_row));
                    }
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = table.take() {
                    blocks.push(MarkdownBlock::Table {
                        header: table.header,
                        rows: table.rows,
                    });
                }
            }
            Event::Start(Tag::Paragraph) => {
                flush_inline(&mut blocks, &mut current);
                current = Some(InlineMarkdownBlock {
                    content: String::new(),
                    marker: marker.take(),
                    list_depth: lists.len(),
                    quote_depth,
                    heading: None,
                });
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush_inline(&mut blocks, &mut current);
                current = Some(InlineMarkdownBlock {
                    content: String::new(),
                    marker: marker.take(),
                    list_depth: lists.len(),
                    quote_depth,
                    heading: Some(level as u8),
                });
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_)) => {
                flush_inline(&mut blocks, &mut current)
            }
            Event::Start(Tag::Strong) | Event::End(TagEnd::Strong) => {
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, "**");
            }
            Event::Start(Tag::Emphasis) | Event::End(TagEnd::Emphasis) => {
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, "*");
            }
            Event::Start(Tag::Strikethrough) | Event::End(TagEnd::Strikethrough) => {
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, "~~");
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                links.push(dest_url.into_string());
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, "[");
            }
            Event::End(TagEnd::Link) => {
                let destination = links.pop().unwrap_or_default();
                append_inline(
                    &mut current,
                    &mut marker,
                    lists.len(),
                    quote_depth,
                    &format!("]({destination})"),
                );
            }
            Event::Start(Tag::Image { .. }) => {
                append_inline(
                    &mut current,
                    &mut marker,
                    lists.len(),
                    quote_depth,
                    "Image: ",
                );
            }
            Event::Text(text) => {
                append_inline(
                    &mut current,
                    &mut marker,
                    lists.len(),
                    quote_depth,
                    &escape_markdown_literal(&text),
                );
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, &text);
            }
            Event::Code(text) => {
                append_inline(
                    &mut current,
                    &mut marker,
                    lists.len(),
                    quote_depth,
                    &format!("`{text}`"),
                );
            }
            Event::SoftBreak | Event::HardBreak => {
                append_inline(&mut current, &mut marker, lists.len(), quote_depth, "\n")
            }
            Event::TaskListMarker(checked) => append_inline(
                &mut current,
                &mut marker,
                lists.len(),
                quote_depth,
                if checked { "[x] " } else { "[ ] " },
            ),
            Event::Rule => {
                flush_inline(&mut blocks, &mut current);
                blocks.push(MarkdownBlock::Rule);
            }
            _ => {}
        }
    }
    flush_inline(&mut blocks, &mut current);
    blocks
}

fn append_inline(
    current: &mut Option<InlineMarkdownBlock>,
    marker: &mut Option<String>,
    list_depth: usize,
    quote_depth: usize,
    text: &str,
) {
    let block = current.get_or_insert_with(|| InlineMarkdownBlock {
        content: String::new(),
        marker: marker.take(),
        list_depth,
        quote_depth,
        heading: None,
    });
    block.content.push_str(text);
}

/// Text events are already decoded by pulldown-cmark. Escape literal Markdown
/// punctuation before handing the reconstructed fragment to GPUI Kit's
/// renderer; otherwise an escaped `\*literal\*` becomes new emphasis.
fn escape_markdown_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '&' {
            escaped.push_str("&amp;");
            continue;
        }
        if matches!(
            character,
            '\\' | '*' | '_' | '`' | '[' | ']' | '~' | '<' | '>' | '#' | '!' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn markdown_view_id(
    session: &str,
    row: &TranscriptRowKey,
    block_index: usize,
    item_index: usize,
) -> String {
    format!(
        "markdown-{session}-{}-{:?}-{block_index}-{item_index}",
        row.message_id, row.slot
    )
}

fn heading_is_block_marker(title: &str) -> bool {
    let title = title.trim_start();
    title.starts_with("- ")
        || title.starts_with("* ")
        || title.starts_with("+ ")
        || title.starts_with("> ")
        || title.starts_with("# ")
        || title.starts_with("```")
        || title.starts_with("~~~")
        || title
            .split_once('.')
            .is_some_and(|(number, rest)| number.parse::<u64>().is_ok() && rest.starts_with(' '))
}

fn simple_markdown_blocks(source: &str) -> Vec<MarkdownBlock> {
    let mut lines = source.lines().peekable();
    let mut blocks = Vec::new();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(language) = line.strip_prefix("```") {
            let mut code = String::new();
            for next in lines.by_ref() {
                if next.starts_with("```") {
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(next);
            }
            blocks.push(MarkdownBlock::Code(language.to_owned(), code));
            continue;
        }
        if let Some((level, title)) = line
            .split_once(' ')
            .filter(|(marks, _)| !marks.is_empty() && marks.bytes().all(|c| c == b'#'))
        {
            blocks.push(MarkdownBlock::Heading(level.len() as u8, title.to_owned()));
            continue;
        }
        if line.starts_with("- ") || line.starts_with("* ") {
            let mut items = vec![line[2..].to_owned()];
            while let Some(next) = lines
                .peek()
                .filter(|next| next.starts_with("- ") || next.starts_with("* "))
            {
                items.push(next[2..].to_owned());
                lines.next();
            }
            blocks.push(MarkdownBlock::List(items));
            continue;
        }
        let mut text = line.to_owned();
        while let Some(next) = lines.peek().filter(|next| {
            !next.trim().is_empty()
                && !next.starts_with("```")
                && !next.starts_with("# ")
                && !next.starts_with("- ")
                && !next.starts_with("* ")
        }) {
            text.push('\n');
            text.push_str(next);
            lines.next();
        }
        blocks.push(MarkdownBlock::Paragraph(text));
    }
    blocks
}

fn inline_image(url: &str) -> Option<Arc<Image>> {
    let _latency = latency::global().span(TraceOperation::ImagePrepare, url.len());
    let (metadata, encoded) = url.strip_prefix("data:")?.split_once(',')?;
    let format = match metadata {
        "image/png;base64" => ImageFormat::Png,
        "image/jpeg;base64" => ImageFormat::Jpeg,
        "image/webp;base64" => ImageFormat::Webp,
        "image/gif;base64" => ImageFormat::Gif,
        _ => return None,
    };
    // Avoid unbounded server-supplied data without rejecting images accepted
    // by the server's 20 MiB decoded-attachment limit.
    if encoded.len() > opencode_gpui::protocol::MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() > opencode_gpui::protocol::MAX_ATTACHMENT_BYTES {
        return None;
    }
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

impl Client {
    fn toggle_latency_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.latency.saving {
            return;
        }
        if latency::global().enabled() {
            self.latency.task.take();
            if let (Some(collector), Some(window_id)) =
                (self.latency.collector.take(), self.latency.window_id.take())
            {
                collect_latency(&mut collector.borrow_mut(), window_id);
            }
            if let Some(report) = latency::global().stop() {
                self.save_latency_report(report, cx);
            }
        } else if let Some(report) = self.latency.failed.take() {
            self.save_latency_report(report, cx);
        } else {
            latency::global().start();
            self.latency.report = None;
            self.latency.copied = false;
            let window_id = window.window_handle().window_id();
            let collector = Rc::new(RefCell::new(cx.foreground_journal().collector()));
            self.latency.collector = Some(collector.clone());
            self.latency.window_id = Some(window_id);
            let bg = cx.background_executor().clone();
            self.latency.task = Some(cx.spawn(async move |_, _| {
                loop {
                    let due = Instant::now() + latency::PROBE_INTERVAL;
                    let worker = bg.clone();
                    // Awaiting one worker is single-flight even during a long UI stall.
                    let posted = bg
                        .spawn(async move {
                            worker.timer(latency::PROBE_INTERVAL).await;
                            Instant::now()
                        })
                        .await;
                    if !latency::global().enabled() {
                        break;
                    }
                    latency::global().probe(due, posted, Instant::now());
                    collect_latency(&mut collector.borrow_mut(), window_id);
                    // No notify, refresh, window update or next-frame callback here.
                }
            }));
        }
        cx.notify();
    }

    fn save_latency_report(&mut self, report: latency::Report, cx: &mut Context<Self>) {
        self.latency.saving = true;
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = worker
                .spawn(async move {
                    let directory = dirs::data_local_dir()
                        .unwrap_or_else(std::env::temp_dir)
                        .join("opencode-gpui/diagnostics");
                    match report.write(&directory) {
                        Ok(path) => Ok(path),
                        Err(_) => Err(report),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.latency.saving = false;
                match result {
                    Ok(path) => this.latency.report = Some(path),
                    Err(report) => this.latency.failed = Some(report),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn latency_controls(&self, cx: &Context<Self>) -> AnyElement {
        let recording = latency::global().enabled();
        let label = if self.latency.saving {
            "Saving lag report…"
        } else if recording {
            "Stop lag capture"
        } else if self.latency.failed.is_some() {
            "Retry saving lag report"
        } else {
            "Record lag"
        };
        let mut view = div().flex().flex_col().gap(self.scaled(5.)).child(
            BaseButton::new("latency-capture")
                .accessibility_label(label)
                .h(self.scaled(21.))
                .line_height(self.scaled(16.))
                .justify_start()
                .disabled(self.latency.saving)
                .when(recording, |button| button.text_color(self.tone(0xb84b45, 0xe79b96)))
                .tooltip(|window, cx| Tooltip::new(
                    "Start/stop latency capture (Ctrl+Shift+L). Timings and counts only; no conversation contents."
                ).build(window, cx))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_latency_capture(window, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(self.scaled(7.))
                        .child(
                            Icon::default()
                                .data(include_bytes!("icons/bug.svg"))
                                .with_size(self.scaled(14.)),
                        )
                        .child(label),
                ),
        );
        if !recording && !self.latency.saving {
            if self.latency.failed.is_some() {
                view = view.child(
                    div()
                        .text_size(self.scaled(10.))
                        .text_color(self.tone(0xb84b45, 0xe79b96))
                        .child("Save failed; capture kept for retry."),
                );
            } else if let Some(path) = &self.latency.report {
                let path = path.to_string_lossy().into_owned();
                view = view
                    .child(
                        div()
                            .text_size(self.scaled(10.))
                            .child("Lag report saved locally"),
                    )
                    .child(
                        BaseButton::new("latency-copy-path")
                            .accessibility_label("Copy saved lag report path")
                            .h(self.scaled(21.))
                            .line_height(self.scaled(16.))
                            .justify_start()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                                this.latency.copied = true;
                                cx.notify();
                            }))
                            .child(if self.latency.copied {
                                "Report path copied"
                            } else {
                                "Copy report path"
                            }),
                    );
            }
        }
        view.into_any_element()
    }

    // Keep GTK light/dark design-token pairs at each call site. Distinct roles
    // can share a light value (notably white text and white inputs) but differ
    // in dark mode, so do not translate colors by looking up their light hex.
    fn tone(&self, light: u32, dark: u32) -> Rgba {
        rgb(if self.dark { dark } else { light })
    }

    fn copy_with_feedback(
        &mut self,
        key: String,
        label: &'static str,
        text: String,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.next_copy_feedback_token = self.next_copy_feedback_token.wrapping_add(1);
        let token = self.next_copy_feedback_token;
        self.copy_feedback = Some(CopyFeedback { key, label, token });
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .copy_feedback
                    .as_ref()
                    .is_some_and(|state| state.token == token)
                {
                    this.copy_feedback = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn copied(&self, key: &str) -> bool {
        self.copy_feedback
            .as_ref()
            .is_some_and(|state| state.key == key)
    }

    fn markdown_code_block(
        &self,
        language: String,
        code: String,
        row_index: usize,
        block_index: usize,
        row_key: &TranscriptRowKey,
        cx: &Context<Self>,
    ) -> AnyElement {
        let copy = code.clone();
        let feedback_key = format!("code:{}:{row_key:?}:{block_index}", self.active);
        let copied = self.copied(&feedback_key);
        div()
            .w_full()
            .rounded(px(7.))
            .border_1()
            .border_color(self.tone(0xd2cdc5, 0x30353a))
            .bg(self.tone(0xf7f5f1, 0x171a1d))
            .overflow_hidden()
            .child(
                div()
                    .h(self.scaled(32.))
                    .px(self.scaled(12.))
                    .flex()
                    .items_center()
                    .bg(self.tone(0xece9e2, 0x14171a))
                    .border_b_1()
                    .border_color(self.tone(0xd2cdc5, 0x282c30))
                    .text_size(self.scaled(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.tone(0x777e7d, 0x899198))
                    .child(language)
                    .child(div().flex_1())
                    .child(
                        BaseButton::new(format!("copy-code-{row_index}-{block_index}"))
                            .accessibility_label(if copied { "Code copied" } else { "Copy code" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_with_feedback(
                                    feedback_key.clone(),
                                    "Code copied",
                                    copy.clone(),
                                    cx,
                                );
                            }))
                            .child(if copied {
                                div()
                                    .text_size(self.scaled(13.))
                                    .text_color(self.tone(0x347453, 0x8fcba2))
                                    .child("✓")
                                    .into_any_element()
                            } else {
                                Icon::default()
                                    .data(include_bytes!("icons/copy.svg"))
                                    .with_size(self.scaled(12.))
                                    .text_color(self.tone(0x777e7d, 0x899198))
                                    .into_any_element()
                            }),
                    ),
            )
            .child(
                div()
                    .id(format!("code-body-{row_index}-{block_index}"))
                    .test_support()
                    .min_h(self.scaled(65.))
                    .p(self.scaled(12.))
                    .font_family(MONO_FONT_FAMILY)
                    .text_size(self.scaled(12.))
                    .child(code),
            )
            .into_any_element()
    }

    fn markdown_body(
        &self,
        row: &TranscriptRow,
        row_index: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut content = div().w_full().flex().flex_col().gap(self.scaled(10.));
        for (block_index, block) in markdown_blocks(&row.body).into_iter().enumerate() {
            // The stateless markdown() helper identifies views by its caller
            // location. Repeated items at one call site then share parser
            // state and can remain stuck as blank loading placeholders.
            let owner = cx.entity().downgrade();
            let markdown_view = |item_index, text| {
                let owner = owner.clone();
                TextView::markdown(
                    markdown_view_id(&self.active, &row.key, block_index, item_index),
                    text,
                )
                .on_link_click(move |url, event, window, app| {
                    if event.is_secondary() {
                        let position = event.position();
                        let url = url.to_string();
                        let _ = owner.update(app, |this, cx| {
                            this.link_menu = Some(LinkMenu { url, position });
                            let focus = this.link_menu_focus[0].clone();
                            window.on_next_frame(move |window, cx| focus.focus(window, cx));
                            cx.notify();
                        });
                    } else if !event.is_middle_click() && may_open_link(url) {
                        app.open_url(url);
                    }
                })
                .w_full()
                .min_w_0()
            };
            let element: AnyElement = match block {
                MarkdownBlock::Heading(level, title) => {
                    let heading = div()
                        .text_size(self.scaled(match level {
                            1 => 18.,
                            2 => 16.,
                            _ => 14.,
                        }))
                        .font_weight(FontWeight::BOLD);
                    if heading_is_block_marker(&title) || !title.contains(['*', '_', '`', '[']) {
                        heading.child(title).into_any_element()
                    } else {
                        heading.child(markdown_view(0, title)).into_any_element()
                    }
                }
                MarkdownBlock::Paragraph(text) => div()
                    .line_height(self.scaled(22.))
                    .child(markdown_view(0, text))
                    .into_any_element(),
                MarkdownBlock::Structured {
                    content,
                    marker,
                    list_depth,
                    quote_depth,
                    heading,
                } => {
                    let mut line = div().w_full().flex().items_start();
                    if let Some(marker) = marker {
                        line = line.child(
                            div()
                                .w(self.scaled(24.))
                                .flex_shrink_0()
                                .text_color(self.tone(0x777e7d, 0xaeb4b9))
                                .child(marker),
                        );
                    }
                    line = line.child(div().flex_1().min_w_0().child(markdown_view(0, content)));
                    let mut block = div()
                        .w_full()
                        .pl(self.scaled(16. * list_depth.saturating_sub(1) as f32))
                        .when(heading.is_none(), |view| view.line_height(self.scaled(22.)))
                        .when_some(heading, |view, level| {
                            view.text_size(self.scaled(if level == 1 {
                                18.
                            } else if level == 2 {
                                16.
                            } else {
                                14.
                            }))
                            .font_weight(FontWeight::BOLD)
                        });
                    if quote_depth > 0 {
                        block = block
                            .border_l_1()
                            .border_color(self.tone(0xb7b3ac, 0x50565b))
                            .py(self.scaled(7.))
                            .pr(self.scaled(12.))
                            .pl(self.scaled(12. + 14. * (quote_depth - 1) as f32))
                            .text_color(self.tone(0x555b5c, 0xb5b9bb));
                    }
                    block.child(line).into_any_element()
                }
                MarkdownBlock::Table { header, rows } => {
                    let mut table = div()
                        .w_full()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(self.tone(0xd2cdc5, 0x30353a));
                    let mut cell_index = 0;
                    for (index, row) in std::iter::once(header).chain(rows).enumerate() {
                        let mut line = div().w_full().flex().when(index == 0, |line| {
                            line.font_weight(FontWeight::BOLD)
                                .bg(self.tone(0xece9e2, 0x202429))
                        });
                        for cell in row {
                            line = line.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p(px(7.))
                                    .border_r_1()
                                    .border_color(self.tone(0xd2cdc5, 0x30353a))
                                    .child(markdown_view(cell_index, cell)),
                            );
                            cell_index += 1;
                        }
                        table = table.child(line);
                    }
                    table.into_any_element()
                }
                MarkdownBlock::Rule => div()
                    .h(px(1.))
                    .w_full()
                    .bg(self.tone(0xc8c3ba, 0x343a3f))
                    .into_any_element(),
                MarkdownBlock::List(items) => {
                    let mut list = div().flex().flex_col().gap(self.scaled(10.));
                    for (item_index, item) in items.into_iter().enumerate() {
                        list = list.child(
                            div()
                                .flex()
                                .line_height(self.scaled(22.))
                                .gap(self.scaled(9.))
                                .child(
                                    div()
                                        .w(self.scaled(17.))
                                        .text_right()
                                        .text_color(self.tone(0x777e7d, 0xaeb4b9))
                                        .child("•"),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .child(markdown_view(item_index, item)),
                                ),
                        );
                    }
                    list.into_any_element()
                }
                MarkdownBlock::Code(language, code) => {
                    self.markdown_code_block(language, code, row_index, block_index, &row.key, cx)
                }
                MarkdownBlock::NestedCode {
                    language,
                    content,
                    list_depth,
                    quote_depth,
                } => {
                    let mut wrapper = div()
                        .w_full()
                        .pl(px(16. * list_depth.saturating_sub(1) as f32));
                    if quote_depth > 0 {
                        wrapper = wrapper
                            .border_l_1()
                            .border_color(self.tone(0xb7b3ac, 0x50565b))
                            .py(px(7.))
                            .pr(px(12.))
                            .pl(px(12. + 14. * (quote_depth - 1) as f32));
                    }
                    wrapper
                        .child(self.markdown_code_block(
                            language,
                            content,
                            row_index,
                            block_index,
                            &row.key,
                            cx,
                        ))
                        .into_any_element()
                }
            };
            content = content.child(element);
        }
        content.into_any_element()
    }

    fn from_live(window: &mut Window, cx: &mut Context<Self>, args: &Args) -> Self {
        let (mut state, state_warning) = match persist::load_with_legacy(&persist::default_path()) {
            Ok(loaded) => loaded,
            Err(error) => (PersistedState::default(), Some(error.to_string())),
        };
        let server = args
            .server
            .clone()
            .unwrap_or(state.connection.server.clone());
        if let Ok(key) = opencode_gpui::api::server_key(&server) {
            ensure_canonical_server_state(&mut state, &server, &key);
        }
        let username = args
            .username
            .clone()
            .unwrap_or(state.connection.username.clone());
        let persisted_password_identity = credentials::same_password_identity(
            &server,
            &username,
            &state.connection.server,
            &state.connection.username,
        );
        let password = credentials::initial_password(
            &SystemKeyring,
            &server,
            &username,
            args.password.clone(),
            state.connection.basic_auth_in_keyring,
            state.connection.basic_auth_in_keyring && persisted_password_identity,
        );
        let persisted_cloudflare_identity = opencode_gpui::api::server_key(&server)
            .ok()
            .zip(opencode_gpui::api::server_key(&state.connection.server).ok())
            .is_some_and(|(current, saved)| current == saved);
        let expect_cloudflare = state.connection.cloudflare_access
            && persisted_cloudflare_identity
            && args.cf_access_client_id.is_none()
            && args.cf_access_client_secret.is_none();
        let cloudflare_access = match (&args.cf_access_client_id, &args.cf_access_client_secret) {
            (Some(id), Some(secret)) => {
                CloudflareAccessCredentials::new(id.clone(), secret.clone()).map(Some)
            }
            (None, None) if expect_cloudflare => credentials::load(&server),
            (None, None) => Ok(None),
            _ => Err(anyhow::anyhow!(
                "Cloudflare Access requires both client ID and secret"
            )),
        };
        let mut warnings: Vec<String> = [state_warning, password.warning]
            .into_iter()
            .flatten()
            .collect();
        let cloudflare_access = match cloudflare_access {
            Ok(Some(credentials)) => Some(credentials),
            Ok(None) => {
                if expect_cloudflare {
                    warnings.push("The saved Cloudflare Access token was not found in the system keyring; enter it in Settings".into());
                }
                None
            }
            Err(error) => {
                warnings.push(error.to_string());
                None
            }
        };
        let config = ApiConfig {
            base_url: server,
            username,
            password: password.password,
            cloudflare_access,
        };
        let scroll = ScrollHandle::new();
        let mut client = Self {
            dark: Theme::global(cx).is_dark(),
            api: None,
            preview_api: false,
            connection_generation: 0,
            bootstrap_in_flight: false,
            bootstrap_after_load: false,
            bootstrap_retry_scheduled: false,
            bootstrap_retry_count: 0,
            bootstrap_events: Vec::new(),
            bootstrap_directories: Vec::new(),
            refresh_open_tabs: false,
            settings: SettingsFields::new(window, cx, state.clone(), config.clone()),
            saved_active: None,
            restored_server_state: false,
            connection_status: "Connecting".into(),
            disconnected: false,
            sse_connected_once: false,
            server_version: None,
            conversations: HashMap::new(),
            loading_messages: HashMap::new(),
            message_load_errors: HashMap::new(),
            skip_prune_for_load: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            follow_bottom: true,
            transcript_pointer_down: false,
            catalogs: HashMap::new(),
            model_retry_count: HashMap::new(),
            model_retry_scheduled: HashSet::new(),
            running_jobs: Jobs::default(),
            jobs_elapsed_refresh_active: false,
            sessions: Vec::new(),
            open_tabs: Vec::new(),
            sidebar_visible: true,
            sidebar_auto_hidden: false,
            sidebar_zoom_initialized: false,
            sidebar_zoom_width: px(0.),
            sidebar_toggle_focus: cx.focus_handle().tab_stop(true),
            tab_focus: HashMap::new(),
            tab_shortcut_hint: false,
            tab_drop_target: None,
            tab_drop_outgoing: HashMap::new(),
            next_tab_drop_token: 0,
            tab_drop_anchor: None,
            bootstrapped: false,
            projects: Vec::new(),
            active: String::new(),
            transcript: HashMap::new(),
            transcript_spans: HashMap::new(),
            transcript_scrolls: HashMap::new(),
            attachments: ImageCache::default(),
            catalog: ModelCatalog::default(),
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(1)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            }),
            composer_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            composer_placeholder_focused: false,
            composer_session: String::new(),
            composers: HashMap::new(),
            attachments_draft: Vec::new(),
            attachment_drafts: HashMap::new(),
            owned_pastes: HashMap::new(),
            overlay: None,
            copy_feedback: None,
            next_copy_feedback_token: 0,
            link_menu: None,
            link_menu_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            autoscroll: None,
            next_autoscroll_token: 0,
            scroll,
            transcript_content_width: px(420.),
            history_focus: cx.focus_handle().tab_stop(true),
            retry_history_focus: cx.focus_handle().tab_stop(true),
            sessions_picker_scroll: ScrollHandle::new(),
            picker_list_scroll: ScrollHandle::new(),
            projects_picker_scroll: ScrollHandle::new(),
            picker_choice_focus: HashMap::new(),
            unread: HashSet::new(),
            offline_busy: HashSet::new(),
            statuses: HashMap::new(),
            jobs: Vec::new(),
            background_requests: background::Requests::default(),
            latency: LatencyCapture::default(),
            background_focus: cx.focus_handle().tab_stop(true),
            terminal_shells: HashMap::new(),
            desktop_notifications: None,
            notification_targets: HashMap::new(),
            forms: Forms::default(),
            form_cancel_focus: cx.focus_handle().tab_stop(true),
            form_cancel_presented: None,
            permissions: Vec::new(),
            permission_in_flight: HashSet::new(),
            permission_container_focus: cx.focus_handle().tab_stop(true),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: None,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            settings_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            settings_session_focus: HashMap::new(),
            settings_sessions_scroll: ScrollHandle::new(),
            settings_highlight: None,
            child_parents: HashMap::new(),
            next_prompt_request_id: 0,
            next_session_request_id: 0,
            focus_composer_pending: false,
            next_model_request_id: 0,
            model_switches: HashMap::new(),
            pending_prompts: HashMap::new(),
            failed_prompt_drafts: HashMap::new(),
            confirmed_prompt_residue: HashMap::new(),
            tray_in_flight: HashSet::new(),
            draft_actions: Vec::new(),
            composer_edit_generation: HashMap::new(),
            local_busy: HashSet::new(),
            local_run_message_ids: HashMap::new(),
            deferred_abort: HashSet::new(),
            abort_timeout_tokens: HashMap::new(),
            next_abort_timeout_token: 0,
            modal: None,
            picker_control_centers: Rc::new(RefCell::new([None, None])),
            modal_container_focus: cx.focus_handle(),
            rename_target: None,
            rename_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            rename_pending: None,
            rename_error: None,
            picker_highlight: None,
            search: cx.new(|cx| InputState::new(window, cx).placeholder("Search models (fuzzy)…")),
            rename: cx.new(|cx| InputState::new(window, cx)),
        };
        cx.subscribe(
            &client.composer,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    latency::global().edited();
                    *this
                        .composer_edit_generation
                        .entry(this.composer_session.clone())
                        .or_default() += 1;
                }
                InputEvent::PressEnter { secondary, shift } if !shift => {
                    this.send_prompt(*secondary, cx);
                }
                _ => {}
            },
        )
        .detach();
        cx.subscribe(
            &client.search,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.picker_highlight = None;
                    if this.modal == Some(Modal::Sessions) {
                        this.sessions_picker_scroll
                            .set_offset(point(px(0.), px(0.)));
                    } else if this.modal == Some(Modal::NewSession) {
                        this.projects_picker_scroll
                            .set_offset(point(px(0.), px(0.)));
                    } else if matches!(this.modal, Some(Modal::Model | Modal::Level)) {
                        this.picker_list_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.accept_modal_choice(cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(&client.rename, |this, _, event: &InputEvent, cx| {
            match event {
                InputEvent::Change => this.rename_error = None,
                InputEvent::PressEnter { .. } => this.rename_session(cx),
                _ => {}
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(
            &client.settings.session_search,
            |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.settings_highlight = None;
                    this.settings_sessions_scroll
                        .set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            },
        )
        .detach();
        let (notifications, actions) = DesktopNotifications::start();
        client.desktop_notifications = Some(notifications);
        let notification_window = window.window_handle();
        cx.spawn(async move |this, cx| {
            while let Ok(notification_id) = actions.recv().await {
                match this.update(cx, |this, cx| {
                    this.activate_session_notification(&notification_id, cx)
                }) {
                    Ok(true) => {
                        let _ =
                            notification_window.update(cx, |_, window, _| window.activate_window());
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
        match ApiHandle::start(config) {
            Ok((api, receiver, key)) => {
                let saved = state.servers.get(&key);
                client.restored_server_state = saved.is_some();
                client.saved_active = saved.and_then(|saved| saved.active.clone());
                client.open_tabs = saved
                    .map(|saved| saved.tabs.iter().map(|tab| tab.id.clone()).collect())
                    .unwrap_or_default();
                client.bootstrap_directories = saved
                    .map(|saved| saved.tabs.iter().map(|tab| tab.directory.clone()).collect())
                    .unwrap_or_default();
                client.unread = saved.map(|saved| saved.unread.clone()).unwrap_or_default();
                client.offline_busy = saved.map(|saved| saved.busy.clone()).unwrap_or_default();
                client.api = Some(api);
                client.request_bootstrap();
                cx.spawn(async move |this, cx| {
                    while let Ok(event) = receiver.recv().await {
                        if this
                            .update(cx, |this, cx| {
                                if this.connection_generation == 0 {
                                    this.handle_live_event(event, cx);
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(error) => client.connection_status = format!("Connection failed: {error}"),
        }
        if !warnings.is_empty() {
            log::warn!("GPUI connection setup: {}", warnings.join("; "));
            client.settings.warning = Some(warnings.join("; "));
            if client.api.is_some() {
                client.connection_status = format!("Connecting · {}", warnings.join("; "));
            }
        }
        client
    }

    fn select_session(&mut self, id: String) {
        self.link_menu = None;
        self.autoscroll = None;
        if !self.open_tabs.contains(&id) {
            self.open_tabs.push(id.clone());
        }
        if self.active != id {
            if !self.active.is_empty() {
                self.attachment_drafts.insert(
                    self.active.clone(),
                    std::mem::take(&mut self.attachments_draft),
                );
            }
            self.attachments_draft = self.attachment_drafts.remove(&id).unwrap_or_default();
            self.scroll = self
                .transcript_scrolls
                .entry(id.clone())
                .or_default()
                .clone();
            self.follow_bottom = true;
            self.active = id.clone();
            // Switching tabs intentionally follows the latest loaded message.
        }
        self.jobs = self.running_jobs.rows(Some(&id));
        if !self
            .conversations
            .get(&id)
            .is_some_and(|conversation| conversation.loaded)
        {
            self.request_newest(&id);
        }
        if let Some(api) = &self.api
            && let Some(session) = self.sessions.iter().find(|session| session.id == id)
        {
            self.catalog = self
                .catalogs
                .get(&session.directory)
                .cloned()
                .unwrap_or_default();
            if self
                .catalogs
                .get(&session.directory)
                .is_none_or(|catalog| catalog.models.is_empty())
                && !self.model_retry_scheduled.contains(&session.directory)
            {
                api.send(Command::LoadModels {
                    directory: session.directory.clone(),
                });
            }
        }
        if self.bootstrapped {
            self.persist_tabs();
        }
    }

    fn toggle_transcript_autoscroll(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.autoscroll.take().is_some() {
            cx.notify();
            return;
        }
        if self.modal.is_some() || !self.scroll.bounds().contains(&position) {
            return;
        }
        self.next_autoscroll_token = self.next_autoscroll_token.wrapping_add(1);
        let token = self.next_autoscroll_token;
        self.autoscroll = Some(TranscriptAutoscroll {
            session: self.active.clone(),
            origin: position,
            pointer: position,
            token,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let still_active = this
                    .update(cx, |this, cx| {
                        if this
                            .autoscroll
                            .as_ref()
                            .is_none_or(|state| state.token != token)
                        {
                            return false;
                        }
                        this.autoscroll_tick(cx)
                    })
                    .unwrap_or(false);
                if !still_active {
                    break;
                }
            }
        })
        .detach();
    }

    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.autoscroll.clone() else {
            return false;
        };
        if state.session != self.active || self.modal.is_some() {
            self.autoscroll = None;
            cx.notify();
            return false;
        }
        let delta = autoscroll_step(state.pointer.y - state.origin.y, self.zoom());
        if delta == px(0.) {
            return true;
        }
        let offset = self.scroll.offset();
        let next_y = (offset.y + delta)
            .max(-self.scroll.max_offset().y)
            .min(px(0.));
        if next_y != offset.y {
            self.follow_bottom = false;
            self.scroll.set_offset(point(offset.x, next_y));
            cx.notify();
        }
        true
    }

    fn ensure_jobs_elapsed_refresh(&mut self, cx: &mut Context<Self>) {
        if self.jobs.is_empty() || self.jobs_elapsed_refresh_active {
            return;
        }
        self.jobs_elapsed_refresh_active = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(u64::from(
                        jobs::ELAPSED_REFRESH_SECONDS,
                    )))
                    .await;
                let keep_refreshing = this
                    .update(cx, |this, cx| {
                        if this.jobs.is_empty() {
                            this.jobs_elapsed_refresh_active = false;
                            false
                        } else {
                            cx.notify();
                            true
                        }
                    })
                    .unwrap_or(false);
                if !keep_refreshing {
                    break;
                }
            }
        })
        .detach();
    }

    fn transcript_width(&self, window: &Window) -> Pixels {
        let frame = match window.window_decorations() {
            Decorations::Client { tiling } => {
                // The client border has no shadow inset, only a 1px frame on
                // untiled sides. Do not use window_paddings here: it assumes
                // the default 20px shadow before the first rendered frame.
                px(if tiling.left { 0. } else { 1. }) + px(if tiling.right { 0. } else { 1. })
            }
            Decorations::Server => px(0.),
        };
        window.viewport_size().width
            - frame
            - self.scaled(if self.sidebar_visible {
                SIDEBAR_WIDTH
            } else {
                0.
            })
            - self.scaled(TRANSCRIPT_SCROLLBAR_GUTTER)
    }

    fn zoom(&self) -> f32 {
        normalized_zoom(self.settings.persisted.zoom_level) as f32
    }

    fn scaled(&self, value: f32) -> Pixels {
        px(value * self.zoom())
    }

    fn set_zoom(&mut self, zoom: f64, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = normalized_zoom(zoom);
        if (self.settings.persisted.zoom_level - zoom).abs() < f64::EPSILON {
            return;
        }
        self.settings.persisted.zoom_level = zoom;
        self.reconcile_zoom_sidebar(window);
        window.set_rem_size(px(16. * zoom as f32));
        // During startup the saved active tab has not been selected yet. Save
        // only the zoom field instead of projecting the still-empty live tab
        // selection over its persisted value.
        if self.api.is_some()
            && !self.preview_api
            && let Err(error) = self.settings.persisted.save(&persist::default_path())
        {
            self.connection_status = format!("State save failed: {error}");
        }
        window.refresh();
        cx.notify();
    }

    fn reconcile_zoom_sidebar(&mut self, window: &Window) {
        // A zoomed fixed-width sidebar must not leave the chat narrower than
        // its controls. Auto-collapse it, but let Ctrl+B reopen it on demand.
        let cramped = window.viewport_size().width < self.scaled(SIDEBAR_WIDTH + 350.);
        if cramped && self.sidebar_visible {
            self.sidebar_visible = false;
            self.sidebar_auto_hidden = true;
        } else if !cramped && self.sidebar_auto_hidden {
            self.sidebar_visible = true;
            self.sidebar_auto_hidden = false;
        }
        self.sidebar_zoom_initialized = true;
        self.sidebar_zoom_width = window.viewport_size().width;
    }

    fn can_open_sidebar(&self, window: &Window) -> bool {
        // A manual reopen must not consume the space reserved for the separate
        // composer selectors and pinned Send/Stop controls at high zoom.
        window.viewport_size().width >= self.scaled(SIDEBAR_WIDTH + 260.)
    }

    fn toggle_sidebar(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.sidebar_visible && !self.can_open_sidebar(window) {
            return;
        }
        self.sidebar_visible = !self.sidebar_visible;
        self.sidebar_auto_hidden = false;
        cx.notify();
    }

    fn request_newest(&mut self, id: &str) {
        if self.loading_messages.contains_key(id) {
            self.reload_after_load.insert(id.to_owned());
            return;
        }
        if let Some(api) = &self.api {
            self.loading_messages.insert(id.to_owned(), None);
            api.send(Command::LoadMessages {
                session_id: id.to_owned(),
                cursor: None,
            });
        }
    }

    fn load_earlier(&mut self, id: &str, cursor: &str, cx: &mut Context<Self>) {
        if self.loading_messages.contains_key(id)
            || self
                .conversations
                .get(id)
                .and_then(|conversation| conversation.next_cursor.as_deref())
                != Some(cursor)
        {
            return;
        }
        if let Some(api) = &self.api {
            self.loading_messages
                .insert(id.to_owned(), Some(cursor.to_owned()));
            api.send(Command::LoadMessages {
                session_id: id.to_owned(),
                cursor: Some(cursor.to_owned()),
            });
            cx.notify();
        }
    }

    fn persist_tabs(&mut self) {
        let key = match opencode_gpui::api::server_key(&self.settings.current.base_url) {
            Ok(key) => key,
            Err(error) => {
                self.connection_status = format!("State save failed: {error}");
                return;
            }
        };
        ensure_canonical_server_state(
            &mut self.settings.persisted,
            &self.settings.current.base_url,
            &key,
        );
        let server = self.settings.persisted.servers.entry(key).or_default();
        self.restored_server_state = true;
        let previous_tabs = std::mem::take(&mut server.tabs);
        server.tabs = self
            .open_tabs
            .iter()
            .filter_map(|id| {
                self.sessions
                    .iter()
                    .find(|session| &session.id == id)
                    .map(|session| PersistedTab {
                        id: session.id.clone(),
                        title: session.title.clone(),
                        directory: session.directory.clone(),
                    })
                    // A partial session snapshot must not erase restored tabs
                    // whose metadata has not been fetched yet.
                    .or_else(|| previous_tabs.iter().find(|tab| &tab.id == id).cloned())
            })
            .collect();
        server.active = (!self.active.is_empty()).then(|| self.active.clone());
        server.unread = self.unread.clone();
        server.busy = self
            .statuses
            .iter()
            .filter(|(_, status)| status.is_busy())
            .map(|(id, _)| id.clone())
            .chain(self.offline_busy.iter().cloned())
            .collect();
        if self.api.is_none() || self.preview_api {
            return;
        }
        if let Err(error) = self.settings.persisted.save(&persist::default_path()) {
            self.connection_status = format!("State save failed: {error}");
        }
    }

    fn set_tab_drop_target(&mut self, destination: Option<(String, bool)>, cx: &mut Context<Self>) {
        if self.tab_drop_target == destination {
            return;
        }
        if let Some(previous) = self.tab_drop_target.take() {
            self.next_tab_drop_token = self.next_tab_drop_token.wrapping_add(1);
            let token = self.next_tab_drop_token;
            self.tab_drop_outgoing.insert(previous.clone(), token);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.tab_drop_outgoing.get(&previous) == Some(&token) {
                        this.tab_drop_outgoing.remove(&previous);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        if let Some(destination) = &destination {
            self.tab_drop_outgoing.remove(destination);
        }
        self.tab_drop_target = destination;
        cx.notify();
    }

    fn drop_tab(&mut self, source: &str, target: &str, cx: &mut Context<Self>) {
        self.tab_drop_anchor = None;
        let Some((destination, after)) = self.tab_drop_target.clone() else {
            return;
        };
        self.set_tab_drop_target(None, cx);
        if self.tab_shortcut_hint || destination != target {
            return;
        }
        if reorder_tab_ids(&mut self.open_tabs, source, target, after) {
            self.persist_tabs();
        }
        cx.notify();
    }

    fn update_tab_status(&mut self, id: String, status: RunStatus) {
        let was_busy = self.statuses.get(&id).is_some_and(RunStatus::is_busy);
        let changed_busy = was_busy != status.is_busy();
        if was_busy && !status.is_busy() && self.open_tabs.contains(&id) {
            self.unread.insert(id.clone());
        }
        if status.is_busy() {
            if self.local_prompt_delivered(&id) {
                self.local_busy.remove(&id);
            }
        } else {
            // A reconnect can miss the entire Busy phase. Once the server has
            // confirmed our user message and reports Idle, its status owns the
            // run; the optimistic Busy marker must not survive indefinitely.
            if !self.pending_prompts.contains_key(&id) && self.local_prompt_delivered(&id) {
                self.local_busy.remove(&id);
            }
            if !self.local_busy.contains(&id) && !self.pending_prompts.contains_key(&id) {
                self.deferred_abort.remove(&id);
                self.abort_timeout_tokens.remove(&id);
                self.local_run_message_ids.remove(&id);
            }
        }
        self.statuses.insert(id.clone(), status);
        self.maybe_dispatch_deferred_abort(&id);
        if changed_busy && self.open_tabs.contains(&id) {
            self.persist_tabs();
        }
    }

    fn session_notification_id(&self, session: &str) -> String {
        let server = opencode_gpui::api::server_key(&self.settings.current.base_url)
            .unwrap_or_else(|_| self.settings.current.base_url.clone());
        notification_id(&server, session)
    }

    fn notify_session_ready(&mut self, session_id: &str) {
        let Some(notifier) = &self.desktop_notifications else {
            return;
        };
        // Restored root tabs may precede a complete session-list snapshot.
        // A generic title needs no metadata, while known child sessions must
        // never generate their own desktop notifications.
        if !self.open_tabs.iter().any(|id| id == session_id)
            || self.child_parents.contains_key(session_id)
            || self
                .sessions
                .iter()
                .any(|session| session.id == session_id && session.parent_id.is_some())
        {
            return;
        }
        let id = self.session_notification_id(session_id);
        self.notification_targets
            .insert(id.clone(), session_id.into());
        let title = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.title.clone())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "OpenCode".into());
        notifier.show(id, title);
        play_session_ready_sound();
    }

    fn withdraw_session_notification(&mut self, session_id: &str) {
        let id = self.session_notification_id(session_id);
        self.notification_targets.remove(&id);
        if let Some(notifier) = &self.desktop_notifications {
            notifier.withdraw(id);
        }
    }

    fn activate_session_notification(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(session_id) = self.notification_targets.remove(id) else {
            return false;
        };
        if id != self.session_notification_id(&session_id) || !self.open_tabs.contains(&session_id)
        {
            if let Some(notifier) = &self.desktop_notifications {
                notifier.withdraw(id.into());
            }
            return false;
        }
        self.select_session(session_id.clone());
        if self.unread.remove(&session_id) {
            self.persist_tabs();
        }
        if let Some(notifier) = &self.desktop_notifications {
            notifier.withdraw(id.into());
        }
        cx.notify();
        true
    }

    fn local_prompt_delivered(&self, session: &str) -> bool {
        self.local_run_message_ids
            .get(session)
            .is_some_and(|message_id| {
                self.conversations
                    .get(session)
                    .is_some_and(|conversation| conversation.has_delivered_user_message(message_id))
            })
    }

    fn maybe_dispatch_deferred_abort(&mut self, session: &str) {
        if !self.statuses.get(session).is_some_and(RunStatus::is_busy)
            || !self.local_prompt_delivered(session)
        {
            return;
        }
        self.local_busy.remove(session);
        if !self.deferred_abort.remove(session) {
            return;
        }
        self.abort_timeout_tokens.remove(session);
        if let Some(api) = &self.api {
            api.send(Command::Abort {
                session_id: session.to_owned(),
            });
        }
    }

    fn is_running(&self, session: &str) -> bool {
        self.statuses.get(session).is_some_and(RunStatus::is_busy)
            || self.local_busy.contains(session)
            || self
                .pending_prompts
                .get(session)
                .is_some_and(|pending| pending.delivery.is_none())
    }

    fn stop_active(&mut self, cx: &mut Context<Self>) {
        if self.active.is_empty() {
            return;
        }
        let session = self.active.clone();
        if (self.local_busy.contains(&session)
            || self
                .pending_prompts
                .get(&session)
                .is_some_and(|pending| pending.delivery.is_none()))
            && (!self.statuses.get(&session).is_some_and(RunStatus::is_busy)
                || !self.local_prompt_delivered(&session))
        {
            if self.deferred_abort.insert(session.clone()) {
                self.schedule_deferred_abort_check(session, cx);
            }
        } else if let Some(api) = &self.api {
            api.send(Command::Abort {
                session_id: session,
            });
        }
        cx.notify();
    }

    fn schedule_deferred_abort_check(&mut self, session: String, cx: &mut Context<Self>) {
        self.next_abort_timeout_token += 1;
        let token = self.next_abort_timeout_token;
        self.abort_timeout_tokens.insert(session.clone(), token);
        let generation = self.connection_generation;
        cx.spawn(async move |this, cx| {
            for attempt in 0..10 {
                cx.background_executor()
                    .timer(Duration::from_millis(200 * (attempt + 1)))
                    .await;
                let keep_waiting = this
                    .update(cx, |this, cx| {
                        if this.connection_generation != generation
                            || this.abort_timeout_tokens.get(&session) != Some(&token)
                            || !this.deferred_abort.contains(&session)
                        {
                            return false;
                        }
                        if attempt == 9 {
                            // Never abort a later, unrelated run because this
                            // one never exposed a Busy state.
                            this.deferred_abort.remove(&session);
                            this.abort_timeout_tokens.remove(&session);
                            if !this.pending_prompts.contains_key(&session) {
                                this.local_busy.remove(&session);
                                this.local_run_message_ids.remove(&session);
                            }
                            this.connection_status =
                                "Stop could not confirm the run started; retry if it is still running"
                                    .into();
                            cx.notify();
                            return false;
                        }
                        this.request_bootstrap();
                        true
                    })
                    .unwrap_or(false);
                if !keep_waiting {
                    break;
                }
            }
        })
        .detach();
    }

    fn close_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        self.tab_drop_target = None;
        self.tab_drop_anchor = None;
        let index = self.open_tabs.iter().position(|tab| tab == id);
        self.open_tabs.retain(|tab| tab != id);
        self.tab_focus.remove(id);
        self.conversations.remove(id);
        self.loading_messages.remove(id);
        self.message_load_errors.remove(id);
        self.skip_prune_for_load.remove(id);
        self.message_events_during_load.remove(id);
        self.reload_after_load.remove(id);
        self.transcript.remove(id);
        self.transcript_spans.remove(id);
        self.attachments.remove_session(id);
        self.transcript_scrolls.remove(id);
        self.unread.remove(id);
        self.withdraw_session_notification(id);
        if self.active == id {
            let next = index
                .and_then(|index| {
                    self.open_tabs
                        .get(index.min(self.open_tabs.len().saturating_sub(1)))
                })
                .cloned();
            if let Some(next) = next {
                self.select_session(next);
            } else {
                self.active.clear();
                self.jobs.clear();
                self.attachments_draft.clear();
            }
        }
        self.composers.remove(id);
        self.background_requests.remove(id);
        self.attachment_drafts.remove(id);
        self.pending_prompts.remove(id);
        self.failed_prompt_drafts.remove(id);
        self.confirmed_prompt_residue.remove(id);
        self.local_busy.remove(id);
        self.local_run_message_ids.remove(id);
        self.deferred_abort.remove(id);
        self.abort_timeout_tokens.remove(id);
        self.draft_actions.retain(|action| match action {
            DraftAction::Clear { session, .. }
            | DraftAction::Restore { session, .. }
            | DraftAction::ClearConfirmed { session, .. } => session != id,
        });
        self.composer_edit_generation.remove(id);
        self.prune_owned_pastes();
        self.persist_tabs();
        cx.notify();
    }

    fn sync_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_session == self.active {
            return;
        }
        if self.active.is_empty() {
            self.composer = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(1)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            });
        } else if let Some(existing) = self.composers.get(&self.active) {
            self.composer = existing.clone();
        } else {
            let composer = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(1)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            });
            let session = self.active.clone();
            cx.subscribe(
                &composer,
                move |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        latency::global().edited();
                        *this
                            .composer_edit_generation
                            .entry(session.clone())
                            .or_default() += 1;
                    }
                    InputEvent::PressEnter { secondary, shift } if !shift => {
                        this.send_prompt(*secondary, cx);
                    }
                    _ => {}
                },
            )
            .detach();
            self.composers.insert(self.active.clone(), composer.clone());
            self.composer = composer;
        }
        self.composer_session = self.active.clone();
        self.composer_placeholder_focused = false;
        self.composer.update(cx, |input, cx| {
            input.set_placeholder("Ask OpenCode anything…", window, cx);
        });
    }

    fn focus_selected_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_composer(window, cx);
        let focus = if self.modal.is_none() && self.visible_permission().is_some() {
            self.permission_container_focus.clone()
        } else {
            self.composer.focus_handle(cx)
        };
        focus.focus(window, cx);
        window.on_next_frame(move |window, cx| focus.focus(window, cx));
    }

    fn selected_model(&self) -> Option<ModelSelection> {
        let session = self
            .sessions
            .iter()
            .find(|session| session.id == self.active);
        model::displayed_model(
            self.model_switches
                .get(&self.active)
                .map(|pending| &pending.selection),
            session,
            &self.catalog,
        )
    }

    fn choose_model(&mut self, model: protocol::ModelRef, cx: &mut Context<Self>) {
        if self.active.is_empty() {
            return;
        }
        let displayed = self.selected_model();
        if let Some(selection) =
            model::model_switch_for_pick(displayed.as_ref(), ModelSelection::from_ref(&model))
        {
            if let Some(api) = &self.api {
                self.next_model_request_id += 1;
                let request_id = self.next_model_request_id;
                self.model_switches.insert(
                    self.active.clone(),
                    PendingModelPick {
                        request_id,
                        selection,
                    },
                );
                api.send(Command::SelectModel {
                    request_id,
                    session_id: self.active.clone(),
                    model,
                });
            } else if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == self.active)
            {
                session.model = Some(SessionModel::from_selection(&selection));
            }
        }
        self.modal = None;
        self.focus_composer_pending = true;
        cx.notify();
    }

    fn create_session(&mut self, directory: String, cx: &mut Context<Self>) {
        if let Some(api) = &self.api {
            self.next_session_request_id += 1;
            api.send(Command::CreateSession {
                request_id: self.next_session_request_id,
                directory,
                title: None,
            });
        }
        self.modal = None;
        self.focus_composer_pending = true;
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.rename_pending.is_none() {
            self.modal = None;
            self.rename_target = None;
            self.rename_error = None;
            self.focus_composer_pending = true;
            cx.notify();
        }
    }

    fn cancel_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.reset_draft(window, cx);
        self.modal = None;
        self.focus_composer_pending = true;
        cx.notify();
    }

    fn rename_session(&mut self, cx: &mut Context<Self>) {
        if self.modal != Some(Modal::Rename) || self.rename_pending.is_some() {
            return;
        }
        let title = self.rename.read(cx).value().trim().to_owned();
        let id = self.rename_target.as_deref().unwrap_or(&self.active);
        if title.is_empty() {
            self.rename_error = Some("Enter a session title".into());
            cx.notify();
            return;
        }
        if id.is_empty() {
            return;
        }
        let unchanged = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .is_some_and(|session| session.title == title);
        if unchanged {
            self.rename_target = None;
            self.rename_error = None;
            self.modal = None;
            self.focus_composer_pending = true;
            cx.notify();
            return;
        }
        if let Some(api) = &self.api {
            self.next_session_request_id += 1;
            self.rename_pending = Some(self.next_session_request_id);
            self.rename_error = None;
            api.send(Command::RenameSession {
                request_id: self.next_session_request_id,
                session_id: id.to_owned(),
                title,
            });
        } else if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.title = title;
            self.rename_target = None;
            self.modal = None;
            self.focus_composer_pending = true;
        }
        cx.notify();
    }

    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_api {
            self.settings.reset_draft(window, cx);
            self.modal = None;
            self.focus_composer_pending = true;
            cx.notify();
            return;
        }
        match self.apply_settings_inner(cx) {
            Ok(()) => {
                self.settings.reset_draft(window, cx);
                self.modal = None;
                self.focus_composer_pending = true;
            }
            Err(error) => self.settings.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn apply_settings_inner(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let server = self
            .settings
            .server
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        let username = self.settings.username.read(cx).value().trim().to_owned();
        let typed_password = self.settings.password.read(cx).value().to_string();
        let client_id = self.settings.client_id.read(cx).value().trim().to_owned();
        let client_secret = self.settings.client_secret.read(cx).value().to_string();
        let url = url::Url::parse(&server).map_err(|_| anyhow::anyhow!("Invalid server URL"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!("The server URL cannot contain credentials, a query, or a fragment");
        }
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        };
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            anyhow::bail!("Remote servers require HTTPS; loopback HTTP is allowed");
        }
        if username.is_empty() {
            anyhow::bail!("Username cannot be empty");
        }
        let old = &self.settings.current;
        let old_stored = self.settings.persisted.connection.basic_auth_in_keyring
            && credentials::same_password_identity(
                &old.base_url,
                &old.username,
                &self.settings.persisted.connection.server,
                &self.settings.persisted.connection.username,
            );
        let password_plan = credentials::plan_password(
            &SystemKeyring,
            PasswordTarget {
                server: &old.base_url,
                username: &old.username,
            },
            old.password.as_deref(),
            old_stored,
            PasswordTarget {
                server: &server,
                username: &username,
            },
            &typed_password,
            self.settings.remember_password,
        );
        let cloudflare_access = if client_id.is_empty() {
            None
        } else if !client_secret.is_empty() {
            Some(CloudflareAccessCredentials::new(
                client_id.clone(),
                client_secret,
            )?)
        } else {
            self.settings
                .current
                .cloudflare_access
                .clone()
                .filter(|old| old.client_id == client_id)
                .ok_or_else(|| anyhow::anyhow!("Enter the Cloudflare client secret for this ID"))?
                .into()
        };
        let config = ApiConfig {
            base_url: server.clone(),
            username: username.clone(),
            password: password_plan.password.clone(),
            cloudflare_access: cloudflare_access.clone(),
        };
        // When transport settings change, open the replacement before writing
        // credentials under its identity. An unchanged config must not tear
        // down the current transcript, tabs, drafts, or SSE connection.
        let next_connection =
            if needs_new_connection(&self.settings.current, &config, self.api.is_some()) {
                Some(ApiHandle::start(config.clone())?)
            } else {
                None
            };
        if let Some(token) = &cloudflare_access {
            credentials::save(&server, token)?;
        } else if old.cloudflare_access.is_some() && old.base_url.trim_end_matches('/') == server {
            credentials::remove(&old.base_url)?;
        }
        let (stored, warning) =
            credentials::apply_password_change(&SystemKeyring, &server, &username, &password_plan);
        self.settings.warning = warning.clone();
        if let Some(warning) = &warning {
            log::warn!("{warning}");
        }
        let mut persisted = self.settings.persisted.clone();
        let previous_key = opencode_gpui::api::server_key(&old.base_url)
            .unwrap_or_else(|_| old.base_url.trim_end_matches('/').to_owned());
        ensure_canonical_server_state(&mut persisted, &old.base_url, &previous_key);
        if next_connection.is_some() && self.api.is_some() {
            persisted
                .servers
                .entry(previous_key.clone())
                .or_default()
                .busy = self
                .statuses
                .iter()
                .filter(|(_, status)| status.is_busy())
                .map(|(id, _)| id.clone())
                .chain(self.offline_busy.iter().cloned())
                .collect();
        }
        let next_unread = next_connection.as_ref().map(|(_, _, key)| {
            ensure_canonical_server_state(&mut persisted, &server, key);
            unread_on_server_switch(
                &mut persisted,
                self.api
                    .as_ref()
                    .map(|_| (previous_key.as_str(), &self.unread)),
                key,
            )
        });
        let next_busy = next_connection.as_ref().map(|(_, _, key)| {
            persisted
                .servers
                .get(key)
                .map(|saved| saved.busy.clone())
                .unwrap_or_default()
        });
        persisted.connection = ConnectionSettings {
            server: server.clone(),
            username,
            cloudflare_access: cloudflare_access.is_some(),
            basic_auth_in_keyring: stored,
        };
        persisted.save(&persist::default_path())?;
        self.settings.persisted = persisted.clone();
        self.settings.current = config;
        if next_connection.is_none() {
            if let Some(warning) = warning {
                self.connection_status = format!("Connected · {warning}");
            }
            return Ok(());
        }
        let (api, receiver, key) = next_connection.expect("new connection was started");
        self.connection_generation += 1;
        let generation = self.connection_generation;
        self.api = Some(api);
        self.connection_status = warning.map_or_else(
            || "Connecting".into(),
            |warning| format!("Connecting · {warning}"),
        );
        self.disconnected = false;
        self.sse_connected_once = false;
        self.sessions.clear();
        self.unread = next_unread.expect("new connection has restored unread state");
        self.offline_busy = next_busy.expect("new connection has restored busy state");
        self.open_tabs = persisted
            .servers
            .get(&key)
            .map(|saved| saved.tabs.iter().map(|tab| tab.id.clone()).collect())
            .unwrap_or_default();
        self.tab_drop_target = None;
        self.tab_drop_anchor = None;
        self.bootstrapped = false;
        self.bootstrap_in_flight = false;
        self.bootstrap_after_load = false;
        self.bootstrap_retry_scheduled = false;
        self.bootstrap_retry_count = 0;
        self.bootstrap_events.clear();
        self.refresh_open_tabs = false;
        self.projects.clear();
        self.active.clear();
        self.composer_session = "reset".into();
        self.composers.clear();
        self.attachments_draft.clear();
        self.attachment_drafts.clear();
        self.pending_prompts.clear();
        self.background_requests.clear();
        self.prune_owned_pastes();
        self.model_switches.clear();
        self.failed_prompt_drafts.clear();
        self.confirmed_prompt_residue.clear();
        self.draft_actions.clear();
        self.composer_edit_generation.clear();
        self.local_busy.clear();
        self.local_run_message_ids.clear();
        self.deferred_abort.clear();
        self.abort_timeout_tokens.clear();
        self.transcript.clear();
        self.transcript_spans.clear();
        self.attachments = ImageCache::default();
        self.transcript_scrolls.clear();
        self.scroll = ScrollHandle::new();
        self.conversations.clear();
        self.terminal_shells.clear();
        if let Some(notifier) = &self.desktop_notifications {
            for id in self.notification_targets.keys() {
                notifier.withdraw(id.clone());
            }
        }
        self.notification_targets.clear();
        self.loading_messages.clear();
        self.message_load_errors.clear();
        self.skip_prune_for_load.clear();
        self.message_events_during_load.clear();
        self.reload_after_load.clear();
        self.follow_bottom = true;
        self.transcript_pointer_down = false;
        self.catalogs.clear();
        self.model_retry_count.clear();
        self.model_retry_scheduled.clear();
        self.statuses.clear();
        self.forms.clear();
        self.permissions.clear();
        self.permission_in_flight.clear();
        self.permission_presented = None;
        self.child_parents.clear();
        self.tray_in_flight.clear();
        self.jobs.clear();
        self.running_jobs = Jobs::default();
        let saved = persisted.servers.get(&key);
        self.restored_server_state = saved.is_some();
        self.saved_active = saved.and_then(|saved| saved.active.clone());
        self.bootstrap_directories = saved
            .map(|saved| saved.tabs.iter().map(|tab| tab.directory.clone()).collect())
            .unwrap_or_default();
        self.request_bootstrap();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update(cx, |this, cx| {
                        if this.connection_generation == generation {
                            this.handle_live_event(event, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Ok(())
    }

    fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        self.link_menu = None;
        self.autoscroll = None;
        self.modal = Some(modal);
        self.picker_highlight = None;
        match modal {
            Modal::Sessions | Modal::NewSession | Modal::Model | Modal::Level => {
                if modal == Modal::Sessions {
                    self.sessions_picker_scroll
                        .set_offset(point(px(0.), px(0.)));
                } else if modal == Modal::NewSession {
                    self.projects_picker_scroll
                        .set_offset(point(px(0.), px(0.)));
                } else if matches!(modal, Modal::Model | Modal::Level) {
                    self.picker_list_scroll.set_offset(point(px(0.), px(0.)));
                }
                let placeholder = match modal {
                    Modal::Model => "Search models (fuzzy)...",
                    Modal::Level => "Search levels (fuzzy)...",
                    Modal::Sessions => "Search tabs...",
                    Modal::NewSession => "Search projects…",
                    _ => "Search…",
                };
                self.search.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx);
                    input.set_value("", window, cx);
                });
                self.search.focus_handle(cx).focus(window, cx);
            }
            Modal::Rename => {
                self.rename_pending = None;
                self.rename_error = None;
                let target = self.rename_target.as_deref().unwrap_or(&self.active);
                let title = self
                    .sessions
                    .iter()
                    .find(|session| session.id == target)
                    .map(|session| session.title.clone())
                    .unwrap_or_default();
                self.rename.update(cx, |input, cx| {
                    input.set_value(title, window, cx);
                    input.select_all(window, cx);
                });
                self.rename.focus_handle(cx).focus(window, cx);
            }
            Modal::Settings => {
                self.settings.reset_draft(window, cx);
                self.settings_highlight = None;
                if self.settings.tab == SettingsTab::Sessions {
                    self.settings_sessions_scroll
                        .set_offset(point(px(0.), px(0.)));
                    self.settings
                        .session_search
                        .focus_handle(cx)
                        .focus(window, cx);
                } else {
                    self.settings.server.focus_handle(cx).focus(window, cx);
                    self.settings
                        .server
                        .update(cx, |input, cx| input.select_all(window, cx));
                }
            }
        }
        cx.notify();
    }

    fn switch_settings_tab(
        &mut self,
        tab: SettingsTab,
        focus_field: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.tab = tab;
        if tab == SettingsTab::Sessions {
            self.settings_highlight = None;
            self.settings_sessions_scroll
                .set_offset(point(px(0.), px(0.)));
        }
        if focus_field {
            match tab {
                SettingsTab::Connection => self.settings.server.focus_handle(cx).focus(window, cx),
                SettingsTab::Sessions => self
                    .settings
                    .session_search
                    .focus_handle(cx)
                    .focus(window, cx),
            }
        } else {
            let index = if tab == SettingsTab::Connection { 0 } else { 1 };
            self.settings_tab_focus[index].focus(window, cx);
        }
        cx.notify();
    }

    fn picker_focus_keys(&self, cx: &Context<Self>) -> Vec<String> {
        let query = self.search.read(cx).value();
        match self.modal {
            Some(Modal::Sessions) => filter_tab_sessions(&self.sessions, &self.open_tabs, &query)
                .into_iter()
                .map(|session| format!("session:{}", session.id))
                .collect(),
            Some(Modal::NewSession) => filter_new_session_projects(
                &self.projects,
                &self.sessions,
                self.sessions
                    .iter()
                    .find(|session| session.id == self.active)
                    .map(|session| session.directory.as_str()),
                &query,
            )
            .into_iter()
            .map(|(_, directory)| format!("project:{directory}"))
            .collect(),
            Some(Modal::Model) => filter_models(&self.catalog.models, &query)
                .into_iter()
                .map(|option| format!("model:{}:{}", option.provider_id, option.model_id))
                .collect(),
            Some(Modal::Level) => {
                let variants = self
                    .selected_model()
                    .as_ref()
                    .and_then(|selection| self.catalog.find(selection))
                    .map(|option| option.variants.clone())
                    .unwrap_or_default();
                filter_levels(&variants, &query)
                    .into_iter()
                    .map(|variant| format!("level:{}", variant.as_deref().unwrap_or("Default")))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn modal_choice_count(&self, cx: &Context<Self>) -> usize {
        let query = self.search.read(cx).value().to_lowercase();
        match self.modal {
            Some(Modal::Sessions) => {
                filter_tab_sessions(&self.sessions, &self.open_tabs, &query).len()
            }
            Some(Modal::NewSession) => filter_new_session_projects(
                &self.projects,
                &self.sessions,
                self.sessions
                    .iter()
                    .find(|session| session.id == self.active)
                    .map(|session| session.directory.as_str()),
                &query,
            )
            .len(),
            Some(Modal::Model) => filter_models(&self.catalog.models, &query).len(),
            Some(Modal::Level) => {
                let variants = self
                    .selected_model()
                    .as_ref()
                    .and_then(|selection| self.catalog.find(selection))
                    .map(|option| option.variants.as_slice())
                    .unwrap_or_default();
                filter_levels(variants, &query).len()
            }
            _ => 0,
        }
    }

    fn accept_modal_choice(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_lowercase();
        let index = self.picker_highlight.unwrap_or_default();
        match self.modal {
            Some(Modal::Sessions) => {
                let id = filter_tab_sessions(&self.sessions, &self.open_tabs, &query)
                    .get(index)
                    .map(|session| session.id.clone());
                if let Some(id) = id {
                    self.select_session(id);
                    self.modal = None;
                }
            }
            Some(Modal::NewSession) => {
                let filtered = filter_new_session_projects(
                    &self.projects,
                    &self.sessions,
                    self.sessions
                        .iter()
                        .find(|session| session.id == self.active)
                        .map(|session| session.directory.as_str()),
                    &query,
                );
                let directory = new_session_choice(&filtered, index, &query);
                if let Some(directory) = directory {
                    self.create_session(directory, cx);
                }
            }
            Some(Modal::Model) => {
                let choice = filter_models(&self.catalog.models, &query)
                    .get(index)
                    .map(|option| protocol::ModelRef {
                        id: option.model_id.clone(),
                        provider_id: option.provider_id.clone(),
                        variant: None,
                    });
                if let Some(choice) = choice {
                    self.choose_model(choice, cx);
                }
            }
            Some(Modal::Level) => {
                let chosen = self.selected_model();
                let variant = chosen
                    .as_ref()
                    .and_then(|selection| self.catalog.find(selection))
                    .map(|option| option.variants.as_slice())
                    .unwrap_or_default();
                let variant = filter_levels(variant, &query).get(index).cloned();
                if let (Some(selection), Some(variant)) = (chosen, variant) {
                    self.choose_model(
                        protocol::ModelRef {
                            id: selection.model_id,
                            provider_id: selection.provider_id,
                            variant,
                        },
                        cx,
                    );
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        trace_input(TraceAction::Key, cx);
        let _latency = latency::global().span(TraceOperation::InputHandler, 1);
        let modifiers = &event.keystroke.modifiers;
        let key = event.keystroke.key.to_ascii_lowercase();
        if (matches!(key.as_str(), "alt" | "alt_l" | "alt_r")
            || (modifiers.alt && !modifiers.control))
            && !self.tab_shortcut_hint
        {
            self.tab_shortcut_hint = true;
            cx.notify();
        }
        if key == "escape"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.shift
        {
            if self.modal == Some(Modal::Rename) && self.rename_pending.is_some() {
                cx.stop_propagation();
                return;
            }
            if let Some(modal) = self.modal.take() {
                if modal == Modal::Settings {
                    self.settings.reset_draft(window, cx);
                }
                self.rename_target = None;
                self.composer.focus_handle(cx).focus(window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if self.link_menu.take().is_some() {
                self.composer.focus_handle(cx).focus(window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if self.autoscroll.take().is_some() {
                cx.stop_propagation();
                cx.notify();
                return;
            }
            // GTK consumes Escape while the permission prompt replaces the
            // composer; it must not acknowledge the active tab's unread mark.
            if self.visible_permission().is_some() {
                cx.stop_propagation();
                return;
            }
            if self.unread.remove(&self.active) {
                let active = self.active.clone();
                self.withdraw_session_notification(&active);
                self.persist_tabs();
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        if key == "f2"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.shift
            && self.modal.is_none()
            && !self.active.is_empty()
        {
            self.rename_target = Some(self.active.clone());
            self.show_modal(Modal::Rename, window, cx);
            cx.stop_propagation();
            return;
        }
        if matches!(key.as_str(), "enter" | "return")
            && self.modal == Some(Modal::Rename)
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.shift
        {
            self.rename_session(cx);
            cx.stop_propagation();
            return;
        }
        if matches!(key.as_str(), "enter" | "return")
            && self.modal == Some(Modal::Settings)
            && self.settings.tab == SettingsTab::Connection
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.shift
        {
            let field_focused = [
                &self.settings.server,
                &self.settings.username,
                &self.settings.password,
                &self.settings.client_id,
                &self.settings.client_secret,
            ]
            .iter()
            .any(|input| input.focus_handle(cx).is_focused(window));
            if modifiers.control || field_focused {
                self.apply_settings(window, cx);
                cx.stop_propagation();
                return;
            }
        }
        if self.modal == Some(Modal::Settings)
            && self.settings.tab == SettingsTab::Sessions
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.shift
        {
            let search_focused = self
                .settings
                .session_search
                .focus_handle(cx)
                .is_focused(window);
            let query = self.settings.session_search.read(cx).value();
            let choices = filter_all_sessions(&self.sessions, &query);
            let row_focused = self
                .settings_session_focus
                .values()
                .any(|focus| focus.is_focused(window));
            if !choices.is_empty() && (search_focused || row_focused) {
                if matches!(key.as_str(), "enter" | "return") {
                    let index = if search_focused {
                        0
                    } else {
                        self.settings_highlight.unwrap_or_default()
                    };
                    self.select_session(choices[index.min(choices.len() - 1)].id.clone());
                    self.modal = None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if key == "down" || (key == "up" && !search_focused) {
                    let next = if search_focused {
                        Some(0)
                    } else if key == "up" && self.settings_highlight == Some(0) {
                        None
                    } else if key == "up" {
                        Some(
                            self.settings_highlight
                                .unwrap_or_default()
                                .saturating_sub(1),
                        )
                    } else {
                        Some(
                            self.settings_highlight
                                .map_or(0, |current| (current + 1).min(choices.len() - 1)),
                        )
                    };
                    self.settings_highlight = next;
                    if let Some(index) = next {
                        self.settings_sessions_scroll.scroll_to_item(index);
                        if let Some(focus) = self.settings_session_focus.get(&choices[index].id) {
                            focus.focus(window, cx);
                        }
                    } else {
                        self.settings
                            .session_search
                            .focus_handle(cx)
                            .focus(window, cx);
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        }
        if matches!(key.as_str(), "enter" | "space") {
            let query = self.search.read(cx).value();
            let focused = match self.modal {
                Some(Modal::Sessions) => {
                    filter_tab_sessions(&self.sessions, &self.open_tabs, &query)
                        .into_iter()
                        .find(|session| {
                            self.picker_choice_focus
                                .get(&format!("session:{}", session.id))
                                .is_some_and(|focus| focus.is_focused(window))
                        })
                        .map(|session| (false, session.id.clone()))
                }
                Some(Modal::NewSession) => filter_new_session_projects(
                    &self.projects,
                    &self.sessions,
                    self.sessions
                        .iter()
                        .find(|session| session.id == self.active)
                        .map(|session| session.directory.as_str()),
                    &query,
                )
                .into_iter()
                .find(|(_, directory)| {
                    self.picker_choice_focus
                        .get(&format!("project:{directory}"))
                        .is_some_and(|focus| focus.is_focused(window))
                })
                .map(|(_, directory)| (true, directory)),
                _ => None,
            };
            if let Some((create, id)) = focused {
                if create {
                    self.create_session(id, cx);
                } else {
                    self.select_session(id);
                    self.modal = None;
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            let focused_model = match self.modal {
                Some(Modal::Model) => filter_models(&self.catalog.models, &query)
                    .into_iter()
                    .find(|option| {
                        self.picker_choice_focus
                            .get(&format!("model:{}:{}", option.provider_id, option.model_id))
                            .is_some_and(|focus| focus.is_focused(window))
                    })
                    .map(|option| protocol::ModelRef {
                        id: option.model_id.clone(),
                        provider_id: option.provider_id.clone(),
                        variant: None,
                    }),
                Some(Modal::Level) => self.selected_model().and_then(|selection| {
                    let variants = self.catalog.find(&selection)?.variants.clone();
                    filter_levels(&variants, &query)
                        .into_iter()
                        .find(|variant| {
                            self.picker_choice_focus
                                .get(&format!(
                                    "level:{}",
                                    variant.as_deref().unwrap_or("Default")
                                ))
                                .is_some_and(|focus| focus.is_focused(window))
                        })
                        .map(|variant| protocol::ModelRef {
                            id: selection.model_id,
                            provider_id: selection.provider_id,
                            variant,
                        })
                }),
                _ => None,
            };
            if let Some(model) = focused_model {
                self.choose_model(model, cx);
                cx.stop_propagation();
                return;
            }
        }
        if matches!(key.as_str(), "up" | "down") && self.modal_choice_count(cx) > 0 {
            let len = self.modal_choice_count(cx);
            self.picker_highlight = Some(if key == "up" {
                self.picker_highlight.unwrap_or_default().saturating_sub(1)
            } else {
                self.picker_highlight
                    .map_or(0, |current| (current + 1).min(len - 1))
            });
            if self.modal == Some(Modal::Sessions) {
                self.sessions_picker_scroll
                    .scroll_to_item(self.picker_highlight.unwrap_or_default());
            } else if self.modal == Some(Modal::NewSession) {
                self.projects_picker_scroll
                    .scroll_to_item(self.picker_highlight.unwrap_or_default());
            } else if matches!(self.modal, Some(Modal::Model | Modal::Level)) {
                self.picker_list_scroll
                    .scroll_to_item(self.picker_highlight.unwrap_or_default());
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if matches!(key.as_str(), "enter" | "return") && self.modal_choice_count(cx) > 0 {
            self.accept_modal_choice(cx);
            cx.stop_propagation();
            return;
        }
        if modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && !modifiers.shift
            && self.modal.is_none()
            && let Some(index) = tab_number_key(&key)
            && let Some(id) = self.open_tabs.get(index).cloned()
        {
            self.select_session(id);
            self.focus_selected_composer(window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if !modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        if key == "q" && !modifiers.shift {
            cx.quit();
            cx.stop_propagation();
            return;
        }
        let zoom_direction = match key.as_str() {
            "=" | "equal" | "+" | "plus" | "kp_add" => Some(1),
            "-" | "minus" | "kp_subtract" => Some(-1),
            "0" | "kp_0" | "numpad0" => Some(0),
            _ => None,
        };
        if let Some(direction) = zoom_direction {
            let next = stepped_zoom(self.settings.persisted.zoom_level, direction);
            self.set_zoom(next, window, cx);
            cx.stop_propagation();
            return;
        }
        if key == "x" && modifiers.shift {
            if let Some(target) = self
                .forms
                .notice(Some(&self.active), &self.child_parents)
                .and_then(|notice| notice.cancel)
            {
                self.cancel_waiting_form(&target);
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if key == "b" && !modifiers.shift {
            self.toggle_sidebar(window, cx);
            cx.stop_propagation();
            return;
        }
        if key == "b" && modifiers.shift && self.modal.is_none() {
            let session = self.active.clone();
            self.send_to_background(&session, cx);
            cx.stop_propagation();
            return;
        }
        if key == "l" && modifiers.shift {
            self.toggle_latency_capture(window, cx);
            cx.stop_propagation();
            return;
        }
        let action = match key.as_str() {
            "t" => Some(Modal::NewSession),
            "p" => Some(Modal::Sessions),
            "," | "comma" => Some(Modal::Settings),
            "m" if self.modal.is_none() => Some(Modal::Model),
            "/" | "slash" | "kp_divide" if self.modal.is_none() => Some(Modal::Level),
            _ => None,
        };
        if let Some(modal) = action {
            self.show_modal(modal, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.modal.is_some() {
            return;
        }
        if key == "u" && !modifiers.shift {
            self.choose_attachments(cx);
            cx.stop_propagation();
            return;
        }
        let selection =
            if matches!(key.as_str(), "tab" | "iso_left_tab") && !self.open_tabs.is_empty() {
                let current = self
                    .open_tabs
                    .iter()
                    .position(|id| *id == self.active)
                    .unwrap_or_default();
                let next = if modifiers.shift || key == "iso_left_tab" {
                    (current + self.open_tabs.len() - 1) % self.open_tabs.len()
                } else {
                    (current + 1) % self.open_tabs.len()
                };
                self.open_tabs.get(next).cloned()
            } else if let Some(index) = tab_number_key(&key) {
                self.open_tabs.get(index).cloned()
            } else {
                None
            };
        if let Some(id) = selection {
            self.select_session(id);
            self.focus_selected_composer(window, cx);
            cx.stop_propagation();
            cx.notify();
        } else if key == "w" && !self.active.is_empty() {
            let id = self.active.clone();
            self.close_tab(&id, cx);
            self.focus_selected_composer(window, cx);
            cx.stop_propagation();
        } else if key == "g" {
            self.composer.focus_handle(cx).focus(window, cx);
            cx.stop_propagation();
        }
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.tab_shortcut_hint
            && (matches!(
                event.keystroke.key.to_ascii_lowercase().as_str(),
                "alt" | "alt_l" | "alt_r"
            ) || !event.keystroke.modifiers.alt)
        {
            self.tab_shortcut_hint = false;
            cx.notify();
        }
    }

    fn active_directories(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|session| self.open_tabs.contains(&session.id))
            .map(|session| session.directory.clone())
            .collect()
    }

    fn invalidate_catalogs(&mut self, directory: Option<String>) -> Vec<String> {
        let mut directories = if let Some(directory) = directory {
            vec![directory]
        } else {
            let mut directories = self.active_directories();
            directories.extend(self.catalogs.keys().cloned());
            directories
        };
        directories.sort();
        directories.dedup();
        let active_directory = self
            .sessions
            .iter()
            .find(|session| session.id == self.active)
            .map(|session| session.directory.as_str());
        for directory in &directories {
            self.catalogs.remove(directory);
            self.model_retry_count.remove(directory);
            self.model_retry_scheduled.remove(directory);
            if active_directory == Some(directory.as_str()) {
                self.catalog = ModelCatalog::default();
            }
        }
        directories
    }

    fn schedule_empty_catalog_retry(&mut self, directory: String, cx: &mut Context<Self>) {
        if self.api.is_none()
            || !self.active_directories().contains(&directory)
            || self.model_retry_scheduled.contains(&directory)
        {
            return;
        }
        let attempts = self.model_retry_count.entry(directory.clone()).or_default();
        if *attempts >= 5 {
            return;
        }
        let delay = 1u64 << (*attempts).min(4);
        *attempts += 1;
        self.model_retry_scheduled.insert(directory.clone());
        let generation = self.connection_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(delay))
                .await;
            let _ = this.update(cx, |this, _| {
                if this.connection_generation != generation
                    || !this.model_retry_scheduled.remove(&directory)
                    || !this.active_directories().contains(&directory)
                    || this
                        .catalogs
                        .get(&directory)
                        .is_some_and(|catalog| !catalog.models.is_empty())
                {
                    return;
                }
                if let Some(api) = &this.api {
                    api.send(Command::LoadModels { directory });
                }
            });
        })
        .detach();
    }

    fn request_bootstrap(&mut self) {
        if self.bootstrap_in_flight {
            self.bootstrap_after_load = true;
            return;
        }
        let Some(api) = &self.api else { return };
        self.bootstrap_in_flight = true;
        self.bootstrap_events.clear();
        let mut directories = self.bootstrap_directories.clone();
        directories.extend(self.active_directories());
        directories.sort();
        directories.dedup();
        api.send(Command::Bootstrap {
            sessions: self.open_tabs.clone(),
            directories,
        });
    }

    fn schedule_bootstrap_retry(&mut self, cx: &mut Context<Self>) {
        if self.bootstrap_retry_scheduled || self.api.is_none() {
            return;
        }
        self.bootstrap_retry_scheduled = true;
        let generation = self.connection_generation;
        let seconds = 1u64 << self.bootstrap_retry_count.min(5);
        self.bootstrap_retry_count = self.bootstrap_retry_count.saturating_add(1).min(5);
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(seconds))
                .await;
            let _ = this.update(cx, |this, _| {
                if this.connection_generation == generation && this.bootstrap_retry_scheduled {
                    this.bootstrap_retry_scheduled = false;
                    this.request_bootstrap();
                }
            });
        })
        .detach();
    }

    fn replay_bootstrap_events(&mut self, cx: &mut Context<Self>) {
        self.bootstrap_in_flight = false;
        for envelope in std::mem::take(&mut self.bootstrap_events) {
            self.handle_live_event(UiEvent::ServerEvent(envelope), cx);
        }
        if std::mem::take(&mut self.bootstrap_after_load) {
            self.request_bootstrap();
        }
    }

    fn upsert_permission(
        &mut self,
        directory: Option<String>,
        request: protocol::PermissionRequest,
    ) {
        if let Some(existing) = self
            .permissions
            .iter_mut()
            .find(|item| item.request.id == request.id)
        {
            existing.directory = directory.or(existing.directory.take());
            existing.request = request;
        } else {
            self.permissions
                .push(PendingPermission { request, directory });
        }
    }

    fn reconcile_pending(&mut self, requests: Vec<PendingRequest>, covered: &HashSet<String>) {
        let missing_permissions = pending::dismissed_requests(
            self.permissions.iter().map(|item| &item.request.id),
            &requests,
            covered,
            |id| {
                self.permissions
                    .iter()
                    .find(|item| item.request.id == id)
                    .and_then(|item| item.directory.clone())
            },
        );
        self.permissions
            .retain(|item| !missing_permissions.contains(&item.request.id));
        for id in missing_permissions {
            self.permission_in_flight.remove(&id);
        }
        let form_ids: Vec<String> = self.forms.ids().map(str::to_owned).collect();
        let missing_forms =
            pending::dismissed_requests(form_ids.iter(), &requests, covered, |id| {
                self.forms.directory(id).map(str::to_owned)
            });
        for id in missing_forms {
            self.forms.remove(&id);
        }
        for pending in requests {
            match pending {
                PendingRequest::Form(form) => self.forms.upsert(form),
                PendingRequest::Permission { directory, request } => {
                    self.upsert_permission(Some(directory), request);
                }
            }
        }
    }

    fn visible_permission(&self) -> Option<PendingPermission> {
        self.permissions
            .iter()
            .find(|item| {
                pending::permission_scope(
                    &item.request.session_id,
                    |id| self.sessions.iter().any(|session| session.id == id),
                    &self.child_parents,
                )
                .is_none_or(|scope| scope == self.active)
            })
            .cloned()
    }

    fn reply_permission(
        &mut self,
        request_id: String,
        session_id: String,
        decision: protocol::PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        if !self.permission_in_flight.insert(request_id.clone()) {
            return;
        }
        if let Some(api) = &self.api {
            api.send(Command::ReplyPermission {
                request_id,
                session_id,
                decision,
            });
        } else {
            self.permissions
                .retain(|item| item.request.id != request_id);
            self.permission_in_flight.remove(&request_id);
        }
        cx.notify();
    }

    fn choose_attachments(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active.clone();
        if session_id.is_empty() {
            return;
        }
        let supports_attachments = self
            .selected_model()
            .as_ref()
            .and_then(|selection| self.catalog.find(selection))
            .is_none_or(|model| model.supports_attachments);
        if !supports_attachments {
            self.connection_status = "Selected model does not accept attachments".into();
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            let picked = rfd::AsyncFileDialog::new().pick_files().await;
            if let Some(files) = picked {
                let _ = this.update(cx, |this, cx| {
                    if !this.open_tabs.contains(&session_id) {
                        return;
                    }
                    let draft = if this.active == session_id {
                        &mut this.attachments_draft
                    } else {
                        this.attachment_drafts
                            .entry(session_id.clone())
                            .or_default()
                    };
                    for file in files {
                        let path = file.path().to_path_buf();
                        if !draft.contains(&path) {
                            draft.push(path);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn paste_attachments(
        &mut self,
        session: &str,
        clipboard: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        let images: Vec<_> = clipboard
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        let paths: Vec<PathBuf> = if images.is_empty() {
            clipboard
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    ClipboardEntry::ExternalPaths(files) => Some(files.0.iter()),
                    _ => None,
                })
                .flatten()
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if images.is_empty() && paths.is_empty() {
            return false;
        }
        // A paste callback can outlive its displayed tab. Never put an image
        // on the session selected after the paste originated.
        if self.active != session || self.composer_session != session {
            return true;
        }
        if self
            .selected_model()
            .as_ref()
            .and_then(|selection| self.catalog.find(selection))
            .is_some_and(|model| !model.supports_attachments)
        {
            self.connection_status = "Selected model does not accept attachments".into();
            cx.notify();
            return true;
        }

        let mut staged: Vec<(PathBuf, Arc<tempfile::NamedTempFile>)> = Vec::new();
        let result = (|| -> Result<Vec<PathBuf>, String> {
            for image in images {
                if image.bytes().len() > protocol::MAX_ATTACHMENT_BYTES {
                    return Err("Pasted image exceeds the 20 MiB attachment limit".into());
                }
                let mut file = tempfile::Builder::new()
                    .prefix("opencode-gpui-paste-")
                    .suffix(&format!(".{}", image.format().extension()))
                    .tempfile()
                    .map_err(|error| format!("Cannot save pasted image: {error}"))?;
                file.write_all(image.bytes())
                    .map_err(|error| format!("Cannot save pasted image: {error}"))?;
                staged.push((file.path().to_path_buf(), Arc::new(file)));
            }
            let mut combined = self.attachments_draft.clone();
            combined.extend(paths.iter().cloned());
            combined.extend(staged.iter().map(|(path, _)| path.clone()));
            opencode_gpui::api::check_attachments(&combined)
                .map_err(|error| format!("Cannot attach pasted file: {error}"))?;
            Ok(combined)
        })();
        match result {
            Ok(combined) => {
                self.owned_pastes.extend(staged);
                self.attachments_draft = combined;
            }
            Err(error) => {
                // Dropping the staged NamedTempFiles removes every partial file.
                self.connection_status = error;
            }
        }
        cx.notify();
        true
    }

    fn cleanup_owned_paste(&mut self, path: &PathBuf) {
        if self.attachments_draft.contains(path)
            || self
                .attachment_drafts
                .values()
                .any(|draft| draft.contains(path))
            || self
                .pending_prompts
                .values()
                .any(|pending| pending.attachments.contains(path))
            || self.draft_actions.iter().any(|action| {
                matches!(action,
                DraftAction::Restore { pending, .. } if pending.attachments.contains(path))
            })
        {
            return;
        }
        self.owned_pastes.remove(path);
    }

    fn prune_owned_pastes(&mut self) {
        self.owned_pastes.retain(|path, _| {
            self.attachments_draft.contains(path)
                || self
                    .attachment_drafts
                    .values()
                    .any(|draft| draft.contains(path))
                || self
                    .pending_prompts
                    .values()
                    .any(|pending| pending.attachments.contains(path))
                || self.draft_actions.iter().any(|action| {
                    matches!(action,
                    DraftAction::Restore { pending, .. } if pending.attachments.contains(path))
                })
        });
    }

    fn resolve_late_prompt_confirmation(&mut self, session: &str) {
        let failed_id = self
            .failed_prompt_drafts
            .get(session)
            .map(|failed| failed.message_id.clone())
            .or_else(|| {
                self.draft_actions.iter().find_map(|action| match action {
                    DraftAction::Restore {
                        session: id,
                        pending,
                    } if id == session => Some(pending.message_id.clone()),
                    _ => None,
                })
            });
        let Some(message_id) = failed_id else { return };
        if !self
            .conversations
            .get(session)
            .is_some_and(|conversation| conversation.has_confirmed_user_message(&message_id))
        {
            return;
        }
        self.draft_actions.retain(|action| {
            !matches!(action,
            DraftAction::Restore { session: id, pending }
                if id == session && pending.message_id == message_id)
        });
        if let Some(failed) = self.failed_prompt_drafts.remove(session) {
            self.draft_actions.push(DraftAction::ClearConfirmed {
                session: session.to_owned(),
                failed,
            });
        } else {
            // The failure's Restore action was cancelled before it painted;
            // the upload worker has finished and no draft owns its temp file.
            self.prune_owned_pastes();
        }
    }

    fn act_on_tray(&mut self, inbox_id: String, request: InboxRequest, cx: &mut Context<Self>) {
        if self.tray_in_flight.contains(&inbox_id) {
            return;
        }
        if let Some(api) = &self.api {
            api.send(Command::Inbox {
                session_id: self.active.clone(),
                inbox_id: inbox_id.clone(),
                request,
            });
            self.tray_in_flight.insert(inbox_id);
            cx.notify();
        }
    }

    fn send_prompt(&mut self, queue: bool, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        if (text.trim().is_empty() && self.attachments_draft.is_empty())
            || self.active.is_empty()
            || self.composer_session != self.active
            || self.pending_prompts.contains_key(&self.active)
            || self.visible_permission().is_some()
        {
            return;
        }
        let Some(api) = self.api.clone() else {
            return;
        };
        if let Err(error) = opencode_gpui::api::check_attachments(&self.attachments_draft) {
            self.connection_status = format!("Cannot send attachment: {error}");
            cx.notify();
            return;
        }
        if let Some(failed) = self.failed_prompt_drafts.get(&self.active)
            && (text != failed.text || self.attachments_draft != failed.attachments)
            && (has_restored_prompt_prefix(&text, &failed.text)
                || (failed.text.is_empty()
                    && !failed.attachments.is_empty()
                    && self.attachments_draft.starts_with(&failed.attachments)))
        {
            self.connection_status = "Previous send may have succeeded; retry it unchanged or remove it from this draft before sending new text".into();
            cx.notify();
            return;
        }
        if let Some(confirmed) = self.confirmed_prompt_residue.get(&self.active) {
            if has_restored_prompt_prefix(&text, &confirmed.text)
                || (!confirmed.attachments.is_empty()
                    && self.attachments_draft.starts_with(&confirmed.attachments))
            {
                self.connection_status = "The earlier prompt was delivered; remove its text or files from this edited draft before sending".into();
                cx.notify();
                return;
            }
            self.confirmed_prompt_residue.remove(&self.active);
        }
        self.next_prompt_request_id += 1;
        let request_id = self.next_prompt_request_id;
        let delivery = self.is_running(&self.active).then_some(if queue {
            protocol::Delivery::Queue
        } else {
            protocol::Delivery::Steer
        });
        let attachments = std::mem::take(&mut self.attachments_draft);
        let message_id = self
            .failed_prompt_drafts
            .remove(&self.active)
            .filter(|failed| failed.text == text && failed.attachments == attachments)
            .map_or_else(protocol::new_message_id, |failed| failed.message_id);
        let pending = PendingPromptSend {
            request_id,
            message_id: message_id.clone(),
            text: text.clone(),
            attachments: attachments.clone(),
            delivery,
            edit_generation: self
                .composer_edit_generation
                .get(&self.active)
                .copied()
                .unwrap_or_default(),
        };
        let paste_lifetime = attachments
            .iter()
            .filter_map(|path| self.owned_pastes.get(path).cloned())
            .collect();
        let session = self.active.clone();
        if self
            .conversations
            .entry(session.clone())
            .or_default()
            .add_local_prompt(
                &pending.message_id,
                &pending.text,
                &pending.attachments,
                delivery,
            )
        {
            self.update_transcript(&session);
        }
        api.send(Command::SendPrompt {
            request_id,
            message_id,
            session_id: self.active.clone(),
            text,
            attachments,
            paste_lifetime,
            delivery,
        });
        // The next run replaces the previous completion notification. Remove
        // it now so a later Busy → Idle shows a fresh desktop banner.
        if self.unread.remove(&session) {
            self.withdraw_session_notification(&session);
            self.persist_tabs();
        }
        self.pending_prompts
            .insert(self.active.clone(), pending.clone());
        if delivery.is_none() {
            self.local_busy.insert(self.active.clone());
            self.local_run_message_ids
                .insert(self.active.clone(), pending.message_id.clone());
        }
        self.draft_actions.push(DraftAction::Clear {
            session: self.active.clone(),
            pending,
        });
        cx.notify();
    }

    fn can_send(&self, cx: &Context<Self>) -> bool {
        let has_input =
            !self.composer.read(cx).value().trim().is_empty() || !self.attachments_draft.is_empty();
        let model = self
            .selected_model()
            .as_ref()
            .and_then(|selection| self.catalog.find(selection));
        !self.active.is_empty()
            && self.composer_session == self.active
            && self.api.is_some()
            && has_input
            && !self.pending_prompts.contains_key(&self.active)
            && !self.deferred_abort.contains(&self.active)
            && !self.draft_actions.iter().any(|action| {
                matches!(action,
                DraftAction::ClearConfirmed { session, .. } if session == &self.active)
            })
            && self.visible_permission().is_none()
            && model.is_some_and(|option| {
                self.attachments_draft.is_empty() || option.supports_attachments
            })
    }

    fn update_transcript(&mut self, session_id: &str) {
        self.update_transcript_with_tail_hint(session_id, None);
    }

    fn update_transcript_with_tail_hint(&mut self, session_id: &str, tail_delta: Option<&str>) {
        let mut latency = latency::global().span(TraceOperation::TranscriptProjection, 0);
        let Some(conversation) = self.conversations.get(session_id) else {
            return;
        };
        let spans = self
            .transcript_spans
            .entry(session_id.to_owned())
            .or_default();
        let rows = self.transcript.entry(session_id.to_owned()).or_default();
        update_session_projection(
            session_id,
            conversation,
            rows,
            spans,
            &mut self.attachments,
            tail_delta,
        );
        latency.units(rows.len());
    }

    fn handle_live_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        let _latency = latency::global().span(TraceOperation::EventApply, 1);
        match event {
            UiEvent::Connection {
                connected: true, ..
            } => {
                self.connection_status = connected_status(
                    self.server_version.as_deref(),
                    false,
                    self.settings.warning.is_some(),
                );
                // Snapshot and SSE subscribe are independent workers. Even on
                // the first connection, take a second snapshot after the
                // stream is live: otherwise a change between the initial
                // snapshot and subscribe is lost from both sources.
                let first_subscription = !self.sse_connected_once;
                self.sse_connected_once = true;
                if first_subscription || self.disconnected {
                    self.refresh_open_tabs = true;
                    self.request_bootstrap();
                    self.disconnected = false;
                }
            }
            UiEvent::Connection {
                connected: false,
                error,
            } => {
                self.disconnected = true;
                self.connection_status = format!(
                    "Disconnected · {} · retrying",
                    safe_connection_error(error.as_deref())
                );
            }
            UiEvent::Bootstrap(Ok(data)) => {
                let retry_needed = data.retry_needed;
                let statuses_complete = data.statuses_complete;
                self.server_version = Some(data.version.clone());
                if data.projects_complete {
                    self.projects = data.projects;
                }
                if !self.disconnected {
                    self.connection_status = connected_status(
                        Some(&data.version),
                        !data.warnings.is_empty(),
                        self.settings.warning.is_some(),
                    );
                }
                if data.sessions_complete {
                    self.sessions = data.sessions;
                    let removed: Vec<String> = self
                        .open_tabs
                        .iter()
                        .filter(|id| !self.sessions.iter().any(|session| &session.id == *id))
                        .cloned()
                        .collect();
                    for id in removed {
                        self.withdraw_session_notification(&id);
                    }
                    self.open_tabs
                        .retain(|id| self.sessions.iter().any(|session| &session.id == id));
                    self.bootstrap_directories = self.active_directories();
                } else {
                    for session in data.sessions {
                        if let Some(existing) = self
                            .sessions
                            .iter_mut()
                            .find(|existing| existing.id == session.id)
                        {
                            *existing = session;
                        } else {
                            self.sessions.push(session);
                        }
                    }
                }
                let observed_busy: Vec<_> = data
                    .statuses
                    .iter()
                    .filter(|(_, status)| status.is_busy())
                    .map(|(id, _)| id.clone())
                    .collect();
                for (id, status) in &data.statuses {
                    if status.is_busy() {
                        if self.local_prompt_delivered(id) {
                            self.local_busy.remove(id);
                        }
                    } else {
                        if data.statuses_complete
                            && !self.pending_prompts.contains_key(id)
                            && self.local_prompt_delivered(id)
                        {
                            self.local_busy.remove(id);
                        }
                        if !self.pending_prompts.contains_key(id) && !self.local_busy.contains(id) {
                            self.deferred_abort.remove(id);
                            self.abort_timeout_tokens.remove(id);
                            self.local_run_message_ids.remove(id);
                        }
                    }
                }
                if data.statuses_complete {
                    self.statuses = data.statuses;
                } else {
                    self.statuses.extend(data.statuses);
                }
                for id in observed_busy {
                    self.maybe_dispatch_deferred_abort(&id);
                }
                if !data.retry_needed {
                    self.tray_in_flight.clear();
                }
                self.reconcile_pending(data.pending, &data.pending_covered);
                for exit in &data.shells.terminal {
                    self.terminal_shells.insert(exit.id.clone(), exit.clone());
                }
                let updated: Vec<String> = self
                    .conversations
                    .iter_mut()
                    .filter_map(|(id, conversation)| {
                        conversation
                            .reconcile_shell_exits(&self.terminal_shells)
                            .then(|| id.clone())
                    })
                    .collect();
                for id in updated {
                    self.update_transcript(&id);
                }
                let directories = self.active_directories();
                let context = jobs::Context {
                    roots: &self.sessions,
                    directories: &directories,
                };
                let busy: HashSet<String> = self
                    .statuses
                    .iter()
                    .filter(|(_, status)| status.is_busy())
                    .map(|(id, _)| id.clone())
                    .collect();
                self.running_jobs
                    .apply_snapshot(Some(&busy), data.shells, &context);
                self.jobs = self.running_jobs.rows(Some(&self.active));
                let wanted = self.running_jobs.take_wanted(&context);
                if !wanted.is_empty()
                    && let Some(api) = &self.api
                {
                    api.send(Command::LoadSessionInfo {
                        session_ids: wanted,
                    });
                }
                let saved_found = self
                    .saved_active
                    .as_ref()
                    .is_some_and(|id| self.sessions.iter().any(|session| &session.id == id));
                // A partial snapshot may not contain the restored tab yet.
                // Do not consume its ID, select an unrelated root, or persist
                // that fallback before the next bootstrap completes.
                let desired = if saved_found {
                    self.saved_active.take()
                } else if self.saved_active.is_some() && !data.sessions_complete {
                    None
                } else {
                    self.saved_active = None;
                    self.sessions
                        .iter()
                        .find(|session| session.id == self.active)
                        .map(|session| session.id.clone())
                        .or_else(|| {
                            self.open_tabs
                                .iter()
                                .find(|id| self.sessions.iter().any(|session| &session.id == *id))
                                .cloned()
                        })
                        .or_else(|| {
                            (!self.restored_server_state && data.sessions_complete)
                                .then(|| {
                                    self.sessions
                                        .iter()
                                        .find(|session| session.parent_id.is_none())
                                        .map(|session| session.id.clone())
                                })
                                .flatten()
                        })
                };
                let refresh_tabs = std::mem::take(&mut self.refresh_open_tabs);
                if refresh_tabs {
                    for id in &self.open_tabs {
                        if let Some(conversation) = self.conversations.get_mut(id) {
                            conversation.loaded = false;
                        }
                    }
                    self.catalogs.clear();
                }
                let first_bootstrap = !self.bootstrapped;
                let first_selection = self.active.is_empty();
                if let Some(id) = desired {
                    self.select_session(id);
                } else {
                    self.active.clear();
                    self.catalog = ModelCatalog::default();
                }
                self.bootstrapped = true;
                if (first_bootstrap || first_selection) && !self.active.is_empty() {
                    // Restored tabs have no focused input yet. Once bootstrap
                    // selects the saved tab, focus its actual composer rather
                    // than leaving keyboard shortcuts on an unmounted root.
                    self.focus_composer_pending = true;
                }
                if refresh_tabs {
                    for id in self.open_tabs.clone() {
                        if id != self.active {
                            self.request_newest(&id);
                        }
                    }
                }
                self.replay_bootstrap_events(cx);
                let unread_before = self.unread.clone();
                // A status can be known before its session metadata arrives.
                // Keep the offline Busy marker until the session list is
                // complete, or the notification would be consumed with no
                // eligible parent session to notify.
                if data.sessions_complete {
                    apply_missed_idle(
                        &mut self.offline_busy,
                        &self.statuses,
                        &self.open_tabs,
                        &mut self.unread,
                        statuses_complete,
                    );
                }
                for id in self
                    .unread
                    .difference(&unread_before)
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    self.notify_session_ready(&id);
                }
                if statuses_complete && data.sessions_complete && self.api.is_some() {
                    self.persist_tabs();
                }
                if retry_needed {
                    self.schedule_bootstrap_retry(cx);
                } else {
                    self.bootstrap_retry_count = 0;
                    self.bootstrap_retry_scheduled = false;
                }
            }
            UiEvent::Bootstrap(Err(_)) => {
                self.connection_status = "Refresh failed · retrying".into();
                self.replay_bootstrap_events(cx);
                self.schedule_bootstrap_retry(cx);
            }
            UiEvent::MessagesLoaded {
                session_id,
                cursor,
                result,
            } => {
                if self.loading_messages.get(&session_id) == Some(&cursor) {
                    self.loading_messages.remove(&session_id);
                    let reload = self.reload_after_load.remove(&session_id);
                    if self.open_tabs.contains(&session_id) {
                        match result {
                            Ok(page) => {
                                let _latency = latency::global()
                                    .span(TraceOperation::HistoryReconcile, page.messages.len());
                                self.message_load_errors.remove(&session_id);
                                let protected = if cursor.is_none() {
                                    self.skip_prune_for_load
                                        .remove(&session_id)
                                        .unwrap_or_default()
                                } else {
                                    HashSet::new()
                                };
                                // Preserve a message identity, not a height or
                                // pixel displacement. GPUI aligns the same row
                                // using the new natural layout after prepend.
                                let anchor = if cursor.is_some() && self.active == session_id {
                                    self.follow_bottom = false;
                                    let first = usize::from(
                                        self.conversations.get(&session_id).is_some_and(
                                            |conversation| conversation.next_cursor.is_some(),
                                        ),
                                    );
                                    self.transcript
                                        .get(&session_id)
                                        .and_then(|rows| {
                                            rows.get(self.scroll.top_item().saturating_sub(first))
                                        })
                                        .map(|row| row.key.clone())
                                } else {
                                    None
                                };
                                let conversation =
                                    self.conversations.entry(session_id.clone()).or_default();
                                if cursor.is_some() {
                                    conversation.prepend_from_api(&page.messages, page.next_cursor);
                                } else {
                                    conversation.replace_from_api(&page.messages, page.next_cursor);
                                    if let Some(queued) = page.queued {
                                        conversation.sync_queued(&queued);
                                        let mut in_flight: HashSet<String> = self
                                            .pending_prompts
                                            .get(&session_id)
                                            .map(|pending| pending.message_id.clone())
                                            .into_iter()
                                            .collect();
                                        in_flight.extend(protected);
                                        conversation.prune_unlisted_local_prompts(&in_flight);
                                    }
                                }
                                if cursor.is_none() {
                                    for event in self
                                        .message_events_during_load
                                        .remove(&session_id)
                                        .unwrap_or_default()
                                    {
                                        let kind = protocol::decode_event(&event);
                                        conversation.apply(&event, &kind);
                                    }
                                } else if !reload {
                                    self.message_events_during_load.remove(&session_id);
                                }
                                conversation.reconcile_shell_exits(&self.terminal_shells);
                                self.update_transcript(&session_id);
                                if let Some(key) = anchor
                                    && let Some(index) = self.transcript[&session_id]
                                        .iter()
                                        .position(|row| row.key == key)
                                {
                                    let first = usize::from(
                                        self.conversations[&session_id].next_cursor.is_some(),
                                    );
                                    self.scroll.scroll_to_top_of_item(index + first);
                                }
                                self.maybe_dispatch_deferred_abort(&session_id);
                                self.resolve_late_prompt_confirmation(&session_id);
                            }
                            Err(MessageLoadError::SessionNotFound) => {
                                self.close_tab(&session_id, cx);
                            }
                            Err(error) => {
                                self.message_load_errors
                                    .insert(session_id.clone(), cursor.clone());
                                if cursor.is_none() {
                                    self.skip_prune_for_load.remove(&session_id);
                                }
                                self.connection_status =
                                    format!("History failed ({session_id}): {error}");
                                if !reload {
                                    self.message_events_during_load.remove(&session_id);
                                }
                            }
                        }
                        if reload {
                            self.request_newest(&session_id);
                        }
                    }
                }
            }
            UiEvent::ModelsLoaded {
                directory,
                result: Ok(catalog),
            } => {
                if catalog.models.is_empty() {
                    self.schedule_empty_catalog_retry(directory.clone(), cx);
                } else {
                    self.model_retry_count.remove(&directory);
                    self.model_retry_scheduled.remove(&directory);
                }
                self.catalogs.insert(directory.clone(), catalog.clone());
                if self
                    .sessions
                    .iter()
                    .any(|session| session.id == self.active && session.directory == directory)
                {
                    self.catalog = catalog;
                }
            }
            UiEvent::ModelsLoaded {
                directory,
                result: Err(error),
            } => {
                self.connection_status = format!("Models failed: {error}");
                self.schedule_empty_catalog_retry(directory, cx);
            }
            UiEvent::SessionCreated { request_id, result } => match result {
                Ok(session) => {
                    let id = session.id.clone();
                    // The SSE creation can arrive before the POST response;
                    // fill an SSE placeholder from the response without
                    // replacing a title already changed by a later event.
                    if let Some(existing) = self.sessions.iter_mut().find(|item| item.id == id) {
                        if existing.title == "Untitled session"
                            && session.title != "Untitled session"
                            && existing.time.updated <= session.time.updated
                        {
                            existing.title = session.title;
                        }
                    } else {
                        model::SessionChange::Created(session).apply(&mut self.sessions);
                    }
                    self.select_session(id);
                    if request_id == self.next_session_request_id {
                        // The new session gets a different TextareaState; any
                        // focus restored when the picker closed belonged to
                        // the previous tab's composer.
                        self.focus_composer_pending = true;
                    }
                }
                Err(error) => {
                    self.connection_status = format!("Create failed: {error}");
                    if request_id == self.next_session_request_id {
                        self.focus_composer_pending = true;
                    }
                }
            },
            UiEvent::SessionRenamed {
                request_id,
                session_id,
                result,
            } => match result {
                Ok(session) => {
                    if let Some(existing) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        *existing = session;
                    }
                    self.persist_tabs();
                    if self.rename_pending == Some(request_id) {
                        self.rename_pending = None;
                        self.rename_target = None;
                        self.rename_error = None;
                        if self.modal == Some(Modal::Rename) {
                            self.modal = None;
                            self.focus_composer_pending = true;
                        }
                    }
                }
                Err(error) => {
                    self.connection_status = format!("Rename failed: {error}");
                    if self.rename_pending == Some(request_id) {
                        self.rename_pending = None;
                        self.rename_error = Some(error);
                    }
                }
            },
            UiEvent::ModelSelected {
                request_id,
                session_id,
                model,
                result,
            } => {
                if self
                    .model_switches
                    .get(&session_id)
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    self.model_switches.remove(&session_id);
                    match result {
                        Ok(()) => {
                            if let Some(session) =
                                self.sessions.iter_mut().find(|s| s.id == session_id)
                            {
                                session.model = Some(SessionModel::from_selection(
                                    &ModelSelection::from_ref(&model),
                                ));
                            }
                        }
                        Err(error) => {
                            self.connection_status = format!("Model change failed: {error}")
                        }
                    }
                }
            }
            UiEvent::PromptAccepted {
                request_id,
                session_id,
                result,
            } => {
                if self
                    .pending_prompts
                    .get(&session_id)
                    .is_some_and(|pending| pending.request_id == request_id)
                    && let Some(pending) = self.pending_prompts.remove(&session_id)
                {
                    match result {
                        Ok(()) => {
                            for path in &pending.attachments {
                                self.cleanup_owned_paste(path);
                            }
                            if self
                                .loading_messages
                                .get(&session_id)
                                .is_some_and(Option::is_none)
                            {
                                self.skip_prune_for_load
                                    .entry(session_id.clone())
                                    .or_default()
                                    .insert(pending.message_id.clone());
                                self.request_newest(&session_id);
                            }
                            if self.deferred_abort.contains(&session_id) {
                                // Inbox acceptance is not evidence that the
                                // run has started. Abort only after a Busy
                                // status (SSE or a refreshed snapshot).
                                self.request_bootstrap();
                            }
                        }
                        Err(error) => {
                            if self
                                .conversations
                                .get(&session_id)
                                .is_some_and(|conversation| {
                                    conversation.has_confirmed_user_message(&pending.message_id)
                                })
                            {
                                self.connection_status = format!(
                                    "Send response failed, but server received prompt: {error}"
                                );
                                for path in &pending.attachments {
                                    self.cleanup_owned_paste(path);
                                }
                                if self.deferred_abort.contains(&session_id) {
                                    self.request_bootstrap();
                                }
                            } else {
                                if self.conversations.get_mut(&session_id).is_some_and(
                                    |conversation| {
                                        conversation
                                            .remove_unconfirmed_local_prompt(&pending.message_id)
                                    },
                                ) {
                                    self.update_transcript(&session_id);
                                }
                                if pending.delivery.is_none()
                                    && self.local_run_message_ids.get(&session_id)
                                        == Some(&pending.message_id)
                                {
                                    if self.deferred_abort.contains(&session_id) {
                                        // A failed HTTP response may have been
                                        // lost after server acceptance. Keep
                                        // the user's Stop request until SSE or
                                        // a bounded refresh proves ownership.
                                        self.request_bootstrap();
                                    } else {
                                        self.local_busy.remove(&session_id);
                                        self.local_run_message_ids.remove(&session_id);
                                        self.abort_timeout_tokens.remove(&session_id);
                                    }
                                }
                                self.connection_status = format!("Send failed: {error}");
                                self.draft_actions.push(DraftAction::Restore {
                                    session: session_id,
                                    pending,
                                });
                            }
                        }
                    }
                }
            }
            UiEvent::FormCancelled { form_id, result } => match result {
                Ok(_) => {
                    self.forms.remove(&form_id);
                }
                Err(error) => self.connection_status = format!("Cancel failed: {error}"),
            },
            UiEvent::PermissionReplied { request_id, result } => {
                self.permission_in_flight.remove(&request_id);
                match result {
                    Ok(_) => self
                        .permissions
                        .retain(|item| item.request.id != request_id),
                    Err(error) => {
                        self.connection_status = format!("Permission reply failed: {error}")
                    }
                }
            }
            UiEvent::InboxSettled {
                session_id,
                inbox_id,
                request,
                result,
            } => {
                self.tray_in_flight.remove(&inbox_id);
                match tray::settlement(request, result) {
                    tray::Settlement::Done => {}
                    tray::Settlement::Reconcile => {
                        self.request_newest(&session_id);
                    }
                    tray::Settlement::Failed(error) => self.connection_status = error,
                }
            }
            UiEvent::Aborted {
                result: Err(error), ..
            } => {
                self.connection_status = format!("Stop failed: {error}");
            }
            UiEvent::Backgrounded {
                request_id,
                session_id,
                result,
            } => {
                let accepted = result.is_ok();
                if self
                    .background_requests
                    .finish(&session_id, request_id, result)
                    && accepted
                {
                    // No optimistic tool completion or job removal: 204 can
                    // be a no-op. SSE and canonical history determine state.
                    self.request_newest(&session_id);
                }
            }
            UiEvent::SessionInfoLoaded(info) => {
                for (_, result) in &info {
                    if let Ok(session) = result
                        && let Some(parent) = &session.parent_id
                    {
                        self.child_parents
                            .insert(session.id.clone(), parent.clone());
                    }
                }
                self.running_jobs.apply_session_info(info);
                self.jobs = self.running_jobs.rows(Some(&self.active));
                let directories = self.active_directories();
                let context = jobs::Context {
                    roots: &self.sessions,
                    directories: &directories,
                };
                let wanted = self.running_jobs.take_wanted(&context);
                if !wanted.is_empty()
                    && let Some(api) = &self.api
                {
                    api.send(Command::LoadSessionInfo {
                        session_ids: wanted,
                    });
                }
            }
            UiEvent::ServerEvent(envelope) => {
                if self.bootstrap_in_flight {
                    self.bootstrap_events.push(envelope);
                    return;
                }
                if let Ok(event) = protocol::Event::deserialize(&envelope.payload) {
                    let kind = protocol::decode_event(&event);
                    if let Some((child, parent)) = pending::subagent_child(&kind) {
                        self.child_parents.insert(child, parent);
                    }
                    if let Some((id, status)) = model::run_status_change(&kind) {
                        let completed = self.statuses.get(&id).is_some_and(RunStatus::is_busy)
                            && status.is_idle()
                            && self.open_tabs.contains(&id);
                        self.update_tab_status(id.clone(), status);
                        if completed {
                            self.notify_session_ready(&id);
                        }
                    }
                    if let Some(change) = model::SessionChange::from_kind(&event, &kind) {
                        let id = change.session_id().to_owned();
                        if matches!(change, model::SessionChange::ModelSelected { .. }) {
                            self.model_switches.remove(&id);
                        }
                        change.apply(&mut self.sessions);
                        if self.active.is_empty()
                            && self.sessions.iter().any(|session| session.id == id)
                        {
                            self.select_session(id.clone());
                        } else if id == self.active
                            && !self.sessions.iter().any(|session| session.id == id)
                        {
                            if let Some(next) =
                                self.sessions.first().map(|session| session.id.clone())
                            {
                                self.select_session(next);
                            } else {
                                self.active.clear();
                                self.catalog = ModelCatalog::default();
                                self.attachments_draft.clear();
                            }
                        }
                        if !self.sessions.iter().any(|session| session.id == id) {
                            self.model_switches.remove(&id);
                            self.open_tabs.retain(|tab| tab != &id);
                            self.conversations.remove(&id);
                            self.background_requests.remove(&id);
                            self.transcript.remove(&id);
                            self.transcript_spans.remove(&id);
                            self.attachments.remove_session(&id);
                            self.transcript_scrolls.remove(&id);
                            self.composers.remove(&id);
                            self.skip_prune_for_load.remove(&id);
                            self.attachment_drafts.remove(&id);
                            self.pending_prompts.remove(&id);
                            self.failed_prompt_drafts.remove(&id);
                            self.confirmed_prompt_residue.remove(&id);
                            self.local_busy.remove(&id);
                            self.local_run_message_ids.remove(&id);
                            self.deferred_abort.remove(&id);
                            self.abort_timeout_tokens.remove(&id);
                            self.draft_actions.retain(|action| match action {
                                DraftAction::Clear { session, .. }
                                | DraftAction::Restore { session, .. }
                                | DraftAction::ClearConfirmed { session, .. } => session != &id,
                            });
                            self.composer_edit_generation.remove(&id);
                            self.unread.remove(&id);
                            self.withdraw_session_notification(&id);
                            self.prune_owned_pastes();
                        }
                    }
                    if let Some(id) = kind.session_id()
                        && self.loading_messages.contains_key(id)
                    {
                        self.message_events_during_load
                            .entry(id.to_owned())
                            .or_default()
                            .push(event.clone());
                    }
                    if let Some(id) = kind.session_id() {
                        let conversation = self.conversations.entry(id.to_owned()).or_default();
                        let changed = conversation.apply(&event, &kind);
                        let reconciled = matches!(
                            kind,
                            protocol::EventKind::ToolSuccess(_)
                                | protocol::EventKind::MessageContentUpdated(_)
                        ) && conversation
                            .reconcile_shell_exits(&self.terminal_shells);
                        if changed || reconciled {
                            let tail_delta = match &kind {
                                protocol::EventKind::TextDelta(data)
                                | protocol::EventKind::ReasoningDelta(data) => {
                                    Some(data.assistant_message_id.as_str())
                                }
                                _ => None,
                            };
                            self.update_transcript_with_tail_hint(id, tail_delta);
                            self.maybe_dispatch_deferred_abort(id);
                        }
                    }
                    if let protocol::EventKind::ShellExited(exit) = &kind {
                        self.terminal_shells.insert(exit.id.clone(), exit.clone());
                        let updated: Vec<String> = self
                            .conversations
                            .iter_mut()
                            .filter_map(|(id, conversation)| {
                                conversation.shell_exited(exit).then(|| id.clone())
                            })
                            .collect();
                        for id in updated {
                            self.update_transcript(&id);
                        }
                    }
                    if matches!(
                        kind,
                        protocol::EventKind::InboxEnqueued(_)
                            | protocol::EventKind::InboxDelivered(_)
                    ) && let Some(id) = kind.session_id()
                    {
                        self.resolve_late_prompt_confirmation(id);
                    }
                    if let protocol::EventKind::InboxDelivered(data) = &kind
                        && self.open_tabs.contains(&data.session_id)
                        && self
                            .conversations
                            .get(&data.session_id)
                            .is_none_or(|conversation| {
                                !conversation.has_user_message(&data.inbox_id)
                                    || conversation.needs_canonical_user_message(&data.inbox_id)
                            })
                    {
                        // Delivery may outlive a missed enqueue event. History
                        // reconciles the user row and any ambiguous retry.
                        self.request_newest(&data.session_id);
                    }
                    if let Some(job_event) =
                        jobs::job_event(&event, &kind, envelope.directory.as_deref())
                    {
                        let directories = self.active_directories();
                        let context = jobs::Context {
                            roots: &self.sessions,
                            directories: &directories,
                        };
                        self.running_jobs.apply_event(job_event, &context);
                        self.jobs = self.running_jobs.rows(Some(&self.active));
                    }
                    if let Some(invalidation) = model::CatalogInvalidation::from_kind(&event, &kind)
                    {
                        let directories = self.invalidate_catalogs(invalidation.directory);
                        if let Some(api) = &self.api {
                            for directory in directories {
                                api.send(Command::LoadModels { directory });
                            }
                        }
                    }
                    if let Some(change) =
                        opencode_gpui::pending::pending_change(&kind, envelope.directory.as_deref())
                    {
                        match change {
                            pending::PendingChange::Permission { directory, request } => {
                                self.upsert_permission(directory, request);
                            }
                            opencode_gpui::pending::PendingChange::Form(form) => {
                                self.forms.upsert(form)
                            }
                            opencode_gpui::pending::PendingChange::Resolved(id) => {
                                self.forms.remove(&id);
                                self.permissions.retain(|item| item.request.id != id);
                                self.permission_in_flight.remove(&id);
                            }
                        }
                    }
                }
            }
            UiEvent::PendingLoaded(snapshot) => {
                self.reconcile_pending(snapshot.requests, &snapshot.covered);
            }
            _ => {}
        }
        cx.notify();
    }

    fn from_preview(window: &mut Window, cx: &mut Context<Self>, overlay: Option<String>) -> Self {
        let settings_sessions = overlay.as_deref() == Some("settings-sessions");
        let modal = match overlay.as_deref() {
            Some("settings") | Some("settings-sessions") => Some(Modal::Settings),
            Some("sessions") => Some(Modal::Sessions),
            Some("new-session") | Some("many-projects") => Some(Modal::NewSession),
            Some("rename") => Some(Modal::Rename),
            Some("model") => Some(Modal::Model),
            Some("level") => Some(Modal::Level),
            _ => None,
        };
        let mut fixture = preview::State::new();
        let bootstrap = match fixture.handle(Command::Bootstrap {
            sessions: vec![],
            directories: vec![],
        }) {
            UiEvent::Bootstrap(Ok(data)) => data,
            _ => unreachable!("the preview fixture always bootstraps"),
        };
        let server = preview::server_state();
        let active = server.active.expect("preview tab");
        let projects = bootstrap.projects.clone();
        let sessions = bootstrap.sessions;
        let mut forms = Forms::default();
        let mut permissions = Vec::new();
        for pending in bootstrap.pending {
            match pending {
                PendingRequest::Form(form) => forms.upsert(form),
                PendingRequest::Permission { directory, request } => {
                    permissions.push(PendingPermission {
                        request,
                        directory: Some(directory),
                    });
                }
            }
        }
        let dirs = vec!["/repo".to_owned()];
        let context = jobs::Context {
            roots: &sessions,
            directories: &dirs,
        };
        let busy: HashSet<String> = bootstrap
            .statuses
            .iter()
            .filter(|(_, status)| status.is_busy())
            .map(|(id, _)| id.clone())
            .collect();
        let mut running_jobs = Jobs::default();
        running_jobs.apply_snapshot(Some(&busy), bootstrap.shells, &context);
        let wanted = running_jobs.take_wanted(&context);
        if let UiEvent::SessionInfoLoaded(info) = fixture.handle(Command::LoadSessionInfo {
            session_ids: wanted,
        }) {
            running_jobs.apply_session_info(info);
        }
        let jobs = running_jobs.rows(Some(&active));
        let mut transcript: HashMap<String, Vec<TranscriptRow>> = HashMap::new();
        let mut transcript_spans = HashMap::new();
        let mut attachments = ImageCache::default();
        let mut conversations = HashMap::new();
        for session in &sessions {
            if let UiEvent::MessagesLoaded {
                result: Ok(page), ..
            } = fixture.handle(Command::LoadMessages {
                session_id: session.id.clone(),
                cursor: None,
            }) {
                let mut conversation = Conversation::default();
                conversation.replace_from_api(&page.messages, page.next_cursor);
                if let Some(queued) = page.queued {
                    conversation.sync_queued(&queued);
                }
                let mut rows = Vec::new();
                let mut spans = Vec::new();
                let _ = update_session_projection(
                    &session.id,
                    &conversation,
                    &mut rows,
                    &mut spans,
                    &mut attachments,
                    None,
                );
                transcript.insert(session.id.clone(), rows);
                transcript_spans.insert(session.id.clone(), spans);
                conversations.insert(session.id.clone(), conversation);
            }
        }
        if overlay.as_deref() == Some("long-transcript") {
            let conversation = conversations.get_mut(&active).unwrap();
            conversation.replace_from_api(&preview::long_transcript_messages(), None);
            update_session_projection(
                &active,
                conversation,
                transcript.get_mut(&active).unwrap(),
                transcript_spans.get_mut(&active).unwrap(),
                &mut attachments,
                None,
            );
        }
        let preview_markdown = match overlay.as_deref() {
            Some("complex-markdown") => Some(COMPLEX_MARKDOWN_PREVIEW),
            Some("long-ordered-list") => Some(LONG_ORDERED_LIST_PREVIEW),
            Some("long-bullet") => Some(LONG_BULLET_OVERFLOW_PREVIEW),
            _ => None,
        };
        let preview_long_jobs = overlay.as_deref() == Some("long-jobs");
        let preview_working_pill = overlay.as_deref() == Some("working-pill");
        let preview_many_projects = overlay.as_deref() == Some("many-projects");
        if let Some(preview_markdown) = preview_markdown
            && let Some(row) = transcript.get_mut(&active).and_then(|rows| {
                rows.iter_mut().find(|row| {
                    row.role == model::Role::Assistant && row.kind == TranscriptRowKind::Normal
                })
            })
        {
            row.body = preview_markdown.into();
            row.render_revision += 1;
        }
        let catalog = match fixture.handle(Command::LoadModels {
            directory: "/repo".into(),
        }) {
            UiEvent::ModelsLoaded {
                result: Ok(catalog),
                ..
            } => catalog,
            _ => ModelCatalog::default(),
        };
        let scroll = ScrollHandle::new();
        let mut client = Self {
            dark: Theme::global(cx).is_dark(),
            api: None,
            preview_api: false,
            connection_generation: 0,
            bootstrap_in_flight: false,
            bootstrap_after_load: false,
            bootstrap_retry_scheduled: false,
            bootstrap_retry_count: 0,
            bootstrap_events: Vec::new(),
            bootstrap_directories: Vec::new(),
            refresh_open_tabs: false,
            settings: SettingsFields::new(
                window,
                cx,
                PersistedState::default(),
                ApiConfig {
                    base_url: "http://127.0.0.1:4096".into(),
                    username: "opencode".into(),
                    password: None,
                    cloudflare_access: None,
                },
            ),
            saved_active: None,
            restored_server_state: false,
            connection_status: "Connected · preview".into(),
            disconnected: false,
            sse_connected_once: false,
            server_version: None,
            conversations,
            loading_messages: HashMap::new(),
            message_load_errors: HashMap::new(),
            skip_prune_for_load: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            follow_bottom: true,
            transcript_pointer_down: false,
            catalogs: HashMap::new(),
            model_retry_count: HashMap::new(),
            model_retry_scheduled: HashSet::new(),
            running_jobs,
            jobs_elapsed_refresh_active: false,
            sessions,
            open_tabs: server.tabs.iter().map(|tab| tab.id.clone()).collect(),
            sidebar_visible: true,
            sidebar_auto_hidden: false,
            sidebar_zoom_initialized: false,
            sidebar_zoom_width: px(0.),
            sidebar_toggle_focus: cx.focus_handle().tab_stop(true),
            tab_focus: HashMap::new(),
            tab_shortcut_hint: false,
            tab_drop_target: None,
            tab_drop_outgoing: HashMap::new(),
            next_tab_drop_token: 0,
            tab_drop_anchor: None,
            bootstrapped: true,
            projects,
            active: active.clone(),
            transcript,
            transcript_spans,
            transcript_scrolls: HashMap::from([(active, scroll.clone())]),
            attachments,
            catalog,
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(1)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            }),
            composer_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            composer_placeholder_focused: false,
            composer_session: String::new(),
            composers: HashMap::new(),
            attachments_draft: Vec::new(),
            attachment_drafts: HashMap::new(),
            owned_pastes: HashMap::new(),
            overlay: if modal.is_none()
                && preview_markdown.is_none()
                && !preview_long_jobs
                && !preview_working_pill
            {
                overlay
            } else {
                None
            },
            copy_feedback: None,
            next_copy_feedback_token: 0,
            link_menu: None,
            link_menu_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            autoscroll: None,
            next_autoscroll_token: 0,
            scroll,
            transcript_content_width: px(420.),
            history_focus: cx.focus_handle().tab_stop(true),
            retry_history_focus: cx.focus_handle().tab_stop(true),
            sessions_picker_scroll: ScrollHandle::new(),
            picker_list_scroll: ScrollHandle::new(),
            projects_picker_scroll: ScrollHandle::new(),
            picker_choice_focus: HashMap::new(),
            unread: server.unread,
            offline_busy: HashSet::new(),
            statuses: bootstrap.statuses,
            jobs,
            background_requests: background::Requests::default(),
            latency: LatencyCapture::default(),
            background_focus: cx.focus_handle().tab_stop(true),
            terminal_shells: HashMap::new(),
            desktop_notifications: None,
            notification_targets: HashMap::new(),
            forms,
            form_cancel_focus: cx.focus_handle().tab_stop(true),
            form_cancel_presented: None,
            permissions,
            permission_in_flight: HashSet::new(),
            permission_container_focus: cx.focus_handle().tab_stop(true),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: None,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            settings_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            settings_session_focus: HashMap::new(),
            settings_sessions_scroll: ScrollHandle::new(),
            settings_highlight: None,
            child_parents: HashMap::new(),
            next_prompt_request_id: 0,
            next_session_request_id: 0,
            focus_composer_pending: false,
            next_model_request_id: 0,
            model_switches: HashMap::new(),
            pending_prompts: HashMap::new(),
            failed_prompt_drafts: HashMap::new(),
            confirmed_prompt_residue: HashMap::new(),
            tray_in_flight: HashSet::new(),
            draft_actions: Vec::new(),
            composer_edit_generation: HashMap::new(),
            local_busy: HashSet::new(),
            local_run_message_ids: HashMap::new(),
            deferred_abort: HashSet::new(),
            abort_timeout_tokens: HashMap::new(),
            next_abort_timeout_token: 0,
            modal,
            picker_control_centers: Rc::new(RefCell::new([None, None])),
            modal_container_focus: cx.focus_handle(),
            rename_target: None,
            rename_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            rename_pending: None,
            rename_error: None,
            picker_highlight: None,
            search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(match modal {
                    Some(Modal::Model) => "Search models (fuzzy)...",
                    Some(Modal::Level) => "Search levels (fuzzy)...",
                    Some(Modal::Sessions) => "Search tabs...",
                    Some(Modal::NewSession) => "Search projects…",
                    _ => "Search…",
                })
            }),
            rename: cx
                .new(|cx| InputState::new(window, cx).default_value("Fix the attach clip padding")),
        };
        if preview_long_jobs {
            client.jobs.push(JobRow {
                id: "long-job-preview".into(),
                kind: JobKind::Shell,
                title: "printf 'CARGO_HOME=%s\\nHOME=%s'".repeat(3),
                owner: Some(
                    "Audit rounded window surfaces and transcript pointer interactions".into(),
                ),
                started: wall_clock_millis() - 130_000,
            });
        }
        if preview_working_pill {
            let active = client.active.clone();
            client.forms.clear();
            client.statuses.insert(active.clone(), RunStatus::Busy);
            if let Some(conversation) = client.conversations.get_mut(&active) {
                conversation.add_local_prompt("msg_working_preview", "continue", &[], None);
            }
            client.update_transcript(&active);
        }
        if preview_many_projects {
            for index in 0..7 {
                client.projects.push(model::Project {
                    worktree: format!("/preview/project-{index}"),
                    name: Some(format!("Project {index}")),
                });
            }
        }
        cx.subscribe(
            &client.search,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.picker_highlight = None;
                    if this.modal == Some(Modal::Sessions) {
                        this.sessions_picker_scroll
                            .set_offset(point(px(0.), px(0.)));
                    } else if this.modal == Some(Modal::NewSession) {
                        this.projects_picker_scroll
                            .set_offset(point(px(0.), px(0.)));
                    } else if matches!(this.modal, Some(Modal::Model | Modal::Level)) {
                        this.picker_list_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.accept_modal_choice(cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(&client.rename, |this, _, event: &InputEvent, cx| {
            match event {
                InputEvent::Change => this.rename_error = None,
                InputEvent::PressEnter { .. } => this.rename_session(cx),
                _ => {}
            }
            cx.notify();
        })
        .detach();
        if settings_sessions {
            client.settings.tab = SettingsTab::Sessions;
        }
        cx.subscribe(
            &client.settings.session_search,
            |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.settings_highlight = None;
                    this.settings_sessions_scroll
                        .set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            },
        )
        .detach();
        if matches!(
            modal,
            Some(Modal::Sessions | Modal::NewSession | Modal::Model | Modal::Level)
        ) {
            let focus = client.search.focus_handle(cx);
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        } else if modal == Some(Modal::Rename) {
            let focus = client.rename.focus_handle(cx);
            let rename = client.rename.clone();
            window.on_next_frame(move |window, cx| {
                focus.focus(window, cx);
                rename.update(cx, |input, cx| input.select_all(window, cx));
            });
        } else if modal == Some(Modal::Settings) {
            if client.settings.tab == SettingsTab::Connection {
                let focus = client.settings.server.focus_handle(cx);
                let server = client.settings.server.clone();
                window.on_next_frame(move |window, cx| {
                    focus.focus(window, cx);
                    server.update(cx, |input, cx| input.select_all(window, cx));
                });
            } else {
                let focus = client.settings.session_search.focus_handle(cx);
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
        }
        let initial_session = client.active.clone();
        cx.subscribe(&client.composer, move |this, _, event: &InputEvent, _| {
            if matches!(event, InputEvent::Change) {
                latency::global().edited();
                *this
                    .composer_edit_generation
                    .entry(initial_session.clone())
                    .or_default() += 1;
            }
        })
        .detach();
        client
    }

    fn from_api_preview(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut client = Self::from_preview(window, cx, None);
        let (api, receiver, _) = ApiHandle::preview();
        client.preview_api = true;
        client.connection_status = "Connecting · preview API".into();
        client.saved_active = Some(client.active.clone());
        client.sessions.clear();
        client.transcript.clear();
        client.transcript_spans.clear();
        client.attachments = ImageCache::default();
        client.conversations.clear();
        client.loading_messages.clear();
        client.message_events_during_load.clear();
        client.forms.clear();
        client.permissions.clear();
        client.api = Some(api);
        client.request_bootstrap();
        cx.subscribe(&client.composer, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { secondary, shift } = event
                && !shift
            {
                this.send_prompt(*secondary, cx);
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_live_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        client
    }

    fn tab(
        &self,
        session: &Session,
        index: usize,
        divided: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let focus = self.tab_focus.get(&session.id).expect("rendered tab focus");
        let selected = self.active == session.id;
        let busy = self
            .statuses
            .get(&session.id)
            .is_some_and(RunStatus::is_busy);
        let unread = self.unread.contains(&session.id);
        let has_jobs = self.running_jobs.sessions_with_jobs().contains(&session.id);
        let (gear, attention) = tab_indicator(busy, unread, has_jobs);
        let dot_color = match attention {
            TabAttention::Busy => {
                if self.dark {
                    0xe5b567
                } else {
                    0xa46910
                }
            }
            TabAttention::Unread => {
                if self.dark {
                    0x62bceb
                } else {
                    0x176899
                }
            }
            TabAttention::Read => {
                if self.dark {
                    0x68736f
                } else {
                    0x7c8682
                }
            }
        };
        let title = session.title.clone();
        let id = session.id.clone();
        let middle_close_id = id.clone();
        let rename_id = id.clone();
        let rename_key_id = id.clone();
        let close_id = id.clone();
        let close_key_id = id.clone();
        let activate_id = id.clone();
        let drop_id = id.clone();
        let motion_id = id.clone();
        let drag_title = title.clone();
        let dark = self.dark;
        let indicator: AnyElement = if self.tab_shortcut_hint && index < 9 {
            div()
                .w(px(14.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(dot_color))
                .child((index + 1).to_string())
                .into_any_element()
        } else if gear {
            status_gear(rgb(dot_color), format!("tab-gear-{}", session.id))
        } else {
            div()
                .w(self.scaled(9.))
                .h(self.scaled(9.))
                .rounded_full()
                .bg(rgb(dot_color))
                .into_any_element()
        };
        div()
            .id(format!("tab-{id}"))
            .role(Role::Button)
            .aria_label(format!("Open session: {}", session.title))
            .test_support()
            .track_focus(&focus[0])
            .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.select_session(activate_id.clone());
                    this.focus_selected_composer(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .relative()
            .h(self.scaled(34.))
            .w_full()
            .px(self.scaled(8.))
            .flex()
            .items_center()
            .gap(self.scaled(7.))
            .rounded(px(6.))
            .bg(if selected {
                self.tone(0xe3e0da, 0x22262a)
            } else {
                self.tone(0xf4f1eb, 0x0d0f11)
            })
            .cursor_pointer()
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<TabDrag>, _, cx| {
                    let bounds = event.bounds;
                    if !bounds.contains(&event.event.position) {
                        return;
                    }
                    let destination = if !this.tab_shortcut_hint && event.drag(cx).0 != motion_id {
                        let origin = this
                            .tab_drop_anchor
                            .as_ref()
                            .filter(|(target, _)| target == &motion_id)
                            .map(|(_, origin)| *origin)
                            .unwrap_or(bounds.origin.y);
                        Some((
                            motion_id.clone(),
                            event.event.position.y >= origin + bounds.size.height / 2.,
                        ))
                    } else {
                        None
                    };
                    if this.tab_drop_target != destination {
                        if destination.as_ref().map(|(target, _)| target)
                            != this.tab_drop_anchor.as_ref().map(|(target, _)| target)
                        {
                            this.tab_drop_anchor = destination
                                .as_ref()
                                .map(|(target, _)| (target.clone(), bounds.origin.y));
                        }
                        this.set_tab_drop_target(destination, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.drop_tab(&drag.0, &drop_id, cx);
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(id.clone());
                this.focus_selected_composer(window, cx);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| {
                    this.close_tab(&middle_close_id, cx);
                    this.focus_selected_composer(window, cx);
                    cx.stop_propagation();
                }),
            )
            .when(divided, |tab| {
                tab.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(1.))
                        .bg(self.tone(0xc8c3ba, 0x2b3034)),
                )
            })
            .child(
                div()
                    .id(format!("drag-tab-{}", session.id))
                    .test_support()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(7.))
                    .when(!self.tab_shortcut_hint, |handle| {
                        handle.on_drag(TabDrag(session.id.clone()), move |_, _, _, cx| {
                            cx.new(|_| TabDragPreview {
                                title: drag_title.clone(),
                                dark,
                            })
                        })
                    })
                    .child(
                        div()
                            .id(format!("tab-status-{}", session.id))
                            .test_support()
                            .w(px(14.))
                            .h(px(14.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(indicator),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if attention != TabAttention::Read {
                                rgb(dot_color)
                            } else {
                                self.tone(0x555b5c, 0xe8e5df)
                            })
                            .font_weight(if attention != TabAttention::Read {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(title),
                    ),
            )
            .child(
                div()
                    .id(format!("rename-{}", session.id))
                    .role(Role::Button)
                    .aria_label(format!("Rename session: {}", session.title))
                    .test_support()
                    .track_focus(&focus[1])
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.rename_target = Some(rename_key_id.clone());
                            this.show_modal(Modal::Rename, window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .text_color(if selected {
                        self.tone(0x555b5c, 0xc8c4bd)
                    } else {
                        self.tone(0xcfd0cc, 0x45494c)
                    })
                    .w(self.scaled(24.))
                    .h(self.scaled(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .hover(|style| style.bg(self.tone(0xe3e0da, 0x303a44)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.rename_target = Some(rename_id.clone());
                        this.show_modal(Modal::Rename, window, cx);
                    }))
                    .child(
                        Icon::default()
                            .data(include_bytes!("icons/edit.svg"))
                            .with_size(self.scaled(16.))
                            .text_color(if selected {
                                self.tone(0x555b5c, 0xc8c4bd)
                            } else {
                                self.tone(0xcfd0cc, 0x45494c)
                            }),
                    ),
            )
            .child(
                div()
                    .id(format!("close-{}", session.id))
                    .role(Role::Button)
                    .aria_label(format!("Close tab: {}", session.title))
                    .test_support()
                    .track_focus(&focus[2])
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.close_tab(&close_key_id, cx);
                            this.focus_selected_composer(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .text_color(if selected {
                        self.tone(0x555b5c, 0xc8c4bd)
                    } else {
                        self.tone(0xcfd0cc, 0x45494c)
                    })
                    .w(self.scaled(24.))
                    .h(self.scaled(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .hover(|style| {
                        style
                            .bg(self.tone(0xb84b45, 0x8e3737))
                            .text_color(rgb(0xffffff))
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab(&close_id, cx);
                        this.focus_selected_composer(window, cx);
                    }))
                    .child(
                        Icon::default()
                            .data(include_bytes!("icons/close.svg"))
                            .with_size(self.scaled(16.)),
                    ),
            )
            .into_any_element()
    }

    fn tab_drop_slot(&self, target: &str, after: bool, cx: &Context<Self>) -> AnyElement {
        let target = target.to_owned();
        let motion_target = target.clone();
        let position = if after { "after" } else { "before" };
        let animation_id = format!("tab-drop-slot-expand-{target}-{position}");
        let midpoint = self.scaled(17.);
        let slot_height = self.scaled(36.);
        div()
            .id(format!("tab-drop-slot-{target}-{position}"))
            .test_support()
            .w_full()
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<TabDrag>, _, cx| {
                    if this.tab_shortcut_hint || event.drag(cx).0 == motion_target {
                        return;
                    }
                    let Some((_, origin)) = this
                        .tab_drop_anchor
                        .as_ref()
                        .filter(|(id, _)| id == &motion_target)
                    else {
                        return;
                    };
                    let destination = Some((
                        motion_target.clone(),
                        event.event.position.y >= *origin + midpoint,
                    ));
                    if this.tab_drop_target != destination {
                        this.set_tab_drop_target(destination, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.drop_tab(&drag.0, &target, cx);
            }))
            .with_animation(
                animation_id,
                Animation::new(Duration::from_millis(150)),
                move |slot, progress| slot.h(px(slot_height.as_f32() * progress)),
            )
            .into_any_element()
    }

    fn tab_drop_closing_slot(
        &self,
        target: &str,
        after: bool,
        token: u64,
        cx: &Context<Self>,
    ) -> AnyElement {
        let target = target.to_owned();
        let drag_target = target.clone();
        let drop_target = target.clone();
        let position = if after { "after" } else { "before" };
        let slot_height = self.scaled(36.);
        div()
            .id(format!("tab-drop-slot-{target}-{position}"))
            .test_support()
            .w_full()
            // An outgoing slot still occupies layout while shrinking. Let a
            // rapid subsequent drag select it rather than swallowing the drop.
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<TabDrag>, _, cx| {
                    if event.drag(cx).0 != drag_target {
                        let side = this
                            .tab_drop_anchor
                            .as_ref()
                            .filter(|(id, _)| id == &drag_target)
                            .map_or(after, |(_, origin)| {
                                event.event.position.y >= *origin + this.scaled(17.)
                            });
                        this.set_tab_drop_target(Some((drag_target.clone(), side)), cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                let side = this
                    .tab_drop_anchor
                    .as_ref()
                    .filter(|(id, _)| id == &drop_target)
                    .map_or(after, |(_, origin)| {
                        window.mouse_position().y >= *origin + this.scaled(17.)
                    });
                this.set_tab_drop_target(Some((drop_target.clone(), side)), cx);
                this.drop_tab(&drag.0, &drop_target, cx);
            }))
            .with_animation(
                format!("tab-drop-slot-collapse-{target}-{position}-{token}"),
                Animation::new(Duration::from_millis(150)),
                move |slot, progress| slot.h(collapsing_slot_height(slot_height, progress)),
            )
            .into_any_element()
    }

    fn session_header(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.sidebar_visible {
            return None;
        }
        let title = self
            .sessions
            .iter()
            .find(|session| session.id == self.active)
            .map(|session| session.title.trim())
            .filter(|title| !title.is_empty())
            .unwrap_or("OpenCode");
        let has_jobs = self
            .running_jobs
            .sessions_with_jobs()
            .contains(&self.active);
        let (gear, attention) = tab_indicator(
            self.is_running(&self.active),
            self.unread.contains(&self.active),
            has_jobs,
        );
        let dot_color = match attention {
            TabAttention::Busy => self.tone(0xa46910, 0xe5b567),
            TabAttention::Unread => self.tone(0x176899, 0x62bceb),
            TabAttention::Read => self.tone(0x7c8682, 0x68736f),
        };
        let status: AnyElement = if gear {
            status_gear(dot_color, format!("header-gear-{}", self.active))
        } else {
            div()
                .w(self.scaled(9.))
                .h(self.scaled(9.))
                .rounded_full()
                .bg(dot_color)
                .into_any_element()
        };
        Some(
            div()
                .id("session-header")
                .test_support()
                .h(self.scaled(50.))
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(self.scaled(8.))
                .px(self.scaled(28.))
                .border_b_1()
                .border_color(self.tone(0xc8c3ba, 0x2a2e32))
                .bg(self.tone(0xf4f1eb, 0x14171a))
                .child(div().w(self.scaled(14.)).flex_shrink_0().child(status))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(self.scaled(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(
                    BaseButton::new("session-header-switch")
                        .accessibility_label("Switch session")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_modal(Modal::Sessions, window, cx);
                        }))
                        .text_size(self.scaled(11.))
                        .text_color(self.tone(0x87908d, 0x899097))
                        .child("Ctrl+P to switch"),
                )
                .into_any_element(),
        )
    }

    fn can_background(&self, session: &str) -> bool {
        self.api.is_some()
            && !self.disconnected
            && !self.loading_messages.contains_key(session)
            && self.statuses.get(session).is_some_and(RunStatus::is_busy)
            && self
                .conversations
                .get(session)
                .is_some_and(Conversation::has_backgroundable_tools)
    }

    fn send_to_background(&mut self, session: &str, cx: &mut Context<Self>) {
        if session != self.active || self.modal.is_some() || !self.can_background(session) {
            return;
        }
        let Some(request_id) = self.background_requests.begin(session) else {
            return;
        };
        if let Some(api) = &self.api {
            api.send(Command::Background {
                request_id,
                session_id: session.to_owned(),
            });
        }
        cx.notify();
    }

    fn background_control(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let pending = self.background_requests.pending(&self.active);
        if !pending && !self.can_background(&self.active) {
            return None;
        }
        let session = self.active.clone();
        let label = if pending {
            "Sending tasks to background…"
        } else {
            "Continue without waiting — send foreground tasks to background"
        };
        let glyph = if pending {
            Icon::default()
                .data(include_bytes!("icons/spinner.svg"))
                .with_size(self.scaled(20.))
                .with_animation(
                    format!("background-pending-{}", self.active),
                    Animation::new(Duration::from_secs(1)).repeat(),
                    |icon, phase| icon.transform(Transformation::rotate(percentage(phase))),
                )
                .into_any_element()
        } else {
            Icon::default()
                .data(include_bytes!("icons/send-to-background.svg"))
                .with_size(self.scaled(20.))
                .into_any_element()
        };
        Some(
            BaseButton::new("send-to-background")
                .accessibility_label(label)
                .track_focus(&self.background_focus)
                .disabled(pending)
                // Keep the header's vertical hit target stable while the artwork zooms.
                .w(self.scaled(32.))
                .h(px(32.))
                .flex_shrink_0()
                .rounded(px(4.))
                .border_1()
                .border_color(rgba(0x00000000))
                .text_color(self.tone(0x485250, 0xe2e6e4))
                .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                .when(!pending, |button| button
                    .cursor_pointer()
                    .hover(|style| style.bg(self.tone(0xe3e0da, 0x22262a))))
                .tooltip(move |window, cx| {
                    Tooltip::new(if pending {
                        "Sending tasks to background…"
                    } else {
                        "Continue without waiting (Ctrl+Shift+B)\nForeground tasks keep running in the background."
                    }).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send_to_background(&session, cx);
                }))
                .child(glyph)
                .into_any_element(),
        )
    }

    fn sidebar(&self, floating: bool, cx: &Context<Self>) -> AnyElement {
        let mut tabs = div()
            .flex()
            .flex_col()
            .p(self.scaled(8.))
            .gap(self.scaled(5.))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, cx| {
                if !event.bounds.contains(&event.event.position) && this.tab_drop_target.is_some() {
                    this.tab_drop_anchor = None;
                    this.set_tab_drop_target(None, cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &TabDrag, _, cx| {
                if let Some((target, _)) = this.tab_drop_target.clone() {
                    this.drop_tab(&drag.0, &target, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.tab_drop_anchor = None;
                    this.set_tab_drop_target(None, cx);
                }),
            );
        let mut previous_inactive = false;
        for (index, id) in self.open_tabs.iter().enumerate() {
            if let Some(session) = self.sessions.iter().find(|session| &session.id == id) {
                let active = session.id == self.active;
                if self.tab_drop_target.as_ref() == Some(&(id.clone(), false)) {
                    tabs = tabs.child(self.tab_drop_slot(id, false, cx));
                } else if let Some(&token) = self.tab_drop_outgoing.get(&(id.clone(), false)) {
                    tabs = tabs.child(self.tab_drop_closing_slot(id, false, token, cx));
                }
                tabs = tabs.child(self.tab(session, index, !active && previous_inactive, cx));
                if self.tab_drop_target.as_ref() == Some(&(id.clone(), true)) {
                    tabs = tabs.child(self.tab_drop_slot(id, true, cx));
                } else if let Some(&token) = self.tab_drop_outgoing.get(&(id.clone(), true)) {
                    tabs = tabs.child(self.tab_drop_closing_slot(id, true, token, cx));
                }
                previous_inactive = !active;
            }
        }
        let mut jobs_panel = div()
            .id("background-jobs")
            .test_support()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(14.))
            .gap(px(8.))
            .border_t_1()
            .border_color(self.tone(0xc8c3ba, 0x24282c));
        let background_control = self.background_control(cx);
        let show_jobs = !self.jobs.is_empty()
            || background_control.is_some()
            || self.background_requests.error(&self.active).is_some();
        if show_jobs {
            let now = wall_clock_millis();
            jobs_panel = jobs_panel.child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.tone(0x737a78, 0x8d959d))
                    .child("BACKGROUND")
                    .child(
                        div()
                            .px(px(6.))
                            .rounded_full()
                            .bg(self.tone(0xe4ddd0, 0x1c242b))
                            .text_color(self.tone(0x5c4a2e, 0xd7c4a3))
                            .child(self.jobs.len().to_string()),
                    )
                    // The hit target extends into the existing padding instead
                    // of growing the header and moving all task rows.
                    .when_some(background_control, |header, control| {
                        header.child(div().absolute().right(px(-6.)).top(px(-8.)).child(control))
                    }),
            );
            if let Some(error) = self.background_requests.error(&self.active) {
                jobs_panel = jobs_panel.child(
                    div()
                        .id("background-error")
                        .role(Role::Label)
                        .aria_label(format!("Send to back failed: {error}"))
                        .text_size(px(11.))
                        .text_color(self.tone(0xb84b45, 0xe79b96))
                        .child(format!("Send to back failed: {error}")),
                );
            }
            for job in &self.jobs {
                let shell = job.kind == JobKind::Shell;
                let (label, elapsed) = job.subtitle_parts(now);
                jobs_panel = jobs_panel.child(
                    div()
                        .min_w_0()
                        .flex()
                        .px(px(6.))
                        .gap(px(8.))
                        .child(
                            div()
                                .mt(px(1.))
                                .size(px(17.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.))
                                .bg(self.tone(
                                    if shell { 0xe3e8e5 } else { 0xf3e7cf },
                                    if shell { 0x1c2723 } else { 0x33281a },
                                ))
                                .text_color(self.tone(
                                    if shell { 0x3f5a4c } else { 0x9c641a },
                                    if shell { 0x9cc2ad } else { 0xe5b567 },
                                ))
                                .font_weight(FontWeight::BOLD)
                                .text_size(px(10.))
                                .child(if shell { "$" } else { "◆" }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .id(format!("job-title-{}", job.id))
                                        .test_support()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .text_color(self.tone(0x252829, 0xd4d8dc))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(job.title.clone()),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex()
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x87908d, 0x6a7279))
                                        .child(
                                            div()
                                                .id(format!("job-label-{}", job.id))
                                                .test_support()
                                                .min_w_0()
                                                .flex_1()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(label),
                                        )
                                        .when_some(elapsed, |view, elapsed| {
                                            view.child(
                                                div()
                                                    .id(format!("job-elapsed-{}", job.id))
                                                    .test_support()
                                                    .role(Role::Label)
                                                    .aria_label(format!("Elapsed {elapsed}"))
                                                    .flex_shrink_0()
                                                    .child(format!(" {elapsed}")),
                                            )
                                        }),
                                ),
                        ),
                );
            }
        }
        div()
            .w(self.scaled(SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(self.tone(0xf4f1eb, 0x0d0f11))
            .when(floating, |view| view.rounded_bl(px(FLOATING_CORNER_RADIUS)))
            .border_r_1()
            .border_color(self.tone(0xc8c3ba, 0x24282c))
            .child(
                BaseButton::new("new-session")
                    .accessibility_label("New session")
                    .h(self.scaled(39.))
                    .px(self.scaled(16.))
                    .line_height(self.scaled(16.))
                    .justify_start()
                    .text_color(self.tone(0x667078, 0x92999f))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_modal(Modal::NewSession, window, cx);
                    }))
                    .child("⊞  New session"),
            )
            .child(tabs)
            .child(div().flex_1())
            .when(show_jobs, |sidebar| sidebar.child(jobs_panel))
            .child(
                div()
                    .p(self.scaled(14.))
                    .border_t_1()
                    .border_color(self.tone(0xded8cb, 0x2b3034))
                    .flex()
                    .flex_col()
                    .gap(self.scaled(12.))
                    .child(self.latency_controls(cx))
                    .child(
                        BaseButton::new("footer-tabs")
                            .accessibility_label("Open tabs")
                            .h(self.scaled(21.))
                            .line_height(self.scaled(16.))
                            .justify_start()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Sessions, window, cx);
                            }))
                            .child("≡  Tabs"),
                    )
                    .child(
                        BaseButton::new("footer-settings")
                            .accessibility_label("Open settings")
                            .h(self.scaled(21.))
                            .line_height(self.scaled(16.))
                            .justify_start()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Settings, window, cx);
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(self.scaled(7.))
                                    .child(
                                        Icon::default()
                                            .data(include_bytes!("icons/settings.svg"))
                                            .with_size(self.scaled(14.)),
                                    )
                                    .child("Settings"),
                            ),
                    ),
            )
            .into_any_element()
    }

    // Keep the actual row and the detached measurement probe on this same
    // renderer. A text-length estimate cannot model GPUI's Markdown wrapping.
    fn message_row(&self, row: &TranscriptRow, index: usize, cx: &Context<Self>) -> Stateful<Div> {
        let user = row.role.label() == "YOU";
        let last = self
            .transcript
            .get(&self.active)
            .is_some_and(|transcript| index + 1 == transcript.len());
        let show_separator = user && !last;
        let shade = if user {
            self.tone(0xe4ddd0, 0x1c242b)
        } else {
            self.tone(0xf4f1eb, 0x101214)
        };
        let mut body = div().flex().flex_col().gap(self.scaled(10.));
        match row.kind {
            TranscriptRowKind::Tool => {
                body = body.child(div().font_family(MONO_FONT_FAMILY).child(row.body.clone()))
            }
            TranscriptRowKind::Error => {
                body = body.child(
                    div()
                        .mt(px(4.))
                        .flex()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(self.tone(0xe8b8b8, 0x664547))
                        .bg(self.tone(0xf3e7e4, 0x271a1b))
                        .child(
                            div()
                                .w(px(3.))
                                .flex_shrink_0()
                                .bg(self.tone(0xcf222e, 0xf85149)),
                        )
                        .child(
                            div()
                                .px(px(14.))
                                .py(px(11.))
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .font_weight(FontWeight::BOLD)
                                        .text_size(px(11.))
                                        .text_color(self.tone(0xcf222e, 0xf85149))
                                        .child("⚠")
                                        .child("Error"),
                                )
                                .child(row.body.clone()),
                        ),
                )
            }
            TranscriptRowKind::Normal if !user => {
                body = body.child(self.markdown_body(row, index, cx))
            }
            _ => body = body.child(row.body.clone()),
        }
        for (image_index, image) in row.images.iter().enumerate() {
            let source = self.attachments.get(&self.active, row, image_index);
            let (width, height) = source
                .and_then(|source| imagesize::blob_size(source.bytes()).ok())
                .map_or_else(
                    || image_display_size(200, 120, self.transcript_content_width.as_f32()),
                    |size| {
                        image_display_size(
                            size.width,
                            size.height,
                            self.transcript_content_width.as_f32(),
                        )
                    },
                );
            let thumbnail = div()
                .h(px(height))
                .w(px(width))
                .rounded(px(5.))
                .border_1()
                .border_color(self.tone(0xd8d1c6, 0x343a40))
                .overflow_hidden();
            body = body.child(match source {
                Some(source) => thumbnail
                    .child(
                        img(source.clone())
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element(),
                None => thumbnail
                    .child(if image.starts_with("data:") {
                        "Image unavailable".to_owned()
                    } else {
                        image.clone()
                    })
                    .into_any_element(),
            });
        }
        div()
            .id(("message", index))
            .w_full()
            .font_family(UI_FONT_FAMILY)
            .text_size(self.scaled(13.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(self.scaled(if user { 10. } else { 6. }))
            .px(self.scaled(28.))
            .pt(self.scaled(18.))
            // Agent segments stay continuous. A final user row replaces its
            // separator with padding to keep consistent spacing on append.
            .pb(self.scaled(if user && last { 21. } else { 20. }))
            .bg(shade)
            .when(user, |view| {
                view.border_t_1()
                    .border_color(self.tone(0xcfc6b6, 0x313a41))
            })
            .when(row.kind == TranscriptRowKind::Reasoning, |view| {
                view.opacity(0.42)
            })
            .when(show_separator, |view| view.border_b_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .text_size(self.scaled(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if user {
                        self.tone(0x666f76, 0xd7c4a3)
                    } else {
                        self.tone(0x666f76, 0x8d959d)
                    })
                    .child(row.role.label().to_owned())
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(self.tone(0x9da5a4, 0x6a7279))
                            .child(timestamp(row.time)),
                    ),
            )
            .child(body)
    }

    fn load_earlier_row(&self, cursor: String, loading: bool, cx: &Context<Self>) -> Stateful<Div> {
        let id = self.active.clone();
        let key_id = id.clone();
        let key_cursor = cursor.clone();
        div()
            .id("load-earlier")
            .font_family(UI_FONT_FAMILY)
            .text_size(px(13.))
            .flex_shrink_0()
            .mx(px(28.))
            .my(px(12.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x30353a))
            .bg(self.tone(0xfffdfa, 0x191c1f))
            .text_color(self.tone(0x555b5c, 0xe8e5df))
            .when(!loading, |button| {
                button
                    .role(Role::Button)
                    .aria_label("Load earlier messages")
                    .track_focus(&self.history_focus)
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.load_earlier(&key_id, &key_cursor, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.load_earlier(&id, &cursor, cx);
                    }))
            })
            .child(if loading {
                "Loading earlier messages…"
            } else {
                "Load earlier messages"
            })
    }

    fn load_earlier_element(
        &self,
        cursor: String,
        loading: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let element = self.load_earlier_row(cursor, loading, cx);
        element.into_any_element()
    }

    fn cancel_waiting_form(&self, target: &pending::CancelTarget) {
        if let Some(api) = &self.api {
            api.send(Command::CancelForm {
                form_id: target.form_id.clone(),
                session_id: target.session_id.clone(),
                directory: target.directory.clone(),
            });
        }
    }

    fn open_web_ui(&mut self, cx: &mut Context<Self>) {
        match opencode_gpui::api::web_ui_url(&self.settings.current.base_url) {
            Ok(url) => cx.open_url(&url),
            Err(error) => {
                self.connection_status = format!("Could not open the web UI: {error:#}");
                cx.notify();
            }
        }
    }

    fn form_notice(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let notice = self.forms.notice(Some(&self.active), &self.child_parents)?;
        let mut bar = div()
            .mx(self.scaled(16.))
            .mb(self.scaled(8.))
            .h(self.scaled(45.))
            .flex()
            .items_center()
            .gap(self.scaled(8.))
            .px(self.scaled(12.))
            .rounded(px(9.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x2d3236))
            .bg(self.tone(0xf5f3ef, 0x15181b))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.tone(0x98600f, 0xd8a55f))
                    .child(notice.text),
            )
            .child(
                BaseButton::new("open-web-ui")
                    .accessibility_label("Open web UI to answer form")
                    .on_click(cx.listener(|this, _, _, cx| this.open_web_ui(cx)))
                    .flex_shrink_0()
                    .text_color(self.tone(0x4d5354, 0xc4c8ca))
                    .child("Open web UI"),
            );
        if let Some(target) = notice.cancel {
            let key_target = target.clone();
            bar = bar.child(
                div()
                    .id("cancel-form")
                    .role(Role::Button)
                    .aria_label("Cancel waiting form")
                    .test_support()
                    .flex_shrink_0()
                    .track_focus(&self.form_cancel_focus)
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.cancel_waiting_form(&key_target);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cancel_waiting_form(&target);
                        cx.notify();
                    }))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x353b40))
                    .bg(self.tone(0xf1efec, 0x393939))
                    .text_color(self.tone(0x252829, 0xf8f7f7))
                    .px(px(10.))
                    .py(px(4.))
                    .child("Cancel"),
            );
        }
        Some(bar.into_any_element())
    }

    fn permission_card(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let prompt = self.visible_permission()?;
        let request = &prompt.request;
        let requester = self
            .sessions
            .iter()
            .find(|session| session.id == request.session_id)
            .map(|session| session.title.clone())
            .or_else(|| {
                self.child_parents
                    .get(&request.session_id)
                    .and_then(|parent| self.sessions.iter().find(|session| &session.id == parent))
                    .map(|parent| format!("a subagent of {}", parent.title))
            })
            .unwrap_or_else(|| format!("session {}", request.session_id));
        let directory = prompt.directory.or_else(|| {
            self.sessions
                .iter()
                .find(|session| session.id == request.session_id)
                .map(|session| session.directory.clone())
                .or_else(|| {
                    self.child_parents
                        .get(&request.session_id)
                        .and_then(|parent| {
                            self.sessions.iter().find(|session| &session.id == parent)
                        })
                        .map(|parent| parent.directory.clone())
                })
        });
        let mut context = format!("Requested by {requester}");
        if let Some(directory) = directory {
            context.push_str(&format!("\n{directory}"));
        }
        if let Some(source) = pending::source_text(request) {
            context.push_str(&format!("\n{source}"));
        }
        let mut details = div().flex().flex_col().gap(px(12.));
        let mut has_details = false;
        if let Some(message) = request
            .message
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            details = details.child(message.to_owned());
            has_details = true;
        }
        if !request.resources.is_empty() {
            details = details.child(permission_detail(request.resources.join("\n"), self.dark));
            has_details = true;
        }
        if let Some(metadata) = pending::metadata_text(request.metadata.as_ref()) {
            details = details.child(permission_metadata(metadata, self.dark));
            has_details = true;
        }
        let always = pending::always_patterns(request);
        if let Some(patterns) = &always {
            details = details
                .child(
                    div()
                        .text_color(self.tone(0x8b5918, 0xd8a55f))
                        .child("Always allow would remember:"),
                )
                .child(permission_detail(patterns.clone(), self.dark));
            has_details = true;
        }
        let mut actions = div().flex().justify_end().gap(px(8.));
        let mut choices = vec![
            ("Deny", protocol::PermissionDecision::Reject),
            ("Allow once", protocol::PermissionDecision::Once),
        ];
        if always.is_some() {
            choices.push(("Always allow", protocol::PermissionDecision::Always));
        }
        let in_flight = self.permission_in_flight.contains(&request.id);
        for (index, (label, decision)) in choices.into_iter().enumerate() {
            let id = request.id.clone();
            let session_id = request.session_id.clone();
            let key_id = id.clone();
            let key_session_id = session_id.clone();
            actions = actions.child(
                div()
                    .id(format!("permission-{label}-{}", request.id))
                    .role(Role::Button)
                    .aria_label(label)
                    .test_support()
                    .track_focus(&self.permission_focus[index])
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !in_flight && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.reply_permission(
                                key_id.clone(),
                                key_session_id.clone(),
                                decision,
                                cx,
                            );
                            cx.stop_propagation();
                        }
                    }))
                    .px(px(12.))
                    .py(px(7.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x353b40))
                    .bg(self.tone(
                        if decision == protocol::PermissionDecision::Once {
                            0xc59535
                        } else {
                            0xfffdfa
                        },
                        if decision == protocol::PermissionDecision::Once {
                            0xd29b52
                        } else {
                            0x1d2124
                        },
                    ))
                    .when(!in_flight, |button| {
                        button
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reply_permission(id.clone(), session_id.clone(), decision, cx);
                            }))
                    })
                    .child(label),
            );
        }
        let action = if request.action.trim().is_empty() {
            "tool action"
        } else {
            request.action.trim()
        };
        let mut card = div()
            .id("permission-card")
            .test_support()
            .role(Role::Group)
            .aria_label(format!("Permission request: {action}"))
            .track_focus(&self.permission_container_focus)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    // Arrival of a prompt must not turn the next composer
                    // keystroke into a permission answer.
                    cx.stop_propagation();
                }
            }))
            .mx(px(16.))
            .mb(px(17.))
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(px(11.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x2d3236))
            .bg(self.tone(0xfffdfa, 0x15181b))
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .child(format!("Allow {action}?")),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .text_color(self.tone(0x737875, 0x899097))
                    .child(context),
            );
        if has_details {
            card = card.child(
                div()
                    .min_h(px(80.))
                    .max_h(px(320.))
                    .overflow_y_scrollbar()
                    .child(details),
            );
        }
        Some(card.child(actions).into_any_element())
    }

    fn transcript_indicator(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let conversation = self.conversations.get(&self.active);
        let has_rows = self
            .transcript
            .get(&self.active)
            .is_some_and(|rows| !rows.is_empty());
        let loading = self.loading_messages.get(&self.active);
        let failure = self.message_load_errors.get(&self.active);
        let (label, spinning) =
            if let Some(RunStatus::Retry { message, .. }) = self.statuses.get(&self.active) {
                (message.clone(), true)
            } else if let Some(cursor) = loading {
                if cursor.is_some() {
                    ("Loading earlier messages".into(), true)
                } else if conversation.is_some_and(|conversation| conversation.loaded) {
                    ("Refreshing conversation".into(), true)
                } else {
                    ("Loading conversation".into(), true)
                }
            } else if let Some(cursor) = failure {
                if cursor.is_some() {
                    ("Could not load earlier messages".into(), false)
                } else if has_rows {
                    ("Could not refresh conversation".into(), false)
                } else {
                    ("Could not load conversation".into(), false)
                }
            } else if self.active.is_empty() {
                ("Open a session to begin".into(), false)
            } else if self.is_running(&self.active) {
                ("OpenCode is working".into(), true)
            } else if !conversation.is_some_and(|conversation| conversation.loaded) {
                ("Loading conversation".into(), true)
            } else if !has_rows {
                ("No messages yet".into(), false)
            } else {
                return None;
            };
        let mut pill = div()
            .id("transcript-status")
            .test_support()
            .mx_auto()
            .mt(px(10.))
            .mb(px(10.))
            .px(px(12.))
            .min_h(px(29.))
            .max_w(px(510.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded_full()
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x34393e))
            .bg(self.tone(0xfffdfa, 0x171a1d))
            .text_size(px(12.))
            .text_color(self.tone(0x555b5c, 0xc4c8ca))
            .when(spinning, |pill| {
                pill.child(
                    div()
                        .id("transcript-status-gear")
                        .test_support()
                        .w(px(14.))
                        .h(px(14.))
                        .flex_shrink_0()
                        .child(status_gear(
                            self.tone(0xa46910, 0xe5b567),
                            format!("transcript-status-gear-{}", self.active),
                        )),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label),
            );
        if let Some(cursor) = failure {
            let session = self.active.clone();
            let retry_cursor = cursor.clone();
            let key_session = session.clone();
            let key_cursor = retry_cursor.clone();
            pill = pill.child(
                div()
                    .id("retry-history")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("Retry conversation load")
                    .track_focus(&self.retry_history_focus)
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .cursor_pointer()
                    .text_color(self.tone(0x8d5918, 0xf1bb72))
                    .child("Retry history")
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            if let Some(cursor) = &key_cursor {
                                this.load_earlier(&key_session, cursor, cx);
                            } else {
                                this.request_newest(&key_session);
                            }
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(cursor) = &retry_cursor {
                            this.load_earlier(&session, cursor, cx);
                        } else {
                            this.request_newest(&session);
                        }
                        cx.notify();
                    })),
            );
        }
        Some(pill.into_any_element())
    }

    fn chat(&self, _window: &Window, cx: &Context<Self>) -> AnyElement {
        if self.follow_bottom {
            self.scroll.scroll_to_bottom();
        }
        let scroll = self.scroll.clone();
        let client = cx.entity().downgrade();
        let session = self.active.clone();
        let following = self.follow_bottom;
        let generation = self.connection_generation;
        let rendered_top = scroll.top_item();
        // GTK reserves a permanent 14px gutter for the transcript scrollbar.
        let mut rows = div()
            .on_children_prepainted(move |_, window, cx| {
                // Read GPUI's resolved scroll state; never calculate row heights
                // or content extents. A late Markdown/image layout can grow the
                // content after the one-shot bottom request was consumed.
                let at_bottom = scroll.offset().y <= -scroll.max_offset().y;
                if following != at_bottom || scroll.top_item() != rendered_top {
                    let client = client.clone();
                    let session = session.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = client.update(cx, |this, cx| {
                            if this.active != session || this.connection_generation != generation {
                                return;
                            }
                            // Recheck after input dispatch, outside prepaint.
                            // Refresh during prepaint itself cannot schedule a frame.
                            let at_bottom = this.scroll.offset().y <= -this.scroll.max_offset().y;
                            let was_following = this.follow_bottom;
                            let request_bottom = was_following && !at_bottom;
                            if at_bottom
                                && !this.transcript_pointer_down
                                && this.autoscroll.is_none()
                            {
                                this.follow_bottom = true;
                            }
                            if request_bottom
                                || was_following != this.follow_bottom
                                || this.scroll.top_item() != rendered_top
                            {
                                // Only render may issue the native bottom request,
                                // using the latest follow flag immediately before
                                // prepaint. Never leave one queued across input.
                                cx.notify();
                            }
                        });
                    });
                }
            })
            .id("transcript")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.follow_bottom = false;
                cx.notify();
            }))
            .w_full()
            .pr(self.scaled(TRANSCRIPT_SCROLLBAR_GUTTER))
            .flex()
            .flex_col();
        let sticky = self.sticky_user_row().map(|row| {
            div()
                .id("sticky-user")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .right(self.scaled(TRANSCRIPT_SCROLLBAR_GUTTER))
                .min_h(px(106.))
                .max_h(px(226.))
                .px(px(28.))
                .pt(px(18.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .bg(self.tone(0xf4f1eb, 0x101214))
                .border_b_1()
                .border_color(self.tone(0xc8c3ba, 0x343a3f))
                .shadow_sm()
                .child(
                    div()
                        .flex()
                        .text_size(px(11.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(self.tone(0x666f76, 0x8d959d))
                        .child("YOU")
                        .child(div().flex_1())
                        .child(
                            div()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(self.tone(0x9da5a4, 0x6a7279))
                                .child(timestamp(row.time)),
                        ),
                )
                .child(
                    div()
                        .id("sticky-body")
                        .test_support()
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .child(row.body.clone()),
                )
                .into_any_element()
        });
        let has_load = self
            .conversations
            .get(&self.active)
            .is_some_and(|conversation| conversation.next_cursor.is_some());
        if has_load {
            let cursor = self.conversations[&self.active]
                .next_cursor
                .clone()
                .unwrap();
            let loading = self.loading_messages.contains_key(&self.active);
            rows = rows.child(self.load_earlier_element(cursor, loading, cx));
        }
        if let Some(transcript) = self.transcript.get(&self.active) {
            for (index, row) in transcript.iter().enumerate() {
                rows = rows.child(self.message_row(row, index, cx));
            }
        }
        div()
            .flex_1()
            .min_h_0()
            .relative()
            // Clip scroll content at the chat edge, including subpixel glyphs above the sticky row.
            .overflow_hidden()
            .child(rows)
            .when_some(sticky, |view, sticky| view.child(sticky))
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    fn sticky_user_row(&self) -> Option<&TranscriptRow> {
        let transcript = self.transcript.get(&self.active)?;
        let has_load = self
            .conversations
            .get(&self.active)
            .is_some_and(|conversation| conversation.next_cursor.is_some());
        let (top, displacement) = self.scroll.logical_scroll_top();
        sticky_user_index(transcript, top, has_load, displacement == px(0.))
            .and_then(|index| transcript.get(index))
    }

    fn resume_warning(&self, cx: &Context<Self>) -> Option<String> {
        let paused = !self
            .statuses
            .get(&self.active)
            .is_some_and(RunStatus::is_busy);
        let count = if paused {
            self.conversations
                .get(&self.active)
                .map_or(0, |conversation| conversation.tray_items().len())
        } else {
            0
        };
        let has_input =
            !self.composer.read(cx).value().trim().is_empty() || !self.attachments_draft.is_empty();
        tray::resume_warning(count, has_input)
    }

    fn tray_view(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let items = self.conversations.get(&self.active)?.tray_items();
        let rows = tray::tray_rows(&items, None, None, &self.tray_in_flight);
        if rows.is_empty() {
            return None;
        }
        let running = self.is_running(&self.active);
        let paused = !running;
        let mut card = div()
            .id("queue-tray")
            .mx(px(16.))
            .mb(px(6.))
            .max_h(px(195.))
            .overflow_y_scroll()
            .p(px(5.))
            .rounded(px(9.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x2d3236))
            .bg(self.tone(0xf5f0e7, 0x15181b))
            .child(
                div()
                    .px(px(8.))
                    .py(px(1.))
                    .flex()
                    .items_center()
                    .when(paused, |header| {
                        header.child(
                            div()
                                .mr(px(5.))
                                .text_color(self.tone(0x858e8c, 0x7d837f))
                                .child("▪"),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::BOLD)
                            .child(tray::header_text(rows.len(), paused)),
                    )
                    .when_some(
                        if paused {
                            tray::resume_request(&rows)
                        } else {
                            None
                        },
                        |header, (id, request)| {
                            header.child(
                                BaseButton::new("resume-tray")
                                    .accessibility_label("Resume waiting prompts")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.act_on_tray(id.clone(), request, cx);
                                    }))
                                    .line_height(px(16.))
                                    .px(px(10.))
                                    .py(px(4.))
                                    .rounded_full()
                                    .bg(self.tone(0xc59535, 0xd29b52))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(self.tone(0x252829, 0x17130e))
                                    .child("▶ Resume"),
                            )
                        },
                    ),
            );
        for (group_index, group) in tray::tray_groups(&rows, running).into_iter().enumerate() {
            card = card.child(
                div()
                    .id(format!("queue-group-{group_index}"))
                    .pt(px(3.))
                    .pl(px(11.))
                    .border_t_1()
                    .border_color(self.tone(0xd8d1c6, 0x262b2f))
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(10.))
                    .text_color(self.tone(0x7c8582, 0x8e938f))
                    .child(group.label),
            );
            for row in group.rows {
                let switch = tray::row_request(&row, tray::RowAction::Switch, paused);
                let cancel = tray::row_request(&row, tray::RowAction::Cancel, paused);
                let badge = tray::badge_text(row.delivery);
                let label = tray::switch_label(row.delivery);
                let summary = row.summary.clone();
                let mut line = div()
                    .px(px(8.))
                    .py(px(2.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(
                        div()
                            .px(px(7.))
                            .py(px(0.))
                            .rounded_full()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(10.))
                            .bg(self.tone(
                                if row.delivery == protocol::Delivery::Queue {
                                    0xe8e3d8
                                } else {
                                    0xe8f2e8
                                },
                                if row.delivery == protocol::Delivery::Queue {
                                    0x23272b
                                } else {
                                    0x3a2d19
                                },
                            ))
                            .child(badge),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(row.summary),
                    );
                if let Some(request) = switch {
                    let id = row.id.clone();
                    line = line.child(
                        BaseButton::new(format!("switch-waiting-{id}"))
                            .accessibility_label(format!(
                                "Switch to {}: {summary}",
                                if label.contains("Queue") {
                                    "queue"
                                } else {
                                    "steer"
                                }
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act_on_tray(id.clone(), request, cx);
                            }))
                            .line_height(px(16.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(0xc8c3ba, 0x353b40))
                            .child(label),
                    );
                }
                if let Some(request) = cancel {
                    let id = row.id.clone();
                    line = line.child(
                        BaseButton::new(format!("cancel-waiting-{id}"))
                            .accessibility_label(format!("Cancel waiting prompt: {summary}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act_on_tray(id.clone(), request, cx);
                            }))
                            .line_height(px(16.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(0xc8c3ba, 0x353b40))
                            .child("×"),
                    );
                }
                card = card.child(line);
            }
        }
        Some(card.into_any_element())
    }

    fn picker_control_probe(&self, model: bool) -> impl IntoElement {
        let centers = self.picker_control_centers.clone();
        let index = usize::from(!model);
        let open = self.modal == Some(if model { Modal::Model } else { Modal::Level });
        // The selector's flex width depends on its label and available room;
        // measure its actual painted bounds instead of estimating a fixed x.
        canvas(
            move |bounds, window, cx| {
                let center = bounds.center().x;
                let mut centers = centers.borrow_mut();
                if centers[index].is_none_or(|previous| (previous - center).abs() > px(0.5)) {
                    centers[index] = Some(center);
                    if open {
                        window.defer(cx, |window, _| window.refresh());
                    }
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
    }

    fn composer(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let can_send = self.can_send(cx);
        let selected_model = self.selected_model();
        let (model, context_limit) =
            model_button_presentation(&self.catalog, selected_model.as_ref());
        let context = context_limit
            .map(|limit| {
                let used = self
                    .conversations
                    .get(&self.active)
                    .and_then(Conversation::context_tokens)
                    .unwrap_or(if self.api.is_none() && self.active == "ses_preview" {
                        13_400
                    } else {
                        0
                    });
                model::format_context_usage(used, limit)
            })
            .unwrap_or_default();
        let running = self.is_running(&self.active);
        let compact = self.transcript_width(window) < self.scaled(400.);
        let mut files = div().px(self.scaled(13.)).flex().gap(self.scaled(7.));
        for (index, path) in self.attachments_draft.iter().enumerate() {
            let target = path.clone();
            let session_id = self.active.clone();
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            files = files.child(
                div()
                    .px(self.scaled(8.))
                    .py(self.scaled(4.))
                    .rounded(px(5.))
                    .bg(self.tone(0xf4f1eb, 0x262b30))
                    .flex()
                    .gap(self.scaled(6.))
                    .child(label.clone())
                    .child(
                        BaseButton::new(format!("remove-attachment-{index}"))
                            .accessibility_label(format!("Remove attachment: {label}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.active != session_id {
                                    return;
                                }
                                let matching = this.attachments_draft.get(index) == Some(&target);
                                let index = matching.then_some(index).or_else(|| {
                                    this.attachments_draft
                                        .iter()
                                        .position(|path| path == &target)
                                });
                                if let Some(index) = index {
                                    this.attachments_draft.remove(index);
                                    this.cleanup_owned_paste(&target);
                                    cx.notify();
                                }
                            }))
                            .child("×"),
                    ),
            );
        }
        let paste_owner = cx.entity();
        let paste_session = self.active.clone();
        div()
            .mx(self.scaled(16.))
            .mb(self.scaled(17.))
            .rounded(px(12.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x30353a))
            .bg(self.tone(0xffffff, 0x191c1f))
            .child(
                Textarea::new(&self.composer)
                    .aria_label("Ask OpenCode anything…")
                    .editor_padding_left(self.scaled(14.))
                    // Base's scrollbar includes editor padding in its viewport
                    // but not its content extent. Put vertical insets on the
                    // frame instead so thumb and text share the same viewport.
                    .editor_padding_top(px(0.))
                    .editor_padding_bottom(px(0.))
                    .py(self.scaled(8.))
                    .on_paste(move |item, _, app| {
                        paste_owner.update(app, |this, cx| {
                            this.paste_attachments(&paste_session, item, cx)
                        })
                    })
                    .bordered(false)
                    // The outer composer owns the background for both editor
                    // and toolbar; borderless alone still paints an input fill.
                    .appearance(false)
                    // Keep the editor's viewport separate from the toolbar;
                    // content overflow uses the input engine's own scrollbar.
                    .h(self.scaled(89.))
                    .overflow_hidden(),
            )
            .when(!self.attachments_draft.is_empty(), |composer| {
                composer.child(files)
            })
            .child(
                div()
                    .id("composer-toolbar")
                    .test_support()
                    .h(self.scaled(46.))
                    .py(self.scaled(7.))
                    .pl(self.scaled(2.))
                    .pr(self.scaled(14.))
                    .flex()
                    .items_center()
                    .gap(self.scaled(8.))
                    .child(
                        div()
                            .id("composer-leading-actions")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(self.scaled(8.))
                            .child(
                                div()
                                    .id("attach-file")
                                    .role(Role::Button)
                                    .aria_label("Attach file")
                                    .test_support()
                                    .track_focus(&self.composer_action_focus[0])
                                    .focus_visible(|style| {
                                        style.border_color(self.tone(0x2356a8, 0x78baff))
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.choose_attachments(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_attachments(cx);
                                    }))
                                    .w(self.scaled(32.))
                                    .h(self.scaled(32.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div().id("composer-attach-artwork").test_support().child(
                                            Icon::default()
                                                .data(include_bytes!("icons/paperclip.svg"))
                                                .with_size(self.scaled(22.))
                                                .text_color(self.tone(0x555b5c, 0xc8c4bd)),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("composer-model")
                                    .role(Role::Button)
                                    .aria_label("Choose model")
                                    .test_support()
                                    .relative()
                                    .track_focus(&self.composer_action_focus[1])
                                    .focus_visible(|style| {
                                        style.border_color(self.tone(0x2356a8, 0x78baff))
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, window, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.show_modal(Modal::Model, window, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_modal(Modal::Model, window, cx);
                                    }))
                                    // A ceiling, not a fixed width: flex can still
                                    // shrink this before reasoning/Send controls.
                                    .max_w(self.scaled(320.))
                                    .min_w(self.scaled(42.))
                                    .h(self.scaled(32.))
                                    .px(self.scaled(8.))
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(model),
                                    )
                                    .child(self.picker_control_probe(true))
                                    .child(div().flex_shrink_0().child("⌄")),
                            )
                            .child(
                                div()
                                    .id("composer-level")
                                    .role(Role::Button)
                                    .aria_label("Choose reasoning level")
                                    .test_support()
                                    .relative()
                                    .track_focus(&self.composer_action_focus[2])
                                    .focus_visible(|style| {
                                        style.border_color(self.tone(0x2356a8, 0x78baff))
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, window, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.show_modal(Modal::Level, window, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_modal(Modal::Level, window, cx);
                                    }))
                                    .h(self.scaled(32.))
                                    .max_w(self.scaled(112.))
                                    .min_w(self.scaled(44.))
                                    .when(
                                        self.transcript_width(window) >= self.scaled(480.),
                                        |control| control.flex_shrink_0(),
                                    )
                                    .px(self.scaled(8.))
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(
                                                selected_model
                                                    .and_then(|selection| selection.variant)
                                                    .unwrap_or_else(|| "Default".into()),
                                            ),
                                    )
                                    .child(self.picker_control_probe(false))
                                    .child(div().flex_shrink_0().child("⌄")),
                            ),
                    )
                    .child(
                        div()
                            .id("composer-trailing-actions")
                            .test_support()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(self.scaled(8.))
                            .when(!compact, |footer| {
                                footer.child(
                                    div()
                                        .id("composer-context-usage")
                                        .test_support()
                                        .whitespace_nowrap()
                                        .text_size(self.scaled(11.))
                                        .text_color(self.tone(0x858e8c, 0x899097))
                                        .child(context),
                                )
                            })
                            .when(
                                running && window.viewport_size().width >= px(900.),
                                |footer| {
                                    footer.child(
                                        div()
                                            .whitespace_nowrap()
                                            .text_size(self.scaled(10.))
                                            .text_color(self.tone(0x858e8c, 0x899097))
                                            .child("Ctrl+Enter to queue"),
                                    )
                                },
                            )
                            .when(running, |footer| {
                                footer.child(
                                    div()
                                        .id("stop-run")
                                        .role(Role::Button)
                                        .aria_label("Stop current run")
                                        .test_support()
                                        .track_focus(&self.composer_action_focus[3])
                                        .focus_visible(|style| {
                                            style.border_color(self.tone(0x2356a8, 0x78baff))
                                        })
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, window, cx| {
                                                if matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                ) {
                                                    this.stop_active(cx);
                                                    this.composer
                                                        .focus_handle(cx)
                                                        .focus(window, cx);
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.stop_active(cx);
                                            this.composer.focus_handle(cx).focus(window, cx);
                                        }))
                                        .w(self.scaled(32.))
                                        .h(self.scaled(32.))
                                        .flex_shrink_0()
                                        .rounded_full()
                                        .border_1()
                                        .border_color(self.tone(0x555b5c, 0x6b7176))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            Icon::default()
                                                .data(include_bytes!("icons/stop.svg"))
                                                .with_size(self.scaled(22.))
                                                .text_color(self.tone(0x555b5c, 0xc8c4bd)),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .id("send-prompt")
                                    .role(Role::Button)
                                    .aria_label("Send prompt")
                                    .test_support()
                                    .when(can_send, |button| {
                                        button
                                            .track_focus(&self.composer_action_focus[4])
                                            .focus_visible(|style| {
                                                style.border_color(self.tone(0x2356a8, 0x78baff))
                                            })
                                            .on_key_down(cx.listener(
                                                |this, event: &KeyDownEvent, _, cx| {
                                                    if matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    ) {
                                                        this.send_prompt(false, cx);
                                                        cx.stop_propagation();
                                                    }
                                                },
                                            ))
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.send_prompt(false, cx);
                                            }))
                                    })
                                    .w(self.scaled(32.))
                                    .h(self.scaled(32.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(self.tone(0xc59535, 0xd29b52))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(self.tone(
                                        if can_send { 0x17130e } else { 0x7a5d29 },
                                        if can_send { 0x17130e } else { 0x815f35 },
                                    ))
                                    .child(
                                        Icon::default()
                                            .data(include_bytes!("icons/send.svg"))
                                            .with_size(self.scaled(22.)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn settings_sessions_body(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let query = self.settings.session_search.read(cx).value();
        let search_focused = self
            .settings
            .session_search
            .focus_handle(cx)
            .is_focused(window);
        let choices = filter_all_sessions(&self.sessions, &query);
        let shown = choices.len();
        let mut rows = div()
            .id("settings-sessions-list")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.settings_sessions_scroll)
            .px(px(21.))
            .flex()
            .flex_col()
            .gap(px(7.));
        for (index, session) in choices.into_iter().enumerate() {
            let id = session.id.clone();
            let key_id = id.clone();
            let open = self.open_tabs.contains(&id);
            let highlighted = open || self.settings_highlight == Some(index);
            rows = rows.child(
                div()
                    .id(format!("settings-session-{id}"))
                    .role(Role::Button)
                    .aria_label(format!("Open session {}", session.title))
                    .track_focus(self.settings_session_focus.get(&id).expect("session focus"))
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(id.clone());
                        this.modal = None;
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(
                            event.keystroke.key.to_ascii_lowercase().as_str(),
                            "enter" | "return" | "space"
                        ) {
                            this.select_session(key_id.clone());
                            this.modal = None;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .px(px(14.))
                    .py(px(8.))
                    .min_w_0()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(self.tone(
                        if highlighted { 0xcbbba4 } else { 0xded8cb },
                        if highlighted { 0x3c4a5f } else { 0x242a34 },
                    ))
                    .bg(self.tone(
                        if highlighted { 0xf3ede3 } else { 0xfffdfa },
                        if highlighted { 0x222b38 } else { 0x1a1f26 },
                    ))
                    .child(
                        div()
                            .flex()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_weight(FontWeight::BOLD)
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .px(px(4.))
                                    .py(px(2.))
                                    .rounded(px(2.))
                                    .border_1()
                                    .border_color(self.tone(
                                        if open { 0xa5dbb4 } else { 0xded8cb },
                                        if open { 0x34d399 } else { 0x60a5fa },
                                    ))
                                    .bg(self.tone(
                                        if open { 0xe4f7e9 } else { 0xf4f1eb },
                                        if open { 0x173329 } else { 0x1b2a40 },
                                    ))
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(9.))
                                    .text_color(self.tone(
                                        if open { 0x167e49 } else { 0x77817e },
                                        if open { 0x34d399 } else { 0x60a5fa },
                                    ))
                                    .child(if open { "OPEN TAB" } else { "SERVER" }),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(5.))
                            .flex()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(self.tone(0x858e8c, 0x7f878e))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(session.directory.clone()),
                            )
                            .child(div().flex_shrink_0().child(timestamp(session.time.updated))),
                    ),
            );
        }
        if shown == 0 {
            rows = rows.child(div().p(px(16.)).child("No matching sessions"));
        }
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(self.tone(0xfffdfa, 0x15181b))
            .child(
                div()
                    .px(px(22.))
                    .pt(px(17.))
                    .pb(px(7.))
                    .border_b_1()
                    .border_color(self.tone(0xded8cb, 0x232932))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(16.))
                            .child("All Sessions"),
                    )
                    .child(
                        div()
                            .mt(px(2.))
                            .text_size(px(11.))
                            .text_color(self.tone(0x818984, 0x899097))
                            .child(format!(
                                "Search all {} workspace sessions on this server",
                                self.sessions.len()
                            )),
                    ),
            )
            .child(
                div()
                    .mx(px(18.))
                    .mt(px(12.))
                    .mb(px(12.))
                    .h(px(50.))
                    .px(px(9.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(if search_focused {
                        self.tone(0x4f99ee, 0x3b82f6)
                    } else {
                        self.tone(0xd3cec5, 0x262c36)
                    })
                    .child(
                        Icon::default()
                            .data(include_bytes!("icons/search.svg"))
                            .with_size(px(16.))
                            .text_color(self.tone(0x8b918e, 0x71717a)),
                    )
                    .child(Input::new(&self.settings.session_search).appearance(false)),
            )
            .child(rows)
            .into_any_element()
    }

    fn activate_link_menu(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(menu) = self.link_menu.take() {
            if open {
                if may_open_link(&menu.url) {
                    cx.open_url(&menu.url);
                }
            } else {
                self.copy_with_feedback("link".into(), "Link copied", menu.url, cx);
            }
            cx.notify();
        }
    }

    fn link_menu_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        if self.modal.is_some() {
            return None;
        }
        let menu = self.link_menu.as_ref()?;
        let width = self.scaled(164.);
        let height = self.scaled(82.);
        let margin = self.scaled(8.);
        let left = menu
            .position
            .x
            .min((window.viewport_size().width - width - margin).max(margin))
            .max(margin);
        let top = menu
            .position
            .y
            .min((window.viewport_size().height - height - margin).max(margin))
            .max(margin);
        let mut popup = div()
            .id("link-context-menu")
            .test_support()
            .absolute()
            .left(left)
            .top(top)
            .w(width)
            .h(height)
            .p(self.scaled(5.))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x30353a))
            .bg(self.tone(0xfffdfa, 0x191c1f))
            .shadow_lg();
        for (index, label, open) in [(0, "Open link", true), (1, "Copy link", false)] {
            popup = popup.child(
                div()
                    .id(if open {
                        "link-menu-open"
                    } else {
                        "link-menu-copy"
                    })
                    .role(Role::Button)
                    .aria_label(label)
                    .test_support()
                    .track_focus(&self.link_menu_focus[index])
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
                    .cursor_pointer()
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.activate_link_menu(open, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activate_link_menu(open, cx);
                        cx.stop_propagation();
                    }))
                    .h(self.scaled(35.))
                    .px(self.scaled(10.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .hover(|style| style.bg(self.tone(0xece9e2, 0x303a44)))
                    .child(label),
            );
        }
        Some(
            div()
                .id("link-context-backdrop")
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.link_menu = None;
                    cx.notify();
                }))
                .child(popup)
                .into_any_element(),
        )
    }

    fn autoscroll_marker(&self) -> Option<AnyElement> {
        let origin = self.autoscroll.as_ref()?.origin;
        let diameter = self.scaled(26.);
        Some(
            div()
                .id("transcript-autoscroll-marker")
                .test_support()
                .absolute()
                .left(origin.x - diameter / 2.)
                .top(origin.y - diameter / 2.)
                .size(diameter)
                .rounded_full()
                .border_1()
                .border_color(self.tone(0x777f7e, 0x899097))
                .bg(self.tone(0xfffdfa, 0x24282c))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    svg()
                        .data(include_bytes!("icons/autoscroll.svg"))
                        .size(self.scaled(16.))
                        .flex_shrink_0()
                        .text_color(self.tone(0x252829, 0xe8e5df)),
                )
                .into_any_element(),
        )
    }

    fn modal_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let modal = self.modal?;
        let picker_panel_width = self
            .scaled(if modal == Modal::Model { 368. } else { 268. })
            .min((window.viewport_size().width - self.scaled(16.)).max(self.scaled(180.)));
        let picker_anchor = if matches!(modal, Modal::Model | Modal::Level) {
            let model = modal == Modal::Model;
            let sidebar = if self.sidebar_visible {
                SIDEBAR_WIDTH
            } else {
                0.
            };
            let button_center = if model {
                134.
            } else {
                let compact = self.transcript_width(window) < self.scaled(400.);
                233. + if compact { 30. } else { 0. }
            };
            let panel_width = picker_panel_width;
            let margin = self.scaled(8.);
            let target = self.picker_control_centers.borrow()[usize::from(!model)]
                .unwrap_or_else(|| self.scaled(sidebar + button_center));
            let left = (target - panel_width / 2.)
                .max(margin)
                .min((window.viewport_size().width - panel_width - margin).max(margin));
            let arrow = self.scaled(20.);
            let arrow_left = (target - left - px(arrow.as_f32() / 2.))
                .max(margin)
                .min((panel_width - arrow - margin).max(margin));
            Some((left, arrow_left))
        } else {
            None
        };
        let backdrop = div()
            .id("modal-backdrop")
            // A modal must also mask scroll hit testing, not just dim the transcript.
            .occlude()
            .absolute()
            .top(self.scaled(46.))
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(rgba(if self.dark { 0x000000a6 } else { 0x00000066 }))
            .when(
                matches!(window.window_decorations(), Decorations::Client { tiling } if !tiling.is_tiled())
                    && !window.is_maximized() && !window.is_fullscreen(),
                |view| view.rounded_bl(px(FLOATING_CORNER_RADIUS)).rounded_br(px(FLOATING_CORNER_RADIUS)),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if !(this.modal == Some(Modal::Rename) && this.rename_pending.is_some()) {
                    if this.modal == Some(Modal::Settings) {
                        this.settings.reset_draft(window, cx);
                    }
                    this.modal = None;
                    this.focus_composer_pending = true;
                    this.rename_target = None;
                    this.rename_error = None;
                    cx.notify();
                }
            }));
        let rename_id = self
            .rename_target
            .clone()
            .unwrap_or_else(|| self.active.clone());
        let rename_key_id = rename_id.clone();
        let rename_feedback_key = format!("session-id:{rename_id}");
        let rename_key_feedback = rename_feedback_key.clone();
        let rename_copied = self.copied(&rename_feedback_key);
        let panel: AnyElement = match modal {
            Modal::Sessions | Modal::NewSession => {
                let sessions = modal == Modal::Sessions;
                let mut card = div()
                    .id(if sessions {
                        "session-picker-card"
                    } else {
                        "new-session-picker-card"
                    })
                    .test_support()
                    .w(px(if sessions { 520. } else { 349. }))
                    .h(px(if sessions { 410. } else { 319. }))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x27272a))
                    .bg(self.tone(0xffffff, 0x18181b))
                    .shadow_lg()
                    .overflow_hidden()
                    // GPUI currently clips children to a rectangle, not this
                    // card's rounded outline. Keep scrolling row backgrounds
                    // away from the lower corner arc.
                    .when(!sessions, |card| card.pb(px(9.)))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(48.))
                            .px(px(12.))
                            .border_b_1()
                            .border_color(self.tone(0xded8cb, 0x27272a))
                            .flex()
                            .items_center()
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/search.svg"))
                                    .with_size(px(16.))
                                    .text_color(self.tone(0x8b918e, 0x71717a)),
                            )
                            .child(Input::new(&self.search).appearance(false)),
                    );
                if sessions {
                    let query = self.search.read(cx).value();
                    let mut list = div()
                        .id("session-picker-list")
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.sessions_picker_scroll)
                        .flex()
                        .flex_col();
                    for (index, session) in
                        filter_tab_sessions(&self.sessions, &self.open_tabs, &query)
                            .into_iter()
                            .enumerate()
                    {
                        let selected = session.id == self.active;
                        let id = session.id.clone();
                        list = list.child(
                            div()
                                .id(format!("session-choice-{}", session.id))
                                .role(Role::Button)
                                .aria_label(format!("Open session: {}", session.title))
                                .test_support()
                                .track_focus(
                                    self.picker_choice_focus
                                        .get(&format!("session:{}", session.id))
                                        .expect("session choice focus"),
                                )
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .h(px(44.))
                                .w_full()
                                .flex_shrink_0()
                                .px(px(20.))
                                .flex()
                                .items_center()
                                .bg(self.tone(
                                    if self.picker_highlight == Some(index) || selected {
                                        0xf1efec
                                    } else {
                                        0xffffff
                                    },
                                    if self.picker_highlight == Some(index) || selected {
                                        0x27272a
                                    } else {
                                        0x18181b
                                    },
                                ))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_session(id.clone());
                                    this.modal = None;
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .id(format!("session-title-{}", session.id))
                                        .test_support()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .font_weight(FontWeight::BOLD)
                                        .child(session.title.clone()),
                                )
                                .child(
                                    div()
                                        .id(format!("session-path-{}", session.id))
                                        .test_support()
                                        .max_w(px(190.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x85909a, 0xa1a1aa))
                                        .child(session.directory.clone()),
                                ),
                        );
                    }
                    card = card.child(list);
                    let key_chip = |label| {
                        div()
                            .h(px(14.))
                            .px(px(2.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(self.tone(0xded8cb, 0x3f3f46))
                            .bg(self.tone(0xf1efec, 0x27272a))
                            .text_color(self.tone(0x666b70, 0xd4d4d8))
                            .font_family(MONO_FONT_FAMILY)
                            .text_size(px(9.))
                            .child(label)
                    };
                    card = card.child(
                        div()
                            .h(px(28.))
                            .border_t_1()
                            .border_color(self.tone(0xded8cb, 0x27272a))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .text_size(px(11.))
                            .text_color(self.tone(0x87908d, 0xa1a1aa))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .child(key_chip("↑"))
                                            .child(key_chip("↓"))
                                            .child("navigate"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .child(key_chip("↵"))
                                            .child("switch"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(key_chip("esc"))
                                    .child("close"),
                            ),
                    );
                } else {
                    let query = self.search.read(cx).value();
                    let choices = filter_new_session_projects(
                        &self.projects,
                        &self.sessions,
                        self.sessions
                            .iter()
                            .find(|session| session.id == self.active)
                            .map(|session| session.directory.as_str()),
                        &query,
                    );
                    let mut list = div()
                        .id("new-session-picker-list")
                        .test_support()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.projects_picker_scroll)
                        .flex()
                        .flex_col();
                    if choices.is_empty() {
                        list = list.child(
                            div()
                                .my(px(16.))
                                .text_color(self.tone(0x758080, 0x7d8590))
                                .child("No matching projects"),
                        );
                    }
                    for (index, (label, directory)) in choices.into_iter().enumerate() {
                        let highlighted = self.picker_highlight == Some(index)
                            || (self.picker_highlight.is_none() && index == 0);
                        let path = directory.clone();
                        let title_id = format!("project-title-{directory}");
                        let path_id = format!("project-path-{directory}");
                        list = list.child(
                            div()
                                .id(format!("project-choice-{directory}"))
                                .role(Role::Button)
                                .aria_label(format!("Create session in {label}: {directory}"))
                                .test_support()
                                .track_focus(
                                    self.picker_choice_focus
                                        .get(&format!("project:{directory}"))
                                        .expect("project choice focus"),
                                )
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.create_session(directory.clone(), cx);
                                }))
                                .h(px(44.))
                                .w_full()
                                .flex_shrink_0()
                                .px(px(20.))
                                .flex()
                                .items_center()
                                .when(highlighted, |row| row.bg(self.tone(0xf1efec, 0x27272a)))
                                .font_weight(FontWeight::BOLD)
                                .child(
                                    div()
                                        .id(title_id)
                                        .test_support()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .id(path_id)
                                        .test_support()
                                        .max_w(px(120.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::NORMAL)
                                        .text_color(self.tone(0x85909a, 0xa1a1aa))
                                        .child(path),
                                ),
                        );
                    }
                    card = card.child(list);
                }
                card.into_any_element()
            }
            Modal::Rename => div()
                .w(px(349.))
                .h(px(221.))
                .p(px(18.))
                .gap(px(10.))
                .flex()
                .flex_col()
                .rounded(px(11.))
                .border_1()
                .border_color(self.tone(0xc8c3ba, 0x2a3038))
                .bg(self.tone(0xfffdfa, 0x15181c))
                .shadow_lg()
                .child("Session title")
                .child(
                    div()
                        .h(px(34.))
                        .border_1()
                        .border_color(self.tone(0x4f99ee, 0x62bceb))
                        .rounded(px(5.))
                        .child(Input::new(&self.rename).appearance(false)),
                )
                .when_some(self.rename_error.clone(), |card, error| {
                    card.child(
                        div()
                            .text_size(px(11.))
                            .text_color(self.tone(0xa12720, 0xf87171))
                            .child(error),
                    )
                })
                .child("Session ID")
                .child(
                    div()
                        .h(px(38.))
                        .border_1()
                        .border_color(self.tone(0xd3cec5, 0x282c32))
                        .rounded(px(5.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .font_family(MONO_FONT_FAMILY)
                                .child(rename_id.clone()),
                        )
                        .child(
                            div()
                                .id("rename-copy-id")
                                .role(Role::Button)
                                .aria_label(if rename_copied {
                                    "Session ID copied"
                                } else {
                                    "Copy session ID"
                                })
                                .test_support()
                                .track_focus(&self.rename_action_focus[0])
                                .focus_visible(|style| style.bg(self.tone(0xe3e0da, 0x303a44)))
                                .cursor_pointer()
                                .on_key_down(cx.listener(
                                    move |this, event: &KeyDownEvent, _, cx| {
                                        if matches!(event.keystroke.key.as_str(), "enter" | "space")
                                        {
                                            this.copy_with_feedback(
                                                rename_key_feedback.clone(),
                                                "Session ID copied",
                                                rename_key_id.clone(),
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_with_feedback(
                                        rename_feedback_key.clone(),
                                        "Session ID copied",
                                        rename_id.clone(),
                                        cx,
                                    );
                                }))
                                .w(self.scaled(24.))
                                .h(self.scaled(24.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(if rename_copied {
                                    div()
                                        .text_size(self.scaled(15.))
                                        .text_color(self.tone(0x347453, 0x8fcba2))
                                        .child("✓")
                                        .into_any_element()
                                } else {
                                    Icon::default()
                                        .data(include_bytes!("icons/copy.svg"))
                                        .with_size(self.scaled(14.))
                                        .text_color(self.tone(0x454b4b, 0xc9ced2))
                                        .into_any_element()
                                }),
                        ),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("rename-cancel")
                                .role(Role::Button)
                                .aria_label("Cancel rename")
                                .test_support()
                                .track_focus(&self.rename_action_focus[1])
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .cursor_pointer()
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.cancel_rename(cx);
                                        cx.stop_propagation();
                                    }
                                }))
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_rename(cx)))
                                .px(px(14.))
                                .py(px(7.))
                                .border_1()
                                .border_color(self.tone(0xd3cec5, 0x30353a))
                                .rounded(px(5.))
                                .child("Cancel"),
                        )
                        .child(
                            div()
                                .id("rename-save")
                                .role(Role::Button)
                                .aria_label("Save session title")
                                .test_support()
                                .track_focus(&self.rename_action_focus[2])
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .cursor_pointer()
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.rename_session(cx);
                                        cx.stop_propagation();
                                    }
                                }))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_session(cx);
                                }))
                                .px(px(14.))
                                .py(px(7.))
                                .bg(rgb(0xd29b52))
                                .text_color(rgb(0x17130e))
                                .font_weight(FontWeight::BOLD)
                                .border_1()
                                .border_color(self.tone(0x1f5c99, 0x5a4424))
                                .rounded(px(5.))
                                .child("Save"),
                        ),
                )
                .into_any_element(),
            Modal::Settings => {
                let card_width = (window.viewport_size().width - px(32.)).min(px(820.));
                let card_height = (window.viewport_size().height - px(78.)).min(px(674.));
                let rail_width = (card_width * 0.34).min(px(280.));
                let fields = [
                    ("OpenCode server URL", &self.settings.server),
                    ("Username", &self.settings.username),
                    ("Password", &self.settings.password),
                    ("Client ID", &self.settings.client_id),
                    ("Client secret", &self.settings.client_secret),
                ];
                let mut content = div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .px(px(24.))
                    .pt(px(16.))
                    .pb(px(16.))
                    .gap(px(10.))
                    .bg(self.tone(0xfffdfa, 0x15181b));
                for (index, (label, input)) in fields.into_iter().enumerate() {
                    let focused = input.focus_handle(cx).is_focused(window);
                    if index == 3 {
                        content = content.child(
                            div()
                                .mt(px(0.))
                                .pt(px(7.))
                                .border_t_1()
                                .border_color(self.tone(0xded8cb, 0x232932))
                                .child("Cloudflare Access service token"),
                        );
                    }
                    content = content.child(div().child(label)).child(
                        div()
                            .h(px(32.))
                            .min_w_0()
                            .overflow_hidden()
                            .px(px(9.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(
                                if focused { 0x4f99ee } else { 0xd3cec5 },
                                if focused { 0x3b82f6 } else { 0x262c36 },
                            ))
                            .bg(self.tone(0xffffff, 0x2e2e2e))
                            .text_color(self.tone(0x5a6261, 0xf0ede7))
                            .child(Input::new(input).appearance(false)),
                    );
                    if index == 2 {
                        let owner = cx.entity().downgrade();
                        let checked = self.settings.remember_password;
                        content = content.child(
                            Checkbox::new("remember-password")
                                .checked(checked)
                                .accessibility_label("Remember the password in the system keyring")
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(12.))
                                .on_change(move |state, _, _, cx| {
                                    let _ = owner.update(cx, |this, cx| {
                                        this.settings.remember_password = state == CheckboxState::Checked;
                                        cx.notify();
                                    });
                                })
                                .child(
                                    CheckboxIndicator::new()
                                        .checked(checked)
                                        .size(px(14.))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(2.))
                                        .border_1()
                                        .border_color(self.tone(0x9da5a3, 0x899097))
                                        .bg(if checked {
                                            rgb(0x2672c7)
                                        } else {
                                            self.tone(0xffffff, 0x2e2e2e)
                                        })
                                        .text_color(rgb(0xffffff))
                                        .child(if checked { "✓" } else { "" }),
                                )
                                .child("Remember the password in the system keyring"),
                        )
                            .child(div().mt(px(10.)).text_size(px(10.)).text_color(self.tone(0x818984, 0x899097))
                                .child("Remote servers require HTTPS. Loopback HTTP is supported for SSH tunnels. A remembered password is used only for this server URL and username; uncheck Remember to remove it."));
                    }
                }
                content = content.child(
                    div()
                        .text_size(px(10.))
                        .text_color(self.tone(0x818984, 0x899097))
                        .child("The token is sent only to HTTPS servers and stored in the Linux system keyring. Clear the client ID to remove it."),
                );
                if let Some(error) = &self.settings.error {
                    content = content.child(
                        div()
                            .text_color(self.tone(0xa6332b, 0xe68178))
                            .child(error.clone()),
                    );
                }
                if let Some(warning) = &self.settings.warning {
                    content = content.child(
                        div()
                            .text_size(px(11.))
                            .text_color(self.tone(0x995b18, 0xe9ad68))
                            .child(warning.clone()),
                    );
                }
                let content = if self.settings.tab == SettingsTab::Connection {
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id("settings-header")
                                .test_support()
                                .h(px(82.))
                                .flex_shrink_0()
                                .px(px(24.))
                                .pt(px(18.))
                                .pb(px(12.))
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .border_b_1()
                                .border_color(self.tone(0xded8cb, 0x232932))
                                .bg(self.tone(0xfffdfa, 0x15181b))
                                .child(
                                    div()
                                        .font_weight(FontWeight::BOLD)
                                        .text_size(px(16.))
                                        .child("Server Connection"),
                                )
                                .child(
                                    div()
                                        .id("settings-description")
                                        .test_support()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x737c7a, 0x899097))
                                        .child("Configure server endpoint, credentials, and Cloudflare tokens"),
                                ),
                        )
                        .child(content)
                        .into_any_element()
                } else {
                    self.settings_sessions_body(window, cx)
                };
                div()
                    .id("settings-card")
                    .test_support()
                    .w(card_width)
                    .h(card_height)
                    .flex_shrink_0()
                    .flex()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x232930))
                    .bg(self.tone(0xf5f0e7, 0x111418))
                    .shadow_lg()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(rail_width)
                            .flex_shrink_0()
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .px(px(8.))
                            .pt(px(16.))
                            .pb(px(16.))
                            .child(
                                div()
                                    .pl(px(8.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(16.))
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .id("settings-tab-connection")
                                    .role(Role::Tab)
                                    .aria_label("Connection")
                                    .aria_selected(self.settings.tab == SettingsTab::Connection)
                                    .test_support()
                                    .track_focus(&self.settings_tab_focus[0])
                                    .focus_visible(|style| {
                                        style.border_color(self.tone(0x2356a8, 0x78baff))
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, window, cx| {
                                            match event.keystroke.key.as_str() {
                                                "enter" | "space" => this.switch_settings_tab(
                                                    SettingsTab::Connection,
                                                    true,
                                                    window,
                                                    cx,
                                                ),
                                                "down" | "right" => this.switch_settings_tab(
                                                    SettingsTab::Sessions,
                                                    false,
                                                    window,
                                                    cx,
                                                ),
                                                _ => return,
                                            }
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.switch_settings_tab(
                                            SettingsTab::Connection,
                                            true,
                                            window,
                                            cx,
                                        );
                                    }))
                                    .mt(px(7.))
                                    .px(px(9.))
                                    .py(px(5.))
                                    .font_weight(if self.settings.tab == SettingsTab::Connection {
                                        FontWeight::BOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(self.tone(
                                        if self.settings.tab == SettingsTab::Connection {
                                            0x252829
                                        } else {
                                            0x77817e
                                        },
                                        if self.settings.tab == SettingsTab::Connection {
                                            0xe8e5df
                                        } else {
                                            0x899097
                                        },
                                    ))
                                    .bg(self.tone(
                                        if self.settings.tab == SettingsTab::Connection {
                                            0xe3e0da
                                        } else {
                                            0xf5f0e7
                                        },
                                        if self.settings.tab == SettingsTab::Connection {
                                            0x25292e
                                        } else {
                                            0x111418
                                        },
                                    ))
                                    .child("⌁   Connection"),
                            )
                            .child(
                                div()
                                    .id("settings-tab-sessions")
                                    .role(Role::Tab)
                                    .aria_label("Sessions")
                                    .aria_selected(self.settings.tab == SettingsTab::Sessions)
                                    .test_support()
                                    .track_focus(&self.settings_tab_focus[1])
                                    .focus_visible(|style| {
                                        style.border_color(self.tone(0x2356a8, 0x78baff))
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, window, cx| {
                                            match event.keystroke.key.as_str() {
                                                "enter" | "space" => this.switch_settings_tab(
                                                    SettingsTab::Sessions,
                                                    true,
                                                    window,
                                                    cx,
                                                ),
                                                "up" | "left" => this.switch_settings_tab(
                                                    SettingsTab::Connection,
                                                    false,
                                                    window,
                                                    cx,
                                                ),
                                                _ => return,
                                            }
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.switch_settings_tab(
                                            SettingsTab::Sessions,
                                            true,
                                            window,
                                            cx,
                                        );
                                    }))
                                    .mt(px(2.))
                                    .px(px(9.))
                                    .py(px(5.))
                                    .font_weight(if self.settings.tab == SettingsTab::Sessions {
                                        FontWeight::BOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(self.tone(
                                        if self.settings.tab == SettingsTab::Sessions {
                                            0x252829
                                        } else {
                                            0x77817e
                                        },
                                        if self.settings.tab == SettingsTab::Sessions {
                                            0xe8e5df
                                        } else {
                                            0x899097
                                        },
                                    ))
                                    .bg(self.tone(
                                        if self.settings.tab == SettingsTab::Sessions {
                                            0xe3e0da
                                        } else {
                                            0xf5f0e7
                                        },
                                        if self.settings.tab == SettingsTab::Sessions {
                                            0x25292e
                                        } else {
                                            0x111418
                                        },
                                    ))
                                    .child("≡   Sessions"),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .pl(px(6.))
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .text_size(px(10.))
                                    .text_color(self.tone(0x929a9a, 0x6a7482))
                                    .child(
                                        div()
                                            .id("settings-host")
                                            .test_support()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(
                                                url::Url::parse(&self.settings.current.base_url)
                                                    .ok()
                                                    .and_then(|url| {
                                                        url.host_str().map(str::to_owned)
                                                    })
                                                    .unwrap_or_else(|| {
                                                        self.settings.current.base_url.clone()
                                                    }),
                                            ),
                                    )
                                    .child("opencode-gpui v0.1.0"),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(content)
                            .child(
                                div()
                                    .h(px(58.))
                                    .flex_shrink_0()
                                    .px(px(22.))
                                    .flex()
                                    .items_center()
                                    .border_t_1()
                                    .border_color(self.tone(0xded8cb, 0x232932))
                                    .bg(self.tone(0xf9f7f3, 0x14171d))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(11.))
                                            .text_color(self.tone(0x818984, 0x899097))
                                            .child(
                                                if self.settings.tab == SettingsTab::Connection {
                                                    "Secrets stay out of the state file"
                                                } else {
                                                    "↑ ↓ to navigate · Enter to open in tab"
                                                },
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("settings-cancel")
                                            .role(Role::Button)
                                            .aria_label(
                                                if self.settings.tab == SettingsTab::Connection {
                                                    "Cancel settings"
                                                } else {
                                                    "Close settings"
                                                },
                                            )
                                            .test_support()
                                            .track_focus(&self.settings_action_focus[0])
                                            .focus_visible(|style| {
                                                style.border_color(self.tone(0x2356a8, 0x78baff))
                                            })
                                            .cursor_pointer()
                                            .on_key_down(cx.listener(
                                                |this, event: &KeyDownEvent, window, cx| {
                                                    if matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    ) {
                                                        this.cancel_settings(window, cx);
                                                        cx.stop_propagation();
                                                    }
                                                },
                                            ))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.cancel_settings(window, cx);
                                            }))
                                            .px(px(14.))
                                            .py(px(7.))
                                            .mr(px(9.))
                                            .border_1()
                                            .border_color(self.tone(0xd3cec5, 0x30353a))
                                            .rounded(px(5.))
                                            .bg(self.tone(0xf1efec, 0x393939))
                                            .text_color(self.tone(0x252829, 0xf8f7f7))
                                            .child(
                                                if self.settings.tab == SettingsTab::Connection {
                                                    "Cancel"
                                                } else {
                                                    "Close"
                                                },
                                            ),
                                    )
                                    .when(self.settings.tab == SettingsTab::Connection, |footer| {
                                        footer.child(
                                            div()
                                                .id("settings-apply")
                                                .role(Role::Button)
                                                .aria_label("Apply settings")
                                                .test_support()
                                                .track_focus(&self.settings_action_focus[1])
                                                .focus_visible(|style| {
                                                    style
                                                        .border_color(self.tone(0x2356a8, 0x78baff))
                                                })
                                                .cursor_pointer()
                                                .on_key_down(cx.listener(
                                                    |this, event: &KeyDownEvent, window, cx| {
                                                        if matches!(
                                                            event.keystroke.key.as_str(),
                                                            "enter" | "space"
                                                        ) {
                                                            this.apply_settings(window, cx);
                                                            cx.stop_propagation();
                                                        }
                                                    },
                                                ))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.apply_settings(window, cx);
                                                }))
                                                .px(px(14.))
                                                .py(px(7.))
                                                .rounded(px(5.))
                                                .bg(rgb(0xd29b52))
                                                .text_color(rgb(0x17130e))
                                                .font_weight(FontWeight::BOLD)
                                                .child("Apply"),
                                        )
                                    }),
                            ),
                    )
                    .into_any_element()
            }
            Modal::Model | Modal::Level => {
                let model = modal == Modal::Model;
                let query = self.search.read(cx).value();
                let count = if model {
                    filter_models(&self.catalog.models, &query).len()
                } else {
                    let variants = self
                        .selected_model()
                        .as_ref()
                        .and_then(|selection| self.catalog.find(selection))
                        .map(|option| option.variants.as_slice())
                        .unwrap_or_default();
                    filter_levels(variants, &query).len()
                };
                let list = div()
                    .w(picker_panel_width)
                    // The GTK popup's external frame is 2px shorter for
                    // models and 4px taller for levels than the natural list.
                    .h(self
                        .scaled(
                            70. + picker_list_height(model, count) + if model { -2. } else { 4. },
                        )
                        .min(
                            (window.viewport_size().height - self.scaled(46. + 79. + 8.))
                                .max(self.scaled(140.)),
                        ))
                    .relative()
                    .rounded(px(8.))
                    .bg(self.tone(0xfffdfa, 0x191c1f))
                    .shadow_lg()
                    .p(self.scaled(14.))
                    .flex()
                    .flex_col()
                    .gap(self.scaled(8.))
                    .child(
                        div()
                            .h(self.scaled(34.))
                            .w_full()
                            .px(self.scaled(6.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(0x4f99ee, 0x62bceb))
                            .bg(self.tone(0xffffff, 0x181c21))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/search.svg"))
                                    .with_size(self.scaled(16.))
                                    .text_color(self.tone(0x8b918e, 0x71717a)),
                            )
                            .child(Input::new(&self.search).appearance(false)),
                    );
                let mut rows = div()
                    .id("model-level-picker-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.picker_list_scroll)
                    .flex()
                    .flex_col()
                    .gap(self.scaled(if model { 8. } else { 0. }));
                if model {
                    let query = self.search.read(cx).value();
                    let choices = filter_models(&self.catalog.models, &query);
                    if choices.is_empty() {
                        rows = rows.child(div().my(self.scaled(16.)).child("No matching models"));
                    }
                    for (index, option) in choices.into_iter().enumerate() {
                        let selected = self.selected_model().as_ref().is_some_and(|preferred| {
                            preferred.provider_id == option.provider_id
                                && preferred.model_id == option.model_id
                        });
                        let selection = protocol::ModelRef {
                            id: option.model_id.clone(),
                            provider_id: option.provider_id.clone(),
                            variant: None,
                        };
                        rows = rows.child(
                            div()
                                .id(format!(
                                    "pick-model-{}-{}",
                                    option.provider_id, option.model_id
                                ))
                                .role(Role::Button)
                                .aria_label(format!("Choose model: {}", option.label))
                                .test_support()
                                .track_focus(
                                    self.picker_choice_focus
                                        .get(&format!(
                                            "model:{}:{}",
                                            option.provider_id, option.model_id
                                        ))
                                        .expect("model choice focus"),
                                )
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_model(selection.clone(), cx);
                                }))
                                .h(self.scaled(52.))
                                .flex_shrink_0()
                                .px(self.scaled(14.))
                                .rounded(px(5.))
                                .bg(self.tone(
                                    if selected || self.picker_highlight == Some(index) {
                                        0xece9e2
                                    } else {
                                        0xfffdfa
                                    },
                                    if selected || self.picker_highlight == Some(index) {
                                        0x22262b
                                    } else {
                                        0x191c1f
                                    },
                                ))
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .gap(self.scaled(2.))
                                        .child(
                                            div()
                                                .when(selected, |title| {
                                                    title.text_color(self.tone(0x2563eb, 0x62bceb))
                                                })
                                                .child(option.label.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(self.scaled(11.))
                                                .text_color(self.tone(0x777e7d, 0x899097))
                                                .child(format!(
                                                    "{}/{}",
                                                    option.provider_id, option.model_id
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_color(self.tone(0x2563eb, 0x62bceb))
                                        .child(if selected { "✓" } else { "" }),
                                ),
                        );
                    }
                } else {
                    let chosen = self.selected_model();
                    let variants = chosen
                        .as_ref()
                        .and_then(|selection| self.catalog.find(selection))
                        .map(|option| option.variants.clone())
                        .unwrap_or_default();
                    let query = self.search.read(cx).value();
                    let choices = filter_levels(&variants, &query);
                    if choices.is_empty() {
                        rows = rows.child(div().my(self.scaled(14.)).child("No matching levels"));
                    }
                    for (index, variant) in choices.into_iter().enumerate() {
                        let level = variant.as_deref().unwrap_or("Default");
                        let selected = chosen
                            .as_ref()
                            .is_some_and(|selection| selection.variant == variant);
                        let model_ref = chosen.as_ref().map(|selection| protocol::ModelRef {
                            id: selection.model_id.clone(),
                            provider_id: selection.provider_id.clone(),
                            variant: variant.clone(),
                        });
                        rows = rows.child(
                            div()
                                .id(format!("pick-level-{level}"))
                                .role(Role::Button)
                                .aria_label(format!("Choose reasoning level: {level}"))
                                .test_support()
                                .track_focus(
                                    self.picker_choice_focus
                                        .get(&format!("level:{level}"))
                                        .expect("level choice focus"),
                                )
                                .focus_visible(|style| {
                                    style.border_color(self.tone(0x2356a8, 0x78baff))
                                })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(model) = &model_ref {
                                        this.choose_model(model.clone(), cx);
                                    }
                                }))
                                .h(self.scaled(33.))
                                .flex_shrink_0()
                                .px(self.scaled(12.))
                                .flex()
                                .items_center()
                                .bg(self.tone(
                                    if selected || self.picker_highlight == Some(index) {
                                        0x3584e4
                                    } else {
                                        0xfffdfa
                                    },
                                    if selected || self.picker_highlight == Some(index) {
                                        0x15539e
                                    } else {
                                        0x191c1f
                                    },
                                ))
                                .text_color(self.tone(
                                    if selected || self.picker_highlight == Some(index) {
                                        0xffffff
                                    } else {
                                        0x252829
                                    },
                                    if selected || self.picker_highlight == Some(index) {
                                        0xffffff
                                    } else {
                                        0xe8e5df
                                    },
                                ))
                                .child(div().flex_1().child(level.to_owned()))
                                .child(if selected { "✓" } else { "" }),
                        );
                    }
                }
                list.child(rows)
                    .child(
                        div()
                            .id("picker-arrow")
                            .test_support()
                            .absolute()
                            .bottom(self.scaled(-20.))
                            .left(picker_anchor.expect("picker anchor").1)
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/popover-arrow.svg"))
                                    .with_size(self.scaled(20.))
                                    .text_color(self.tone(0xfffdfa, 0x191c1f)),
                            ),
                    )
                    .into_any_element()
            }
        };
        let backdrop = if matches!(modal, Modal::Model | Modal::Level) {
            backdrop
                .items_start()
                .justify_end()
                .pl(picker_anchor.expect("picker anchor").0)
                .pb(self.scaled(79.))
                .bg(rgba(0x00000000))
        } else {
            backdrop
        };
        Some(
            backdrop
                .child(
                    div()
                        .id("modal-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(panel)
                        .focus_trap("client-modal", &self.modal_container_focus),
                )
                .into_any_element(),
        )
    }
}

impl Render for Client {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _latency = latency::global().span(TraceOperation::UiBuild, 1);
        if latency::global().enabled() {
            let viewport = window.viewport_size();
            latency::global().context(latency::Context {
                transcript_rows: self.transcript.get(&self.active).map_or(0, Vec::len),
                // Legacy trace field; natural overflow has no app-owned measurements.
                measured_rows: 0,
                busy_tabs: self
                    .open_tabs
                    .iter()
                    .filter(|id| {
                        self.statuses
                            .get(id.as_str())
                            .is_some_and(RunStatus::is_busy)
                            || self.local_busy.contains(id.as_str())
                    })
                    .count(),
                background_jobs: self.jobs.len(),
                sidebar_visible: self.sidebar_visible,
                window_active: window.is_window_active(),
                window_width: viewport.width.as_f32(),
                window_height: viewport.height.as_f32(),
                display_scale: window.scale_factor(),
                zoom: self.zoom(),
            });
        }
        self.dark = Theme::global(cx).is_dark();
        if !self.sidebar_zoom_initialized || self.sidebar_zoom_width != window.viewport_size().width
        {
            self.reconcile_zoom_sidebar(window);
        }
        window.set_rem_size(self.scaled(16.));
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let floating = matches!(window.window_decorations(), Decorations::Client { tiling } if !tiling.is_tiled())
            && !window.is_maximized()
            && !window.is_fullscreen();
        let controls = window.window_controls();
        let title_offset = if client_decorated {
            TITLE_BAR_HEIGHT
                * (1 + usize::from(controls.minimize) + usize::from(controls.maximize)) as f32
        } else {
            px(0.)
        };
        self.transcript_content_width =
            (self.transcript_width(window) - self.scaled(56.)).max(px(1.));
        self.sync_composer(window, cx);
        if self.focus_composer_pending
            && self.modal.is_none()
            && self.visible_permission().is_none()
        {
            self.focus_composer_pending = false;
            let focus = self.composer.focus_handle(cx);
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
        let composer_focused = self.composer.focus_handle(cx).is_focused(window);
        if composer_focused != self.composer_placeholder_focused {
            self.composer_placeholder_focused = composer_focused;
            self.composer.update(cx, |input, cx| {
                input.set_placeholder(
                    if composer_focused {
                        ""
                    } else {
                        "Ask OpenCode anything…"
                    },
                    window,
                    cx,
                );
            });
        }
        self.tab_focus.retain(|id, _| self.open_tabs.contains(id));
        let form_cancel_id = self
            .forms
            .notice(Some(&self.active), &self.child_parents)
            .and_then(|notice| notice.cancel.map(|target| target.form_id));
        if self.form_cancel_presented != form_cancel_id {
            if self.form_cancel_presented.is_some()
                && self.form_cancel_focus.is_focused(window)
                && self.modal.is_none()
                && self.visible_permission().is_none()
            {
                let focus = self.composer.focus_handle(cx);
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
            self.form_cancel_presented = form_cancel_id;
        }
        for id in &self.open_tabs {
            self.tab_focus
                .entry(id.clone())
                .or_insert_with(|| std::array::from_fn(|_| cx.focus_handle().tab_stop(true)));
        }
        if self.modal == Some(Modal::Settings) && self.settings.tab == SettingsTab::Sessions {
            let query = self.settings.session_search.read(cx).value();
            let ids: Vec<_> = filter_all_sessions(&self.sessions, &query)
                .into_iter()
                .map(|session| session.id.clone())
                .collect();
            self.settings_session_focus.retain(|id, _| ids.contains(id));
            for id in ids {
                self.settings_session_focus
                    .entry(id)
                    .or_insert_with(|| cx.focus_handle().tab_stop(true));
            }
        }
        let picker_focus_keys = self.picker_focus_keys(cx);
        self.picker_choice_focus
            .retain(|key, _| picker_focus_keys.contains(key));
        for key in picker_focus_keys {
            self.picker_choice_focus
                .entry(key)
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
        for action in std::mem::take(&mut self.draft_actions) {
            let (session, pending, restore) = match action {
                DraftAction::Clear { session, pending } => (session, pending, false),
                DraftAction::Restore { session, pending } => (session, pending, true),
                DraftAction::ClearConfirmed { session, failed } => {
                    let composer = if session == self.active {
                        Some(self.composer.clone())
                    } else {
                        self.composers.get(&session).cloned()
                    };
                    if let Some(composer) = composer {
                        let current = composer.read(cx).value().to_string();
                        let unchanged = should_clear_submitted(
                            &current,
                            &failed.restored_text,
                            self.composer_edit_generation
                                .get(&session)
                                .copied()
                                .unwrap_or_default(),
                            failed.edit_generation,
                        );
                        let attachments = if session == self.active {
                            &mut self.attachments_draft
                        } else {
                            self.attachment_drafts.entry(session.clone()).or_default()
                        };
                        if unchanged && attachments.starts_with(&failed.attachments) {
                            let remaining = if failed.text.is_empty() {
                                current.clone()
                            } else {
                                let suffix = current.strip_prefix(&failed.text).unwrap_or(&current);
                                suffix.strip_prefix('\n').unwrap_or(suffix).to_owned()
                            };
                            attachments.drain(..failed.attachments.len());
                            if remaining != current {
                                composer
                                    .update(cx, |input, cx| input.set_value(remaining, window, cx));
                            }
                            for path in failed.attachments {
                                self.cleanup_owned_paste(&path);
                            }
                        } else if has_restored_prompt_prefix(&current, &failed.text)
                            || (!failed.attachments.is_empty()
                                && attachments.starts_with(&failed.attachments))
                        {
                            self.confirmed_prompt_residue.insert(session, failed);
                            self.connection_status =
                                "Earlier prompt delivered; review edited draft to avoid resending it".into();
                        }
                    }
                    continue;
                }
            };
            let composer = if session == self.active {
                Some(self.composer.clone())
            } else {
                self.composers.get(&session).cloned()
            };
            let Some(composer) = composer else { continue };
            let current = composer.read(cx).value().to_string();
            if !restore {
                if should_clear_submitted(
                    &current,
                    &pending.text,
                    self.composer_edit_generation
                        .get(&session)
                        .copied()
                        .unwrap_or_default(),
                    pending.edit_generation,
                ) {
                    composer.update(cx, |input, cx| input.set_value("", window, cx));
                }
                // Submitted files moved out of the session draft at enqueue.
                continue;
            }
            let original_unchanged = should_clear_submitted(
                &current,
                &pending.text,
                self.composer_edit_generation
                    .get(&session)
                    .copied()
                    .unwrap_or_default(),
                pending.edit_generation,
            );
            let restored = restored_failed_text(&current, &pending.text, original_unchanged);
            if restored != current {
                composer.update(cx, |input, cx| {
                    input.set_value(restored.clone(), window, cx);
                });
            }
            let attachments = if session == self.active {
                &mut self.attachments_draft
            } else {
                self.attachment_drafts.entry(session.clone()).or_default()
            };
            let mut restored_files = pending.attachments.clone();
            restored_files.append(attachments);
            *attachments = restored_files;
            self.failed_prompt_drafts.insert(
                session.clone(),
                FailedPromptDraft {
                    message_id: pending.message_id,
                    text: pending.text,
                    restored_text: restored,
                    attachments: pending.attachments,
                    edit_generation: self
                        .composer_edit_generation
                        .get(&session)
                        .copied()
                        .unwrap_or_default(),
                },
            );
        }
        let visible_permission_id = if self.modal.is_none() {
            self.visible_permission().map(|item| item.request.id)
        } else {
            None
        };
        if let Some(id) = visible_permission_id {
            if self.permission_presented.as_deref() != Some(&id) {
                // A second queued request must get a fresh, inert focus target
                // when the first request's focused action disappears.
                self.permission_presented = Some(id);
                self.permission_container_focus.focus(window, cx);
            }
        } else if self.permission_presented.take().is_some() && self.modal.is_none() {
            let focus = self.composer.focus_handle(cx);
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
        let composer_slot = self
            .permission_card(cx)
            .unwrap_or_else(|| self.composer(window, cx));
        self.ensure_jobs_elapsed_refresh(cx);
        let pointer_client = cx.entity().downgrade();
        let root = div()
            .id("app-root")
            .size_full()
            .relative()
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, (), window, _| {
                        latency::global().paint();
                        window.on_mouse_event::<ScrollWheelEvent>(|_, phase, _, cx| {
                            if phase.capture() {
                                trace_input(TraceAction::Scroll, cx);
                            }
                        });
                        let client = pointer_client.clone();
                        window.on_mouse_event::<MouseUpEvent>(move |event, phase, _, cx| {
                            if phase.capture() {
                                trace_input(TraceAction::Pointer, cx);
                                // Window-wide release also covers a thumb released
                                // outside the transcript or over an opaque child.
                                if event.button == MouseButton::Left {
                                    let _ = client.update(cx, |this, cx| {
                                        if this.transcript_pointer_down {
                                            this.transcript_pointer_down = false;
                                            cx.notify();
                                        }
                                    });
                                }
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .capture_key_down(cx.listener(Self::handle_key_down))
            .capture_key_up(cx.listener(Self::handle_key_up))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                let show_numbers = event.modifiers.alt && !event.modifiers.control;
                if this.tab_shortcut_hint != show_numbers {
                    this.tab_shortcut_hint = show_numbers;
                    cx.notify();
                }
            }))
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if this.modal.is_none() && this.scroll.bounds().contains(&event.position) {
                    // Yield to scrollbar dragging and selection before the
                    // next render can issue another follow-bottom request.
                    this.follow_bottom = false;
                    this.transcript_pointer_down = event.button == MouseButton::Left;
                    cx.notify();
                }
                if event.button == MouseButton::Middle
                    && this.modal.is_none()
                    && this.link_menu.is_none()
                    && this.scroll.bounds().contains(&event.position)
                {
                    this.toggle_transcript_autoscroll(event.position, cx);
                    cx.stop_propagation();
                } else if this.autoscroll.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                if let Some(state) = this.autoscroll.as_mut() {
                    state.pointer = event.position;
                }
            }))
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.autoscroll.take().is_some() {
                    cx.notify();
                }
            }))
            .flex()
            .flex_col()
            .font_family(UI_FONT_FAMILY)
            .text_size(rems(13. / 16.))
            .text_color(self.tone(0x252829, 0xe8e5df))
            .when(!floating, |view| view.bg(self.tone(0xf4f1eb, 0x101214)))
            .child(
                TitleBar::new()
                    .h(self.scaled(46.))
                    .pl(px(0.))
                    .when(floating, |bar| {
                        // Keep the close-button hover ink away from the rounded corner.
                        bar.pr(px(FLOATING_CORNER_RADIUS))
                            .rounded_tl(px(FLOATING_CORNER_RADIUS))
                            .rounded_tr(px(FLOATING_CORNER_RADIUS))
                    })
                    .border_color(self.tone(0xc8c3ba, 0x2a2e32))
                    .bg(self.tone(0xf4f1eb, 0x14171a))
                    .child(
                        div()
                            .w_full()
                            .h_full()
                            .relative()
                            .flex()
                            .items_center()
                            // TitleBar renders controls to the right of this content.
                            // Offset by their width so GTK's title widget stays
                            // centered across the whole window at any width.
                            .child(div().w(title_offset).flex_shrink_0())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap(px(10.))
                                            .when(client_decorated, |view| {
                                                view.child(
                                                    div()
                                                        .font_weight(FontWeight::BOLD)
                                                        .child("OpenCode"),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .max_w(px(360.))
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(px(11.))
                                                    .text_color(self.tone(0x77817e, 0x899097))
                                                    .child(self.connection_status.clone()),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .h_full()
                                    .w(self.scaled(if self.sidebar_visible {
                                        SIDEBAR_WIDTH
                                    } else {
                                        48.
                                    }))
                                    .pl(self.scaled(16.))
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .id("sidebar-toggle")
                                            .role(Role::Button)
                                            .aria_label(if self.sidebar_visible {
                                                "Hide sidebar"
                                            } else if !self.can_open_sidebar(window) {
                                                "Expand window to show sidebar"
                                            } else {
                                                "Show sidebar"
                                            })
                                            .test_support()
                                            .track_focus(&self.sidebar_toggle_focus)
                                            .focus_visible(|style| {
                                                style.border_color(self.tone(0x2356a8, 0x78baff))
                                            })
                                            .cursor_pointer()
                                            .on_key_down(cx.listener(
                                                |this, event: &KeyDownEvent, window, cx| {
                                                    if matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    ) {
                                                        this.toggle_sidebar(window, cx);
                                                        cx.stop_propagation();
                                                    }
                                                },
                                            ))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.toggle_sidebar(window, cx)
                                            }))
                                            .child("◫"),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(self.sidebar_visible, |view| {
                        view.child(self.sidebar(floating, cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .bg(self.tone(0xf4f1eb, 0x101214))
                            .when(floating, |view| {
                                let view = view.rounded_br(px(FLOATING_CORNER_RADIUS));
                                if self.sidebar_visible {
                                    view
                                } else {
                                    view.rounded_bl(px(FLOATING_CORNER_RADIUS))
                                }
                            })
                            .when_some(self.session_header(cx), |view, header| view.child(header))
                            .child(self.chat(window, cx))
                            .when_some(self.overlay.as_ref(), |view, overlay| {
                                view.child(overlay.clone())
                            })
                            .when_some(self.transcript_indicator(cx), |view, pill| view.child(pill))
                            .when_some(self.form_notice(cx), |view, notice| view.child(notice))
                            .when_some(self.tray_view(cx), |view, tray| view.child(tray))
                            .when(!self.sidebar_visible, |view| {
                                view.when_some(
                                    self.background_requests.error(&self.active),
                                    |view, error| {
                                        view.child(
                                            div()
                                                .id("background-error-hidden")
                                                .role(Role::Label)
                                                .aria_label(format!("Send to back failed: {error}"))
                                                .mx(px(16.))
                                                .mb(px(6.))
                                                .text_size(px(11.))
                                                .text_color(self.tone(0xb84b45, 0xe79b96))
                                                .child(format!("Send to back failed: {error}")),
                                        )
                                    },
                                )
                            })
                            .when_some(self.resume_warning(cx), |view, warning| {
                                view.child(
                                    div()
                                        .mx(px(16.))
                                        .mb(px(6.))
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x995b18, 0xe9ad68))
                                        .child(warning),
                                )
                            })
                            .child(composer_slot),
                    ),
            )
            .when_some(self.autoscroll_marker(), |view, marker| view.child(marker))
            .when_some(self.modal_view(window, cx), |view, modal| view.child(modal))
            .when_some(self.link_menu_view(window, cx), |view, menu| {
                view.child(menu)
            })
            .when_some(self.copy_feedback.as_ref(), |view, feedback| {
                view.child(
                    div()
                        .id("copy-feedback")
                        .test_support()
                        .role(Role::Label)
                        .aria_label(feedback.label)
                        .absolute()
                        .right(self.scaled(18.))
                        .bottom(self.scaled(18.))
                        .px(self.scaled(12.))
                        .py(self.scaled(7.))
                        .rounded(self.scaled(6.))
                        .bg(self.tone(0x252829, 0xe8e5df))
                        .text_color(self.tone(0xffffff, 0x252829))
                        .text_size(self.scaled(11.))
                        .child(feedback.label),
                )
            });
        client_frame(root.into_any_element(), window, self.dark)
    }
}

fn client_window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("OpenCode".into()),
            ..TitleBar::title_bar_options()
        }),
        window_decorations: Some(WindowDecorations::Client),
        window_background: WindowBackgroundAppearance::Transparent,
        // Keep separate model/level controls and pinned Send/Stop usable at
        // maximum zoom; the height also leaves room for the zoomed picker.
        window_min_size: Some(size(px(480.), px(480.))),
        ..TitleBar::window_options()
    }
}

const FLOATING_CORNER_RADIUS: f32 = 9.;

fn collapsing_slot_height(slot_height: Pixels, progress: f32) -> Pixels {
    px(slot_height.as_f32() * (1. - progress))
}

fn frame_resize_edge(
    position: Point<Pixels>,
    size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let hit = px(8.);
    let left = !tiling.left && position.x <= hit;
    let right = !tiling.right && position.x >= size.width - hit;
    let top = !tiling.top && position.y <= hit;
    let bottom = !tiling.bottom && position.y >= size.height - hit;
    match (top, bottom, left, right) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::TopRight),
        (_, true, true, _) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, true, _, _) => Some(ResizeEdge::Bottom),
        (_, _, true, _) => Some(ResizeEdge::Left),
        (_, _, _, true) => Some(ResizeEdge::Right),
        _ => None,
    }
}

fn client_frame(root: AnyElement, window: &mut Window, dark: bool) -> AnyElement {
    let Decorations::Client { tiling } = window.window_decorations() else {
        return root;
    };
    // A transparent window and independently rounded painted surfaces leave
    // genuine alpha outside the floating frame. WindowBorder's opaque backdrop
    // painted those corners square even when its inner child was rounded.
    window.set_background_appearance(WindowBackgroundAppearance::Transparent);
    window.set_client_inset(px(0.));
    let tiling = if window.is_maximized() || window.is_fullscreen() {
        Tiling::tiled()
    } else {
        tiling
    };
    let floating = !tiling.is_tiled();
    let frame_color = rgb(if dark { 0x34393d } else { 0xb7b2aa });
    let size = window.viewport_size();
    let band = px(8.);
    let mut frame = div()
        .id("client-frame")
        .size_full()
        .relative()
        .when(floating, |frame| frame.rounded(px(FLOATING_CORNER_RADIUS)))
        .border_color(frame_color)
        .when(!tiling.top, |frame| frame.border_t_1())
        .when(!tiling.bottom, |frame| frame.border_b_1())
        .when(!tiling.left, |frame| frame.border_l_1())
        .when(!tiling.right, |frame| frame.border_r_1())
        .capture_any_mouse_down(move |event: &MouseDownEvent, window, cx| {
            if event.button == MouseButton::Left
                && let Some(edge) = frame_resize_edge(event.position, size, tiling)
            {
                window.start_window_resize(edge);
                cx.stop_propagation();
            }
        })
        .child(root);
    // Cursor hitboxes follow the narrow client-edge resize bands. Corner zones
    // are added last so they take precedence where two edge bands overlap.
    let mut zones = Vec::new();
    if !tiling.top {
        zones.push((ResizeEdge::Top, px(0.), px(0.), size.width, band));
    }
    if !tiling.bottom {
        zones.push((
            ResizeEdge::Bottom,
            px(0.),
            size.height - band,
            size.width,
            band,
        ));
    }
    if !tiling.left {
        zones.push((ResizeEdge::Left, px(0.), px(0.), band, size.height));
    }
    if !tiling.right {
        zones.push((
            ResizeEdge::Right,
            size.width - band,
            px(0.),
            band,
            size.height,
        ));
    }
    for (edge, disabled, left, top) in [
        (
            ResizeEdge::TopLeft,
            tiling.top || tiling.left,
            px(0.),
            px(0.),
        ),
        (
            ResizeEdge::TopRight,
            tiling.top || tiling.right,
            size.width - band,
            px(0.),
        ),
        (
            ResizeEdge::BottomLeft,
            tiling.bottom || tiling.left,
            px(0.),
            size.height - band,
        ),
        (
            ResizeEdge::BottomRight,
            tiling.bottom || tiling.right,
            size.width - band,
            size.height - band,
        ),
    ] {
        if !disabled {
            zones.push((edge, left, top, band, band));
        }
    }
    for (edge, left, top, width, height) in zones {
        let cursor = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
            ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        };
        frame = frame.child(
            div()
                .absolute()
                .left(left)
                .top(top)
                .w(width)
                .h(height)
                .cursor(cursor),
        );
    }
    frame.into_any_element()
}

fn follow_system_appearance(window: &mut Window, cx: &mut App) {
    // On Linux the live window appearance can differ from the application
    // default sampled before the window opens.
    Theme::sync_system_appearance(Some(window), cx);
    window
        .observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        })
        .detach();
}

pub fn run(args: Args) {
    application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(fonts::bundled_fonts())
                .expect("register bundled Manrope and Monaspace Neon fonts");
            // The Linux backend can skip an invalid font while returning Ok.
            // Do not silently substitute system typography if registration fails.
            let families = cx.text_system().all_font_names();
            for family in [UI_FONT_FAMILY, MONO_FONT_FAMILY] {
                assert!(
                    families.iter().any(|name| name == family),
                    "missing bundled font: {family}"
                );
            }
            gpui_kit::init(cx);
            let theme_override = match std::env::var("OPENCODE_GPUI_THEME").as_deref() {
                Ok("dark") => Some(ThemeMode::Dark),
                Ok("light") => Some(ThemeMode::Light),
                _ => None,
            };
            if let Some(mode) = theme_override {
                Theme::change(mode, None, cx);
            } else {
                Theme::sync_system_appearance(None, cx);
            }
            Theme::update(cx, |theme| {
                theme.font_family = UI_FONT_FAMILY.into();
                theme.mono_font_family = MONO_FONT_FAMILY.into();
            });
            let (handle, _) = gpui_kit::open_window(client_window_options(), cx, |window, cx| {
                if theme_override.is_none() {
                    follow_system_appearance(window, cx);
                }
                cx.new(|cx| {
                    if args.preview {
                        Client::from_preview(window, cx, args.drawer.clone())
                    } else if args.preview_api {
                        Client::from_api_preview(window, cx)
                    } else {
                        Client::from_live(window, cx, &args)
                    }
                })
            })
            .expect("open GPUI window");
            handle
                .update(cx, |_, window, cx| {
                    gpui_kit::component::use_client_frame(window, cx);
                })
                .expect("configure GPUI client frame");
        });
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        COMPLEX_MARKDOWN_PREVIEW, LONG_ORDERED_LIST_PREVIEW, MarkdownBlock, SESSION_PICKER_LIMIT,
        TabAttention, filter_all_sessions, filter_levels, filter_models,
        filter_new_session_projects, filter_tab_sessions, frame_resize_edge, fuzzy_score,
        markdown_blocks, markdown_view_id, model, needs_new_connection, new_session_choice,
        picker_list_height, reorder_tab_ids, safe_connection_error, splice_transcript,
        sticky_user_index, tab_indicator, tab_number_key, unread_on_server_switch,
        update_session_projection,
    };
    use gpui_kit::{ResizeEdge, Tiling, WindowBackgroundAppearance, point, px, size};
    use opencode_gpui::model::RunStatus;
    use opencode_gpui::persist::PersistedState;
    use opencode_gpui::protocol;
    use serde_json::json;

    #[test]
    fn autoscroll_dead_zone_and_sensitivity_scale_with_zoom() {
        for zoom in [0.7, 1., 1.75] {
            let edge = 16. * zoom;
            for distance in [-edge, -edge / 2., 0., edge / 2., edge] {
                assert_eq!(super::autoscroll_step(px(distance), zoom), px(0.));
            }
            for direction in [-1., 1.] {
                for extra in [0.5 * zoom, 8. * zoom] {
                    let actual =
                        super::autoscroll_step(px(direction * (edge + extra)), zoom).as_f32();
                    let expected = -direction * extra / 8.;
                    assert!((actual - expected).abs() < 0.00001);
                }
            }
        }
    }

    #[test]
    fn autoscroll_is_linear_without_a_speed_floor_or_cap() {
        for (distance, step) in [
            (17., 0.125),
            (24., 1.),
            (80., 8.),
            (416., 50.),
            (816., 100.),
        ] {
            assert_eq!(super::autoscroll_step(px(distance), 1.), px(-step));
            assert_eq!(super::autoscroll_step(px(-distance), 1.), px(step));
        }
    }

    fn snapshot(conversation: &mut model::Conversation, values: Vec<serde_json::Value>) {
        conversation.replace_from_api(
            &values
                .into_iter()
                .map(protocol::SessionMessage::from_value)
                .collect::<Vec<_>>(),
            None,
        );
    }

    fn full_rows(conversation: &model::Conversation) -> Vec<model::TranscriptRow> {
        conversation
            .messages
            .iter()
            .filter(|message| !message.in_tray())
            .flat_map(|message| message.rows())
            .collect()
    }

    #[test]
    fn live_window_uses_custom_client_decorations() {
        let options = super::client_window_options();
        assert_eq!(
            options.window_decorations,
            Some(gpui_kit::WindowDecorations::Client)
        );
        let titlebar = options.titlebar.expect("titlebar options");
        assert_eq!(titlebar.title.as_deref(), Some("OpenCode"));
        assert!(titlebar.appears_transparent);
        assert_eq!(
            options.window_background,
            WindowBackgroundAppearance::Transparent
        );
    }

    #[test]
    fn floating_frame_retains_resize_edges_but_tiled_edges_do_not_resize() {
        let viewport = size(px(780.), px(800.));
        let floating = Tiling::default();
        assert_eq!(
            frame_resize_edge(point(px(2.), px(2.)), viewport, floating),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            frame_resize_edge(point(px(777.), px(400.)), viewport, floating),
            Some(ResizeEdge::Right)
        );
        assert_eq!(
            frame_resize_edge(point(px(7.), px(400.)), viewport, floating),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            frame_resize_edge(point(px(9.), px(400.)), viewport, floating),
            None
        );
        assert_eq!(
            frame_resize_edge(point(px(390.), px(797.)), viewport, floating),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(
            frame_resize_edge(point(px(390.), px(400.)), viewport, floating),
            None
        );
        assert_eq!(
            frame_resize_edge(point(px(2.), px(2.)), viewport, Tiling::tiled()),
            None
        );
    }

    #[test]
    fn streaming_tail_splice_skips_ten_thousand_unchanged_message_spans() {
        let mut conversation = model::Conversation::default();
        snapshot(
            &mut conversation,
            (0..10_000)
                .map(|index| {
                    json!({
                        "id": format!("history_{index}"), "type": "user",
                        "time": { "created": index + 1 }, "text": "Earlier turn"
                    })
                })
                .collect(),
        );
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        let delta = |id: &str, text: &str| {
            json!({
                "id": id, "created": 20000, "type": "session.text.delta",
                "data": { "sessionID": "ses_a", "assistantMessageID": "stream_tail",
                    "ordinal": 0, "delta": text }
            })
        };
        assert!(conversation.apply_event(&delta("evt_00000000000000000000000001", "First")));
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        super::SPAN_MATCHES.with(|count| count.set(0));
        assert!(conversation.apply_event(&delta("evt_00000000000000000000000002", " second")));
        let change = super::splice_transcript_with_tail_hint(
            &conversation,
            &mut rows,
            &mut spans,
            Some("stream_tail"),
        )
        .unwrap();
        assert_eq!(change.range, 10_000..10_001);
        assert_eq!(
            super::SPAN_MATCHES.with(|count| count.get()),
            0,
            "streamed tail tokens must not rescan the history prefix"
        );
        assert_eq!(rows, full_rows(&conversation));

        // A later queued item makes the assistant no longer the tail; fall
        // back to the generic splice rather than silently reusing bad indices.
        assert!(conversation.apply_event(&json!({
            "id": "evt_00000000000000000000000003", "created": 20001,
            "type": "session.inbox.enqueued",
            "data": { "sessionID": "ses_a", "inboxID": "queued",
                "item": { "type": "user", "payload": { "text": "later" },
                    "delivery": "queue" } }
        })));
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert!(conversation.apply_event(&delta("evt_00000000000000000000000004", " third")));
        assert!(
            super::splice_transcript_with_tail_hint(
                &conversation,
                &mut rows,
                &mut spans,
                Some("stream_tail"),
            )
            .is_some()
        );
        assert_eq!(rows, full_rows(&conversation));
    }

    #[test]
    fn spliced_projection_matches_full_rows_for_prepend_stream_snapshot_and_queue() {
        let mut conversation = model::Conversation::default();
        let entry = |id: &str, text: &str| {
            json!({
                "id": id, "type": "user", "time": { "created": 1 }, "text": text
            })
        };
        snapshot(
            &mut conversation,
            vec![entry("middle", "middle"), entry("tail", "tail")],
        );
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        let first = splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(first.range, 0..2);
        assert_eq!(rows, full_rows(&conversation));
        assert!(splice_transcript(&conversation, &mut rows, &mut spans).is_none());

        conversation.prepend_from_api(
            &[protocol::SessionMessage::from_value(entry(
                "earlier", "earlier",
            ))],
            None,
        );
        let prepend = splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(prepend.range, 0..1);
        assert_eq!(rows, full_rows(&conversation));

        let event = |id: &str, kind: &str, data| {
            json!({
                "id": id, "created": 2000, "type": kind, "data": data
            })
        };
        assert!(conversation.apply_event(&event(
            "evt_00000000000000000000000001", "session.text.delta",
            json!({ "sessionID": "ses_a", "assistantMessageID": "assistant", "ordinal": 0, "delta": "hello" }),
        )));
        let stream = splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(stream.range, 3..4);
        assert_eq!(rows, full_rows(&conversation));
        assert!(conversation.apply_event(&event(
            "evt_00000000000000000000000002", "session.inbox.enqueued",
            json!({ "sessionID": "ses_a", "inboxID": "queued", "item": { "type": "user", "payload": { "text": "later" }, "delivery": "queue" } }),
        )));
        assert!(splice_transcript(&conversation, &mut rows, &mut spans).is_some());
        assert_eq!(rows, full_rows(&conversation));
        assert_eq!(spans.last().unwrap().row_count, 0);
        assert!(conversation.apply_event(&event(
            "evt_00000000000000000000000003",
            "session.inbox.delivered",
            json!({ "sessionID": "ses_a", "inboxID": "queued" }),
        )));
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(rows, full_rows(&conversation));

        assert!(conversation.apply_event(&event(
            "evt_00000000000000000000000004",
            "session.text.delta",
            json!({ "sessionID": "ses_a", "assistantMessageID": "assistant", "ordinal": 0, "delta": "!" }),
        )));
        let middle = splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(
            middle.range,
            3..4,
            "the delivered user row stays in the suffix"
        );
        assert_eq!(rows, full_rows(&conversation));

        let old_epoch = conversation.cache_epoch();
        snapshot(
            &mut conversation,
            vec![entry("middle", "replacement"), entry("tail", "tail")],
        );
        assert_ne!(conversation.cache_epoch(), old_epoch);
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(rows, full_rows(&conversation));
        assert_eq!(rows[0].body, "replacement");
    }

    #[test]
    fn streaming_tail_of_tool_heavy_message_keeps_unchanged_row_revisions() {
        let mut conversation = model::Conversation::default();
        let mut content = vec![json!({ "type": "text", "text": "Starting work" })];
        for index in 0..12 {
            content.push(json!({
                "type": "tool", "id": format!("call_{index}"), "name": "glob",
                "time": { "created": 7 },
                "state": { "status": "completed", "input": { "pattern": "*.rs" }, "content": [] }
            }));
        }
        content.push(json!({ "type": "text", "text": "Answer" }));
        snapshot(
            &mut conversation,
            vec![json!({
                "id": "assistant", "type": "assistant", "time": { "created": 1 },
                "agent": "build", "content": content
            })],
        );
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert!(
            rows.len() > 12,
            "tool and text segments must form distinct rows"
        );
        let original = rows.clone();

        assert!(conversation.apply_event(&json!({
            "id": "evt_00000000000000000000000001", "created": 2000,
            "type": "session.text.delta", "data": {
                "sessionID": "ses_a", "assistantMessageID": "assistant",
                "ordinal": 1, "delta": " streamed"
            }
        })));
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(rows.len(), original.len());
        let changed: Vec<_> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| (row.body != original[index].body).then_some(index))
            .collect();
        assert_eq!(changed, vec![rows.len() - 1]);
        for (old, row) in original.iter().zip(&rows).take(rows.len() - 1) {
            assert_eq!(row.key, old.key);
            assert_eq!(row.render_revision(), old.render_revision());
        }
        // An unrelated replacement snapshot with the same IDs must not reuse
        // the previous epoch's retained row revisions.
        snapshot(
            &mut conversation,
            vec![json!({
                "id": "assistant", "type": "assistant", "time": { "created": 1 },
                "agent": "build", "content": [ { "type": "text", "text": "Answer" } ]
            })],
        );
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        assert_eq!(rows, full_rows(&conversation));
    }

    #[test]
    fn image_registration_does_not_decode_before_render() {
        let mut cache = super::ImageCache::default();
        let rows: Vec<_> = (0..10_000)
            .map(|index| {
                let mut row = cache_row(&format!("image_{index}"), "caption");
                row.images = vec!["data:image/png;base64,aGVsbG8=".into()];
                row
            })
            .collect();
        cache.update(
            "ses_a",
            &rows,
            &super::ProjectionChange {
                range: 0..rows.len(),
                removed_images: Vec::new(),
            },
        );
        assert_eq!(cache.entries.len(), 10_000);
        assert_eq!(cache.decodes.get(), 0);
        assert!(cache.get("ses_a", &rows[9999], 0).is_some());
        assert_eq!(cache.decodes.get(), 1);
    }

    #[test]
    fn image_cache_reuses_arc_across_index_shift_and_revision_but_replaces_exact_url() {
        let mut cache = super::ImageCache::default();
        let mut image = cache_row("image", "caption");
        image.images = vec!["data:image/png;base64,aGVsbG8=".into()];
        let mut rows = vec![image.clone()];
        let change = super::ProjectionChange {
            range: 0..1,
            removed_images: Vec::new(),
        };
        cache.update("ses_a", &rows, &change);
        assert_eq!(
            cache.decodes.get(),
            0,
            "projection must not decode offscreen images"
        );
        let original = cache.get("ses_a", &image, 0).unwrap().clone();
        assert_eq!(cache.decodes.get(), 1);
        rows.insert(0, cache_row("earlier", "first"));
        image.render_revision += 1;
        rows[1] = image.clone();
        cache.update(
            "ses_a",
            &rows,
            &super::ProjectionChange {
                range: 1..2,
                removed_images: vec![(image.key.clone(), 0)],
            },
        );
        assert!(std::sync::Arc::ptr_eq(
            &original,
            cache.get("ses_a", &image, 0).unwrap()
        ));
        assert_eq!(cache.decodes.get(), 1);
        image.images[0] = "data:image/png;base64,d29ybGQ=".into();
        rows[1] = image.clone();
        cache.update(
            "ses_a",
            &rows,
            &super::ProjectionChange {
                range: 1..2,
                removed_images: vec![(image.key.clone(), 0)],
            },
        );
        assert!(!std::sync::Arc::ptr_eq(
            &original,
            cache.get("ses_a", &image, 0).unwrap()
        ));
        assert_eq!(cache.decodes.get(), 2);

        cache.update("ses_b", &[image.clone()], &change);
        assert_eq!(cache.decodes.get(), 2);
        assert!(!std::sync::Arc::ptr_eq(
            cache.get("ses_a", &image, 0).unwrap(),
            cache.get("ses_b", &image, 0).unwrap()
        ));
        assert_eq!(cache.decodes.get(), 3);
        cache.remove_session("ses_a");
        assert!(cache.get("ses_a", &image, 0).is_none());
        assert!(cache.get("ses_b", &image, 0).is_some());
    }

    #[test]
    fn image_cache_remembers_invalid_urls_and_cleans_removed_image_slots() {
        let mut cache = super::ImageCache::default();
        let mut row = cache_row("image", "caption");
        row.images = vec![
            "data:image/png;base64,%%%".into(),
            "data:image/png;base64,aGVsbG8=".into(),
        ];
        let change = super::ProjectionChange {
            range: 0..1,
            removed_images: Vec::new(),
        };
        cache.update("ses_a", &[row.clone()], &change);
        cache.update("ses_a", &[row.clone()], &change);
        assert_eq!(cache.decodes.get(), 0);
        assert!(cache.get("ses_a", &row, 0).is_none());
        assert!(cache.get("ses_a", &row, 0).is_none());
        assert_eq!(cache.decodes.get(), 1, "invalid URL is memoized");
        row.images.truncate(1);
        cache.update(
            "ses_a",
            &[row.clone()],
            &super::ProjectionChange {
                range: 0..1,
                removed_images: vec![(row.key.clone(), 0), (row.key.clone(), 1)],
            },
        );
        assert_eq!(cache.entries.len(), 1);
        cache.update(
            "ses_a",
            &[],
            &super::ProjectionChange {
                range: 0..0,
                removed_images: vec![(row.key.clone(), 0)],
            },
        );
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn replacement_snapshot_clears_colliding_image_identity() {
        let mut conversation = model::Conversation::default();
        let entry = |encoded: &str| {
            json!({
                "id": "msg_same", "type": "user", "time": { "created": 1 },
                "files": [{ "data": encoded, "mime": "image/png", "source": { "type": "inline" } }]
            })
        };
        let encoded = "aGVsbG8=";
        snapshot(&mut conversation, vec![entry(encoded)]);
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        let mut images = super::ImageCache::default();
        let _ = update_session_projection(
            "ses_a",
            &conversation,
            &mut rows,
            &mut spans,
            &mut images,
            None,
        );
        assert_eq!(rows, full_rows(&conversation));
        let before = images.get("ses_a", &rows[0], 0).unwrap().clone();
        snapshot(&mut conversation, vec![entry(encoded)]);
        let _ = update_session_projection(
            "ses_a",
            &conversation,
            &mut rows,
            &mut spans,
            &mut images,
            None,
        );
        assert_eq!(rows, full_rows(&conversation));
        assert!(!std::sync::Arc::ptr_eq(
            &before,
            images.get("ses_a", &rows[0], 0).unwrap()
        ));
        assert_eq!(images.entries.len(), 1);
    }

    fn cache_row(id: &str, body: &str) -> model::TranscriptRow {
        model::TranscriptRow {
            key: model::TranscriptRowKey {
                message_id: id.into(),
                slot: model::TranscriptRowSlot::NormalAfter(None),
            },
            render_revision: 0,
            role: model::Role::Assistant,
            body: body.into(),
            images: vec![],
            time: 1,
            kind: model::TranscriptRowKind::Normal,
        }
    }

    #[test]
    fn paged_projection_contains_only_requested_pages_without_a_render_window() {
        let all = opencode_gpui::preview::long_transcript_messages();
        let mut conversation = model::Conversation::default();
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        let mut images = super::ImageCache::default();
        conversation.replace_from_api(&all[1100..], Some("older".into()));
        update_session_projection(
            "session",
            &conversation,
            &mut rows,
            &mut spans,
            &mut images,
            None,
        );
        assert_eq!(rows.len(), 100);
        assert_eq!(rows, full_rows(&conversation));
        assert_eq!(conversation.next_cursor.as_deref(), Some("older"));
        // Only explicit page responses add history; no projection cap is applied.
        conversation.prepend_from_api(&all[600..1100], Some("oldest".into()));
        update_session_projection(
            "session",
            &conversation,
            &mut rows,
            &mut spans,
            &mut images,
            None,
        );
        assert_eq!(rows.len(), 600);
        conversation.prepend_from_api(&all[..600], None);
        update_session_projection(
            "session",
            &conversation,
            &mut rows,
            &mut spans,
            &mut images,
            None,
        );
        assert_eq!(rows.len(), 1200);
        assert_eq!(rows, full_rows(&conversation));
        assert_eq!(conversation.next_cursor, None);
    }

    #[test]
    fn zoom_ladder_bounds_and_reset_match_gtk() {
        assert_eq!(super::stepped_zoom(1.0, 1), 1.1);
        assert_eq!(super::stepped_zoom(1.0, -1), 0.9);
        assert_eq!(super::stepped_zoom(1.0, 0), 1.0);
        assert_eq!(super::stepped_zoom(1.75, 1), 1.75);
        assert_eq!(super::stepped_zoom(0.7, -1), 0.7);
        assert_eq!(super::stepped_zoom(1.37, 1), 1.5);
        assert_eq!(super::stepped_zoom(1.37, -1), 1.3);
        assert_eq!(super::normalized_zoom(f64::NAN), 1.0);
        assert_eq!(super::normalized_zoom(f64::INFINITY), 1.0);
        assert_eq!(super::normalized_zoom(3.0), 1.75);
    }

    #[test]
    fn numbered_tab_shortcuts_only_accept_one_through_nine() {
        assert_eq!(tab_number_key("1"), Some(0));
        assert_eq!(tab_number_key("numpad9"), Some(8));
        assert_eq!(tab_number_key("kp_4"), Some(3));
        assert_eq!(tab_number_key("0"), None);
        assert_eq!(tab_number_key("10"), None);
        assert_eq!(tab_number_key("x"), None);
    }

    #[test]
    fn session_picker_fuzzy_ranks_title_or_directory_and_caps_results() {
        assert!(fuzzy_score("rtrl", "Refactor the retry logic").is_some());
        assert!(fuzzy_score("zqx", "Refactor the retry logic").is_none());
        let mut sessions = Vec::new();
        for index in 0..210 {
            sessions.push(model::Session {
                id: format!("ses_{index}"),
                directory: format!("/workspace/project-{index}"),
                title: format!("Session {index}"),
                time: model::SessionTime {
                    created: 0,
                    updated: 0,
                    archived: None,
                },
                parent_id: None,
                model: None,
            });
        }
        sessions[150].title = "Refactor the retry logic".into();
        let tabs: Vec<_> = sessions.iter().map(|session| session.id.clone()).collect();
        assert_eq!(
            filter_tab_sessions(&sessions, &tabs, "").len(),
            SESSION_PICKER_LIMIT
        );
        assert_eq!(
            filter_tab_sessions(&sessions, &tabs, "rtrl")[0].id,
            "ses_150"
        );
        assert_eq!(
            filter_tab_sessions(&sessions, &tabs, "project-209")[0].id,
            "ses_209"
        );
        assert!(filter_tab_sessions(&sessions, &tabs, "zqx").is_empty());
        let open = vec!["ses_150".to_owned(), "ses_0".to_owned()];
        assert_eq!(
            filter_tab_sessions(&sessions, &open, "")
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ses_150", "ses_0"]
        );
        assert!(filter_tab_sessions(&sessions, &open, "project-209").is_empty());
    }

    #[test]
    fn all_sessions_fuzzy_ranks_by_score_then_recency_with_gtk_limit() {
        let sessions: Vec<_> = (0..205)
            .map(|index| model::Session {
                id: format!("session-{index}"),
                directory: format!("/work/project-{index}"),
                title: if index == 1 || index == 2 {
                    "Alpha session".into()
                } else {
                    format!("Session {index}")
                },
                time: model::SessionTime {
                    created: index as u64,
                    updated: index as u64,
                    archived: None,
                },
                parent_id: None,
                model: None,
            })
            .collect();
        let all = filter_all_sessions(&sessions, "");
        assert_eq!(all.len(), SESSION_PICKER_LIMIT);
        assert_eq!(all[0].id, "session-204");
        assert_eq!(all.last().unwrap().id, "session-5");
        let alpha = filter_all_sessions(&sessions, "alps");
        assert_eq!(
            alpha
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["session-2", "session-1"]
        );
        assert_eq!(
            filter_all_sessions(&sessions, "project-187")[0].id,
            "session-187"
        );
    }

    #[test]
    fn unchanged_connection_does_not_discard_the_live_client() {
        let current = super::ApiConfig {
            base_url: "http://127.0.0.1:4096".into(),
            username: "opencode".into(),
            password: Some("local test".into()),
            cloudflare_access: None,
        };
        assert!(!needs_new_connection(&current, &current, true));
        assert!(needs_new_connection(&current, &current, false));
        let mut changed = current.clone();
        changed.password = Some("another local test".into());
        assert!(needs_new_connection(&current, &changed, true));
        assert_eq!(
            safe_connection_error(Some("HTTP 401: bearer local-test-value")),
            "Authentication failed (401)"
        );
        assert_eq!(
            safe_connection_error(Some("network timed out")),
            "Connection timed out"
        );
        assert_eq!(
            safe_connection_error(Some("server echoed private body")),
            "Connection failed"
        );
    }

    #[test]
    fn canonical_server_key_recovers_raw_api_suffix_tabs_without_deleting_them() {
        let configured = "https://EXAMPLE.com/prefix/api/";
        let key = opencode_gpui::api::server_key(configured).unwrap();
        assert_eq!(key, "https://example.com/prefix");
        let mut state = PersistedState::default();
        let legacy = configured.trim_end_matches('/');
        state.servers.insert(
            legacy.into(),
            opencode_gpui::persist::ServerState {
                tabs: vec![opencode_gpui::persist::PersistedTab {
                    id: "ses_restored".into(),
                    title: "Restored".into(),
                    directory: "/repo".into(),
                }],
                active: Some("ses_restored".into()),
                ..Default::default()
            },
        );
        super::ensure_canonical_server_state(&mut state, configured, &key);
        assert_eq!(state.servers[&key].active.as_deref(), Some("ses_restored"));
        assert!(
            state.servers.contains_key(legacy),
            "legacy state is copied, not moved"
        );
    }

    #[test]
    fn unavailable_saved_model_is_not_relabelled_as_the_first_catalog_option() {
        let catalog = model::ModelCatalog {
            models: vec![model::ModelOption {
                provider_id: "available".into(),
                model_id: "first".into(),
                label: "First model".into(),
                variants: vec![],
                supports_attachments: true,
                context_limit: Some(128_000),
            }],
            preferred: None,
        };
        let missing = model::ModelSelection {
            provider_id: "missing".into(),
            model_id: "saved".into(),
            variant: None,
        };
        assert_eq!(
            super::model_button_presentation(&catalog, Some(&missing)),
            ("saved".into(), None)
        );
        assert_eq!(
            super::model_button_presentation(&catalog, None),
            ("Choose model".into(), None)
        );
    }

    #[test]
    fn composer_model_label_hides_only_the_routing_provider() {
        let selected = model::ModelSelection {
            provider_id: "openrouter".into(),
            model_id: "google/gemini-preview".into(),
            variant: None,
        };
        let catalog = model::ModelCatalog {
            models: vec![model::ModelOption {
                provider_id: selected.provider_id.clone(),
                model_id: selected.model_id.clone(),
                label: "openrouter / Vendor / Gemini Pro Preview".into(),
                variants: vec![],
                supports_attachments: true,
                context_limit: Some(1_000_000),
            }],
            preferred: None,
        };
        assert_eq!(
            super::model_button_presentation(&catalog, Some(&selected)),
            ("Vendor / Gemini Pro Preview".into(), Some(1_000_000))
        );
        assert_eq!(
            catalog.models[0].label,
            "openrouter / Vendor / Gemini Pro Preview"
        );
        assert_eq!(selected.provider_id, "openrouter");
        assert_eq!(selected.model_id, "google/gemini-preview");
    }

    #[test]
    fn switching_servers_restores_only_that_servers_unread_marks() {
        let mut persisted = PersistedState::default();
        let a = "https://a.example.com";
        let b = "https://b.example.com";
        let a_unread = HashSet::from(["ses_a".to_owned()]);
        let b_unread = HashSet::from(["ses_b".to_owned()]);

        let mut current = unread_on_server_switch(&mut persisted, Some((a, &a_unread)), b);
        assert!(current.is_empty(), "A's unread mark leaked into B");
        current.insert("ses_b".into());
        let restored_a = unread_on_server_switch(&mut persisted, Some((b, &current)), a);
        assert_eq!(restored_a, a_unread);
        let restored_b = unread_on_server_switch(
            &mut persisted,
            Some(("https://a.example.com/", &restored_a)),
            b,
        );
        assert_eq!(restored_b, b_unread);

        // A failed initial connection must not overwrite the old server's
        // saved marks with an empty, uninitialized in-memory set.
        assert_eq!(unread_on_server_switch(&mut persisted, None, a), a_unread);
    }

    #[test]
    fn model_and_level_filters_follow_gtk_fuzzy_ranking() {
        assert_eq!(picker_list_height(true, 0), 100.);
        assert_eq!(picker_list_height(true, 2), 112.);
        assert_eq!(picker_list_height(true, 100), 280.);
        assert_eq!(picker_list_height(false, 0), 80.);
        assert_eq!(picker_list_height(false, 4), 132.);
        assert_eq!(picker_list_height(false, 100), 240.);
        let models = vec![
            model::ModelOption {
                provider_id: "openai".into(),
                model_id: "gpt-5.6".into(),
                label: "GPT-5.6".into(),
                variants: vec!["medium".into(), "high".into()],
                supports_attachments: true,
                context_limit: Some(200_000),
            },
            model::ModelOption {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet-4.6".into(),
                label: "Claude Sonnet 4.6".into(),
                variants: vec![],
                supports_attachments: true,
                context_limit: Some(200_000),
            },
        ];
        assert_eq!(
            filter_models(&models, "cst")[0].model_id,
            "claude-sonnet-4.6"
        );
        assert_eq!(
            filter_models(&models, "anthropic / claude")[0].provider_id,
            "anthropic"
        );
        assert!(filter_models(&models, "zqx").is_empty());
        let levels = filter_levels(&models[0].variants, "hgh");
        assert_eq!(levels, vec![Some("high".into())]);
        assert_eq!(
            filter_levels(&models[0].variants, ""),
            vec![None, Some("medium".into()), Some("high".into())]
        );
    }

    #[test]
    fn new_session_picker_includes_session_paths_and_custom_directory() {
        let projects = vec![
            model::Project {
                worktree: "/code/alpha".into(),
                name: Some("Named Alpha".into()),
            },
            model::Project {
                worktree: "/code/bee".into(),
                name: None,
            },
        ];
        let sessions = vec![
            model::Session {
                id: "active".into(),
                directory: "/code/zeta".into(),
                title: "Zeta".into(),
                time: model::SessionTime {
                    created: 0,
                    updated: 0,
                    archived: None,
                },
                parent_id: None,
                model: None,
            },
            model::Session {
                id: "other".into(),
                directory: "/code/bee".into(),
                title: "Bee".into(),
                time: model::SessionTime {
                    created: 0,
                    updated: 0,
                    archived: None,
                },
                parent_id: None,
                model: None,
            },
        ];
        let all = filter_new_session_projects(&projects, &sessions, Some("/code/zeta"), "");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], ("zeta".into(), "/code/zeta".into()));
        assert_eq!(
            filter_new_session_projects(&projects, &sessions, None, "zt")[0].1,
            "/code/zeta"
        );
        assert_eq!(new_session_choice(&all, 99, ""), Some(all[0].1.clone()));
        assert_eq!(
            new_session_choice(&[], 0, " /custom/project "),
            Some("/custom/project".into())
        );
        assert_eq!(new_session_choice(&[], 0, "  "), None);
    }

    #[test]
    fn tab_reorder_inserts_before_or_after_without_losing_tabs() {
        let mut tabs = vec!["a".into(), "b".into(), "c".into()];
        assert!(reorder_tab_ids(&mut tabs, "a", "c", true));
        assert_eq!(tabs, ["b", "c", "a"]);
        assert!(reorder_tab_ids(&mut tabs, "a", "b", false));
        assert_eq!(tabs, ["a", "b", "c"]);
        assert!(!reorder_tab_ids(&mut tabs, "a", "b", false));
        assert!(!reorder_tab_ids(&mut tabs, "missing", "b", true));
        assert!(!reorder_tab_ids(&mut tabs, "a", "missing", true));
        assert_eq!(tabs, ["a", "b", "c"]);
    }

    #[test]
    fn background_activity_and_attention_have_gtk_precedence() {
        use TabAttention::{Busy, Read, Unread};
        assert_eq!(tab_indicator(false, false, false), (false, Read));
        assert_eq!(tab_indicator(false, true, false), (false, Unread));
        assert_eq!(tab_indicator(false, false, true), (true, Read));
        assert_eq!(tab_indicator(false, true, true), (true, Unread));
        assert_eq!(tab_indicator(true, false, false), (true, Busy));
        assert_eq!(tab_indicator(true, true, false), (true, Busy));
        assert_eq!(tab_indicator(true, false, true), (true, Busy));
        assert_eq!(tab_indicator(true, true, true), (true, Busy));
    }

    #[test]
    fn draft_text_transitions_preserve_new_input() {
        assert!(!super::has_restored_prompt_prefix("high priority", "hi"));
        assert!(super::has_restored_prompt_prefix("hi\nnext", "hi"));
        assert!(!super::should_clear_submitted("high priority", "hi", 1, 1));
        assert!(!super::should_clear_submitted("hi", "hi", 2, 1));
        assert!(super::should_clear_submitted("hi", "hi", 1, 1));
        assert_eq!(
            super::restored_failed_text("new", "sent", false),
            "sent\nnew"
        );
        assert_eq!(
            super::restored_failed_text("sent again", "sent", false),
            "sent\nsent again"
        );
        assert_eq!(super::restored_failed_text("sent", "sent", true), "sent");
    }

    #[test]
    fn sticky_uses_native_child_index_without_row_geometry() {
        let mut rows: Vec<_> = (0..5)
            .map(|index| cache_row(&index.to_string(), "body"))
            .collect();
        rows[0].role = model::Role::User;
        rows[3].role = model::Role::User;
        assert_eq!(sticky_user_index(&rows, 0, false, true), None);
        assert_eq!(sticky_user_index(&rows, 0, false, false), Some(0));
        assert_eq!(sticky_user_index(&rows, 2, false, true), Some(0));
        assert_eq!(sticky_user_index(&rows, 3, false, true), None);
        assert_eq!(sticky_user_index(&rows, 3, false, false), Some(3));
        assert_eq!(sticky_user_index(&rows, 4, false, true), Some(3));
        // The load-earlier control is an immediate child, but not a message.
        assert_eq!(sticky_user_index(&rows, 0, true, false), None);
        assert_eq!(sticky_user_index(&rows, 1, true, true), None);
        assert_eq!(sticky_user_index(&rows, 3, true, true), Some(0));
        assert_eq!(sticky_user_index(&rows, 5, true, true), Some(3));
        assert_eq!(sticky_user_index(&[], 0, false, true), None);
        assert_eq!(sticky_user_index(&rows, 50, false, true), None);
    }

    #[test]
    fn heading_list_and_fenced_code_keep_their_content() {
        assert_eq!(
            markdown_blocks(
                "# Padding\n\nDraw it at **22px**.\n\n- inner\n- outer\n\n```rust\npaperclip_icon(22)\n```"
            ),
            vec![
                MarkdownBlock::Heading(1, "Padding".into()),
                MarkdownBlock::Paragraph("Draw it at **22px**.".into()),
                MarkdownBlock::List(vec!["inner".into(), "outer".into()]),
                MarkdownBlock::Code("rust".into(), "paperclip_icon(22)".into()),
            ]
        );
    }

    #[test]
    fn complex_markdown_preserves_structure_and_copy_code() {
        let source = "> A quoted line\n\n1. Ordered item\n   - Nested item\n\n| Name | Value |\n| --- | --- |\n| clip | 22px |\n\n```rust\nlet x = 22;\n```\n\n- [x] done";
        let blocks = markdown_blocks(source);
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, quote_depth: 1, .. } if content == "A quoted line")));
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, marker: Some(marker), list_depth: 1, .. } if content == "Ordered item" && marker == "1.")));
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, marker: Some(marker), list_depth: 2, .. } if content == "Nested item" && marker == "•")));
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Table { header, rows } if header == &["Name", "Value"] && rows == &[vec!["clip", "22px"]])));
        assert!(blocks.contains(&MarkdownBlock::Code("rust".into(), "let x = 22;".into())));
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, .. } if content == "[x] done")));
        assert!(markdown_blocks(COMPLEX_MARKDOWN_PREVIEW)
            .iter()
            .any(|block| matches!(block, MarkdownBlock::Code(language, code) if language == "rust" && code.contains("paperclip_icon"))));
        assert!(
            markdown_blocks("   ```rust\nlet x = 22;\n   ```")
                .contains(&MarkdownBlock::Code("rust".into(), "let x = 22;".into()))
        );
        assert!(
            markdown_blocks("~~~sh\necho ok\n~~~")
                .contains(&MarkdownBlock::Code("sh".into(), "echo ok".into()))
        );
        assert!(markdown_blocks("- parent\n\n      code line").iter().any(
            |block| matches!(block, MarkdownBlock::NestedCode { content, list_depth: 1, .. } if content.contains("code line"))
        ));
        let nested = markdown_blocks(
            "> ```rust\n> let x = 22;\n> ```\n\n![clip](https://example.com/clip.png)",
        );
        assert!(nested.iter().any(|block| matches!(block, MarkdownBlock::NestedCode { language, content, quote_depth: 1, .. } if language == "rust" && content == "let x = 22;")));
        assert!(nested.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, .. } if content == "Image: clip")));
        assert!(markdown_blocks("> \\*literal\\* and **strong**")
            .iter()
            .any(|block| matches!(block, MarkdownBlock::Structured { content, quote_depth: 1, .. } if content == "\\*literal\\* and **strong**")));
        assert!(markdown_blocks("> &amp;copy;").iter().any(
            |block| matches!(block, MarkdownBlock::Structured { content, .. } if content == "&amp;copy;")
        ));
        assert!(markdown_blocks("> <b>bold</b>").iter().any(
            |block| matches!(block, MarkdownBlock::Structured { content, .. } if content == "<b>bold</b>")
        ));
        assert_eq!(
            super::escape_markdown_literal("*literal* [bracket]"),
            "\\*literal\\* \\[bracket\\]"
        );
        assert!(super::heading_is_block_marker("- item"));
        assert!(super::heading_is_block_marker("# Heading"));
        assert!(!super::heading_is_block_marker("**Important**"));
    }

    #[test]
    fn linked_ordered_list_items_have_distinct_stable_markdown_views() {
        let row = cache_row("long_list", LONG_ORDERED_LIST_PREVIEW);
        let blocks = markdown_blocks(LONG_ORDERED_LIST_PREVIEW);
        let list_items = blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                if let MarkdownBlock::Structured {
                    marker: Some(marker),
                    content,
                    ..
                } = block
                {
                    Some((index, marker, content))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(list_items.len(), 4);
        let ids = list_items
            .iter()
            .map(|(index, _, _)| markdown_view_id("ses_one", &row.key, *index, 0))
            .collect::<HashSet<_>>();
        assert_eq!(
            ids.len(),
            list_items.len(),
            "each list item needs its own parser state"
        );
        assert!(list_items.iter().all(|(_, _, content)| !content.is_empty()));
        assert_ne!(
            markdown_view_id("ses_one", &row.key, list_items[0].0, 0),
            markdown_view_id("ses_two", &row.key, list_items[0].0, 0)
        );
    }

    #[test]
    fn message_timestamps_use_local_zone_and_hide_unset_values() {
        let eastern = jiff::tz::TimeZone::fixed(jiff::tz::offset(-5));
        assert_eq!(super::timestamp_in_zone(0, eastern.clone()), "");
        assert_eq!(super::timestamp_in_zone(500, eastern.clone()), "");
        assert_eq!(
            super::timestamp_in_zone(1_704_067_200, eastern.clone()),
            "2023-12-31 19:00"
        );
        assert_eq!(
            super::timestamp_in_zone(1_704_067_200_000, eastern),
            "2023-12-31 19:00"
        );
    }

    #[test]
    fn offline_busy_reconciles_only_against_known_or_complete_statuses() {
        let mut offline_busy = HashSet::from([
            "still-running".into(),
            "completed".into(),
            "not-covered".into(),
        ]);
        let statuses = std::collections::HashMap::from([
            ("still-running".into(), RunStatus::Busy),
            ("completed".into(), RunStatus::Idle),
        ]);
        let tabs = vec![
            "still-running".into(),
            "completed".into(),
            "not-covered".into(),
        ];
        let mut unread = HashSet::new();
        super::apply_missed_idle(&mut offline_busy, &statuses, &tabs, &mut unread, false);
        assert_eq!(unread, HashSet::from(["completed".into()]));
        assert_eq!(
            offline_busy,
            HashSet::from(["still-running".into(), "not-covered".into()])
        );
        super::apply_missed_idle(&mut offline_busy, &statuses, &tabs, &mut unread, true);
        assert_eq!(
            unread,
            HashSet::from(["completed".into(), "not-covered".into()])
        );
        assert_eq!(offline_busy, HashSet::from(["still-running".into()]));
    }

    #[test]
    fn transcript_images_preserve_aspect_ratio_without_exceeding_viewport() {
        assert_eq!(super::image_display_size(200, 120, 420.), (200., 120.));
        assert_eq!(super::image_display_size(4_000, 2_000, 420.), (420., 210.));
        assert_eq!(super::image_display_size(2_000, 4_000, 420.), (360., 720.));
        assert_eq!(super::image_display_size(4_000, 2_000, 640.), (640., 320.));
        assert_eq!(super::image_display_size(4_000, 2_000, 180.), (180., 90.));
        assert_eq!(super::image_display_size(0, 0, 420.), (200., 120.));
    }
}
