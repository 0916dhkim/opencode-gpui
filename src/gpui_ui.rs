//! GPUI Kit shell backed by the v2 transport, or its deterministic preview fixture.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use gpui_kit::base::{Button as BaseButton, Checkbox, CheckboxIndicator, CheckboxState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::text::markdown;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{Icon, Sizable, VirtualListScrollHandle, v_virtual_list};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use opencode_gpui::{
    api::{ApiConfig, ApiHandle, Command, InboxRequest, MessageLoadError, ServerEnvelope, UiEvent},
    credentials::{self, CloudflareAccessCredentials, PasswordTarget, SystemKeyring},
    jobs::{self, JobKind, JobRow, Jobs},
    model::{
        self, Conversation, ModelCatalog, ModelSelection, Project, RunStatus, Session,
        SessionModel, TranscriptRow, TranscriptRowKey, TranscriptRowKind,
    },
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

fn model_button_presentation(
    catalog: &ModelCatalog,
    selected: Option<&ModelSelection>,
) -> (String, Option<u64>) {
    if catalog.models.is_empty() {
        return ("No models available".into(), None);
    }
    match selected {
        Some(selection) => catalog.find(selection).map_or_else(
            || {
                (
                    format!("{}/{}", selection.provider_id, selection.model_id),
                    None,
                )
            },
            |option| (option.label.clone(), option.context_limit),
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

fn filter_tab_sessions<'a>(sessions: &'a [Session], query: &str) -> Vec<&'a Session> {
    let query = query.trim();
    let mut scored: Vec<_> = sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
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

struct Client {
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
    connection_status: String,
    disconnected: bool,
    sse_connected_once: bool,
    server_version: Option<String>,
    conversations: HashMap<String, Conversation>,
    loading_messages: HashMap<String, Option<String>>,
    /// Locally accepted IDs while an older newest-history request was in
    /// flight. Its stale response cannot prove those sends are absent.
    skip_prune_for_load: HashMap<String, HashSet<String>>,
    message_events_during_load: HashMap<String, Vec<protocol::Event>>,
    reload_after_load: HashSet<String>,
    preserve_scroll: Option<TranscriptAnchor>,
    follow_bottom: Option<(String, Point<Pixels>)>,
    catalogs: HashMap<String, ModelCatalog>,
    model_retry_count: HashMap<String, u8>,
    model_retry_scheduled: HashSet<String>,
    running_jobs: Jobs,
    sessions: Vec<Session>,
    open_tabs: Vec<String>,
    tab_focus: HashMap<String, [FocusHandle; 3]>,
    tab_shortcut_hint: bool,
    tab_drop_target: Option<(String, bool)>,
    bootstrapped: bool,
    projects: Vec<Project>,
    active: String,
    transcript: HashMap<String, Vec<TranscriptRow>>,
    row_heights: HashMap<String, Rc<RefCell<RowHeightCache>>>,
    virtual_scrolls: HashMap<String, VirtualListScrollHandle>,
    #[cfg(test)]
    measurement_probe: Option<(Pixels, Rc<RefCell<Vec<Pixels>>>)>,
    #[cfg(test)]
    rendered_rows: Rc<RefCell<Vec<usize>>>,
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
    scroll: VirtualListScrollHandle,
    safe_scroll: HashMap<String, (RowLayoutStamp, Point<Pixels>)>,
    safe_anchors: HashMap<String, (RowLayoutStamp, TranscriptAnchor)>,
    pending_jump: Option<PendingTranscriptJump>,
    history_focus: FocusHandle,
    sessions_picker_scroll: ScrollHandle,
    picker_list_scroll: ScrollHandle,
    projects_picker_scroll: ScrollHandle,
    picker_choice_focus: HashMap<String, FocusHandle>,
    unread: HashSet<String>,
    statuses: HashMap<String, RunStatus>,
    jobs: Vec<JobRow>,
    forms: Forms,
    form_cancel_focus: FocusHandle,
    form_cancel_presented: Option<String>,
    permissions: Vec<PendingPermission>,
    permission_in_flight: HashSet<String>,
    permission_container_focus: FocusHandle,
    permission_focus: [FocusHandle; 3],
    permission_presented: Option<String>,
    settings_tab_focus: [FocusHandle; 2],
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
    rename_target: Option<String>,
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

// Increment this when the row's typography or layout rules change. The other
// parts of the stamp follow the live width, theme and conversation snapshot.
const TRANSCRIPT_ROW_STYLE_REVISION: u64 = 2;
const TRANSCRIPT_MEASURE_BATCH: usize = 64;
const TRANSCRIPT_PROGRESSIVE_MIN_ROWS: usize = 256;
const COMPLEX_MARKDOWN_PREVIEW: &str = "> A quoted explanation of the clip alignment.\n\n| Part | Size |\n| --- | ---: |\n| paperclip | 22px |\n| send | 22px |\n\n1. Keep the inner wire visible\n   - Check at both theme settings\n2. Match the composer actions\n\n- [x] Measure the icon\n- [ ] Verify the layout\n\n```rust\npaperclip_icon(COMPOSER_ICON_PX)\n```";
const SIDEBAR_WIDTH: f32 = 270.;
const TRANSCRIPT_SCROLLBAR_GUTTER: f32 = 14.;

#[derive(Clone, Copy, Debug, PartialEq)]
struct RowLayoutStamp {
    width: Pixels,
    dark: bool,
    style_revision: u64,
    epoch: u64,
}

#[derive(Clone, Copy)]
struct CachedRowHeight {
    revision: u64,
    height: Pixels,
}

#[derive(Clone)]
struct TranscriptAnchor {
    session: String,
    key: TranscriptRowKey,
    within: Pixels,
    offset: Point<Pixels>,
}

#[derive(Clone)]
struct PendingTranscriptJump {
    stamp: RowLayoutStamp,
    anchor: TranscriptAnchor,
}

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
    old_rows: Vec<TranscriptRow>,
    old_images: HashMap<(TranscriptRowKey, usize), Arc<Image>>,
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
    // tail edit must not invalidate every already measured tool/reasoning row
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
    let old_rows = rows.splice(start..end, replacements).collect();
    spans.splice(prefix..spans.len() - suffix, new_spans);
    Some(ProjectionChange {
        range: start..new_end,
        removed_images,
        old_rows,
        old_images: HashMap::new(),
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

    /// A replaced offscreen row must not decode an old image merely to keep
    /// a stale snapshot; only already painted images need that retention.
    fn peek(&self, session: &str, row: &TranscriptRow, index: usize) -> Option<&Arc<Image>> {
        self.entries
            .get(&(session.to_owned(), row.key.clone(), index))
            .filter(|entry| row.images.get(index) == Some(&entry.url))
            .and_then(|entry| entry.image.get().and_then(Option::as_ref))
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
    change.map(|mut change| {
        for row in &change.old_rows {
            for index in 0..row.images.len() {
                if let Some(image) = images.peek(session, row, index) {
                    change
                        .old_images
                        .insert((row.key.clone(), index), image.clone());
                }
            }
        }
        images.update(session, rows, &change);
        change
    })
}

#[derive(Default)]
struct RowHeightCache {
    stamp: Option<RowLayoutStamp>,
    entries: HashMap<TranscriptRowKey, CachedRowHeight>,
    /// Only replaced measured rows, retained until the next exact layout.
    stale: HashMap<TranscriptRowKey, StaleMeasuredRow>,
    load_height: Option<(bool, Pixels)>,
    #[cfg(test)]
    layouts: usize,
}

#[derive(Clone)]
struct StaleMeasuredRow {
    row: TranscriptRow,
    height: Pixels,
    images: Vec<Option<Arc<Image>>>,
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

impl RowHeightCache {
    fn prepare_stamp(&mut self, stamp: RowLayoutStamp) {
        if self.stamp != Some(stamp) {
            self.entries.clear();
            self.stale.clear();
            self.load_height = None;
            self.stamp = Some(stamp);
        }
    }

    fn missing(&mut self, stamp: RowLayoutStamp, rows: &[TranscriptRow]) -> Vec<usize> {
        self.prepare_stamp(stamp);
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| self.height(row).is_none().then_some(index))
            .collect()
    }

    /// Choose a bounded exact-layout batch nearest the first-visit tail.
    /// This planner does not change what is painted until the virtual list can
    /// safely admit partially measured ranges without blank visible slots.
    fn missing_tail(
        &mut self,
        stamp: RowLayoutStamp,
        rows: &[TranscriptRow],
        budget: usize,
        target_height: Pixels,
    ) -> Vec<usize> {
        self.prepare_stamp(stamp);
        let mut indices = Vec::with_capacity(budget);
        let mut exact_height = px(0.);
        for (index, row) in rows.iter().enumerate().rev() {
            if let Some(height) = self.height(row) {
                exact_height += height;
                if exact_height >= target_height {
                    break;
                }
            } else {
                indices.push(index);
                if indices.len() == budget {
                    break;
                }
            }
        }
        indices.reverse();
        indices
    }

    fn missing_range(
        &mut self,
        stamp: RowLayoutStamp,
        rows: &[TranscriptRow],
        range: std::ops::Range<usize>,
        budget: usize,
    ) -> Vec<usize> {
        self.prepare_stamp(stamp);
        rows.iter()
            .enumerate()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .filter_map(|(index, row)| self.height(row).is_none().then_some(index))
            .take(budget)
            .collect()
    }

    fn exact_tail(&self, rows: &[TranscriptRow]) -> (usize, Pixels) {
        let mut count = 0;
        let mut height = px(0.);
        for row in rows.iter().rev() {
            let Some(exact) = self.height(row) else { break };
            count += 1;
            height += exact;
        }
        (count, height)
    }

    fn provisional_prefix(&self, rows: &[TranscriptRow], index: usize, has_load: bool) -> Pixels {
        let mut top = if has_load {
            self.load_height.map_or(px(48.), |(_, height)| height)
        } else {
            px(0.)
        };
        for row in rows.iter().take(index) {
            top += self
                .entries
                .get(&row.key)
                .map_or(px(64.), |entry| entry.height);
        }
        top
    }

    fn provisional_viewport(
        &self,
        rows: &[TranscriptRow],
        has_load: bool,
        offset: Point<Pixels>,
        viewport_height: Pixels,
    ) -> std::ops::Range<usize> {
        let first_row = usize::from(has_load);
        let visible_top = (-offset.y).max(px(0.));
        let visible_bottom = visible_top + viewport_height;
        let mut top = px(0.);
        let mut start = None;
        let mut end = first_row;
        if has_load {
            let height = self.load_height.map_or(px(48.), |(_, height)| height);
            if height > visible_top {
                start = Some(0);
            }
            top += height;
        }
        for (index, row) in rows.iter().enumerate() {
            let height = self
                .entries
                .get(&row.key)
                .map_or(px(64.), |entry| entry.height);
            let item = index + first_row;
            if top + height > visible_top && start.is_none() {
                start = Some(item);
            }
            if top < visible_bottom {
                end = item + 1;
            }
            top += height;
            if top >= visible_bottom && start.is_some() {
                break;
            }
        }
        let length = rows.len() + first_row;
        let start = start.unwrap_or(length.saturating_sub(1));
        start..end.max(start + 1).min(length)
    }

    fn viewport_ready(
        &self,
        rows: &[TranscriptRow],
        has_load: bool,
        loading: bool,
        range: std::ops::Range<usize>,
    ) -> bool {
        range.into_iter().all(|index| {
            if has_load && index == 0 {
                self.load_height
                    .is_some_and(|(was_loading, _)| was_loading == loading)
            } else {
                let row = &rows[index - usize::from(has_load)];
                self.height(row).is_some()
                    || self.stale.get(&row.key).is_some_and(|old| {
                        self.entries.get(&row.key).is_some_and(|entry| {
                            entry.revision == old.row.render_revision()
                                && entry.height == old.height
                        })
                    })
            }
        })
    }

    fn record(
        &mut self,
        stamp: RowLayoutStamp,
        key: TranscriptRowKey,
        revision: u64,
        height: Pixels,
    ) {
        // A superseding render can replace the snapshot before this layout runs.
        if self.stamp == Some(stamp) {
            self.stale.remove(&key);
            self.entries
                .insert(key, CachedRowHeight { revision, height });
            #[cfg(test)]
            {
                self.layouts += 1;
            }
        }
    }

    fn height(&self, row: &TranscriptRow) -> Option<Pixels> {
        self.entries
            .get(&row.key)
            .and_then(|entry| (entry.revision == row.render_revision()).then_some(entry.height))
    }

    fn retain_replaced(&mut self, rows: &[TranscriptRow], change: &ProjectionChange) {
        // The key is semantic, not the row index: prepends may shift the slot.
        // Keep the first still-measured revision through rapid token updates.
        // Only the spliced message range can replace or remove a stale row.
        // Building a map of all history on every streamed token costs O(history).
        let changed_keys: HashSet<_> = change.old_rows.iter().map(|row| &row.key).collect();
        let current: HashMap<_, _> = rows[change.range.clone()]
            .iter()
            .map(|row| (&row.key, row))
            .collect();
        self.stale.retain(|key, old| {
            !changed_keys.contains(key)
                || current
                    .get(key)
                    .is_some_and(|row| row.render_revision() != old.row.render_revision())
        });
        for row in &change.old_rows {
            let Some(new) = current.get(&row.key) else {
                continue;
            };
            if new.render_revision() == row.render_revision() || self.stale.contains_key(&row.key) {
                continue;
            }
            let Some(entry) = self.entries.get(&row.key).copied() else {
                continue;
            };
            if entry.revision != row.render_revision() {
                continue;
            }
            self.stale.insert(
                row.key.clone(),
                StaleMeasuredRow {
                    row: row.clone(),
                    height: entry.height,
                    images: (0..row.images.len())
                        .map(|index| change.old_images.get(&(row.key.clone(), index)).cloned())
                        .collect(),
                },
            );
        }
    }

    fn prefix(&self, rows: &[TranscriptRow], index: usize, has_load: bool) -> Option<Pixels> {
        let mut top = if has_load {
            self.load_height?.1
        } else {
            px(0.)
        };
        for row in rows.iter().take(index) {
            top += self.height(row)?;
        }
        Some(top)
    }

    fn sizes(
        &self,
        rows: &[TranscriptRow],
        has_load: bool,
        width: Pixels,
    ) -> Option<Rc<Vec<Size<Pixels>>>> {
        let mut sizes = Vec::with_capacity(rows.len() + usize::from(has_load));
        if has_load {
            sizes.push(size(width, self.load_height?.1));
        }
        for row in rows {
            sizes.push(size(width, self.height(row)?));
        }
        Some(Rc::new(sizes))
    }

    /// A new-row miss is an empty placeholder for one layout frame. A changed
    /// row can instead paint its last measured snapshot at this exact height;
    /// the unmeasured revision never enters a definite-height virtual slot.
    fn provisional_sizes(
        &self,
        rows: &[TranscriptRow],
        has_load: bool,
        width: Pixels,
    ) -> Rc<Vec<Size<Pixels>>> {
        let mut sizes = Vec::with_capacity(rows.len() + usize::from(has_load));
        if has_load {
            sizes.push(size(
                width,
                self.load_height.map_or(px(48.), |(_, height)| height),
            ));
        }
        for row in rows {
            let height = self
                .entries
                .get(&row.key)
                .map_or(px(64.), |entry| entry.height);
            sizes.push(size(width, height));
        }
        Rc::new(sizes)
    }

    fn provisional_positions(&self, rows: &[TranscriptRow], has_load: bool) -> Vec<Pixels> {
        let mut top = if has_load {
            self.load_height.map_or(px(48.), |(_, height)| height)
        } else {
            px(0.)
        };
        rows.iter()
            .map(|row| {
                let origin = top;
                top += self
                    .entries
                    .get(&row.key)
                    .map_or(px(64.), |entry| entry.height);
                origin
            })
            .collect()
    }
}

/// Detached rows are laid out at a definite width during GPUI's layout phase,
/// never prepainted, painted, or inserted into the transcript scroll area.
/// `layout_as_root` panics if called from the view's render method instead.
struct TranscriptMeasurementProbe {
    rows: Vec<(TranscriptRowKey, u64, AnyElement)>,
    load: Option<(bool, AnyElement)>,
    stamp: RowLayoutStamp,
    cache: Rc<RefCell<RowHeightCache>>,
    #[cfg(test)]
    heights: Option<Rc<RefCell<Vec<Pixels>>>>,
    spacer: Div,
}

impl IntoElement for TranscriptMeasurementProbe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TranscriptMeasurementProbe {
    type RequestLayoutState = <Div as Element>::RequestLayoutState;
    type PrepaintState = <Div as Element>::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some((loading, control)) = &mut self.load {
            let height = control
                .layout_as_root(
                    size(
                        AvailableSpace::Definite(self.stamp.width),
                        AvailableSpace::MinContent,
                    ),
                    window,
                    cx,
                )
                .height;
            let mut cache = self.cache.borrow_mut();
            if cache.stamp == Some(self.stamp) {
                cache.load_height = Some((*loading, height));
            }
        }
        let measured: Vec<_> = self
            .rows
            .iter_mut()
            .map(|(key, revision, row)| {
                let height = row
                    .layout_as_root(
                        size(
                            AvailableSpace::Definite(self.stamp.width),
                            AvailableSpace::MinContent,
                        ),
                        window,
                        cx,
                    )
                    .height;
                (key.clone(), *revision, height)
            })
            .collect();
        #[cfg(test)]
        if let Some(heights) = &self.heights {
            *heights.borrow_mut() = measured.iter().map(|(_, _, height)| *height).collect();
        }
        let mut cache = self.cache.borrow_mut();
        for (key, revision, height) in measured {
            cache.record(self.stamp, key, revision, height);
        }
        // Re-render once the provisional virtual slots have exact heights.
        #[cfg(test)]
        if self.heights.is_none() {
            window.refresh();
        }
        #[cfg(not(test))]
        window.refresh();
        self.spacer
            .request_layout(global_id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.spacer
            .prepaint(global_id, inspector_id, bounds, layout, window, cx)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        paint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.spacer
            .paint(global_id, inspector_id, bounds, layout, paint, window, cx);
    }
}

fn timestamp(time: u64) -> String {
    jiff::Timestamp::from_millisecond(time as i64)
        .map(|timestamp| {
            timestamp
                .to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn sticky_user_index(
    users: impl IntoIterator<Item = (usize, Pixels, Pixels)>,
    viewport_top: Pixels,
    viewport_bottom: Pixels,
) -> Option<usize> {
    let mut last_past_top = None;
    let mut flush_with_top = false;
    for (index, top, bottom) in users {
        if top < viewport_top - px(1.) {
            last_past_top = Some(index);
        } else if top <= viewport_top + px(1.) && bottom <= viewport_bottom + px(1.) {
            flush_with_top = true;
        }
    }
    if flush_with_top { None } else { last_past_top }
}

fn permission_detail(text: String, dark: bool) -> AnyElement {
    div()
        .p(px(8.))
        .rounded(px(5.))
        .bg(rgb(if dark { 0x15191c } else { 0xefede8 }))
        .text_color(rgb(if dark { 0xd2cec7 } else { 0x34312d }))
        .font_family("DejaVu Sans Mono")
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
        .font_family("DejaVu Sans Mono")
        .text_size(px(11.))
        .child(text)
        .into_any_element()
}

fn markdown_blocks(source: &str) -> Vec<MarkdownBlock> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut list_depth = 0;
    let complex = source
        .lines()
        .any(|line| line.trim_start().starts_with("~~~"))
        || Parser::new_ext(source, options).any(|event| match event {
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
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
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
                if checked { "☑ " } else { "☐ " },
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
    let (metadata, encoded) = url.strip_prefix("data:")?.split_once(',')?;
    let format = match metadata {
        "image/png;base64" => ImageFormat::Png,
        "image/jpeg;base64" => ImageFormat::Jpeg,
        "image/webp;base64" => ImageFormat::Webp,
        "image/gif;base64" => ImageFormat::Gif,
        _ => return None,
    };
    // Avoid decoding an unbounded server-supplied data URL on the UI thread.
    if encoded.len() > 14_000_000 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

impl Client {
    // Keep GTK light/dark design-token pairs at each call site. Distinct roles
    // can share a light value (notably white text and white inputs) but differ
    // in dark mode, so do not translate colors by looking up their light hex.
    fn tone(&self, light: u32, dark: u32) -> Rgba {
        rgb(if self.dark { dark } else { light })
    }

    fn markdown_code_block(
        &self,
        language: String,
        code: String,
        row_index: usize,
        block_index: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let copy = code.clone();
        div()
            .w_full()
            .rounded(px(7.))
            .border_1()
            .border_color(self.tone(0xd2cdc5, 0x30353a))
            .bg(self.tone(0xf7f5f1, 0x171a1d))
            .overflow_hidden()
            .child(
                div()
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .bg(self.tone(0xece9e2, 0x14171a))
                    .border_b_1()
                    .border_color(self.tone(0xd2cdc5, 0x282c30))
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.tone(0x777e7d, 0x899198))
                    .child(language)
                    .child(div().flex_1())
                    .child(
                        BaseButton::new(format!("copy-code-{row_index}-{block_index}"))
                            .accessibility_label("Copy code")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            }))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/copy.svg"))
                                    .with_size(px(12.))
                                    .text_color(self.tone(0x777e7d, 0x899198)),
                            ),
                    ),
            )
            .child(
                div()
                    .min_h(px(65.))
                    .p(px(12.))
                    .font_family("monospace")
                    .text_size(px(12.))
                    .child(code),
            )
            .into_any_element()
    }

    fn markdown_body(&self, source: &str, row_index: usize, cx: &Context<Self>) -> AnyElement {
        let mut content = div().w_full().flex().flex_col().gap(px(10.));
        for (block_index, block) in markdown_blocks(source).into_iter().enumerate() {
            let element: AnyElement = match block {
                MarkdownBlock::Heading(level, title) => div()
                    .text_size(px(match level {
                        1 => 18.,
                        2 => 16.,
                        _ => 14.,
                    }))
                    .font_weight(FontWeight::BOLD)
                    .child(title)
                    .into_any_element(),
                MarkdownBlock::Paragraph(text) => div()
                    .line_height(px(22.))
                    .child(markdown(text))
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
                                .w(px(24.))
                                .flex_shrink_0()
                                .text_color(self.tone(0x777e7d, 0xaeb4b9))
                                .child(marker),
                        );
                    }
                    line = line.child(div().flex_1().min_w_0().child(markdown(content)));
                    let mut block = div()
                        .w_full()
                        .pl(px(16. * list_depth.saturating_sub(1) as f32))
                        .when(heading.is_none(), |view| view.line_height(px(22.)))
                        .when_some(heading, |view, level| {
                            view.text_size(px(if level == 1 {
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
                            .pl(px(10. + 12. * (quote_depth - 1) as f32))
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
                                    .child(markdown(cell)),
                            );
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
                    let mut list = div().flex().flex_col().gap(px(10.));
                    for item in items {
                        list = list.child(
                            div()
                                .flex()
                                .line_height(px(22.))
                                .gap(px(9.))
                                .child(
                                    div()
                                        .w(px(17.))
                                        .text_right()
                                        .text_color(self.tone(0x777e7d, 0xaeb4b9))
                                        .child("•"),
                                )
                                .child(markdown(item)),
                        );
                    }
                    list.into_any_element()
                }
                MarkdownBlock::Code(language, code) => {
                    self.markdown_code_block(language, code, row_index, block_index, cx)
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
                            .pl(px(10. + 12. * (quote_depth - 1) as f32));
                    }
                    wrapper
                        .child(self.markdown_code_block(
                            language,
                            content,
                            row_index,
                            block_index,
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
        let password = credentials::initial_password(
            &SystemKeyring,
            &server,
            &username,
            args.password.clone(),
            state.connection.basic_auth_in_keyring,
            state.connection.basic_auth_in_keyring,
        );
        let cloudflare_access = match (&args.cf_access_client_id, &args.cf_access_client_secret) {
            (Some(id), Some(secret)) => {
                CloudflareAccessCredentials::new(id.clone(), secret.clone()).map(Some)
            }
            (None, None) if state.connection.cloudflare_access => credentials::load(&server),
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
            Ok(credentials) => credentials,
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
        let scroll = VirtualListScrollHandle::new();
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
            connection_status: "Connecting".into(),
            disconnected: false,
            sse_connected_once: false,
            server_version: None,
            conversations: HashMap::new(),
            loading_messages: HashMap::new(),
            skip_prune_for_load: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            preserve_scroll: None,
            follow_bottom: None,
            catalogs: HashMap::new(),
            model_retry_count: HashMap::new(),
            model_retry_scheduled: HashSet::new(),
            running_jobs: Jobs::default(),
            sessions: Vec::new(),
            open_tabs: Vec::new(),
            tab_focus: HashMap::new(),
            tab_shortcut_hint: false,
            tab_drop_target: None,
            bootstrapped: false,
            projects: Vec::new(),
            active: String::new(),
            transcript: HashMap::new(),
            transcript_spans: HashMap::new(),
            row_heights: HashMap::new(),
            virtual_scrolls: HashMap::new(),
            #[cfg(test)]
            measurement_probe: None,
            #[cfg(test)]
            rendered_rows: Rc::new(RefCell::new(Vec::new())),
            attachments: ImageCache::default(),
            catalog: ModelCatalog::default(),
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
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
            scroll,
            safe_scroll: HashMap::new(),
            safe_anchors: HashMap::new(),
            pending_jump: None,
            history_focus: cx.focus_handle().tab_stop(true),
            sessions_picker_scroll: ScrollHandle::new(),
            picker_list_scroll: ScrollHandle::new(),
            projects_picker_scroll: ScrollHandle::new(),
            picker_choice_focus: HashMap::new(),
            unread: HashSet::new(),
            statuses: HashMap::new(),
            jobs: Vec::new(),
            forms: Forms::default(),
            form_cancel_focus: cx.focus_handle().tab_stop(true),
            form_cancel_presented: None,
            permissions: Vec::new(),
            permission_in_flight: HashSet::new(),
            permission_container_focus: cx.focus_handle().tab_stop(true),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: None,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
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
            rename_target: None,
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
        match ApiHandle::start(config) {
            Ok((api, receiver, key)) => {
                let saved = state.servers.get(&key);
                client.saved_active = saved.and_then(|saved| saved.active.clone());
                client.open_tabs = saved
                    .map(|saved| saved.tabs.iter().map(|tab| tab.id.clone()).collect())
                    .unwrap_or_default();
                client.bootstrap_directories = saved
                    .map(|saved| saved.tabs.iter().map(|tab| tab.directory.clone()).collect())
                    .unwrap_or_default();
                client.unread = saved.map(|saved| saved.unread.clone()).unwrap_or_default();
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
            let first_visit = !self.virtual_scrolls.contains_key(&id);
            self.scroll = self
                .virtual_scrolls
                .entry(id.clone())
                .or_insert_with(VirtualListScrollHandle::new)
                .clone();
            self.preserve_scroll = None;
            self.pending_jump = None;
            self.follow_bottom = first_visit.then(|| (id.clone(), self.scroll.offset()));
            self.active = id.clone();
            // First visits pin after exact heights arrive. Returning to a
            // previously viewed tab retains its own scroll offset.
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
        server.tabs = self
            .open_tabs
            .iter()
            .filter_map(|id| self.sessions.iter().find(|session| &session.id == id))
            .map(|session| PersistedTab {
                id: session.id.clone(),
                title: session.title.clone(),
                directory: session.directory.clone(),
            })
            .collect();
        server.active = (!self.active.is_empty()).then(|| self.active.clone());
        server.unread = self.unread.clone();
        if self.api.is_none() || self.preview_api {
            return;
        }
        if let Err(error) = self.settings.persisted.save(&persist::default_path()) {
            self.connection_status = format!("State save failed: {error}");
        }
    }

    fn drop_tab(&mut self, source: &str, target: &str, cx: &mut Context<Self>) {
        let Some((destination, after)) = self.tab_drop_target.take() else {
            return;
        };
        if self.tab_shortcut_hint || destination != target {
            cx.notify();
            return;
        }
        if reorder_tab_ids(&mut self.open_tabs, source, target, after) {
            self.persist_tabs();
        }
        cx.notify();
    }

    fn update_tab_status(&mut self, id: String, status: RunStatus) {
        let was_busy = self.statuses.get(&id).is_some_and(RunStatus::is_busy);
        if was_busy
            && !status.is_busy()
            && self.open_tabs.contains(&id)
            && self.unread.insert(id.clone())
        {
            self.persist_tabs();
        }
        if status.is_busy() {
            if self.local_prompt_delivered(&id) {
                self.local_busy.remove(&id);
            }
        } else if !self.local_busy.contains(&id) && !self.pending_prompts.contains_key(&id) {
            self.deferred_abort.remove(&id);
            self.abort_timeout_tokens.remove(&id);
            self.local_run_message_ids.remove(&id);
        }
        self.statuses.insert(id.clone(), status);
        self.maybe_dispatch_deferred_abort(&id);
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
        let index = self.open_tabs.iter().position(|tab| tab == id);
        self.open_tabs.retain(|tab| tab != id);
        self.tab_focus.remove(id);
        self.conversations.remove(id);
        self.loading_messages.remove(id);
        self.skip_prune_for_load.remove(id);
        self.message_events_during_load.remove(id);
        self.reload_after_load.remove(id);
        self.transcript.remove(id);
        self.transcript_spans.remove(id);
        self.attachments.remove_session(id);
        self.row_heights.remove(id);
        self.virtual_scrolls.remove(id);
        self.unread.remove(id);
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
                    .auto_grow(2, 8)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            });
        } else if let Some(existing) = self.composers.get(&self.active) {
            self.composer = existing.clone();
        } else {
            let composer = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            });
            let session = self.active.clone();
            cx.subscribe(
                &composer,
                move |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
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
        }
        cx.notify();
    }

    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_api {
            self.settings.reset_draft(window, cx);
            self.modal = None;
            cx.notify();
            return;
        }
        match self.apply_settings_inner(cx) {
            Ok(()) => {
                self.settings.reset_draft(window, cx);
                self.modal = None;
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
        self.open_tabs = persisted
            .servers
            .get(&key)
            .map(|saved| saved.tabs.iter().map(|tab| tab.id.clone()).collect())
            .unwrap_or_default();
        self.tab_drop_target = None;
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
        self.row_heights.clear();
        self.virtual_scrolls.clear();
        self.scroll = VirtualListScrollHandle::new();
        self.safe_scroll.clear();
        self.safe_anchors.clear();
        self.pending_jump = None;
        self.conversations.clear();
        self.loading_messages.clear();
        self.skip_prune_for_load.clear();
        self.message_events_during_load.clear();
        self.reload_after_load.clear();
        self.preserve_scroll = None;
        self.follow_bottom = None;
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

    fn modal_choice_count(&self, cx: &Context<Self>) -> usize {
        let query = self.search.read(cx).value().to_lowercase();
        match self.modal {
            Some(Modal::Sessions) => filter_tab_sessions(&self.sessions, &query).len(),
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
                let id = filter_tab_sessions(&self.sessions, &query)
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
            // GTK consumes Escape while the permission prompt replaces the
            // composer; it must not acknowledge the active tab's unread mark.
            if self.visible_permission().is_some() {
                cx.stop_propagation();
                return;
            }
            if self.unread.remove(&self.active) {
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
                            (self.settings_highlight.unwrap_or_default() + 1)
                                .min(choices.len() - 1),
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
                Some(Modal::Sessions) => filter_tab_sessions(&self.sessions, &query)
                    .into_iter()
                    .find(|session| {
                        self.picker_choice_focus
                            .get(&format!("session:{}", session.id))
                            .is_some_and(|focus| focus.is_focused(window))
                    })
                    .map(|session| (false, session.id.clone())),
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
            let current = self.picker_highlight.unwrap_or_default();
            self.picker_highlight = Some(if key == "up" {
                current.saturating_sub(1)
            } else {
                (current + 1).min(len - 1)
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
        if matches!(
            event.keystroke.key.to_ascii_lowercase().as_str(),
            "alt" | "alt_l" | "alt_r"
        ) && self.tab_shortcut_hint
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
        self.prepare_follow_bottom(session_id);
        let Some(conversation) = self.conversations.get(session_id) else {
            return;
        };
        let spans = self
            .transcript_spans
            .entry(session_id.to_owned())
            .or_default();
        let rows = self.transcript.entry(session_id.to_owned()).or_default();
        if let Some(change) = update_session_projection(
            session_id,
            conversation,
            rows,
            spans,
            &mut self.attachments,
            tail_delta,
        ) && let Some(cache) = self.row_heights.get(session_id)
        {
            let mut cache = cache.borrow_mut();
            if cache
                .stamp
                .is_some_and(|stamp| stamp.epoch == conversation.cache_epoch())
            {
                cache.retain_replaced(rows, &change);
            } else {
                cache.stale.clear();
            }
        }
    }

    fn prepare_follow_bottom(&mut self, session_id: &str) {
        if session_id == self.active
            && self.preserve_scroll.is_none()
            && self.transcript.contains_key(session_id)
        {
            let offset = self.scroll.offset();
            if offset.y + self.scroll.max_offset().y <= px(16.) {
                self.follow_bottom = Some((session_id.to_owned(), offset));
            } else {
                self.follow_bottom = None;
            }
        }
    }

    fn transcript_anchor(&self) -> Option<TranscriptAnchor> {
        let rows = self.transcript.get(&self.active)?;
        let offset = self.scroll.offset();
        let has_load = self
            .conversations
            .get(&self.active)
            .is_some_and(|conversation| conversation.next_cursor.is_some());
        let cache = self.row_heights.get(&self.active)?.borrow();
        let mut top = cache.provisional_prefix(rows, 0, has_load);
        let visible_top = -offset.y;
        for row in rows {
            let bottom = top
                + cache
                    .entries
                    .get(&row.key)
                    .map_or(px(64.), |entry| entry.height);
            if bottom > visible_top {
                return Some(TranscriptAnchor {
                    session: self.active.clone(),
                    key: row.key.clone(),
                    // A viewport in the load-earlier control anchors the
                    // first message at its existing on-screen position.
                    within: visible_top - top,
                    offset,
                });
            }
            top = bottom;
        }
        None
    }

    /// The Kit decides the visible range in prepaint. Before it reaches that
    /// callback, hold the last exact viewport while measuring an unmeasured
    /// jump; otherwise the callback would paint empty provisional slots.
    fn guard_unmeasured_scroll(&mut self, window: &Window) {
        let Some(rows) = self.transcript.get(&self.active) else {
            return;
        };
        if rows.len() <= TRANSCRIPT_PROGRESSIVE_MIN_ROWS {
            return;
        }
        if let Some(anchor) = &self.preserve_scroll
            && anchor.session == self.active
        {
            if self.scroll.offset() == anchor.offset && rows.iter().any(|row| row.key == anchor.key)
            {
                return;
            }
            // A user movement or deletion of the anchored row supersedes it.
            self.preserve_scroll = None;
        }
        let width = window.viewport_size().width - px(SIDEBAR_WIDTH + TRANSCRIPT_SCROLLBAR_GUTTER);
        let stamp = RowLayoutStamp {
            width,
            dark: self.dark,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: self
                .conversations
                .get(&self.active)
                .map_or(0, Conversation::cache_epoch),
        };
        let has_load = self
            .conversations
            .get(&self.active)
            .is_some_and(|conversation| conversation.next_cursor.is_some());
        let loading = self.loading_messages.contains_key(&self.active);
        let cache = self
            .row_heights
            .entry(self.active.clone())
            .or_default()
            .clone();
        let cache = cache.borrow();
        if cache.stamp != Some(stamp) {
            self.safe_scroll.remove(&self.active);
            self.safe_anchors.remove(&self.active);
            if self
                .follow_bottom
                .as_ref()
                .is_some_and(|(session, offset)| {
                    session == &self.active && *offset == self.scroll.offset()
                })
            {
                self.pending_jump = None;
                return;
            }
            // Keep a scrolled-up row stable across a resize/theme/snapshot
            // invalidation while only its neighborhood is remeasured.
            let current = self.scroll.offset();
            let visible_top = (-current.y).max(px(0.));
            let sizes = cache.provisional_sizes(rows, has_load, width);
            let mut top = px(0.);
            let mut row_index = rows.len() - 1;
            for (index, item) in sizes.iter().enumerate() {
                if top + item.height > visible_top {
                    row_index = index
                        .saturating_sub(usize::from(has_load))
                        .min(rows.len() - 1);
                    break;
                }
                top += item.height;
            }
            let within = visible_top - cache.provisional_prefix(rows, row_index, has_load);
            let estimated_top = px(row_index as f32 * 64. + if has_load { 48. } else { 0. });
            let parked = point(current.x, -(estimated_top + within));
            self.pending_jump = Some(PendingTranscriptJump {
                stamp,
                anchor: TranscriptAnchor {
                    session: self.active.clone(),
                    key: rows[row_index].key.clone(),
                    within,
                    offset: parked,
                },
            });
            self.scroll.set_offset(parked);
            return;
        }
        let current = self.scroll.offset();
        if cache.sizes(rows, has_load, width).is_some() {
            self.safe_scroll
                .insert(self.active.clone(), (stamp, current));
            self.pending_jump = None;
            return;
        }
        let viewport_height = if self.scroll.bounds().size.height > px(0.) {
            self.scroll.bounds().size.height
        } else {
            (window.viewport_size().height - px(220.)).max(px(160.))
        };
        let safe = self
            .safe_scroll
            .get(&self.active)
            .filter(|(saved_stamp, _)| *saved_stamp == stamp)
            .map(|(_, offset)| *offset);
        if let Some(jump) = &self.pending_jump
            && jump.stamp == stamp
            && jump.anchor.session == self.active
            && (safe == Some(current) || (safe.is_none() && current == jump.anchor.offset))
        {
            if let Some(index) = rows.iter().position(|row| row.key == jump.anchor.key) {
                if cache.height(&rows[index]).is_none() {
                    return; // Do not use a 64px estimate for a tall anchor.
                }
                let top = cache.provisional_prefix(rows, index, has_load);
                let target = point(current.x, -(top + jump.anchor.within));
                let range = cache.provisional_viewport(rows, has_load, target, viewport_height);
                if cache.viewport_ready(rows, has_load, loading, range) {
                    self.scroll.set_offset(target);
                    self.safe_scroll
                        .insert(self.active.clone(), (stamp, target));
                    self.pending_jump = None;
                }
                return;
            }
            self.pending_jump = None; // The anchored row was deleted.
        }
        let range = cache.provisional_viewport(rows, has_load, current, viewport_height);
        if cache.viewport_ready(rows, has_load, loading, range) {
            if safe != Some(current) {
                // A second user movement into a measured region supersedes the
                // earlier deferred jump; never resume it on a later frame.
                self.pending_jump = None;
            }
            self.safe_scroll
                .insert(self.active.clone(), (stamp, current));
            return;
        }
        if safe == Some(current)
            || (safe.is_none()
                && self
                    .follow_bottom
                    .as_ref()
                    .is_some_and(|(session, offset)| session == &self.active && *offset == current))
        {
            return;
        }
        let visible_top = (-current.y).max(px(0.));
        let sizes = cache.provisional_sizes(rows, has_load, width);
        let mut top = px(0.);
        let mut row_index = rows.len() - 1;
        for (index, item) in sizes.iter().enumerate() {
            if top + item.height > visible_top {
                row_index = index
                    .saturating_sub(usize::from(has_load))
                    .min(rows.len() - 1);
                break;
            }
            top += item.height;
        }
        let row_top = cache.provisional_prefix(rows, row_index, has_load);
        self.pending_jump = Some(PendingTranscriptJump {
            stamp,
            anchor: TranscriptAnchor {
                session: self.active.clone(),
                key: rows[row_index].key.clone(),
                within: visible_top - row_top,
                offset: current,
            },
        });
        self.follow_bottom = None;
        if let Some(safe) = safe {
            self.scroll.set_offset(safe);
        }
    }

    fn correct_scroll(&mut self, window: &mut Window) {
        let Some(rows) = self.transcript.get(&self.active) else {
            return;
        };
        let width = window.viewport_size().width - px(SIDEBAR_WIDTH + TRANSCRIPT_SCROLLBAR_GUTTER);
        let stamp = RowLayoutStamp {
            width,
            dark: self.dark,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: self
                .conversations
                .get(&self.active)
                .map_or(0, Conversation::cache_epoch),
        };
        let has_load = self
            .conversations
            .get(&self.active)
            .is_some_and(|conversation| conversation.next_cursor.is_some());
        let Some(cache) = self.row_heights.get(&self.active) else {
            return;
        };
        let cache = cache.borrow();
        if cache.stamp != Some(stamp) {
            return;
        }
        let exact_sizes = cache.sizes(rows, has_load, width);
        if let Some(mut anchor) = self.preserve_scroll.take() {
            if anchor.session == self.active
                && self.scroll.offset() == anchor.offset
                && let Some(index) = rows.iter().position(|row| row.key == anchor.key)
            {
                let top = if exact_sizes.is_some() {
                    cache
                        .prefix(rows, index, has_load)
                        .unwrap_or_else(|| cache.provisional_prefix(rows, index, has_load))
                } else {
                    cache.provisional_prefix(rows, index, has_load)
                };
                let next = point(anchor.offset.x, -(top + anchor.within));
                self.scroll.set_offset(next);
                self.safe_scroll.insert(self.active.clone(), (stamp, next));
                if exact_sizes.is_none() {
                    anchor.offset = next;
                    self.preserve_scroll = Some(anchor);
                }
            }
        } else if self.preserve_scroll.is_none()
            && let Some((session, offset)) = self.follow_bottom.clone()
            && session == self.active
        {
            if self.scroll.offset() != offset {
                self.follow_bottom = None; // The user moved away from the tail.
                return;
            }
            let viewport_height = if self.scroll.bounds().size.height > px(0.) {
                self.scroll.bounds().size.height
            } else {
                (window.viewport_size().height - px(220.)).max(px(160.))
            };
            // The virtual list may use provisional *offscreen* prefix sizes,
            // but never place its visible tail in an unmeasured slot.
            if exact_sizes.is_none() && cache.exact_tail(rows).1 < viewport_height + px(32.) {
                return;
            }
            let sizes = exact_sizes
                .as_ref()
                .cloned()
                .unwrap_or_else(|| cache.provisional_sizes(rows, has_load, width));
            let content_height = sizes.iter().fold(px(0.), |sum, item| sum + item.height);
            let next = point(offset.x, -(content_height - viewport_height).max(px(0.)));
            self.scroll.set_offset(next);
            self.safe_scroll.insert(self.active.clone(), (stamp, next));
            if exact_sizes.is_some() {
                self.follow_bottom = None;
            } else {
                self.follow_bottom = Some((session, next));
            }
        } else if self.pending_jump.is_none()
            && let Some((saved_stamp, mut anchor)) = self.safe_anchors.get(&self.active).cloned()
            && saved_stamp == stamp
            && anchor.offset == self.scroll.offset()
            && let Some(index) = rows.iter().position(|row| row.key == anchor.key)
        {
            // An offscreen sticky/header row can replace its 64px estimate
            // with a much taller exact height. Keep the painted semantic row
            // at the same within-row position when that prefix changes.
            let top = cache.provisional_prefix(rows, index, has_load);
            let next = point(anchor.offset.x, -(top + anchor.within));
            if next != anchor.offset {
                self.scroll.set_offset(next);
                self.safe_scroll.insert(self.active.clone(), (stamp, next));
                anchor.offset = next;
                self.safe_anchors
                    .insert(self.active.clone(), (stamp, anchor));
            }
        }
    }

    fn handle_live_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Connection {
                connected: true, ..
            } => {
                self.connection_status = self
                    .server_version
                    .as_ref()
                    .map(|version| format!("Connected · {version}"))
                    .unwrap_or_else(|| "Connected".into());
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
                self.server_version = Some(data.version.clone());
                if data.projects_complete {
                    self.projects = data.projects;
                }
                if !self.disconnected {
                    self.connection_status = if data.warnings.is_empty() {
                        format!("Connected · {}", data.version)
                    } else {
                        format!("Connected · {} · Partial refresh", data.version)
                    };
                }
                if data.sessions_complete {
                    self.sessions = data.sessions;
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
                    } else if !self.pending_prompts.contains_key(id)
                        && !self.local_busy.contains(id)
                    {
                        self.deferred_abort.remove(id);
                        self.abort_timeout_tokens.remove(id);
                        self.local_run_message_ids.remove(id);
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
                let desired = self
                    .saved_active
                    .take()
                    .filter(|id| self.sessions.iter().any(|session| &session.id == id))
                    .or_else(|| {
                        self.sessions
                            .iter()
                            .find(|session| session.id == self.active)
                            .map(|session| session.id.clone())
                    })
                    .or_else(|| {
                        self.sessions
                            .iter()
                            .find(|session| session.parent_id.is_none())
                            .map(|session| session.id.clone())
                    });
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
                if let Some(id) = desired {
                    self.select_session(id);
                } else {
                    self.active.clear();
                    self.catalog = ModelCatalog::default();
                }
                self.bootstrapped = true;
                if first_bootstrap && !self.active.is_empty() {
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
                                let protected = if cursor.is_none() {
                                    self.skip_prune_for_load
                                        .remove(&session_id)
                                        .unwrap_or_default()
                                } else {
                                    HashSet::new()
                                };
                                if cursor.is_some() && self.active == session_id {
                                    self.preserve_scroll = self.transcript_anchor();
                                    self.follow_bottom = None;
                                }
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
                                self.update_transcript(&session_id);
                                self.maybe_dispatch_deferred_abort(&session_id);
                                self.resolve_late_prompt_confirmation(&session_id);
                            }
                            Err(MessageLoadError::SessionNotFound) => {
                                self.close_tab(&session_id, cx);
                            }
                            Err(error) => {
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
                        self.update_tab_status(id, status);
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
                            self.transcript.remove(&id);
                            self.transcript_spans.remove(&id);
                            self.attachments.remove_session(&id);
                            self.row_heights.remove(&id);
                            self.virtual_scrolls.remove(&id);
                            self.safe_scroll.remove(&id);
                            self.safe_anchors.remove(&id);
                            if self
                                .pending_jump
                                .as_ref()
                                .is_some_and(|jump| jump.anchor.session == id)
                            {
                                self.pending_jump = None;
                            }
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
                    if let Some(id) = kind.session_id()
                        && self
                            .conversations
                            .entry(id.to_owned())
                            .or_default()
                            .apply(&event, &kind)
                    {
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
            Some("new-session") => Some(Modal::NewSession),
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
        let complex_markdown = overlay.as_deref() == Some("complex-markdown");
        if complex_markdown
            && let Some(row) = transcript.get_mut(&active).and_then(|rows| {
                rows.iter_mut().find(|row| {
                    row.role == model::Role::Assistant && row.kind == TranscriptRowKind::Normal
                })
            })
        {
            row.body = COMPLEX_MARKDOWN_PREVIEW.into();
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
        let scroll = VirtualListScrollHandle::new();
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
            connection_status: "Connected · preview".into(),
            disconnected: false,
            sse_connected_once: false,
            server_version: None,
            conversations,
            loading_messages: HashMap::new(),
            skip_prune_for_load: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            preserve_scroll: None,
            follow_bottom: Some((active.clone(), scroll.offset())),
            catalogs: HashMap::new(),
            model_retry_count: HashMap::new(),
            model_retry_scheduled: HashSet::new(),
            running_jobs,
            sessions,
            open_tabs: server.tabs.iter().map(|tab| tab.id.clone()).collect(),
            tab_focus: HashMap::new(),
            tab_shortcut_hint: false,
            tab_drop_target: None,
            bootstrapped: true,
            projects,
            active: active.clone(),
            transcript,
            transcript_spans,
            row_heights: HashMap::new(),
            virtual_scrolls: HashMap::from([(active, scroll.clone())]),
            #[cfg(test)]
            measurement_probe: None,
            #[cfg(test)]
            rendered_rows: Rc::new(RefCell::new(Vec::new())),
            attachments,
            catalog,
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
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
            overlay: if modal.is_none() && !complex_markdown {
                overlay
            } else {
                None
            },
            scroll,
            safe_scroll: HashMap::new(),
            safe_anchors: HashMap::new(),
            pending_jump: None,
            history_focus: cx.focus_handle().tab_stop(true),
            sessions_picker_scroll: ScrollHandle::new(),
            picker_list_scroll: ScrollHandle::new(),
            projects_picker_scroll: ScrollHandle::new(),
            picker_choice_focus: HashMap::new(),
            unread: server.unread,
            statuses: bootstrap.statuses,
            jobs,
            forms,
            form_cancel_focus: cx.focus_handle().tab_stop(true),
            form_cancel_presented: None,
            permissions,
            permission_in_flight: HashSet::new(),
            permission_container_focus: cx.focus_handle().tab_stop(true),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: None,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
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
            rename_target: None,
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
        let drop_cue = self
            .tab_drop_target
            .as_ref()
            .filter(|(target, _)| target == &session.id)
            .map(|(_, after)| *after);
        let indicator: AnyElement = if self.tab_shortcut_hint && index < 9 {
            div()
                .w(px(14.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(dot_color))
                .child((index + 1).to_string())
                .into_any_element()
        } else if gear {
            Icon::default()
                .data(include_bytes!("icons/settings.svg"))
                .with_size(px(14.))
                .text_color(rgb(dot_color))
                .into_any_element()
        } else {
            div()
                .w(px(9.))
                .h(px(9.))
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
            .h(px(34.))
            .w_full()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(7.))
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
                        Some((
                            motion_id.clone(),
                            event.event.position.y >= bounds.origin.y + bounds.size.height / 2.,
                        ))
                    } else {
                        None
                    };
                    if this.tab_drop_target != destination {
                        this.tab_drop_target = destination;
                        cx.notify();
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
                    .child(indicator)
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
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.rename_target = Some(rename_id.clone());
                        this.show_modal(Modal::Rename, window, cx);
                    }))
                    .child(
                        Icon::default()
                            .data(include_bytes!("icons/edit.svg"))
                            .with_size(px(16.))
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
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab(&close_id, cx);
                        this.focus_selected_composer(window, cx);
                    }))
                    .child(
                        Icon::default()
                            .data(include_bytes!("icons/close.svg"))
                            .with_size(px(16.))
                            .text_color(if selected {
                                self.tone(0x555b5c, 0xc8c4bd)
                            } else {
                                self.tone(0xcfd0cc, 0x45494c)
                            }),
                    ),
            )
            .when_some(drop_cue, |tab, after| {
                tab.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .when(after, |cue| cue.bottom_0())
                        .when(!after, |cue| cue.top_0())
                        .h(px(2.))
                        .bg(self.tone(0xa46910, 0xe5b567)),
                )
            })
            .into_any_element()
    }

    fn sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let mut tabs = div()
            .flex()
            .flex_col()
            .p(px(8.))
            .gap(px(5.))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, cx| {
                if !event.bounds.contains(&event.event.position)
                    && this.tab_drop_target.take().is_some()
                {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.tab_drop_target.take().is_some() {
                        cx.notify();
                    }
                }),
            );
        let mut previous_inactive = false;
        for (index, id) in self.open_tabs.iter().enumerate() {
            if let Some(session) = self.sessions.iter().find(|session| &session.id == id) {
                let active = session.id == self.active;
                tabs = tabs.child(self.tab(session, index, !active && previous_inactive, cx));
                previous_inactive = !active;
            }
        }
        let mut jobs_panel = div()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(14.))
            .gap(px(8.))
            .border_t_1()
            .border_color(self.tone(0xc8c3ba, 0x24282c));
        if !self.jobs.is_empty() {
            jobs_panel = jobs_panel.child(
                div()
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
                    ),
            );
            for job in &self.jobs {
                let shell = job.kind == JobKind::Shell;
                let (label, elapsed) = job.subtitle_parts(1_704_067_320_000);
                jobs_panel = jobs_panel.child(
                    div()
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
                                        .text_size(px(12.))
                                        .text_color(self.tone(0x252829, 0xd4d8dc))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(job.title.clone()),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x87908d, 0x6a7279))
                                        .child(label)
                                        .when_some(elapsed, |view, elapsed| {
                                            view.child(format!(" · {elapsed}"))
                                        }),
                                ),
                        ),
                );
            }
        }
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(self.tone(0xf4f1eb, 0x0d0f11))
            .border_r_1()
            .border_color(self.tone(0xc8c3ba, 0x24282c))
            .child(
                BaseButton::new("new-session")
                    .accessibility_label("New session")
                    .h(px(39.))
                    .px(px(16.))
                    .line_height(px(16.))
                    .justify_start()
                    .text_color(self.tone(0x667078, 0x92999f))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_modal(Modal::NewSession, window, cx);
                    }))
                    .child("⊞  New session"),
            )
            .child(tabs)
            .child(div().flex_1())
            .child(jobs_panel)
            .child(
                div()
                    .p(px(14.))
                    .border_t_1()
                    .border_color(self.tone(0xded8cb, 0x2b3034))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        BaseButton::new("footer-tabs")
                            .accessibility_label("Open tabs")
                            .h(px(21.))
                            .line_height(px(16.))
                            .justify_start()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Sessions, window, cx);
                            }))
                            .child("≡  Tabs"),
                    )
                    .child(
                        BaseButton::new("footer-settings")
                            .accessibility_label("Open settings")
                            .h(px(21.))
                            .line_height(px(16.))
                            .justify_start()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Settings, window, cx);
                            }))
                            .child("⚙  Settings"),
                    ),
            )
            .into_any_element()
    }

    // Keep the actual row and the detached measurement probe on this same
    // renderer. A text-length estimate cannot model GPUI's Markdown wrapping.
    fn message_row(&self, row: &TranscriptRow, index: usize, cx: &Context<Self>) -> Stateful<Div> {
        self.message_row_with_images(row, index, None, cx)
    }

    fn message_row_with_images(
        &self,
        row: &TranscriptRow,
        index: usize,
        stale_images: Option<&[Option<Arc<Image>>]>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let user = row.role.label() == "YOU";
        let shade = if user {
            self.tone(0xe4ddd0, 0x1c242b)
        } else {
            self.tone(0xf4f1eb, 0x101214)
        };
        let mut body = div().flex().flex_col().gap(px(10.));
        match row.kind {
            TranscriptRowKind::Tool => {
                body = body.child(div().font_family("monospace").child(row.body.clone()))
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
                body = body.child(self.markdown_body(&row.body, index, cx))
            }
            _ => body = body.child(row.body.clone()),
        }
        for (image_index, image) in row.images.iter().enumerate() {
            let source = if let Some(stale_images) = stale_images {
                stale_images.get(image_index).and_then(Option::as_ref)
            } else {
                self.attachments.get(&self.active, row, image_index)
            };
            let thumbnail = div()
                .h(px(120.))
                .w(px(200.))
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
            // The transcript inherits these from app-root. Make them explicit
            // so a detached root gets the same text metrics as a mounted row.
            .font_family("Noto Sans")
            .text_size(px(13.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(if user { 10. } else { 6. }))
            .px(px(28.))
            .pt(px(18.))
            .pb(px(20.))
            .bg(shade)
            .border_b_1()
            .border_color(self.tone(0xc8c3ba, 0x24282c))
            .child(
                div()
                    .flex()
                    .items_center()
                    .text_size(px(11.))
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

    fn message(&self, row: &TranscriptRow, index: usize, cx: &Context<Self>) -> AnyElement {
        self.message_with_images(row, index, None, cx)
    }

    fn message_with_images(
        &self,
        row: &TranscriptRow,
        index: usize,
        stale_images: Option<&[Option<Arc<Image>>]>,
        cx: &Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        self.rendered_rows.borrow_mut().push(index);
        let element = self.message_row_with_images(row, index, stale_images, cx);
        #[cfg(test)]
        {
            use gpui_kit::base::TestSupportExt as _;
            element.test_support().into_any_element()
        }
        #[cfg(not(test))]
        {
            element.into_any_element()
        }
    }

    /// The fixed sidebar and gutter leave the same definite width for mounted
    /// and detached rows, including the first frame after a window resize.
    fn row_measurement_probe(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<TranscriptMeasurementProbe> {
        let transcript = self.transcript.get(&self.active)?;
        let content_width =
            window.viewport_size().width - px(SIDEBAR_WIDTH + TRANSCRIPT_SCROLLBAR_GUTTER);
        #[cfg(test)]
        let (width, heights) = self
            .measurement_probe
            .as_ref()
            .map(|(width, heights)| (*width, Some(heights.clone())))
            .unwrap_or((content_width, None));
        #[cfg(not(test))]
        let width = content_width;
        if width <= px(0.) {
            return None;
        }
        let stamp = RowLayoutStamp {
            width,
            dark: self.dark,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: self
                .conversations
                .get(&self.active)
                .map_or(0, Conversation::cache_epoch),
        };
        let cache = self
            .row_heights
            .entry(self.active.clone())
            .or_default()
            .clone();
        let load = self
            .conversations
            .get(&self.active)
            .and_then(|conversation| conversation.next_cursor.clone());
        let target_anchor = self
            .pending_jump
            .as_ref()
            .filter(|jump| jump.stamp == stamp && jump.anchor.session == self.active)
            .map(|jump| &jump.anchor)
            .or_else(|| {
                self.preserve_scroll
                    .as_ref()
                    .filter(|anchor| anchor.session == self.active)
            });
        let pending_range = target_anchor.and_then(|anchor| {
            transcript
                .iter()
                .position(|row| row.key == anchor.key)
                .map(|index| {
                    let cache = cache.borrow();
                    if cache.stamp != Some(stamp) || cache.height(&transcript[index]).is_none() {
                        return index..index + 1;
                    }
                    let top = cache.provisional_prefix(transcript, index, load.is_some());
                    let viewport_height = if self.scroll.bounds().size.height > px(0.) {
                        self.scroll.bounds().size.height
                    } else {
                        (window.viewport_size().height - px(220.)).max(px(160.))
                    };
                    let range = cache.provisional_viewport(
                        transcript,
                        load.is_some(),
                        point(px(0.), -(top + anchor.within)),
                        viewport_height,
                    );
                    range.start.saturating_sub(1 + usize::from(load.is_some()))
                        ..(range.end + 1)
                            .min(transcript.len() + usize::from(load.is_some()))
                            .saturating_sub(usize::from(load.is_some()))
                })
        });
        let mut missing = if transcript.len() > TRANSCRIPT_PROGRESSIVE_MIN_ROWS {
            if let Some(range) = pending_range {
                cache
                    .borrow_mut()
                    .missing_range(stamp, transcript, range, TRANSCRIPT_MEASURE_BATCH)
            } else {
                let viewport_height = if self.scroll.bounds().size.height > px(0.) {
                    self.scroll.bounds().size.height
                } else {
                    (window.viewport_size().height - px(220.)).max(px(160.))
                };
                let follows_bottom =
                    self.follow_bottom
                        .as_ref()
                        .is_some_and(|(session, offset)| {
                            session == &self.active && *offset == self.scroll.offset()
                        });
                let need_tail = follows_bottom && {
                    let cache = cache.borrow();
                    cache.stamp != Some(stamp)
                        || cache.exact_tail(transcript).1 < viewport_height + px(32.)
                };
                if need_tail {
                    cache.borrow_mut().missing_tail(
                        stamp,
                        transcript,
                        TRANSCRIPT_MEASURE_BATCH,
                        viewport_height + px(32.),
                    )
                } else {
                    let range = cache.borrow().provisional_viewport(
                        transcript,
                        load.is_some(),
                        self.scroll.offset(),
                        viewport_height,
                    );
                    cache.borrow_mut().missing_range(
                        stamp,
                        transcript,
                        range.start.saturating_sub(1 + usize::from(load.is_some()))
                            ..(range.end + 1)
                                .min(transcript.len() + usize::from(load.is_some()))
                                .saturating_sub(usize::from(load.is_some())),
                        TRANSCRIPT_MEASURE_BATCH,
                    )
                }
            }
        } else {
            cache.borrow_mut().missing(stamp, transcript)
        };
        if transcript.len() > TRANSCRIPT_PROGRESSIVE_MIN_ROWS && self.scroll.offset().y < px(-1.) {
            let cache = cache.borrow();
            let positions = cache.provisional_positions(transcript, load.is_some());
            let visible_top = -self.scroll.offset().y;
            let sticky = transcript
                .iter()
                .enumerate()
                .filter(|(index, row)| row.role.label() == "YOU" && positions[*index] < visible_top)
                .map(|(index, _)| index)
                .next_back();
            if let Some(index) = sticky
                && cache.height(&transcript[index]).is_none()
                && !missing.contains(&index)
            {
                missing.push(index);
            }
        }
        let loading = self.loading_messages.contains_key(&self.active);
        let measure_load = load.as_ref().is_some_and(|_| {
            cache
                .borrow()
                .load_height
                .is_none_or(|(was_loading, _)| was_loading != loading)
        });
        #[cfg(test)]
        let missing = if heights.is_some() {
            (0..transcript.len()).collect::<Vec<_>>()
        } else {
            missing
        };
        if missing.is_empty() && !measure_load {
            return None;
        }
        Some(TranscriptMeasurementProbe {
            rows: missing
                .into_iter()
                .map(|index| {
                    let row = &transcript[index];
                    (
                        row.key.clone(),
                        row.render_revision(),
                        self.message_row(row, index, cx).into_any_element(),
                    )
                })
                .collect(),
            load: if measure_load {
                load.map(|cursor| {
                    (
                        loading,
                        self.load_earlier_row(cursor, loading, cx)
                            .into_any_element(),
                    )
                })
            } else {
                None
            },
            stamp,
            cache,
            #[cfg(test)]
            heights,
            spacer: div().size(px(0.)),
        })
    }

    fn load_earlier_row(&self, cursor: String, loading: bool, cx: &Context<Self>) -> Stateful<Div> {
        let id = self.active.clone();
        let key_id = id.clone();
        let key_cursor = cursor.clone();
        div()
            .id("load-earlier")
            .font_family("Noto Sans")
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
        #[cfg(test)]
        {
            use gpui_kit::base::TestSupportExt as _;
            element.test_support().into_any_element()
        }
        #[cfg(not(test))]
        {
            element.into_any_element()
        }
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
            .mx(px(16.))
            .mb(px(8.))
            .h(px(45.))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .rounded(px(9.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x2d3236))
            .bg(self.tone(0xf5f3ef, 0x15181b))
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.tone(0x98600f, 0xd8a55f))
                    .child(notice.text),
            )
            .child(
                BaseButton::new("open-web-ui")
                    .accessibility_label("Open web UI to answer form")
                    .on_click(cx.listener(|this, _, _, cx| this.open_web_ui(cx)))
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

    fn working_pill(&self) -> Option<AnyElement> {
        self.statuses
            .get(&self.active)
            .is_some_and(RunStatus::is_busy)
            .then(|| {
                div()
                    .mx_auto()
                    .mb(px(10.))
                    .px(px(12.))
                    .h(px(29.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded_full()
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x34393e))
                    .bg(self.tone(0xfffdfa, 0x171a1d))
                    .text_size(px(12.))
                    .text_color(self.tone(0x555b5c, 0xc4c8ca))
                    .child("◌")
                    .child("OpenCode is working")
                    .into_any_element()
            })
    }

    fn chat(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        // GTK reserves a permanent 14px gutter for the transcript scrollbar.
        let mut rows = div()
            .id("transcript")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(self.scroll.base_handle())
            .w_full()
            .pr(px(TRANSCRIPT_SCROLLBAR_GUTTER))
            .flex()
            .flex_col();
        let sticky = self.sticky_user_row(window).map(|row| {
            div()
                .id("sticky-user")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .right(px(TRANSCRIPT_SCROLLBAR_GUTTER))
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
        let width = window.viewport_size().width - px(SIDEBAR_WIDTH + TRANSCRIPT_SCROLLBAR_GUTTER);
        let stamp = RowLayoutStamp {
            width,
            dark: self.dark,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: self
                .conversations
                .get(&self.active)
                .map_or(0, Conversation::cache_epoch),
        };
        let layout = self.transcript.get(&self.active).and_then(|transcript| {
            self.row_heights.get(&self.active).and_then(|cache| {
                let cache = cache.borrow();
                (cache.stamp == Some(stamp)).then(|| {
                    let load_current = !has_load
                        || cache.load_height.map(|(loading, _)| loading)
                            == Some(self.loading_messages.contains_key(&self.active));
                    // Freeze which slots had exact heights at render time.
                    // The probe records misses during request_layout, before
                    // virtual prepaint. Reading the live cache in the renderer
                    // would paint freshly measured content into *old* sizes.
                    let mut ready = Vec::with_capacity(transcript.len() + usize::from(has_load));
                    if has_load {
                        ready.push(load_current);
                    }
                    ready.extend(transcript.iter().map(|row| cache.height(row).is_some()));
                    let stale = transcript
                        .iter()
                        .enumerate()
                        .filter_map(|(index, row)| {
                            (!ready[index + usize::from(has_load)])
                                .then(|| cache.stale.get(&row.key))
                                .flatten()
                                .filter(|old| {
                                    old.row.render_revision() != row.render_revision()
                                        && cache.entries.get(&row.key).is_some_and(|entry| {
                                            entry.revision == old.row.render_revision()
                                                && entry.height == old.height
                                        })
                                })
                                .map(|old| (index, old.clone()))
                        })
                        .collect::<HashMap<_, _>>();
                    let viewport_height = if self.scroll.bounds().size.height > px(0.) {
                        self.scroll.bounds().size.height
                    } else {
                        (window.viewport_size().height - px(220.)).max(px(160.))
                    };
                    let viewport = cache.provisional_viewport(
                        transcript,
                        has_load,
                        self.scroll.offset(),
                        viewport_height,
                    );
                    let safe_partial = cache.viewport_ready(
                        transcript,
                        has_load,
                        self.loading_messages.contains_key(&self.active),
                        viewport,
                    );
                    let anchor_unmeasured = self
                        .pending_jump
                        .as_ref()
                        .filter(|jump| jump.stamp == stamp && jump.anchor.session == self.active)
                        .map(|jump| &jump.anchor)
                        .or_else(|| {
                            self.preserve_scroll
                                .as_ref()
                                .filter(|anchor| anchor.session == self.active)
                        })
                        .and_then(|anchor| transcript.iter().find(|row| row.key == anchor.key))
                        .is_some_and(|row| cache.height(row).is_none());
                    let parked_at_safe_viewport = self.preserve_scroll.is_none()
                        && self.safe_scroll.get(&self.active).is_some_and(
                            |(saved_stamp, offset)| {
                                *saved_stamp == stamp && *offset == self.scroll.offset()
                            },
                        )
                        && safe_partial;
                    // A single newly appended visible row has no stale
                    // snapshot. Do not paint an empty provisional virtual
                    // slot for one frame; use the bounded natural slice until
                    // its exact height is available.
                    if (anchor_unmeasured && !parked_at_safe_viewport) || !safe_partial {
                        return None;
                    }
                    let sizes = if ready.iter().all(|ready| *ready) {
                        cache.sizes(transcript, has_load, width)
                    } else {
                        None
                    }
                    .unwrap_or_else(|| cache.provisional_sizes(transcript, has_load, width));
                    Some((sizes, ready, stale))
                })?
            })
        });
        let content: AnyElement = if let Some((sizes, ready, stale)) = layout {
            let session = self.active.clone();
            v_virtual_list(
                cx.entity(),
                "transcript",
                sizes,
                move |client, range, _, cx| {
                    let Some(transcript) = client.transcript.get(&session) else {
                        return Vec::new();
                    };
                    let has_load = client
                        .conversations
                        .get(&session)
                        .is_some_and(|conversation| conversation.next_cursor.is_some());
                    range
                        .map(|index| {
                            if has_load && index == 0 {
                                if !ready[index] {
                                    return div().into_any_element();
                                }
                                let loading = client.loading_messages.contains_key(&session);
                                let cursor =
                                    client.conversations[&session].next_cursor.clone().unwrap();
                                client.load_earlier_element(cursor, loading, cx)
                            } else if !ready[index] {
                                let row_index = index - usize::from(has_load);
                                stale.get(&row_index).map_or_else(
                                    || div().into_any_element(),
                                    |old| {
                                        client.message_with_images(
                                            &old.row,
                                            row_index,
                                            Some(&old.images),
                                            cx,
                                        )
                                    },
                                )
                            } else {
                                let row_index = index - usize::from(has_load);
                                client.message(&transcript[row_index], row_index, cx)
                            }
                        })
                        .collect()
                },
            )
            .track_scroll(&self.scroll)
            .pr(px(TRANSCRIPT_SCROLLBAR_GUTTER))
            .into_any_element()
        } else {
            // A first layout or width/style change has no usable measurements.
            // Natural rows avoid guessing their geometry while the probe runs.
            // On a large first visit paint only a bounded tail rather than
            // mounting the entire unmeasured history before virtualization.
            if let Some(transcript) = self.transcript.get(&self.active) {
                let tail_only = transcript.len() > TRANSCRIPT_PROGRESSIVE_MIN_ROWS
                    && self
                        .follow_bottom
                        .as_ref()
                        .is_some_and(|(id, _)| id == &self.active);
                let pending_index = self
                    .pending_jump
                    .as_ref()
                    .filter(|jump| jump.stamp == stamp && jump.anchor.session == self.active)
                    .map(|jump| &jump.anchor)
                    .or_else(|| {
                        self.preserve_scroll
                            .as_ref()
                            .filter(|anchor| anchor.session == self.active)
                    })
                    .and_then(|anchor| transcript.iter().position(|row| row.key == anchor.key));
                let has_load = self
                    .conversations
                    .get(&self.active)
                    .is_some_and(|conversation| conversation.next_cursor.is_some());
                let current_range = if pending_index.is_none()
                    && transcript.len() > TRANSCRIPT_PROGRESSIVE_MIN_ROWS
                    && !tail_only
                {
                    self.row_heights.get(&self.active).map(|cache| {
                        let viewport_height = if self.scroll.bounds().size.height > px(0.) {
                            self.scroll.bounds().size.height
                        } else {
                            (window.viewport_size().height - px(220.)).max(px(160.))
                        };
                        cache.borrow().provisional_viewport(
                            transcript,
                            has_load,
                            self.scroll.offset(),
                            viewport_height,
                        )
                    })
                } else {
                    None
                };
                let target_index = pending_index.or_else(|| {
                    current_range.as_ref().map(|range| {
                        range
                            .start
                            .saturating_sub(usize::from(has_load))
                            .min(transcript.len() - 1)
                    })
                });
                let bounded_target = transcript.len() > TRANSCRIPT_PROGRESSIVE_MIN_ROWS
                    && !tail_only
                    && target_index.is_some();
                let start = if let Some(index) = target_index.filter(|_| bounded_target) {
                    let within = self
                        .pending_jump
                        .as_ref()
                        .filter(|jump| jump.stamp == stamp && jump.anchor.session == self.active)
                        .map(|jump| jump.anchor.within)
                        .or_else(|| {
                            self.preserve_scroll
                                .as_ref()
                                .filter(|anchor| anchor.session == self.active)
                                .map(|anchor| anchor.within)
                        })
                        .unwrap_or(px(0.));
                    if within >= px(0.) {
                        // A tall anchor can begin far above the viewport. Do
                        // not put unmeasured natural rows ahead of it after an
                        // estimated spacer: their real heights would shift it.
                        index
                    } else {
                        index.saturating_sub(TRANSCRIPT_MEASURE_BATCH)
                    }
                } else if tail_only {
                    let measured_tail = self.row_heights.get(&self.active).map_or(0, |cache| {
                        let cache = cache.borrow();
                        if cache.stamp == Some(stamp) {
                            cache.exact_tail(transcript).0
                        } else {
                            0
                        }
                    });
                    transcript
                        .len()
                        .saturating_sub(measured_tail + TRANSCRIPT_MEASURE_BATCH)
                } else {
                    0
                };
                let end =
                    target_index
                        .filter(|_| bounded_target)
                        .map_or(transcript.len(), |index| {
                            (index + TRANSCRIPT_MEASURE_BATCH + 1)
                                .max(current_range.as_ref().map_or(0, |range| {
                                    range.end.saturating_sub(usize::from(has_load))
                                        + TRANSCRIPT_MEASURE_BATCH
                                }))
                                .min(transcript.len())
                        });
                if bounded_target
                    && start > 0
                    && let Some(cache) = self.row_heights.get(&self.active)
                {
                    let height = cache
                        .borrow()
                        .provisional_prefix(transcript, start, has_load);
                    rows = rows.child(div().h(height).flex_shrink_0());
                }
                if !tail_only
                    && start == 0
                    && let Some(cursor) = self
                        .conversations
                        .get(&self.active)
                        .and_then(|conversation| conversation.next_cursor.clone())
                {
                    let loading = self.loading_messages.contains_key(&self.active);
                    rows = rows.child(self.load_earlier_element(cursor, loading, cx));
                }
                for (index, row) in transcript.iter().enumerate().take(end).skip(start) {
                    rows = rows.child(self.message(row, index, cx));
                }
                if bounded_target
                    && end < transcript.len()
                    && let Some(cache) = self.row_heights.get(&self.active)
                {
                    let cache = cache.borrow();
                    let remaining =
                        cache.provisional_prefix(transcript, transcript.len(), has_load)
                            - cache.provisional_prefix(transcript, end, has_load);
                    rows = rows.child(div().h(remaining).flex_shrink_0());
                }
            }
            rows.into_any_element()
        };
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(content)
            .when_some(sticky, |view, sticky| view.child(sticky))
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    fn sticky_user_row(&self, window: &Window) -> Option<&TranscriptRow> {
        if self.scroll.offset().y >= px(-1.) {
            return None;
        }
        let transcript = self.transcript.get(&self.active)?;
        let viewport = self.scroll.bounds();
        let top = viewport.origin.y;
        let bottom = top + viewport.size.height;
        let first_child = usize::from(
            self.conversations
                .get(&self.active)
                .is_some_and(|conversation| conversation.next_cursor.is_some()),
        );
        let measured = self
            .row_heights
            .get(&self.active)
            .map(|cache| cache.borrow());
        let measured = measured.as_ref().filter(|cache| {
            cache.stamp.is_some_and(|stamp| {
                stamp.dark == self.dark
                    && stamp.style_revision == TRANSCRIPT_ROW_STYLE_REVISION
                    && stamp.width
                        == window.viewport_size().width
                            - px(SIDEBAR_WIDTH + TRANSCRIPT_SCROLLBAR_GUTTER)
                    && stamp.epoch
                        == self
                            .conversations
                            .get(&self.active)
                            .map_or(0, Conversation::cache_epoch)
            })
        });
        let positions =
            measured.map(|cache| cache.provisional_positions(transcript, first_child != 0));
        let users = transcript.iter().enumerate().filter_map(|(index, row)| {
            if row.role.label() != "YOU" {
                return None;
            }
            if let (Some(cache), Some(positions)) = (measured, &positions) {
                let row_top = top + self.scroll.offset().y + positions[index];
                let height = cache
                    .entries
                    .get(&row.key)
                    .map_or(px(64.), |entry| entry.height);
                Some((index, row_top, row_top + height))
            } else {
                let bounds = self.scroll.bounds_for_item(index + first_child)?;
                let row_top = bounds.origin.y + self.scroll.offset().y;
                Some((index, row_top, row_top + bounds.size.height))
            }
        });
        sticky_user_index(users, top, bottom).and_then(|index| {
            let row = transcript.get(index)?;
            (measured.is_none_or(|cache| cache.height(row).is_some())).then_some(row)
        })
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
        let mut files = div().px(px(13.)).flex().gap(px(7.));
        for (index, path) in self.attachments_draft.iter().enumerate() {
            let target = path.clone();
            let session_id = self.active.clone();
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            files = files.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(5.))
                    .bg(self.tone(0xf4f1eb, 0x262b30))
                    .flex()
                    .gap(px(6.))
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
            .mx(px(16.))
            .mb(px(17.))
            .rounded(px(12.))
            .border_1()
            .border_color(self.tone(0xc8c3ba, 0x30353a))
            .bg(self.tone(0xffffff, 0x191c1f))
            .child(
                Textarea::new(&self.composer)
                    .aria_label("Ask OpenCode anything…")
                    .on_paste(move |item, _, app| {
                        paste_owner.update(app, |this, cx| {
                            this.paste_attachments(&paste_session, item, cx)
                        })
                    })
                    .bordered(false)
                    .h(px(89.)),
            )
            .when(!self.attachments_draft.is_empty(), |composer| {
                composer.child(files)
            })
            .child(
                div()
                    .h(px(46.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(18.))
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
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.choose_attachments(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_attachments(cx);
                            }))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/paperclip.svg"))
                                    .with_size(px(22.))
                                    .text_color(self.tone(0x555b5c, 0xc8c4bd)),
                            ),
                    )
                    .child(
                        div()
                            .id("composer-model")
                            .role(Role::Button)
                            .aria_label("Choose model")
                            .test_support()
                            .track_focus(&self.composer_action_focus[1])
                            .focus_visible(|style| {
                                style.border_color(self.tone(0x2356a8, 0x78baff))
                            })
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.show_modal(Modal::Model, window, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Model, window, cx);
                            }))
                            .max_w(px(160.))
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
                            .child("⌄"),
                    )
                    .child(
                        div()
                            .id("composer-level")
                            .role(Role::Button)
                            .aria_label("Choose reasoning level")
                            .test_support()
                            .track_focus(&self.composer_action_focus[2])
                            .focus_visible(|style| {
                                style.border_color(self.tone(0x2356a8, 0x78baff))
                            })
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.show_modal(Modal::Level, window, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Level, window, cx);
                            }))
                            .child(format!(
                                "{}⌄",
                                selected_model
                                    .and_then(|selection| selection.variant)
                                    .unwrap_or_else(|| "Default".into())
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .text_color(self.tone(0x858e8c, 0x899097))
                            .child(context),
                    )
                    .when(
                        running && window.viewport_size().width >= px(900.),
                        |footer| {
                            footer.child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.))
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
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.stop_active(cx);
                                        cx.stop_propagation();
                                    }
                                }))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.stop_active(cx);
                                }))
                                .w(px(32.))
                                .h(px(32.))
                                .rounded_full()
                                .border_1()
                                .border_color(self.tone(0x555b5c, 0x6b7176))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child("■"),
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
                            .w(px(32.))
                            .h(px(32.))
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
                                    .with_size(px(22.)),
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
                            .text_size(px(11.))
                            .text_color(self.tone(0x858e8c, 0x7f878e))
                            .child(div().flex_1().child(session.directory.clone()))
                            .child(timestamp(session.time.updated)),
                    ),
            );
        }
        if shown == 0 {
            rows = rows.child(div().p(px(16.)).child("No matching sessions"));
        }
        div()
            .flex_1()
            .min_h_0()
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

    fn modal_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let modal = self.modal?;
        let backdrop = div()
            .id("modal-backdrop")
            .absolute()
            .top(px(46.))
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(rgba(if self.dark { 0x000000a6 } else { 0x00000066 }))
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
        let panel: AnyElement = match modal {
            Modal::Sessions | Modal::NewSession => {
                let sessions = modal == Modal::Sessions;
                let mut card = div()
                    .w(px(if sessions { 520. } else { 349. }))
                    .h(px(if sessions { 410. } else { 319. }))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, if sessions { 0x27272a } else { 0x2a3038 }))
                    .bg(self.tone(0xffffff, if sessions { 0x18181b } else { 0x15181c }))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(if sessions { 48. } else { 55. }))
                            .px(px(12.))
                            .border_b_1()
                            .border_color(self.tone(
                                if sessions { 0xded8cb } else { 0x4f99ee },
                                if sessions { 0x27272a } else { 0x62bceb },
                            ))
                            .when(!sessions, |header| header.border_1().rounded(px(6.)))
                            .flex()
                            .items_center()
                            .when(sessions, |view| {
                                view.child(
                                    Icon::default()
                                        .data(include_bytes!("icons/search.svg"))
                                        .with_size(px(16.))
                                        .text_color(self.tone(0x8b918e, 0x71717a)),
                                )
                            })
                            .child(Input::new(&self.search).appearance(false)),
                    );
                if sessions {
                    let query = self.search.read(cx).value();
                    let mut list = div()
                        .id("session-picker-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.sessions_picker_scroll)
                        .flex()
                        .flex_col();
                    for (index, session) in filter_tab_sessions(&self.sessions, &query)
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
                                .flex_shrink_0()
                                .mx(px(10.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .rounded(px(6.))
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
                                        .flex_1()
                                        .font_weight(FontWeight::BOLD)
                                        .child(session.title.clone()),
                                )
                                .child(
                                    div()
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
                            .font_family("monospace")
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
                        .flex_1()
                        .min_h_0()
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
                        let path = directory.clone();
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
                                .flex_shrink_0()
                                .m(px(4.))
                                .px(px(18.))
                                .flex()
                                .items_center()
                                .rounded(px(6.))
                                .bg(self.tone(
                                    if self.picker_highlight.is_none_or(|at| at == index) {
                                        0xece9e2
                                    } else {
                                        0xffffff
                                    },
                                    if self.picker_highlight.is_none_or(|at| at == index) {
                                        0x252d37
                                    } else {
                                        0x15181c
                                    },
                                ))
                                .font_weight(FontWeight::BOLD)
                                .child(div().flex_1().child(label))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::NORMAL)
                                        .text_color(self.tone(0x758080, 0x7d8590))
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
                                .font_family("monospace")
                                .child(rename_id.clone()),
                        )
                        .child(
                            div()
                                .id("rename-copy-id")
                                .cursor_pointer()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        rename_id.clone(),
                                    ));
                                }))
                                .child("▣"),
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
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.rename_pending.is_none() {
                                        this.modal = None;
                                        this.rename_target = None;
                                        this.rename_error = None;
                                        cx.notify();
                                    }
                                }))
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
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_session(cx);
                                }))
                                .px(px(14.))
                                .py(px(7.))
                                .bg(self.tone(0xc59535, 0xd29b52))
                                .border_1()
                                .border_color(self.tone(0x1f5c99, 0x5a4424))
                                .rounded(px(5.))
                                .child("Save"),
                        ),
                )
                .into_any_element(),
            Modal::Settings => {
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
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(70.))
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
                    .w(px(820.))
                    .h(px(674.))
                    .flex_shrink_0()
                    .flex()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x232930))
                    .bg(self.tone(0xf5f0e7, 0x111418))
                    .shadow_lg()
                    .child(
                        div()
                            .w(px(280.))
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
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .text_size(px(10.))
                                    .text_color(self.tone(0x929a9a, 0x6a7482))
                                    .child(
                                        url::Url::parse(&self.settings.current.base_url)
                                            .ok()
                                            .and_then(|url| url.host_str().map(str::to_owned))
                                            .unwrap_or_else(|| {
                                                self.settings.current.base_url.clone()
                                            }),
                                    )
                                    .child("opencode-gpui v0.1.0"),
                            ),
                    )
                    .child(
                        div().w(px(540.)).flex().flex_col().child(content).child(
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
                                        .text_size(px(11.))
                                        .text_color(self.tone(0x818984, 0x899097))
                                        .child(if self.settings.tab == SettingsTab::Connection {
                                            "Secrets stay out of the state file"
                                        } else {
                                            "↑ ↓ to navigate · Enter to open in tab"
                                        }),
                                )
                                .child(
                                    div()
                                        .id("settings-cancel")
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.settings.reset_draft(window, cx);
                                            this.modal = None;
                                            cx.notify();
                                        }))
                                        .px(px(14.))
                                        .py(px(7.))
                                        .mr(px(9.))
                                        .border_1()
                                        .border_color(self.tone(0xd3cec5, 0x30353a))
                                        .rounded(px(5.))
                                        .bg(self.tone(0xf1efec, 0x393939))
                                        .text_color(self.tone(0x252829, 0xf8f7f7))
                                        .child(if self.settings.tab == SettingsTab::Connection {
                                            "Cancel"
                                        } else {
                                            "Close"
                                        }),
                                )
                                .when(self.settings.tab == SettingsTab::Connection, |footer| {
                                    footer.child(
                                        div()
                                            .id("settings-apply")
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.apply_settings(window, cx);
                                            }))
                                            .px(px(14.))
                                            .py(px(7.))
                                            .rounded(px(5.))
                                            .bg(self.tone(0xc59535, 0xd29b52))
                                            .text_color(self.tone(0x17130e, 0x17130e))
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
                    .w(px(if model { 368. } else { 268. }))
                    // The GTK popup's external frame is 2px shorter for
                    // models and 4px taller for levels than the natural list.
                    .h(px(70.
                        + picker_list_height(model, count)
                        + if model { -2. } else { 4. }))
                    .relative()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x30353a))
                    .bg(self.tone(0xfffdfa, 0x191c1f))
                    .shadow_lg()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .h(px(34.))
                            .w_full()
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(0x4f99ee, 0x62bceb))
                            .bg(self.tone(0xffffff, 0x181c21))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/search.svg"))
                                    .with_size(px(16.))
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
                    .gap(px(if model { 8. } else { 0. }));
                if model {
                    let query = self.search.read(cx).value();
                    let choices = filter_models(&self.catalog.models, &query);
                    if choices.is_empty() {
                        rows = rows.child(div().my(px(16.)).child("No matching models"));
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
                                .h(px(52.))
                                .flex_shrink_0()
                                .px(px(14.))
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
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .when(selected, |title| {
                                                    title.text_color(self.tone(0x2563eb, 0x62bceb))
                                                })
                                                .child(option.label.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.))
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
                        rows = rows.child(div().my(px(14.)).child("No matching levels"));
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
                                .h(px(33.))
                                .flex_shrink_0()
                                .px(px(12.))
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
                            .absolute()
                            .bottom(px(-20.))
                            .left(px(if model { 174. } else { 119. }))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/popover-arrow.svg"))
                                    .with_size(px(20.))
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
                .pl(px(if modal == Modal::Model { 220. } else { 374. }))
                .pb(px(79.))
                .bg(rgba(0x00000000))
        } else if modal == Modal::Settings {
            backdrop.items_start().justify_start().pt(px(36.))
        } else {
            backdrop
        };
        Some(
            backdrop
                .child(
                    div()
                        .id("modal-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(panel),
                )
                .into_any_element(),
        )
    }
}

impl Render for Client {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.dark = Theme::global(cx).is_dark();
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
        let picker_focus_keys: Vec<_> = match self.modal {
            Some(Modal::Sessions) => {
                let query = self.search.read(cx).value();
                filter_tab_sessions(&self.sessions, &query)
                    .into_iter()
                    .map(|session| format!("session:{}", session.id))
                    .collect()
            }
            Some(Modal::NewSession) => {
                let query = self.search.read(cx).value();
                filter_new_session_projects(
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
                .collect()
            }
            Some(Modal::Model) => {
                let query = self.search.read(cx).value();
                filter_models(&self.catalog.models, &query)
                    .into_iter()
                    .map(|option| format!("model:{}:{}", option.provider_id, option.model_id))
                    .collect()
            }
            Some(Modal::Level) => {
                let query = self.search.read(cx).value();
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
        };
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
        self.guard_unmeasured_scroll(window);
        if self.preserve_scroll.is_none()
            && self.pending_jump.is_none()
            && self.follow_bottom.is_none()
            && let Some(cache) = self.row_heights.get(&self.active)
            && let Some(stamp) = cache.borrow().stamp
            && self
                .safe_scroll
                .get(&self.active)
                .is_some_and(|(saved_stamp, offset)| {
                    *saved_stamp == stamp && *offset == self.scroll.offset()
                })
            && self
                .safe_anchors
                .get(&self.active)
                .is_none_or(|(saved_stamp, anchor)| {
                    *saved_stamp != stamp || anchor.offset != self.scroll.offset()
                })
            && let Some(anchor) = self.transcript_anchor()
        {
            self.safe_anchors
                .insert(self.active.clone(), (stamp, anchor));
        }
        let row_probe = self.row_measurement_probe(window, cx);
        self.correct_scroll(window);
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
        let root = div()
            .id("app-root")
            .size_full()
            .relative()
            .capture_key_down(cx.listener(Self::handle_key_down))
            .capture_key_up(cx.listener(Self::handle_key_up))
            .flex()
            .flex_col()
            .font_family("Noto Sans")
            .text_size(px(13.))
            .text_color(self.tone(0x252829, 0xe8e5df))
            .bg(self.tone(0xf4f1eb, 0x101214))
            .child(
                div()
                    .h(px(46.))
                    .w_full()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(self.tone(0xc8c3ba, 0x2a2e32))
                    .bg(self.tone(0xf4f1eb, 0x14171a))
                    .child(div().w(px(270.)).pl(px(16.)).child("◫"))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .pl(px(27.))
                            .child(div().font_weight(FontWeight::BOLD).child("OpenCode"))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(px(11.))
                                    .text_color(self.tone(0x77817e, 0x899097))
                                    .child(self.connection_status.clone()),
                            ),
                    )
                    .child(
                        div()
                            .pr(px(16.))
                            .flex()
                            .gap(px(20.))
                            .child("−")
                            .child("□")
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.chat(window, cx))
                            .when_some(self.overlay.as_ref(), |view, overlay| {
                                view.child(overlay.clone())
                            })
                            .when_some(self.working_pill(), |view, pill| view.child(pill))
                            .when_some(self.form_notice(cx), |view, notice| view.child(notice))
                            .when_some(self.tray_view(cx), |view, tray| view.child(tray))
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
            .when_some(self.modal_view(window, cx), |view, modal| view.child(modal));
        root.when_some(row_probe, |view, probe| view.child(probe))
    }
}

pub fn run(args: Args) {
    application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::sync_system_appearance(None, cx);
            match std::env::var("OPENCODE_GPUI_THEME").as_deref() {
                Ok("dark") => Theme::change(ThemeMode::Dark, None, cx),
                Ok("light") => Theme::change(ThemeMode::Light, None, cx),
                _ => {}
            }
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("OpenCode".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
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
        });
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

    use super::{
        ApiHandle, COMPLEX_MARKDOWN_PREVIEW, Client, ClipboardItem, Image, ImageFormat,
        MarkdownBlock, Modal, RowHeightCache, RowLayoutStamp, SESSION_PICKER_LIMIT,
        TRANSCRIPT_ROW_STYLE_REVISION, TabAttention, Theme, ThemeMode, UiEvent,
        VirtualListScrollHandle, filter_all_sessions, filter_levels, filter_models,
        filter_new_session_projects, filter_tab_sessions, fuzzy_score, markdown_blocks, model,
        needs_new_connection, new_session_choice, picker_list_height, reorder_tab_ids,
        safe_connection_error, splice_transcript, sticky_user_index, tab_indicator, tab_number_key,
        unread_on_server_switch, update_session_projection,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AppContext, Bounds, Focusable, Role, TestAppContext, WindowBounds, WindowOptions, point,
        px, size,
    };
    use opencode_gpui::model::RunStatus;
    use opencode_gpui::persist::PersistedState;
    use opencode_gpui::protocol;
    use serde_json::json;

    fn inbox_enqueued(session: &str, id: &str, text: &str) -> UiEvent {
        UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
            directory: Some("/repo".into()),
            payload: json!({
                "id": format!("evt_{id}"), "created": 1234,
                "type": "session.inbox.enqueued",
                "data": { "sessionID": session, "inboxID": id,
                    "item": { "type": "user", "payload": { "text": text },
                        "delivery": "steer" } }
            }),
        })
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

    #[gpui_kit::test]
    fn client_stream_event_uses_tail_projection_hint(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                snapshot(
                    client.conversations.get_mut(&session).unwrap(),
                    (0..1_000)
                        .map(|index| {
                            json!({
                                "id": format!("old_{index}"), "type": "user",
                                "time": { "created": index + 1 }, "text": "Older"
                            })
                        })
                        .collect(),
                );
                client.update_transcript(&session);
                let delta = |ordinal: u8, text: &str| {
                    UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                        directory: Some("/repo".into()),
                        payload: json!({
                            "id": format!("evt_{ordinal:026}"), "created": 2000,
                            "type": "session.text.delta", "data": {
                                "sessionID": session, "assistantMessageID": "stream_tail",
                                "ordinal": 0, "delta": text
                            }
                        }),
                    })
                };
                client.handle_live_event(delta(1, "First"), cx);
                super::SPAN_MATCHES.with(|count| count.set(0));
                client.handle_live_event(delta(2, " second"), cx);
                assert_eq!(super::SPAN_MATCHES.with(|count| count.get()), 0);
                assert!(
                    client.transcript[&session]
                        .last()
                        .unwrap()
                        .body
                        .contains("First second")
                );
            });
        })
        .unwrap();
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
    fn streaming_tail_of_tool_heavy_message_keeps_unchanged_row_heights() {
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
        let stamp = RowLayoutStamp {
            width: px(500.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: conversation.cache_epoch(),
        };
        let mut heights = RowHeightCache::default();
        assert_eq!(heights.missing(stamp, &rows).len(), rows.len());
        for row in &rows {
            heights.record(stamp, row.key.clone(), row.render_revision(), px(60.));
        }

        assert!(conversation.apply_event(&json!({
            "id": "evt_00000000000000000000000001", "created": 2000,
            "type": "session.text.delta", "data": {
                "sessionID": "ses_a", "assistantMessageID": "assistant",
                "ordinal": 1, "delta": " streamed"
            }
        })));
        let change = splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
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
        heights.retain_replaced(&rows, &change);
        assert_eq!(heights.missing(stamp, &rows), changed);
        assert_eq!(heights.stale.len(), 1);

        // An unrelated replacement snapshot with the same IDs must not reuse
        // the previous epoch's cached heights or retained row revisions.
        snapshot(
            &mut conversation,
            vec![json!({
                "id": "assistant", "type": "assistant", "time": { "created": 1 },
                "agent": "build", "content": [ { "type": "text", "text": "Answer" } ]
            })],
        );
        splice_transcript(&conversation, &mut rows, &mut spans).unwrap();
        let next_stamp = RowLayoutStamp {
            epoch: conversation.cache_epoch(),
            ..stamp
        };
        assert_eq!(heights.missing(next_stamp, &rows), vec![0]);
    }

    #[test]
    fn ten_thousand_offscreen_images_are_registered_without_decoding() {
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
                old_rows: Vec::new(),
                old_images: Default::default(),
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
            old_rows: Vec::new(),
            old_images: Default::default(),
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
                old_rows: Vec::new(),
                old_images: Default::default(),
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
                old_rows: Vec::new(),
                old_images: Default::default(),
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
            old_rows: Vec::new(),
            old_images: Default::default(),
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
                old_rows: Vec::new(),
                old_images: Default::default(),
            },
        );
        assert_eq!(cache.entries.len(), 1);
        cache.update(
            "ses_a",
            &[],
            &super::ProjectionChange {
                range: 0..0,
                removed_images: vec![(row.key.clone(), 0)],
                old_rows: Vec::new(),
                old_images: Default::default(),
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

    #[test]
    fn stale_row_keeps_exact_image_when_url_changes_and_clears_on_invalidation() {
        let stamp = RowLayoutStamp {
            width: px(746.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: 1,
        };
        let mut old = cache_row("image", "old caption");
        old.images = vec!["data:image/png;base64,aGVsbG8=".into()];
        let mut images = super::ImageCache::default();
        images.update(
            "ses_a",
            std::slice::from_ref(&old),
            &super::ProjectionChange {
                range: 0..1,
                removed_images: Vec::new(),
                old_rows: Vec::new(),
                old_images: Default::default(),
            },
        );
        let old_image = images.get("ses_a", &old, 0).unwrap().clone();
        let mut cache = RowHeightCache::default();
        cache.missing(stamp, std::slice::from_ref(&old));
        cache.record(stamp, old.key.clone(), old.render_revision(), px(180.));

        let mut new = old.clone();
        new.render_revision += 1;
        new.body = "new caption".into();
        new.images[0] = "data:image/png;base64,d29ybGQ=".into();
        let mut change = super::ProjectionChange {
            range: 0..1,
            removed_images: vec![(old.key.clone(), 0)],
            old_rows: vec![old.clone()],
            old_images: Default::default(),
        };
        change
            .old_images
            .insert((old.key.clone(), 0), old_image.clone());
        images.update("ses_a", std::slice::from_ref(&new), &change);
        cache.retain_replaced(std::slice::from_ref(&new), &change);
        let stale = &cache.stale[&old.key];
        assert_eq!(stale.row, old);
        assert_eq!(stale.height, px(180.));
        assert!(std::sync::Arc::ptr_eq(
            stale.images[0].as_ref().unwrap(),
            &old_image
        ));
        assert!(!std::sync::Arc::ptr_eq(
            images.get("ses_a", &new, 0).unwrap(),
            &old_image
        ));
        // Prepending another message must not discard this offscreen stale row.
        let prepended = cache_row("earlier", "earlier");
        cache.retain_replaced(
            &[prepended, new.clone()],
            &super::ProjectionChange {
                range: 0..1,
                removed_images: Vec::new(),
                old_rows: Vec::new(),
                old_images: Default::default(),
            },
        );
        assert_eq!(cache.stale.len(), 1);
        cache.retain_replaced(
            &[],
            &super::ProjectionChange {
                range: 0..0,
                removed_images: Vec::new(),
                old_rows: vec![new.clone()],
                old_images: Default::default(),
            },
        );
        assert!(cache.stale.is_empty());
        cache.retain_replaced(std::slice::from_ref(&new), &change);
        assert_eq!(cache.stale.len(), 1);
        cache.missing(
            RowLayoutStamp {
                width: px(700.),
                ..stamp
            },
            std::slice::from_ref(&new),
        );
        assert!(cache.stale.is_empty());
        cache.missing(stamp, std::slice::from_ref(&old));
        cache.record(stamp, old.key.clone(), old.render_revision(), px(180.));
        cache.retain_replaced(std::slice::from_ref(&new), &change);
        cache.record(stamp, new.key.clone(), new.render_revision(), px(200.));
        assert!(cache.stale.is_empty());
    }

    #[test]
    fn superseding_tokens_keep_the_last_measured_revision_until_layout() {
        let stamp = RowLayoutStamp {
            width: px(746.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: 1,
        };
        let old = cache_row("stream", "measured");
        let mut cache = RowHeightCache::default();
        cache.missing(stamp, std::slice::from_ref(&old));
        cache.record(stamp, old.key.clone(), 0, px(88.));
        let mut current = old.clone();
        for revision in 1..=4 {
            let mut next = current.clone();
            next.render_revision = revision;
            next.body.push_str(" more");
            cache.retain_replaced(
                &[next.clone()],
                &super::ProjectionChange {
                    range: 0..1,
                    removed_images: Vec::new(),
                    old_rows: vec![current],
                    old_images: Default::default(),
                },
            );
            let stale = &cache.stale[&old.key];
            assert_eq!(stale.row, old);
            assert_eq!(stale.height, px(88.));
            current = next;
        }
        cache.record(
            stamp,
            current.key.clone(),
            current.render_revision(),
            px(130.),
        );
        assert!(cache.stale.is_empty());
        assert_eq!(cache.height(&current), Some(px(130.)));
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
    fn row_height_planner_bounds_cold_tail_and_preserves_semantic_keys() {
        let stamp = RowLayoutStamp {
            width: px(746.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: 1,
        };
        let mut cache = RowHeightCache::default();
        let mut rows: Vec<_> = (0..10_000)
            .map(|index| cache_row(&format!("message-{index}"), "a line of Markdown"))
            .collect();
        assert_eq!(
            cache.missing_tail(stamp, &rows, 32, px(1500.)),
            (9968..10_000).collect::<Vec<_>>()
        );
        for row in rows.iter().skip(9968) {
            cache.record(stamp, row.key.clone(), 0, px(36.));
        }
        assert!(
            cache.missing_tail(stamp, &rows, 32, px(500.)).is_empty(),
            "covered viewport must not trigger offscreen measurement"
        );
        assert_eq!(
            cache.missing_tail(stamp, &rows, 32, px(1500.)),
            (9936..9968).collect::<Vec<_>>()
        );
        rows.insert(0, cache_row("prepended", "older page"));
        assert_eq!(
            cache.missing_tail(stamp, &rows, 2, px(1500.)),
            vec![9967, 9968]
        );
        let last = rows.last_mut().unwrap();
        last.render_revision += 1;
        assert_eq!(
            cache.missing_tail(stamp, &rows, 32, px(500.)),
            vec![10_000],
            "streaming tail remeasures only the changed row"
        );
        assert_eq!(
            cache.missing_tail(stamp, &rows, 2, px(1500.)),
            vec![9968, 10_000]
        );
        let changed = RowLayoutStamp {
            dark: true,
            ..stamp
        };
        assert_eq!(
            cache.missing_tail(changed, &rows, 2, px(1500.)),
            vec![9999, 10_000]
        );
    }

    #[test]
    fn row_height_cache_retains_prepend_and_invalidates_stream_snapshot_width_theme_and_style() {
        let stamp = RowLayoutStamp {
            width: px(746.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: 7,
        };
        let mut cache = RowHeightCache::default();
        let old = cache_row("old", "wrapped Markdown");
        assert_eq!(cache.missing(stamp, std::slice::from_ref(&old)), vec![0]);
        cache.record(stamp, old.key.clone(), old.render_revision(), px(88.));
        assert_eq!(
            cache.missing(stamp, std::slice::from_ref(&old)),
            Vec::<usize>::new()
        );
        assert_eq!(cache.height(&old), Some(px(88.)));

        let earlier = cache_row("earlier", "history");
        assert_eq!(
            cache.missing(stamp, &[earlier.clone(), old.clone()]),
            vec![0]
        );
        assert_eq!(
            cache.height(&old),
            Some(px(88.)),
            "prepend preserves the old key"
        );
        let mut streamed = old.clone();
        streamed.body.push_str(" and more text");
        streamed.render_revision += 1;
        assert_eq!(
            cache.missing(stamp, &[earlier, streamed.clone()]),
            vec![0, 1]
        );
        cache.record(
            stamp,
            streamed.key.clone(),
            streamed.render_revision(),
            px(112.),
        );
        assert_eq!(cache.height(&streamed), Some(px(112.)));

        // Snapshot replacement can reuse the same ID and revision with different
        // content; the epoch, rather than a content hash, guards that collision.
        let replaced = cache_row("old", "new snapshot");
        let snapshot = RowLayoutStamp { epoch: 8, ..stamp };
        assert_eq!(
            cache.missing(snapshot, std::slice::from_ref(&replaced)),
            vec![0]
        );
        cache.record(
            snapshot,
            replaced.key.clone(),
            replaced.render_revision(),
            px(64.),
        );
        assert_eq!(cache.height(&replaced), Some(px(64.)));
        for changed in [
            RowLayoutStamp {
                width: px(1116.),
                ..snapshot
            },
            RowLayoutStamp {
                dark: true,
                ..snapshot
            },
            RowLayoutStamp {
                style_revision: snapshot.style_revision + 1,
                ..snapshot
            },
        ] {
            assert_eq!(
                cache.missing(changed, std::slice::from_ref(&replaced)),
                vec![0]
            );
            cache.record(
                changed,
                replaced.key.clone(),
                replaced.render_revision(),
                px(80.),
            );
        }
        let mut other_session = RowHeightCache::default();
        assert_eq!(other_session.missing(stamp, &[old]), vec![0]);
    }

    #[gpui_kit::test]
    fn first_measured_virtual_frame_pins_to_the_actual_tail(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                assert_eq!(
                    client.follow_bottom.as_ref().map(|(id, _)| id),
                    Some(&client.active)
                );
                client.transcript.insert(
                    client.active.clone(),
                    (0..100)
                        .map(|index| cache_row(&format!("initial_{index}"), "Transcript text"))
                        .collect(),
                );
                client.row_heights.remove(&client.active);
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            client.read(cx).rendered_rows.borrow_mut().clear();
            window.render_frame(cx);
            let state = client.read(cx);
            assert!(state.scroll.offset().y < px(0.));
            assert!(state.rendered_rows.borrow().contains(&99));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn measured_virtual_list_only_renders_visible_rows_and_uses_exact_control_height(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let rows: Vec<_> = (0..300)
                    .map(|index| {
                        let mut row = cache_row(
                            &format!("long_{index}"),
                            "A transcript line with enough words to render.",
                        );
                        if matches!(index, 50 | 170 | 260) {
                            row.role = model::Role::User;
                            row.body = format!("Prompt {index}");
                        }
                        row
                    })
                    .collect();
                client.transcript.insert(client.active.clone(), rows);
                client.row_heights.remove(&client.active);
                client
                    .conversations
                    .get_mut(&client.active)
                    .unwrap()
                    .next_cursor = Some("older".into());
                client.scroll = VirtualListScrollHandle::new();
                client
                    .virtual_scrolls
                    .insert(client.active.clone(), client.scroll.clone());
                client.follow_bottom = None;
                cx.notify();
            });
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.simulate_next_frame(cx);
            })
            .expect("headless window stays open");
        };
        // Force one exact warm-up for this full-cache virtual-list test;
        // ordinary cold opening intentionally leaves offscreen rows unmeasured.
        cx.update_window(handle, |_, window, cx| {
            let width = window.viewport_size().width
                - px(super::SIDEBAR_WIDTH + super::TRANSCRIPT_SCROLLBAR_GUTTER);
            client.update(cx, |client, cx| {
                client.measurement_probe = Some((width, Rc::new(RefCell::new(Vec::new()))));
                cx.notify();
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client.measurement_probe = None;
                cx.notify();
            });
        })
        .unwrap();
        render(cx);
        cx.update_window(handle, |_, window, cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.entries.len(), 300);
            assert_eq!(
                cache.load_height.unwrap().1,
                window.find("load-earlier").bounds().size.height
            );
            let control = window.find("load-earlier");
            assert_eq!(control.role(), Some(Role::Button));
            assert_eq!(control.label(), Some("Load earlier messages"));
            let focus = client.history_focus.clone();
            drop(cache);
            focus.focus(window, cx);
            assert!(focus.is_focused(window));
            window.press("enter", cx);
        })
        .expect("headless window stays open");
        render(cx); // first virtual frame
        cx.update(|cx| client.read(cx).rendered_rows.borrow_mut().clear());
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let rendered = client.rendered_rows.borrow();
            assert!(
                rendered.len() < 30,
                "only a viewport plus the measuring item: {rendered:?}"
            );
            assert!(rendered.iter().all(|index| *index < 30));
            drop(rendered);
            client.rendered_rows.borrow_mut().clear();
            let rows = &client.transcript[&client.active];
            let top = client.row_heights[&client.active]
                .borrow()
                .prefix(rows, 185, true)
                .unwrap();
            client.scroll.set_offset(point(px(0.), -(top + px(5.))));
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let rendered = client.rendered_rows.borrow();
            assert!(
                rendered.iter().any(|index| *index > 170),
                "scroll reaches offscreen rows: {rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .rev()
                    .take(30)
                    .all(|index| *index == 0 || *index > 170)
            );
        });
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(
                client
                    .read(cx)
                    .sticky_user_row(window)
                    .unwrap()
                    .key
                    .message_id,
                "long_170"
            );
        })
        .unwrap();
        let anchor = cx.update(|cx| {
            client
                .read(cx)
                .transcript_anchor()
                .expect("visible identity")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.preserve_scroll = Some(anchor.clone());
                client.follow_bottom = None;
                let rows = client.transcript.get_mut(&client.active).unwrap();
                rows.splice(
                    0..0,
                    [
                        cache_row("prepended_a", "older first"),
                        cache_row("prepended_b", "older second"),
                    ],
                );
                cx.notify();
            })
        });
        for _ in 0..4 {
            render(cx);
            cx.run_until_parked();
        }
        cx.update(|cx| {
            let client = client.read(cx);
            let rows = &client.transcript[&client.active];
            let cache = client.row_heights[&client.active].borrow();
            let index = rows.iter().position(|row| row.key == anchor.key).unwrap();
            let top = cache.provisional_prefix(rows, index, true);
            assert_eq!(
                client.scroll.offset().y,
                -(top + anchor.within),
                "prepend retains row identity and local offset"
            );
        });
    }

    #[test]
    fn measured_prefix_preserves_row_identity_and_local_offset_across_prepend() {
        let stamp = RowLayoutStamp {
            width: px(746.),
            dark: false,
            style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
            epoch: 1,
        };
        let mut cache = RowHeightCache::default();
        let old = [
            cache_row("a", "first"),
            cache_row("b", "anchor"),
            cache_row("c", "last"),
        ];
        cache.missing(stamp, &old);
        for (row, height) in old.iter().zip([px(40.), px(90.), px(200.)]) {
            cache.record(stamp, row.key.clone(), row.render_revision(), height);
        }
        cache.load_height = Some((false, px(35.)));
        let old_top = cache.prefix(&old, 1, true).unwrap();
        let within = px(23.);
        let old_offset = -(old_top + within);
        let earlier = cache_row("older", "earlier");
        cache.record(
            stamp,
            earlier.key.clone(),
            earlier.render_revision(),
            px(55.),
        );
        let new = [earlier, old[0].clone(), old[1].clone(), old[2].clone()];
        let index = new.iter().position(|row| row.key == old[1].key).unwrap();
        let new_top = cache.prefix(&new, index, true).unwrap();
        let new_offset = -(new_top + within);
        assert_eq!(new_offset - old_offset, px(-55.));
        assert_eq!(new_top + new_offset, old_top + old_offset);
        assert_eq!(cache.provisional_positions(&new, true)[index], new_top);
    }

    #[gpui_kit::test]
    fn appended_visible_row_paints_on_first_frame_without_mounting_history(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let id = client.active.clone();
                let messages = (0..300)
                    .map(|index| {
                        protocol::SessionMessage::from_value(json!({
                            "id": format!("stream_{index}"), "type": "assistant",
                            "time": { "created": 1 },
                            "content": [{ "type": "text", "text": "Short answer." }]
                        }))
                    })
                    .collect::<Vec<_>>();
                client
                    .conversations
                    .get_mut(&id)
                    .unwrap()
                    .replace_from_api(&messages, None);
                client.update_transcript(&id);
                cx.notify();
            })
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };
        render(cx);
        render(cx);
        cx.update(|cx| client.read(cx).scroll.base_handle().scroll_to_bottom());
        render(cx);
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let id = client.active.clone();
                let event = json!({
                    "id": "evt_00000000000000000000000005", "created": 2000,
                    "type": "session.text.delta", "data": {
                    "sessionID": id, "assistantMessageID": "stream_300",
                        "ordinal": 0, "delta": "Newly streamed paragraph"
                    }
                });
                assert!(
                    client
                        .conversations
                        .get_mut(&id)
                        .unwrap()
                        .apply_event(&event)
                );
                client.update_transcript(&id);
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            })
        });
        render(cx);
        cx.update_window(handle, |_, window, cx| {
            let client = client.read(cx);
            let rendered = client.rendered_rows.borrow();
            assert!(
                rendered.contains(&300),
                "appended row was blank on first frame"
            );
            assert!(
                rendered.len() < 70,
                "mounted offscreen history: {rendered:?}"
            );
            let _mounted = window.find(("message", 300usize));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn streamed_replacement_renders_old_measured_row_then_corrected_row(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let id = client.active.clone();
                let messages = (0..150)
                    .map(|index| {
                        protocol::SessionMessage::from_value(json!({
                            "id": format!("stream_{index}"),
                            "type": "assistant",
                            "time": { "created": 1 },
                            "content": [{ "type": "text", "text": "Short answer." }]
                        }))
                    })
                    .collect::<Vec<_>>();
                client
                    .conversations
                    .get_mut(&id)
                    .unwrap()
                    .replace_from_api(&messages, None);
                client.update_transcript(&id);
                cx.notify();
            });
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };
        render(cx);
        render(cx);
        cx.update(|cx| client.read(cx).scroll.base_handle().scroll_to_bottom());
        render(cx);
        let mut last_height = cx.update(|cx| {
            let client = client.read(cx);
            client.row_heights[&client.active]
                .borrow()
                .height(client.transcript[&client.active].last().unwrap())
                .unwrap()
        });
        let mut previous_body = "Short answer.".to_owned();
        for token in 1..=3 {
            cx.update(|cx| {
                client.update(cx, |client, cx| {
                    let id = client.active.clone();
                    let event = json!({
                        "id": format!("evt_0000000000000000000000000{token}"),
                        "created": 2000,
                        "type": "session.text.delta",
                        "data": {
                            "sessionID": id,
                            "assistantMessageID": "stream_149",
                            "ordinal": 0,
                            "delta": " A much longer streamed paragraph with wrapping words.".repeat(10),
                        }
                    });
                    assert!(client.conversations.get_mut(&id).unwrap().apply_event(&event));
                    client.update_transcript(&id);
                    let cache = client.row_heights[&id].borrow();
                    let old = cache.stale.values().next().expect("retained measured row");
                    assert_eq!(old.height, last_height);
                    assert_eq!(old.row.body, previous_body);
                    assert_eq!(
                        cache.provisional_sizes(&client.transcript[&id], false, cache.stamp.unwrap().width)[149].height,
                        last_height,
                        "the pending revision keeps the prior exact-height slot"
                    );
                    previous_body = client.transcript[&id].last().unwrap().body.clone();
                    assert_eq!(cache.stale.len(), 1);
                    client.rendered_rows.borrow_mut().clear();
                    cx.notify();
                });
            });
            // The probe measures the new revision during layout. The virtual
            // callback must still use the old, exact-height snapshot here.
            render(cx);
            cx.update_window(handle, |_, window, cx| {
                let client = client.read(cx);
                let rendered = client.rendered_rows.borrow();
                assert!(rendered.contains(&149), "no blank streaming row");
                assert!(rendered.len() < 50, "mounted history: {rendered:?}");
                let _mounted = window.find(("message", 149usize));
            })
            .unwrap();
            cx.update(|cx| client.read(cx).rendered_rows.borrow_mut().clear());
            render(cx);
            last_height = cx
                .update_window(handle, |_, window, cx| {
                    let client = client.read(cx);
                    let rendered = client.rendered_rows.borrow();
                    assert!(rendered.contains(&149), "corrected row was not rendered");
                    assert!(
                        rendered.len() < 50,
                        "corrected frame mounted history: {rendered:?}"
                    );
                    let cache = client.row_heights[&client.active].borrow();
                    assert!(cache.stale.is_empty(), "corrected row releases snapshot");
                    let exact = cache
                        .height(client.transcript[&client.active].last().unwrap())
                        .unwrap();
                    let _mounted = window.find(("message", 149usize));
                    assert_eq!(
                        cache
                            .sizes(
                                &client.transcript[&client.active],
                                false,
                                cache.stamp.unwrap().width,
                            )
                            .unwrap()[149]
                            .height,
                        exact
                    );
                    exact
                })
                .unwrap();
        }
    }

    #[gpui_kit::test]
    fn measuring_an_offscreen_sticky_prompt_keeps_the_visible_semantic_row(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let width = window.viewport_size().width
                - px(super::SIDEBAR_WIDTH + super::TRANSCRIPT_SCROLLBAR_GUTTER);
            client.update(cx, |client, cx| {
                let mut rows: Vec<_> = (0..1000)
                    .map(|index| cache_row(&format!("history_{index}"), "Short response."))
                    .collect();
                rows[200].role = model::Role::User;
                rows[200].body = "A very long user prompt.\n\n".repeat(100);
                client.transcript.insert(client.active.clone(), rows);
                let stamp = RowLayoutStamp {
                    width,
                    dark: client.dark,
                    style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
                    epoch: client.conversations[&client.active].cache_epoch(),
                };
                let mut cache = RowHeightCache::default();
                cache.prepare_stamp(stamp);
                for row in &client.transcript[&client.active][490..530] {
                    cache.record(stamp, row.key.clone(), 0, px(32.));
                }
                let top = cache.provisional_prefix(&client.transcript[&client.active], 500, false);
                client
                    .row_heights
                    .insert(client.active.clone(), Rc::new(RefCell::new(cache)));
                client.scroll = VirtualListScrollHandle::new();
                let offset = point(px(0.), -(top + px(5.)));
                client.scroll.set_offset(offset);
                client.follow_bottom = None;
                client
                    .safe_scroll
                    .insert(client.active.clone(), (stamp, offset));
                client.safe_anchors.insert(
                    client.active.clone(),
                    (
                        stamp,
                        super::TranscriptAnchor {
                            session: client.active.clone(),
                            key: client.transcript[&client.active][500].key.clone(),
                            within: px(5.),
                            offset,
                        },
                    ),
                );
                cx.notify();
            });
        })
        .unwrap();
        let previous = cx.update(|cx| client.read(cx).scroll.offset().y);
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            let rows = &current.transcript[&current.active];
            let cache = current.row_heights[&current.active].borrow();
            assert!(
                cache
                    .height(&rows[200])
                    .is_some_and(|height| height > px(500.))
            );
            assert!(current.scroll.offset().y < previous - px(400.));
            assert_eq!(current.transcript_anchor().unwrap().key, rows[500].key);
        });
    }

    #[gpui_kit::test]
    fn resize_measures_tall_semantic_anchor_before_admitting_its_viewport(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let width = window.viewport_size().width
                - px(super::SIDEBAR_WIDTH + super::TRANSCRIPT_SCROLLBAR_GUTTER);
            client.update(cx, |client, cx| {
                let mut rows: Vec<_> = (0..1000)
                    .map(|index| cache_row(&format!("history_{index}"), "Short answer."))
                    .collect();
                rows[500].body =
                    "A long paragraph that must wrap across the viewport.\n\n".repeat(200);
                client.transcript.insert(client.active.clone(), rows);
                let stamp = RowLayoutStamp {
                    width,
                    dark: client.dark,
                    style_revision: TRANSCRIPT_ROW_STYLE_REVISION,
                    epoch: client.conversations[&client.active].cache_epoch(),
                };
                let mut cache = RowHeightCache::default();
                cache.prepare_stamp(stamp);
                for (index, row) in client.transcript[&client.active].iter().enumerate() {
                    cache.record(
                        stamp,
                        row.key.clone(),
                        0,
                        if index == 500 { px(1500.) } else { px(32.) },
                    );
                }
                client
                    .row_heights
                    .insert(client.active.clone(), Rc::new(RefCell::new(cache)));
                client.scroll = VirtualListScrollHandle::new();
                client.scroll.set_offset(point(px(0.), px(-17_000.)));
                client.follow_bottom = None;
                client
                    .safe_scroll
                    .insert(client.active.clone(), (stamp, client.scroll.offset()));
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
            window.resize(size(px(1130.), px(900.)));
        })
        .unwrap();
        for _ in 0..4 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                let painted = current.rendered_rows.borrow();
                assert!(
                    painted.len() < 256,
                    "tall-row resize mounted history: {painted:?}"
                );
                assert!(
                    painted.contains(&500),
                    "tall anchor was not presented: {painted:?}"
                );
                let anchor = window.find(("message", 500usize)).bounds();
                let viewport = current.scroll.bounds();
                assert!(
                    anchor.origin.y < viewport.origin.y + viewport.size.height
                        && anchor.origin.y + anchor.size.height > viewport.origin.y,
                    "tall row was constructed but missed viewport: {anchor:?} vs {viewport:?}"
                );
            })
            .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            let rows = &current.transcript[&current.active];
            let cache = current.row_heights[&current.active].borrow();
            assert!(
                cache
                    .height(&rows[500])
                    .is_some_and(|height| height > px(1000.))
            );
            assert_eq!(
                current.scroll.offset().y,
                -(cache.provisional_prefix(rows, 500, false) + px(1000.)),
                "the old within-row offset must remain in the tall row"
            );
        });
    }

    #[gpui_kit::test]
    fn early_scroll_before_first_exact_tail_stays_bounded(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.transcript.insert(
                    client.active.clone(),
                    (0..1000)
                        .map(|index| cache_row(&format!("history_{index}"), "A line."))
                        .collect(),
                );
                client.row_heights.remove(&client.active);
                client.follow_bottom = Some((client.active.clone(), client.scroll.offset()));
                client.scroll.set_offset(point(px(0.), px(-30_000.)));
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                let painted = current.rendered_rows.borrow();
                assert!(
                    painted.len() < 256,
                    "early scroll mounted all history: {painted:?}"
                );
            })
            .unwrap();
        }
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, _| {
                let stamp = client.row_heights[&client.active].borrow().stamp.unwrap();
                let old = client.transcript[&client.active][100].key.clone();
                client.safe_scroll.remove(&client.active);
                client.pending_jump = Some(super::PendingTranscriptJump {
                    stamp,
                    anchor: super::TranscriptAnchor {
                        session: client.active.clone(),
                        key: old.clone(),
                        within: px(0.),
                        offset: client.scroll.offset(),
                    },
                });
                client.scroll.set_offset(point(px(0.), px(-40_000.)));
                client.guard_unmeasured_scroll(window);
                let newer = &client.pending_jump.as_ref().unwrap().anchor;
                assert_ne!(
                    newer.key, old,
                    "user scroll must supersede the pending resize target"
                );
                assert_eq!(newer.offset, client.scroll.offset());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn partial_prepend_at_load_control_keeps_a_bounded_old_first_row(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.transcript.insert(
                    client.active.clone(),
                    (0..400)
                        .map(|index| cache_row(&format!("history_{index}"), "A short response."))
                        .collect(),
                );
                client
                    .conversations
                    .get_mut(&client.active)
                    .unwrap()
                    .next_cursor = Some("older".into());
                client.row_heights.remove(&client.active);
                client.scroll = VirtualListScrollHandle::new();
                client.follow_bottom = None;
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let anchor = client
                    .transcript_anchor()
                    .expect("load control anchors first row");
                assert_eq!(anchor.key.message_id, "history_0");
                assert!(anchor.within < px(0.));
                client.preserve_scroll = Some(anchor);
                client.transcript.get_mut(&client.active).unwrap().splice(
                    0..0,
                    (0..80).map(|index| cache_row(&format!("older_{index}"), "Older response.")),
                );
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let rendered = client.read(cx).rendered_rows.borrow();
                assert!(
                    rendered.len() < 256,
                    "prepend mounted whole history: {rendered:?}"
                );
                assert!(
                    rendered.contains(&80),
                    "old first row disappeared: {rendered:?}"
                );
            })
            .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            assert!(
                current.preserve_scroll.is_some(),
                "history remains partly measured"
            );
            current.scroll.set_offset(point(px(0.), px(-15_000.)));
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                assert!(
                    current.preserve_scroll.is_none(),
                    "user movement cancels the anchor"
                );
                let rendered = current.rendered_rows.borrow();
                assert!(
                    rendered.len() < 256,
                    "scroll after prepend remounted history: {rendered:?}"
                );
            })
            .unwrap();
        }
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let anchor = client.transcript_anchor().expect("visible semantic row");
                client.preserve_scroll = Some(anchor.clone());
                client
                    .transcript
                    .get_mut(&client.active)
                    .unwrap()
                    .retain(|row| row.key != anchor.key);
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let current = client.read(cx);
            assert!(
                current.preserve_scroll.is_none(),
                "deleted anchor was not retired"
            );
            let rendered = current.rendered_rows.borrow();
            assert!(
                rendered.len() < 256,
                "deleted anchor remounted history: {rendered:?}"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    #[ignore = "manual headless CPU profile; compare 1k and 10k before setting a release budget"]
    fn profile_large_streaming_transcript_frames(cx: &mut TestAppContext) {
        use std::time::Instant;

        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        let count: usize = std::env::var("TRANSCRIPT_PROFILE_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10_000);
        let complex = std::env::var_os("TRANSCRIPT_PROFILE_COMPLEX").is_some();
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let messages = (0..count)
                    .map(|index| {
                        let entry = if complex && index % 13 == 0 {
                            json!({
                                "id": format!("history_{index}"), "type": "assistant",
                                "time": { "created": index + 1 },
                                "content": [{ "type": "text", "text":
                                    "# Step\n\n- First **bold** line\n- Second `code` line\n\n```rust\nfn main() {}\n```"
                                }]
                            })
                        } else {
                            json!({
                            "id": format!("history_{index}"), "type": "user",
                            "time": { "created": index + 1 }, "text": "Earlier conversation"
                            })
                        };
                        protocol::SessionMessage::from_value(entry)
                    })
                    .collect::<Vec<_>>();
                client
                    .conversations
                    .get_mut(&session)
                    .unwrap()
                    .replace_from_api(&messages, None);
                client.update_transcript(&session);
                cx.notify();
            })
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update(|cx| client.read(cx).scroll.base_handle().scroll_to_bottom());
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        let mut samples = Vec::new();
        for index in 0..50 {
            let start = Instant::now();
            cx.update(|cx| {
                client.update(cx, |client, cx| {
                let event = UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                    directory: Some("/repo".into()),
                    payload: json!({
                        "id": format!("evt_{index:026}"), "created": 20000 + index,
                        "type": "session.text.delta", "data": {
                            "sessionID": client.active.clone(), "assistantMessageID": "stream_tail",
                            "ordinal": 0, "delta": " one more token"
                        }
                    }),
                });
                client.handle_live_event(event, cx);
            })
            });
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        println!(
            "rows={count} complex={complex} 50 streamed updates+headless frames: median={:?} p95={:?} max={:?}",
            samples[25], samples[47], samples[49]
        );
    }

    #[gpui_kit::test]
    fn cold_ten_thousand_row_transcript_measures_a_bounded_tail_first(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.transcript.insert(
                    client.active.clone(),
                    (0..10_000)
                        .map(|index| cache_row(&format!("history_{index}"), "A short answer."))
                        .collect(),
                );
                client.row_heights.remove(&client.active);
                client.follow_bottom = Some((client.active.clone(), client.scroll.offset()));
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let current = client.read(cx);
            let cache = current.row_heights[&current.active].borrow();
            assert!(
                cache.layouts <= super::TRANSCRIPT_MEASURE_BATCH * 2,
                "first paint measured {} rows",
                cache.layouts
            );
            let rendered = current.rendered_rows.borrow();
            assert!(
                rendered.len() <= super::TRANSCRIPT_MEASURE_BATCH * 2,
                "first paint: {rendered:?}"
            );
            assert!(
                rendered.contains(&9999),
                "painted tail is present on first frame"
            );
        })
        .unwrap();
        cx.update(|cx| client.read(cx).rendered_rows.borrow_mut().clear());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let current = client.read(cx);
            let cache = current.row_heights[&current.active].borrow();
            assert!(
                cache.layouts <= super::TRANSCRIPT_MEASURE_BATCH * 4,
                "unbounded second layout"
            );
            assert!(
                current.rendered_rows.borrow().contains(&9999),
                "bottom row is mounted"
            );
            let _mounted = window.find(("message", 9999usize));
        })
        .unwrap();
        let settled_layouts = cx.update(|cx| {
            let current = client.read(cx);
            current.row_heights[&current.active].borrow().layouts
        });
        for _ in 0..5 {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            assert_eq!(
                current.row_heights[&current.active].borrow().layouts,
                settled_layouts,
                "idle cold open must not measure the other 10,000 rows"
            );
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                assert!(
                    client.follow_bottom.is_some(),
                    "cold measurement still in progress"
                );
                client
                    .transcript
                    .get_mut(&client.active)
                    .unwrap()
                    .push(cache_row("history_10000", "Fresh reply at the tail."));
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let current = client.read(cx);
            let painted = current.rendered_rows.borrow();
            assert!(
                painted.len() < 256,
                "append remounted the history: {painted:?}"
            );
            assert!(
                painted.contains(&9999),
                "old tail disappeared before new row was measured"
            );
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(client.read(cx).rendered_rows.borrow().contains(&10_000));
        })
        .unwrap();
        let before_stream = cx.update(|cx| {
            let current = client.read(cx);
            current.row_heights[&current.active].borrow().layouts
        });
        for _ in 0..20 {
            cx.update(|cx| {
                client.update(cx, |client, cx| {
                    let tail = client
                        .transcript
                        .get_mut(&client.active)
                        .unwrap()
                        .last_mut()
                        .unwrap();
                    tail.body.push_str(" One more token.");
                    tail.render_revision += 1;
                    cx.notify();
                });
            });
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            let measured = current.row_heights[&current.active].borrow().layouts - before_stream;
            assert!(
                measured <= 40,
                "streaming remeasured {measured} offscreen rows"
            );
        });
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, _| {
                client.scroll.set_offset(point(px(0.), px(-300_000.)));
                client.guard_unmeasured_scroll(window);
                assert!(client.pending_jump.is_some());
                let safe = client.safe_scroll[&client.active].1;
                client.scroll.set_offset(point(safe.x, safe.y + px(100.)));
                client.guard_unmeasured_scroll(window);
                assert!(
                    client.pending_jump.is_none(),
                    "returning to a measured range cancels the jump"
                );
                client.guard_unmeasured_scroll(window);
                assert!(client.pending_jump.is_none(), "canceled jump resumed");
            });
        })
        .unwrap();
        cx.update(|cx| {
            let current = client.read(cx);
            assert!(current.safe_scroll.contains_key(&current.active));
            current.scroll.set_offset(point(px(0.), px(-300_000.)));
            current.rendered_rows.borrow_mut().clear();
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                let rows = &current.transcript[&current.active];
                let cache = current.row_heights[&current.active].borrow();
                let viewport = cache.provisional_viewport(
                    rows,
                    false,
                    current.scroll.offset(),
                    current.scroll.bounds().size.height,
                );
                assert!(cache.viewport_ready(rows, false, false, viewport.clone()));
                let painted = current.rendered_rows.borrow();
                assert!(painted.len() < 256, "unbounded jump frame: {painted:?}");
                assert!(
                    painted.iter().any(|index| viewport.contains(index)),
                    "blank visible jump frame at {viewport:?}: {painted:?}"
                );
            })
            .unwrap();
        }
        cx.update(|cx| {
            let current = client.read(cx);
            let cache = current.row_heights[&current.active].borrow();
            assert!(
                cache.layouts < 600,
                "jump measured unbounded history: {}",
                cache.layouts
            );
            assert!(current.pending_jump.is_none(), "jump did not settle");
            let rendered = current.rendered_rows.borrow();
            assert!(
                rendered.len() < 256,
                "jump mounted all history: {rendered:?}"
            );
            assert!(
                rendered.iter().any(|index| (4000..6000).contains(index)),
                "no mid-history rows painted: {rendered:?}"
            );
        });
        let changed_index = cx.update(|cx| {
            let current = client.read(cx);
            let cache = current.row_heights[&current.active].borrow();
            cache
                .provisional_viewport(
                    &current.transcript[&current.active],
                    false,
                    current.scroll.offset(),
                    current.scroll.bounds().size.height,
                )
                .start
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let row = &mut client.transcript.get_mut(&client.active).unwrap()[changed_index];
                row.render_revision += 1;
                row.body.push_str(" Newly streamed text.");
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let painted = client.read(cx).rendered_rows.borrow();
            assert!(
                painted.len() < 256,
                "visible update remounted history: {painted:?}"
            );
            assert!(painted.contains(&changed_index));
        })
        .unwrap();
        cx.update_window(handle, |_, window, _| {
            window.resize(size(px(1130.), px(900.)));
        })
        .unwrap();
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                let painted = current.rendered_rows.borrow();
                assert!(painted.len() < 256, "resize mounted history: {painted:?}");
                assert!(
                    painted.iter().any(|index| (4000..6000).contains(index)),
                    "resize lost the visible anchor: {painted:?}"
                );
            })
            .unwrap();
        }
        let anchored = cx.update(|cx| {
            let current = client.read(cx);
            current
                .transcript_anchor()
                .expect("measured mid-history anchor")
                .key
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.preserve_scroll = client.transcript_anchor();
                assert!(client.preserve_scroll.is_some());
                let history = (0..80)
                    .map(|index| cache_row(&format!("older_{index}"), "An earlier response."));
                client
                    .transcript
                    .get_mut(&client.active)
                    .unwrap()
                    .splice(0..0, history);
                client.rendered_rows.borrow_mut().clear();
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                client.read(cx).rendered_rows.borrow_mut().clear();
                window.render_frame(cx);
                let current = client.read(cx);
                let painted = current.rendered_rows.borrow();
                assert!(
                    painted.len() < 256,
                    "prepend mounted all history: {painted:?}"
                );
                assert!(
                    current
                        .transcript_anchor()
                        .is_some_and(|anchor| anchor.key == anchored),
                    "prepend lost the semantic viewport anchor"
                );
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn long_stream_remeasures_one_row_without_remounting_history(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.transcript.insert(
                    client.active.clone(),
                    (0..300)
                        .map(|i| {
                            let mut row = cache_row(&format!("history_{i}"), "previous message");
                            if i == 170 {
                                row.role = model::Role::User;
                            }
                            row
                        })
                        .collect(),
                );
                client.row_heights.remove(&client.active);
                cx.notify();
            });
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.simulate_next_frame(cx);
            })
            .unwrap();
        };
        // Explicitly warm the full list; ordinary cold opening now leaves
        // offscreen history unmeasured until navigation requests it.
        cx.update_window(handle, |_, window, cx| {
            let width = window.viewport_size().width
                - px(super::SIDEBAR_WIDTH + super::TRANSCRIPT_SCROLLBAR_GUTTER);
            client.update(cx, |client, cx| {
                client.measurement_probe = Some((width, Rc::new(RefCell::new(Vec::new()))));
                cx.notify();
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client.measurement_probe = None;
                cx.notify();
            });
        })
        .unwrap();
        render(cx);
        let baseline_layouts = cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.entries.len(), 300);
            cache.layouts
        });
        for token in 1..=12 {
            cx.update(|cx| {
                client.update(cx, |client, cx| {
                    let tail = client
                        .transcript
                        .get_mut(&client.active)
                        .unwrap()
                        .last_mut()
                        .unwrap();
                    tail.body
                        .push_str(" More streamed Markdown with wrapping words.");
                    tail.render_revision += 1;
                    client.rendered_rows.borrow_mut().clear();
                    cx.notify();
                });
            });
            render(cx);
            cx.update(|cx| {
                let client = client.read(cx);
                let rendered = client.rendered_rows.borrow();
                assert!(
                    rendered.len() < 50,
                    "token {token} mounted history: {rendered:?}"
                );
                assert!(
                    rendered.iter().all(|index| *index == 0 || *index > 280),
                    "only near-tail rows were requested"
                );
                assert_eq!(
                    client.row_heights[&client.active].borrow().layouts,
                    baseline_layouts + token
                );
            });
            cx.update_window(handle, |_, window, cx| {
                assert_eq!(
                    client
                        .read(cx)
                        .sticky_user_row(window)
                        .unwrap()
                        .key
                        .message_id,
                    "history_170",
                    "the offscreen sticky prompt persists during tail remeasurement"
                );
            })
            .unwrap();
            render(cx);
        }
    }

    #[gpui_kit::test]
    fn stream_follows_true_bottom_of_tall_tail_but_preserves_scrolled_up_position(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let mut rows: Vec<_> = (0..30)
                    .map(|i| cache_row(&format!("stream_{i}"), "short row"))
                    .collect();
                rows.push(cache_row(
                    "tail",
                    &"A wrapped paragraph with words.\n\n".repeat(90),
                ));
                client.transcript.insert(client.active.clone(), rows);
                client.row_heights.remove(&client.active);
                cx.notify();
            })
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.simulate_next_frame(cx);
            })
            .unwrap();
        };
        render(cx);
        render(cx);
        cx.update(|cx| client.read(cx).scroll.base_handle().scroll_to_bottom());
        render(cx);
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            assert!(client.scroll.max_offset().y > px(500.));
            assert_eq!(client.scroll.offset().y, -client.scroll.max_offset().y);
            assert!(
                client.row_heights[&client.active]
                    .borrow()
                    .height(client.transcript[&client.active].last().unwrap())
                    .unwrap()
                    > client.scroll.bounds().size.height
            );
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let id = client.active.clone();
                client.prepare_follow_bottom(&id);
                let tail = client.transcript.get_mut(&id).unwrap().last_mut().unwrap();
                tail.body
                    .push_str(&"\n\nMore wrapped words in a paragraph.".repeat(35));
                tail.render_revision += 1;
                assert!(client.follow_bottom.is_some());
                cx.notify();
            })
        });
        // One provisional frame measures the changed tail. The corrected
        // frame must render the tail itself, not merely update scroll state
        // after an old visible range has already been painted.
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update(|cx| client.read(cx).rendered_rows.borrow_mut().clear());
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update(|cx| {
            let client = client.read(cx);
            assert!(
                client.rendered_rows.borrow().contains(&30),
                "the corrected frame must actually render the tall tail"
            );
            assert_eq!(client.scroll.offset().y, -client.scroll.max_offset().y);
        });
        cx.update(|cx| {
            let client = client.read(cx);
            assert_eq!(
                client.scroll.offset().y,
                -client.scroll.max_offset().y,
                "stream follows the real bottom, not just the last item's top"
            );
        });
        cx.update(|cx| client.read(cx).scroll.set_offset(point(px(0.), px(-120.))));
        render(cx);
        let parked = cx.update(|cx| client.read(cx).scroll.offset());
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let id = client.active.clone();
                client.prepare_follow_bottom(&id);
                assert!(client.follow_bottom.is_none());
                let tail = client.transcript.get_mut(&id).unwrap().last_mut().unwrap();
                tail.body.push_str(" more");
                tail.render_revision += 1;
                cx.notify();
            })
        });
        for _ in 0..3 {
            render(cx);
            cx.run_until_parked();
        }
        cx.update(|cx| assert_eq!(client.read(cx).scroll.offset(), parked));
    }

    #[gpui_kit::test]
    fn tab_switch_retains_previous_scroll_after_prepend_correction(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.transcript.insert(
                    client.active.clone(),
                    (0..80)
                        .map(|i| cache_row(&format!("tab_{i}"), "same row text"))
                        .collect(),
                );
                client.row_heights.remove(&client.active);
                cx.notify();
            })
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            window.simulate_next_frame(cx);
        })
        .unwrap();
        cx.update(|cx| client.read(cx).scroll.set_offset(point(px(0.), px(-550.))));
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        let (old_session, old_handle) = cx.update(|cx| {
            let client = client.read(cx);
            (client.active.clone(), client.scroll.clone())
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let anchor = client.transcript_anchor().unwrap();
                let first = &client.transcript[&client.active][0];
                let cache = client.row_heights[&client.active].clone();
                let stamp = cache.borrow().stamp.unwrap();
                let height = cache.borrow().height(first).unwrap();
                let added = cache_row("new_before_tab_switch", "same row text");
                cache.borrow_mut().record(
                    stamp,
                    added.key.clone(),
                    added.render_revision(),
                    height,
                );
                client
                    .transcript
                    .get_mut(&client.active)
                    .unwrap()
                    .insert(0, added);
                client.preserve_scroll = Some(anchor);
                cx.notify();
            })
        });
        // Correction runs before range selection, not in a delayed callback.
        // Switching tabs must retain the old handle's resolved offset.
        let retained_offset = cx.update(|cx| client.read(cx).scroll.offset());
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.select_session("ses_other".into());
                cx.notify();
            })
        });
        cx.update_window(handle, |_, window, cx| {
            window.simulate_next_frame(cx);
        })
        .unwrap();
        cx.update(|cx| {
            assert_eq!(client.read(cx).active, "ses_other");
            assert_eq!(
                old_handle.offset(),
                retained_offset,
                "switching cannot move the old tab"
            );
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.select_session(old_session.clone());
                cx.notify();
            })
        });
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        assert_eq!(
            old_handle.offset(),
            retained_offset,
            "returning to a tab retains its scroll offset"
        );
    }

    #[gpui_kit::test]
    fn runtime_row_cache_measures_only_misses_during_layout(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless transcript window")
        });
        let row = cache_row(
            "runtime",
            "A paragraph with **Markdown** that wraps in the viewport.",
        );
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client
                    .transcript
                    .insert(client.active.clone(), vec![row.clone()]);
                client.row_heights.remove(&client.active);
                cx.notify();
            });
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .expect("headless window stays open");
        };
        render(cx);
        let initial = cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.layouts, 1);
            cache.height(&row).expect("measured height")
        });
        cx.update_window(handle, |_, window, _| {
            assert_eq!(
                window.find(("message", 0usize)).bounds().size.height,
                initial
            );
        })
        .expect("headless window stays open");
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            assert_eq!(client.row_heights[&client.active].borrow().layouts, 1);
        });

        let earlier = cache_row("earlier", "older history");
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client
                    .transcript
                    .insert(client.active.clone(), vec![earlier.clone(), row.clone()]);
                cx.notify();
            });
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.layouts, 2, "only the prepended row is measured");
            assert_eq!(cache.height(&row), Some(initial));
        });

        let mut streamed = row.clone();
        streamed
            .body
            .push_str(" Additional streamed text changes the layout.");
        streamed.render_revision += 1;
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client
                    .transcript
                    .insert(client.active.clone(), vec![earlier, streamed.clone()]);
                cx.notify();
            });
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.layouts, 3, "only the streamed row is remeasured");
            assert!(cache.height(&streamed).is_some());
        });

        cx.update_window(handle, |_, window, cx| {
            window.resize(size(px(1130.), px(900.)));
            window.bounds_changed(cx);
        })
        .expect("headless window stays open");
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.layouts, 5, "both rows remeasure at the new width");
            assert_eq!(
                cache.stamp.unwrap().width,
                client.scroll.bounds().size.width - px(14.)
            );
        });
        cx.update_window(handle, |_, window, cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            for (index, row) in client.transcript[&client.active].iter().enumerate() {
                let mounted = window.find(("message", index)).bounds();
                assert_eq!(mounted.size.width, cache.stamp.unwrap().width);
                assert_eq!(cache.height(row), Some(mounted.size.height));
            }
        })
        .expect("headless window stays open");

        let was_dark = cx.update(|cx| client.read(cx).dark);
        cx.update(|cx| {
            Theme::change(
                if was_dark {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                },
                None,
                cx,
            );
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            let cache = client.row_heights[&client.active].borrow();
            assert_eq!(cache.layouts, 7, "theme change invalidates both rows");
            assert_eq!(cache.stamp.unwrap().dark, !was_dark);
        });

        let first_session = cx.update(|cx| client.read(cx).active.clone());
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.select_session("ses_other".into());
                client.transcript.insert(
                    client.active.clone(),
                    vec![cache_row("other", "second session")],
                );
                cx.notify();
            });
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            assert_eq!(client.row_heights["ses_other"].borrow().layouts, 1);
            assert_eq!(client.row_heights[&first_session].borrow().layouts, 7);
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.select_session(first_session.clone());
                cx.notify();
            });
        });
        render(cx);
        cx.update(|cx| {
            let client = client.read(cx);
            assert_eq!(client.row_heights[&first_session].borrow().layouts, 7);
        });
    }

    #[gpui_kit::test]
    fn detached_row_layout_matches_mounted_bounds_at_two_widths(cx: &mut TestAppContext) {
        let rows = [
            model::TranscriptRow {
                key: model::TranscriptRowKey {
                    message_id: "layout_prose".into(),
                    slot: model::TranscriptRowSlot::NormalAfter(None),
                },
                render_revision: 0,
                role: model::Role::Assistant,
                body: "A wrapped paragraph with **emphasis**, `inline code`, and enough words to change line breaks between narrow and wide transcript viewports. The same Markdown element must determine both the offscreen height and the mounted height. This continuation makes the narrow case wrap another time.".into(),
                images: vec![],
                time: 1,
                kind: model::TranscriptRowKind::Normal,
            },
            model::TranscriptRow {
                key: model::TranscriptRowKey {
                    message_id: "layout_code".into(),
                    slot: model::TranscriptRowSlot::NormalAfter(None),
                },
                render_revision: 0,
                role: model::Role::Assistant,
                body: "# Code example\n\n```rust\nfn example() {\n    println!(\"a line of code longer than the available space in the narrow transcript\");\n}\n```\n\n- First list item with enough text to wrap when the viewport is narrow.\n- Second item".into(),
                images: vec![],
                time: 2,
                kind: model::TranscriptRowKind::Normal,
            },
            model::TranscriptRow {
                key: model::TranscriptRowKey {
                    message_id: "layout_image".into(),
                    slot: model::TranscriptRowSlot::NormalAfter(None),
                },
                render_revision: 0,
                role: model::Role::User,
                body: "This user message includes an image and text that wraps at the narrower width, making the thumbnail part of a variable-height row.".into(),
                images: vec!["data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==".into()],
                time: 3,
                kind: model::TranscriptRowKind::Normal,
            },
        ];
        cx.update(gpui_kit::init);
        let mut prose_heights = Vec::new();
        for width in [760., 1130.] {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(width), px(900.)),
                ))),
                ..Default::default()
            };
            let (handle, client) = cx.update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    cx.new(|cx| Client::from_preview(window, cx, None))
                })
                .expect("headless transcript window")
            });
            cx.update(|cx| {
                client.update(cx, |client, cx| {
                    client
                        .transcript
                        .insert(client.active.clone(), rows.to_vec());
                    client.attachments.remove_session(&client.active);
                    client.attachments.update(
                        &client.active,
                        &rows,
                        &super::ProjectionChange {
                            range: 0..rows.len(),
                            removed_images: Vec::new(),
                            old_rows: Vec::new(),
                            old_images: Default::default(),
                        },
                    );
                    cx.notify();
                });
            });
            let actual = cx
                .update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    let content_width = client.read(cx).scroll.bounds().size.width - px(14.);
                    assert!(content_width > px(0.), "viewport must be laid out before measuring");
                    let detached_heights = Rc::new(RefCell::new(Vec::new()));
                    client.update(cx, |client, cx| {
                        client.measurement_probe = Some((content_width, detached_heights.clone()));
                        cx.notify();
                    });
                    window.render_frame(cx);
                    let detached_heights = detached_heights.borrow();
                    assert_eq!(detached_heights.len(), rows.len());
                    rows.iter()
                        .enumerate()
                        .map(|(index, _)| {
                            let mounted = window.find(("message", index)).bounds();
                            assert_eq!(mounted.size.width, content_width);
                            assert_eq!(
                                detached_heights[index], mounted.size.height,
                                "row {index} at window width {width} (content width {content_width:?})"
                            );
                            mounted.size.height
                        })
                        .collect::<Vec<_>>()
                })
                .expect("headless window stays open");
            prose_heights.push(actual[0]);
        }
        assert!(
            prose_heights[0] > prose_heights[1],
            "prose must wrap differently at the two widths"
        );
    }

    #[gpui_kit::test]
    fn permission_action_exposes_role_name_and_updates_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.select_session("ses_other".into());
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let deny = window.find("permission-Deny-per_preview");
            assert_eq!(deny.role(), Some(Role::Button));
            assert_eq!(deny.label(), Some("Deny"));
            let card = window.find("permission-card");
            assert_eq!(card.role(), Some(Role::Group));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            assert!(
                client
                    .read(cx)
                    .permission_container_focus
                    .is_focused(window)
            );
            window.press("enter", cx);
            window.press("space", cx);
            assert!(window.try_find("permission-Deny-per_preview").is_some());
            let focus = client.read(cx).permission_focus[0].clone();
            focus.focus(window, cx);
            window.press("space", cx);
            assert!(window.try_find("permission-Deny-per_preview").is_none());
        })
        .unwrap();
        cx.update(|cx| assert!(client.read(cx).permissions.is_empty()));
    }

    #[gpui_kit::test]
    fn queued_permission_gets_inert_focus_after_first_reply(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let mut second = client.permissions[0].clone();
                second.request.id = "per_second".into();
                client.permissions.push(second);
                client.select_session("ses_other".into());
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let deny_focus = client.read(cx).permission_focus[0].clone();
            deny_focus.focus(window, cx);
            client.update(cx, |client, cx| {
                client.handle_live_event(
                    super::UiEvent::PermissionReplied {
                        request_id: "per_preview".into(),
                        result: Ok(opencode_gpui::api::Settled::Done),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("permission-Deny-per_second").is_some());
            assert!(
                client
                    .read(cx)
                    .permission_container_focus
                    .is_focused(window)
            );
            window.press("enter", cx);
            window.press("space", cx);
            assert_eq!(client.read(cx).permissions.len(), 1);
            assert!(window.try_find("permission-Deny-per_second").is_some());
            deny_focus.focus(window, cx);
            window.press("space", cx);
            assert!(client.read(cx).permissions.is_empty());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn waiting_form_cancel_is_a_focusable_named_action(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let web = window.find("open-web-ui");
            assert_eq!(web.role(), Some(Role::Button));
            assert_eq!(web.label(), Some("Open web UI to answer form"));
            window.click("open-web-ui", cx);
            let cancel = window.find("cancel-form");
            assert_eq!(cancel.role(), Some(Role::Button));
            assert_eq!(cancel.label(), Some("Cancel waiting form"));
            let focus = client.read(cx).form_cancel_focus.clone();
            focus.focus(window, cx);
            assert!(focus.is_focused(window));
            // The static fixture has no API worker, but keyboard activation
            // must be handled without leaking Space into the composer.
            window.press("space", cx);
            assert!(window.try_find("cancel-form").is_some());
            let form_id = {
                let state = client.read(cx);
                state
                    .forms
                    .notice(Some(&state.active), &state.child_parents)
                    .unwrap()
                    .cancel
                    .unwrap()
                    .form_id
            };
            client.update(cx, |client, cx| {
                client.handle_live_event(
                    super::UiEvent::FormCancelled {
                        form_id,
                        result: Ok(opencode_gpui::api::Settled::Done),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            assert!(window.try_find("cancel-form").is_none());
            assert!(client.read(cx).composer.focus_handle(cx).is_focused(window));
        })
        .unwrap();
        assert_eq!(cx.opened_url().as_deref(), Some("http://127.0.0.1:4096/"));
    }

    #[gpui_kit::test]
    fn clipboard_image_keyboard_paste_uses_composer_attachment_hook(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let focus = client.read(cx).composer.focus_handle(cx);
            focus.focus(window, cx);
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                ImageFormat::Png,
                vec![137, 80, 78, 71, 13, 10, 26, 10],
            )));
            window.press("ctrl-v", cx);
            window.render_frame(cx);
            assert_eq!(client.read(cx).attachments_draft.len(), 1);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn clipboard_image_is_session_owned_and_lives_through_upload(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        let bytes = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let image = Image::from_bytes(ImageFormat::Png, bytes.clone());
        let clipboard = ClipboardItem::new_image(&image);
        let (api, receiver, _) = ApiHandle::preview();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client.api = Some(api);
                assert!(!client.paste_attachments(
                    &client.active.clone(),
                    &ClipboardItem::new_string("plain text".into()),
                    cx,
                ));
                assert!(client.paste_attachments("another-session", &clipboard, cx));
                assert!(client.attachments_draft.is_empty());
                let session = client.active.clone();
                assert!(client.paste_attachments(&session, &clipboard, cx));
                let path = client.attachments_draft[0].clone();
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert!(client.owned_pastes.contains_key(&path));
                client.send_prompt(false, cx);
                assert!(
                    path.exists(),
                    "file must survive until the worker encodes it"
                );
                let accepted = (0..8)
                    .map(|_| receiver.recv_blocking().expect("preview worker response"))
                    .find(|event| matches!(event, UiEvent::PromptAccepted { .. }))
                    .expect("prompt acceptance");
                client.handle_live_event(accepted, cx);
                assert!(
                    !path.exists(),
                    "accepted upload releases the private paste file"
                );
                assert!(!client.owned_pastes.contains_key(&path));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn late_confirmed_paste_before_restore_render_releases_ui_file_owner(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        let clipboard = ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            vec![137, 80, 78, 71, 13, 10, 26, 10],
        ));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                let session = client.active.clone();
                assert!(client.paste_attachments(&session, &clipboard, cx));
                let path = client.attachments_draft[0].clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("response lost".into()),
                    },
                    cx,
                );
                client.handle_live_event(inbox_enqueued(&session, &pending.message_id, ""), cx);
                assert!(!client.owned_pastes.contains_key(&path));
                assert!(
                    !client
                        .draft_actions
                        .iter()
                        .any(|action| matches!(action, super::DraftAction::Restore { .. }))
                );
            });
            window.render_frame(cx);
            assert!(client.read(cx).attachments_draft.is_empty());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn pending_failed_paste_restoration_survives_other_tab_cleanup(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        let clipboard = ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            vec![137, 80, 78, 71, 13, 10, 26, 10],
        ));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client.preview_api = true;
                let session = client.active.clone();
                assert!(client.paste_attachments(&session, &clipboard, cx));
                let path = client.attachments_draft[0].clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("transient failure".into()),
                    },
                    cx,
                );
                client.open_tabs.push("ses_unrelated".into());
                client.close_tab("ses_unrelated", cx);
                assert!(
                    path.exists(),
                    "queued restoration still owns the paste file"
                );
            });
            window.render_frame(cx);
            client.update(cx, |client, _| {
                let path = client.attachments_draft[0].clone();
                assert!(path.exists());
                assert!(opencode_gpui::api::check_attachments(&[path]).is_ok());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn closing_a_tab_keeps_upload_bytes_until_the_worker_finishes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        let clipboard = ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            vec![137, 80, 78, 71, 13, 10, 26, 10],
        ));
        let (api, receiver, _) = ApiHandle::preview();
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.api = Some(api);
                client.preview_api = true;
                let session = client.active.clone();
                assert!(client.paste_attachments(&session, &clipboard, cx));
                let path = client.attachments_draft[0].clone();
                let guard = client.owned_pastes[&path].clone();
                client.send_prompt(false, cx);
                client.close_tab(&session, cx);
                assert!(!client.owned_pastes.contains_key(&path));
                assert!(path.exists());
                let accepted = (0..8)
                    .map(|_| receiver.recv_blocking().expect("preview worker response"))
                    .find(|event| matches!(event, UiEvent::PromptAccepted { .. }))
                    .expect("prompt acceptance after tab close");
                client.handle_live_event(accepted, cx);
                drop(guard);
                assert!(!path.exists(), "worker releases detached file after upload");
            });
        });
    }

    #[gpui_kit::test]
    fn duplicate_attachment_chips_remove_one_named_file_at_a_time(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.attachments_draft = vec![PathBuf::from("/work/photo.png"); 2];
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let remove = window.find("remove-attachment-0");
            assert_eq!(remove.role(), Some(Role::Button));
            assert_eq!(remove.label(), Some("Remove attachment: photo.png"));
            window.click("remove-attachment-0", cx);
            assert_eq!(client.read(cx).attachments_draft.len(), 1);
            window.render_frame(cx);
            let remove = window.find("remove-attachment-0");
            assert_eq!(remove.role(), Some(Role::Button));
            window.press("space", cx);
            assert!(client.read(cx).attachments_draft.is_empty());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn waiting_tray_actions_are_named_keyboard_buttons(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let parked = client
                    .sessions
                    .iter()
                    .find(|session| session.title.starts_with("Stopped with parked"))
                    .unwrap()
                    .id
                    .clone();
                client.select_session(parked);
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let resume = window.find("resume-tray");
            assert_eq!(resume.role(), Some(Role::Button));
            assert_eq!(resume.label(), Some("Resume waiting prompts"));
            let id = client.read(cx).conversations[&client.read(cx).active].tray_items()[0]
                .id
                .clone();
            let switch = window.find(format!("switch-waiting-{id}"));
            assert_eq!(switch.role(), Some(Role::Button));
            assert!(switch.label().unwrap().starts_with("Switch to "));
            let cancel = window.find(format!("cancel-waiting-{id}"));
            assert_eq!(cancel.role(), Some(Role::Button));
            assert!(cancel.label().unwrap().starts_with("Cancel waiting prompt"));
            window.click(format!("switch-waiting-{id}"), cx);
            window.press("space", cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn settings_password_checkbox_exposes_and_changes_checked_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("settings".into())))
            })
            .expect("headless settings window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let checkbox = window.find("remember-password");
            assert_eq!(checkbox.role(), Some(Role::CheckBox));
            assert_eq!(checkbox.checked(), Some(true));
            window.click("remember-password", cx);
            assert_eq!(window.find("remember-password").checked(), Some(false));
            let connection = window.find("settings-tab-connection");
            assert_eq!(connection.role(), Some(Role::Tab));
            assert_eq!(connection.label(), Some("Connection"));
            assert_eq!(connection.selected(), Some(true));
            window.click("settings-tab-sessions", cx);
            assert_eq!(window.find("settings-tab-sessions").selected(), Some(true));
            assert_eq!(
                window.find("settings-tab-connection").selected(),
                Some(false)
            );
            let connection_focus = client.read(cx).settings_tab_focus[0].clone();
            connection_focus.focus(window, cx);
            window.render_frame(cx);
            window.press("down", cx);
            assert_eq!(window.find("settings-tab-sessions").selected(), Some(true));
            assert!(client.read(cx).settings_tab_focus[1].is_focused(window));
            window.press("up", cx);
            assert_eq!(
                window.find("settings-tab-connection").selected(),
                Some(true)
            );
            assert!(client.read(cx).settings_tab_focus[0].is_focused(window));
            window.press("enter", cx);
            assert_eq!(
                window.find("settings-tab-connection").selected(),
                Some(true)
            );
            assert!(
                client
                    .read(cx)
                    .settings
                    .server
                    .focus_handle(cx)
                    .is_focused(window)
            );
            let sessions_focus = client.read(cx).settings_tab_focus[1].clone();
            sessions_focus.focus(window, cx);
            window.render_frame(cx);
            window.press("space", cx);
            assert_eq!(window.find("settings-tab-sessions").selected(), Some(true));
            assert!(
                client
                    .read(cx)
                    .settings
                    .session_search
                    .focus_handle(cx)
                    .is_focused(window)
            );
        })
        .unwrap();
        cx.update(|cx| assert!(!client.read(cx).settings.remember_password));
    }

    #[gpui_kit::test]
    fn picker_rows_are_named_focusable_keyboard_actions(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("sessions".into())))
            })
            .expect("headless picker window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let session = client
                .read(cx)
                .sessions
                .iter()
                .find(|s| s.id != client.read(cx).active)
                .unwrap()
                .clone();
            let control = window.find(format!("session-choice-{}", session.id));
            assert_eq!(control.role(), Some(Role::Button));
            assert_eq!(
                control.label(),
                Some(format!("Open session: {}", session.title).as_str())
            );
            let focus =
                client.read(cx).picker_choice_focus[&format!("session:{}", session.id)].clone();
            focus.focus(window, cx);
            assert!(focus.is_focused(window));
            window.press("enter", cx);
            assert_eq!(client.read(cx).active, session.id);
            assert!(client.read(cx).modal.is_none());

            client.update(cx, |client, cx| {
                client.show_modal(Modal::NewSession, window, cx)
            });
            window.render_frame(cx);
            let query = client.read(cx).search.read(cx).value();
            let first = filter_new_session_projects(
                &client.read(cx).projects,
                &client.read(cx).sessions,
                Some(session.directory.as_str()),
                &query,
            )[0]
            .clone();
            let control = window.find(format!("project-choice-{}", first.1));
            assert_eq!(control.role(), Some(Role::Button));
            let focus =
                client.read(cx).picker_choice_focus[&format!("project:{}", first.1)].clone();
            focus.focus(window, cx);
            window.press("space", cx);
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn model_and_level_rows_activate_the_focused_choice(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("model".into())))
            })
            .expect("headless picker window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let option = client
                .read(cx)
                .catalog
                .models
                .iter()
                .find(|option| option.variants.len() > 1)
                .unwrap()
                .clone();
            let key = format!("model:{}:{}", option.provider_id, option.model_id);
            let control = window.find(format!(
                "pick-model-{}-{}",
                option.provider_id, option.model_id
            ));
            assert_eq!(control.role(), Some(Role::Button));
            assert_eq!(
                control.label(),
                Some(format!("Choose model: {}", option.label).as_str())
            );
            let focus = client.read(cx).picker_choice_focus[&key].clone();
            focus.focus(window, cx);
            window.press("space", cx);
            assert_eq!(
                client.read(cx).selected_model().unwrap().model_id,
                option.model_id
            );
            client.update(cx, |client, cx| client.show_modal(Modal::Level, window, cx));
            window.render_frame(cx);
            let variant = option.variants.last().unwrap().clone();
            let control = window.find(format!("pick-level-{variant}"));
            assert_eq!(control.role(), Some(Role::Button));
            let focus = client.read(cx).picker_choice_focus[&format!("level:{variant}")].clone();
            focus.focus(window, cx);
            window.press("enter", cx);
            assert_eq!(
                client.read(cx).selected_model().unwrap().variant.as_deref(),
                Some(variant.as_str())
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn composer_picker_shortcuts_open_the_corresponding_modal(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.read(cx).composer.focus_handle(cx).focus(window, cx);
            window.press("ctrl-m", cx);
            assert!(client.read(cx).modal == Some(Modal::Model));
            window.press("escape", cx);
            assert!(client.read(cx).modal.is_none());
            window.press("ctrl-/", cx);
            assert!(client.read(cx).modal == Some(Modal::Level));
            window.press("escape", cx);
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sidebar_shortcuts_navigate_rename_and_acknowledge_unread(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        let tabs = cx.update(|cx| client.read(cx).open_tabs.clone());
        assert!(tabs.len() >= 2);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.read(cx).composer.focus_handle(cx).focus(window, cx);
            window.press("alt-2", cx);
            assert_eq!(client.read(cx).active, tabs[1]);
            assert!(client.read(cx).tab_shortcut_hint);
            window.press("alt", cx);
            assert!(!client.read(cx).tab_shortcut_hint);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                client
                    .read(cx)
                    .permission_container_focus
                    .is_focused(window)
            );
            client.update(cx, |client, cx| {
                client.unread.insert(client.active.clone());
                cx.notify();
            });
            window.render_frame(cx);
            window.press("escape", cx);
            assert!(client.read(cx).unread.contains(&tabs[1]));
            window.press("ctrl-1", cx);
            assert_eq!(client.read(cx).active, tabs[0]);
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-shift-tab", cx);
            assert_eq!(client.read(cx).active, *tabs.last().unwrap());
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("f2", cx);
            assert!(client.read(cx).modal == Some(Modal::Rename));
            window.press("escape", cx);
            assert!(client.read(cx).modal.is_none());
            client.update(cx, |client, cx| {
                client.unread.insert(client.active.clone());
                cx.notify();
            });
            window.render_frame(cx);
            window.press("escape", cx);
            assert!(!client.read(cx).unread.contains(&client.read(cx).active));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sidebar_navigation_controls_are_named_buttons(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            for (id, name) in [
                ("new-session", "New session"),
                ("footer-tabs", "Open tabs"),
                ("footer-settings", "Open settings"),
            ] {
                let control = window.find(id);
                assert_eq!(control.role(), Some(Role::Button));
                assert_eq!(control.label(), Some(name));
            }
            window.click("footer-tabs", cx);
            assert!(client.read(cx).modal == Some(Modal::Sessions));
            window.press("escape", cx);
            window.click("footer-settings", cx);
            assert!(client.read(cx).modal == Some(Modal::Settings));
            window.press("escape", cx);
            window.click("new-session", cx);
            assert!(client.read(cx).modal == Some(Modal::NewSession));
        })
        .unwrap();
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
        assert_eq!(
            filter_tab_sessions(&sessions, "").len(),
            SESSION_PICKER_LIMIT
        );
        assert_eq!(filter_tab_sessions(&sessions, "rtrl")[0].id, "ses_150");
        assert_eq!(
            filter_tab_sessions(&sessions, "project-209")[0].id,
            "ses_209"
        );
        assert!(filter_tab_sessions(&sessions, "zqx").is_empty());
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

    #[gpui_kit::test]
    fn settings_sessions_keyboard_focuses_and_scrolls_to_offscreen_result(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("settings".into())))
            })
            .expect("headless settings window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                for index in 0..70 {
                    let mut session = client.sessions[0].clone();
                    session.id = format!("settings-extra-{index:02}");
                    session.title = format!("Extra {index}");
                    session.time.updated = index as u64 + 10_000;
                    client.sessions.push(session);
                }
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("settings-tab-sessions", cx);
            window.render_frame(cx);
            assert!(
                client
                    .read(cx)
                    .settings
                    .session_search
                    .focus_handle(cx)
                    .is_focused(window)
            );
            window.press("down", cx);
            assert_eq!(client.read(cx).settings_highlight, Some(0));
            let first_id = filter_all_sessions(&client.read(cx).sessions, "")[0]
                .id
                .clone();
            assert!(client.read(cx).settings_session_focus[&first_id].is_focused(window));
            window.press("up", cx);
            assert!(client.read(cx).settings_highlight.is_none());
            assert!(
                client
                    .read(cx)
                    .settings
                    .session_search
                    .focus_handle(cx)
                    .is_focused(window)
            );
            for _ in 0..35 {
                window.press("down", cx);
            }
            window.render_frame(cx);
            assert_eq!(client.read(cx).settings_highlight, Some(34));
            assert!(client.read(cx).settings_sessions_scroll.offset().y < px(0.));
            let expected = filter_all_sessions(&client.read(cx).sessions, "")[34]
                .id
                .clone();
            window.press("enter", cx);
            assert!(client.read(cx).modal.is_none());
            assert_eq!(client.read(cx).active, expected);
        })
        .unwrap();
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

    #[gpui_kit::test]
    fn connection_events_do_not_erase_keyring_warning(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.settings.warning = Some("Keyring unavailable".into());
                client.handle_live_event(
                    super::UiEvent::Connection {
                        connected: false,
                        error: Some("HTTP 401: private echoed value".into()),
                    },
                    cx,
                );
                assert!(
                    client
                        .connection_status
                        .contains("Authentication failed (401)")
                );
                assert!(!client.connection_status.contains("private echoed value"));
                client.handle_live_event(
                    super::UiEvent::Connection {
                        connected: true,
                        error: None,
                    },
                    cx,
                );
                assert_eq!(
                    client.settings.warning.as_deref(),
                    Some("Keyring unavailable")
                );
            });
        });
    }

    #[gpui_kit::test]
    fn settings_cancel_discards_draft_secrets_and_enter_applies(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("settings".into())))
            })
            .expect("headless settings window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let original = client.read(cx).settings.current.base_url.clone();
            client.update(cx, |client, cx| {
                client.settings.server.update(cx, |input, cx| {
                    input.set_value("https://discard.test", window, cx)
                });
                client.settings.password.update(cx, |input, cx| {
                    input.set_value("discarded test value", window, cx)
                });
                client.settings.client_secret.update(cx, |input, cx| {
                    input.set_value("discarded token value", window, cx)
                });
                client.settings.remember_password = false;
            });
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.press("escape", cx);
            assert!(client.read(cx).modal.is_none());
            assert_eq!(
                client.read(cx).settings.server.read(cx).value().as_ref(),
                original
            );
            assert_eq!(
                client.read(cx).settings.password.read(cx).value().as_ref(),
                ""
            );
            assert_eq!(
                client
                    .read(cx)
                    .settings
                    .client_secret
                    .read(cx)
                    .value()
                    .as_ref(),
                ""
            );
            assert!(client.read(cx).settings.remember_password);
            client.update(cx, |client, cx| {
                client.preview_api = true;
                client.show_modal(Modal::Settings, window, cx);
            });
            window.render_frame(cx);
            assert!(
                client
                    .read(cx)
                    .settings
                    .server
                    .focus_handle(cx)
                    .is_focused(window)
            );
            window.press("enter", cx);
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn created_session_response_and_sse_each_add_only_one_row(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, _, cx| {
            client.update(cx, |client, cx| {
                for (id, sse_first, newer_rename) in [
                    ("ses_sse_first", true, false),
                    ("ses_reply_first", false, false),
                    ("ses_renamed_first", true, true),
                ] {
                    let mut reply = client.sessions[0].clone();
                    reply.id = id.into();
                    reply.title = "POST title".into();
                    reply.time.created = 500;
                    reply.time.updated = 500;
                    let event = || {
                        super::UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                            directory: Some("/repo".into()),
                            payload: json!({
                                "id": format!("evt_{id}"),
                                "created": 500,
                                "type": "session.created",
                                "location": { "directory": "/repo" },
                                "data": {
                                    "sessionID": id,
                                    "slug": "new-session",
                                    "location": { "directory": "/repo" }
                                }
                            }),
                        })
                    };
                    let response = || super::UiEvent::SessionCreated {
                        request_id: 1,
                        result: Ok(reply.clone()),
                    };
                    if sse_first {
                        client.handle_live_event(event(), cx);
                        if newer_rename {
                            client.handle_live_event(
                                super::UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                                    directory: Some("/repo".into()),
                                    payload: json!({
                                        "id": format!("evt_renamed_{id}"),
                                        "created": 600,
                                        "type": "session.renamed",
                                        "location": { "directory": "/repo" },
                                        "data": { "sessionID": id, "title": "Newer title" }
                                    }),
                                }),
                                cx,
                            );
                        }
                        client.handle_live_event(response(), cx);
                    } else {
                        client.handle_live_event(response(), cx);
                        client.handle_live_event(event(), cx);
                    }
                    let matches: Vec<_> = client.sessions.iter().filter(|s| s.id == id).collect();
                    assert_eq!(matches.len(), 1, "{id} was duplicated");
                    assert_eq!(client.open_tabs.iter().filter(|tab| *tab == id).count(), 1);
                    assert_eq!(client.active, id);
                    assert_eq!(
                        matches[0].title,
                        if newer_rename {
                            "Newer title"
                        } else {
                            "POST title"
                        }
                    );
                }
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn new_session_picker_and_create_response_focus_the_current_composer(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client.show_modal(Modal::NewSession, window, cx);
            });
            window.render_frame(cx);
            assert!(client.read(cx).search.focus_handle(cx).is_focused(window));
            let old_composer = client.read(cx).composer.clone();
            client.update(cx, |client, cx| client.create_session("/repo".into(), cx));
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            assert!(old_composer.focus_handle(cx).is_focused(window));

            client.update(cx, |client, cx| {
                let mut session = client.sessions[0].clone();
                session.id = "ses_created".into();
                client.handle_live_event(
                    super::UiEvent::SessionCreated {
                        request_id: client.next_session_request_id,
                        result: Ok(session),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            let current = client.read(cx).composer.clone();
            assert_ne!(current.entity_id(), old_composer.entity_id());
            assert!(current.focus_handle(cx).is_focused(window));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn first_live_bootstrap_focuses_the_restored_session_composer(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let event = super::preview::State::new().handle(super::Command::Bootstrap {
                sessions: Vec::new(),
                directories: Vec::new(),
            });
            client.update(cx, |client, cx| {
                client.bootstrapped = false;
                client.saved_active = Some(client.active.clone());
                client.handle_live_event(event, cx);
                assert!(client.focus_composer_pending);
            });
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            assert!(client.read(cx).composer.focus_handle(cx).is_focused(window));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn first_sse_subscription_rechecks_the_bootstrap_snapshot(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.bootstrap_in_flight = true;
                client.handle_live_event(
                    super::UiEvent::Connection {
                        connected: true,
                        error: None,
                    },
                    cx,
                );
                assert!(
                    client.bootstrap_after_load,
                    "first subscribe coalesces a second snapshot"
                );
                assert!(client.refresh_open_tabs);
                client.bootstrap_after_load = false;
                client.bootstrap_in_flight = false;
                client.refresh_open_tabs = false;
                client.handle_live_event(
                    super::UiEvent::Connection {
                        connected: true,
                        error: None,
                    },
                    cx,
                );
                assert!(
                    !client.bootstrap_after_load,
                    "duplicate connected status does not loop"
                );
                assert!(!client.refresh_open_tabs);
            });
        });
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

    #[gpui_kit::test]
    fn persisted_tabs_use_the_transport_mount_root_key(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, _| {
                client.settings.current.base_url = "http://127.0.0.1:4096/api".into();
                client.persist_tabs();
                let key =
                    opencode_gpui::api::server_key(&client.settings.current.base_url).unwrap();
                assert_eq!(key, "http://127.0.0.1:4096");
                assert_eq!(
                    client.settings.persisted.servers[&key].active.as_deref(),
                    Some(client.active.as_str())
                );
                assert!(
                    !client
                        .settings
                        .persisted
                        .servers
                        .contains_key("http://127.0.0.1:4096/api")
                );
            });
        });
    }

    #[gpui_kit::test]
    fn model_picker_does_not_pin_a_displayed_default_and_ignores_stale_replies(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let displayed = client.selected_model().expect("preview default");
                let different = client
                    .catalog
                    .models
                    .iter()
                    .find(|option| {
                        option.provider_id != displayed.provider_id
                            || option.model_id != displayed.model_id
                    })
                    .expect("second preview model");
                let picked = protocol::ModelRef {
                    id: different.model_id.clone(),
                    provider_id: different.provider_id.clone(),
                    variant: None,
                };
                let (api, _receiver, _) = opencode_gpui::api::ApiHandle::preview();
                client.api = Some(api);
                client.choose_model(displayed.to_ref(), cx);
                assert_eq!(client.next_model_request_id, 0);
                assert!(client.model_switches.is_empty());
                assert!(client.focus_composer_pending);

                client.choose_model(picked.clone(), cx);
                assert_eq!(
                    client.selected_model(),
                    Some(model::ModelSelection::from_ref(&picked))
                );
                let first = client.model_switches[&client.active].request_id;
                client.choose_model(displayed.to_ref(), cx);
                let second = client.model_switches[&client.active].request_id;
                assert!(second > first);
                let active = client.active.clone();
                client.handle_live_event(
                    super::UiEvent::ModelSelected {
                        request_id: first,
                        session_id: active.clone(),
                        model: picked,
                        result: Ok(()),
                    },
                    cx,
                );
                assert_eq!(client.model_switches[&active].request_id, second);
                assert_eq!(client.selected_model(), Some(displayed.clone()));
                client.handle_live_event(
                    super::UiEvent::ModelSelected {
                        request_id: second,
                        session_id: active.clone(),
                        model: displayed.to_ref(),
                        result: Ok(()),
                    },
                    cx,
                );
                assert!(!client.model_switches.contains_key(&active));
                assert_eq!(client.selected_model(), Some(displayed));
            });
        });
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
            ("missing/saved".into(), None)
        );
        assert_eq!(
            super::model_button_presentation(&catalog, None),
            ("Choose model".into(), None)
        );
    }

    #[gpui_kit::test]
    fn empty_model_catalog_schedules_bounded_retry_and_recovers(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let recovered = client.catalog.clone();
                let (api, _receiver, _) = opencode_gpui::api::ApiHandle::preview();
                client.api = Some(api);
                client.handle_live_event(
                    super::UiEvent::ModelsLoaded {
                        directory: "/repo".into(),
                        result: Ok(model::ModelCatalog::default()),
                    },
                    cx,
                );
                assert!(client.catalog.models.is_empty());
                assert!(client.model_retry_scheduled.contains("/repo"));
                assert_eq!(client.model_retry_count["/repo"], 1);
                client.handle_live_event(
                    super::UiEvent::ModelsLoaded {
                        directory: "/repo".into(),
                        result: Ok(recovered),
                    },
                    cx,
                );
                assert!(!client.catalog.models.is_empty());
                assert!(!client.model_retry_scheduled.contains("/repo"));
                assert!(!client.model_retry_count.contains_key("/repo"));
            });
        });
    }

    #[gpui_kit::test]
    fn locationless_model_event_invalidates_all_loaded_and_open_catalogs(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                assert!(!client.catalog.models.is_empty());
                let other = client.catalog.clone();
                client.catalogs.insert("/other".into(), other);
                client.handle_live_event(
                    super::UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                        directory: None,
                        payload: json!({
                            "id": "evt_catalog_global", "created": 1,
                            "type": "model.updated", "data": {}
                        }),
                    }),
                    cx,
                );
                assert!(client.catalogs.is_empty());
                assert!(client.catalog.models.is_empty());
                assert!(client.active_directories().contains(&"/repo".to_owned()));
            });
        });
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

    #[gpui_kit::test]
    fn new_session_picker_keyboard_reaches_scrolled_projects(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                for index in 0..15 {
                    client.projects.push(model::Project {
                        worktree: format!("/code/extra-{index:02}"),
                        name: None,
                    });
                }
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, cx| {
                client.show_modal(Modal::NewSession, window, cx)
            });
            window.render_frame(cx);
            assert!(client.read(cx).search.focus_handle(cx).is_focused(window));
            for _ in 0..12 {
                window.press("down", cx);
            }
            window.render_frame(cx);
            assert_eq!(client.read(cx).picker_highlight, Some(12));
            assert!(client.read(cx).projects_picker_scroll.offset().y < px(0.));
            window.press("enter", cx);
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn model_picker_keyboard_reaches_scrolled_results(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                for index in 0..15 {
                    let mut option = client.catalog.models[0].clone();
                    option.model_id = format!("model-extra-{index}");
                    option.label = format!("Extra model {index}");
                    client.catalog.models.push(option);
                }
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, cx| client.show_modal(Modal::Model, window, cx));
            window.render_frame(cx);
            assert!(client.read(cx).search.focus_handle(cx).is_focused(window));
            for _ in 0..12 {
                window.press("down", cx);
            }
            window.render_frame(cx);
            assert_eq!(client.read(cx).picker_highlight, Some(12));
            assert!(client.read(cx).picker_list_scroll.offset().y < px(0.));
            window.press("enter", cx);
            assert_eq!(
                client.read(cx).selected_model().unwrap().model_id,
                "model-extra-10"
            );
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn level_picker_keyboard_reaches_scrolled_variants(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let model = client.selected_model().expect("preview model");
                let option = client
                    .catalog
                    .models
                    .iter_mut()
                    .find(|option| {
                        option.provider_id == model.provider_id && option.model_id == model.model_id
                    })
                    .expect("catalog model");
                option
                    .variants
                    .extend((0..15).map(|index| format!("level-extra-{index}")));
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, cx| client.show_modal(Modal::Level, window, cx));
            window.render_frame(cx);
            assert!(client.read(cx).search.focus_handle(cx).is_focused(window));
            for _ in 0..12 {
                window.press("down", cx);
            }
            window.render_frame(cx);
            assert_eq!(client.read(cx).picker_highlight, Some(12));
            assert!(client.read(cx).picker_list_scroll.offset().y < px(0.));
            window.press("enter", cx);
            assert_eq!(
                client.read(cx).selected_model().unwrap().variant.as_deref(),
                Some("level-extra-8")
            );
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sessions_picker_keyboard_reaches_scrolled_results(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                for index in 0..15 {
                    let mut session = client.sessions[0].clone();
                    session.id = format!("ses_extra_{index}");
                    session.title = format!("Extra session {index}");
                    client.sessions.push(session);
                }
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            client.update(cx, |client, cx| {
                client.show_modal(Modal::Sessions, window, cx)
            });
            window.render_frame(cx);
            assert!(
                client.read(cx).search.focus_handle(cx).is_focused(window),
                "sessions picker search must receive focus when opened"
            );
            assert_eq!(filter_tab_sessions(&client.read(cx).sessions, "").len(), 20);
            for _ in 0..12 {
                window.press("down", cx);
            }
            window.render_frame(cx);
            window.render_frame(cx);
            assert_eq!(client.read(cx).picker_highlight, Some(12));
            assert!(
                client.read(cx).sessions_picker_scroll.offset().y < px(0.),
                "offset {:?}, max {:?}, bounds {:?}, child12 {:?}, children {}",
                client.read(cx).sessions_picker_scroll.offset(),
                client.read(cx).sessions_picker_scroll.max_offset(),
                client.read(cx).sessions_picker_scroll.bounds(),
                client.read(cx).sessions_picker_scroll.bounds_for_item(12),
                client.read(cx).sessions_picker_scroll.children_count()
            );
            window.press("enter", cx);
            assert_eq!(client.read(cx).active, "ses_extra_7");
            assert!(client.read(cx).modal.is_none());
        })
        .unwrap();
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

    #[gpui_kit::test]
    fn dragging_session_tabs_keeps_drafts_and_persists_order(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        let original = cx.update(|cx| client.read(cx).open_tabs.clone());
        assert!(original.len() >= 2);
        let first = &original[0];
        let second = &original[1];
        let mut reordered = original.clone();
        reordered.swap(0, 1);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let composer = client.read(cx).composer.clone();
            composer.update(cx, |input, cx| {
                input.set_value("Unsent first draft", window, cx)
            });
            client.update(cx, |client, cx| {
                client.select_session(second.clone());
                cx.notify();
            });
            window.render_frame(cx);
            let composer = client.read(cx).composer.clone();
            composer.update(cx, |input, cx| {
                input.set_value("Unsent second draft", window, cx)
            });
            client.update(cx, |client, cx| {
                client.select_session(first.clone());
                client
                    .attachment_drafts
                    .insert(first.clone(), vec!["first.txt".into()]);
                client
                    .attachment_drafts
                    .insert(second.clone(), vec!["second.txt".into()]);
                cx.notify();
            });
            window.render_frame(cx);
            window.drag_to(format!("drag-tab-{first}"), format!("tab-{second}"), cx);
            assert_eq!(client.read(cx).open_tabs, reordered);
            assert_eq!(client.read(cx).active, *first);
            assert_eq!(
                client.read(cx).composer.read(cx).value().as_ref(),
                "Unsent first draft"
            );
            assert_eq!(
                client.read(cx).composers[second].read(cx).value().as_ref(),
                "Unsent second draft"
            );
            assert_eq!(
                client.read(cx).attachment_drafts.get(first).unwrap(),
                &vec![PathBuf::from("first.txt")]
            );
            assert_eq!(
                client.read(cx).attachment_drafts.get(second).unwrap(),
                &vec![PathBuf::from("second.txt")]
            );
        })
        .unwrap();

        // The same snapshot used by the real state writer round-trips in tab order.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state.json");
        cx.update(|cx| {
            let saved = &client.read(cx).settings.persisted;
            saved.save(&path).unwrap();
            let (restored, warning) = PersistedState::load(&path).unwrap();
            assert!(warning.is_none());
            let server = restored.servers.get("http://127.0.0.1:4096").unwrap();
            assert_eq!(
                server
                    .tabs
                    .iter()
                    .map(|tab| tab.id.as_str())
                    .collect::<Vec<_>>(),
                reordered.iter().map(String::as_str).collect::<Vec<_>>()
            );
            assert_eq!(server.active.as_deref(), Some(first.as_str()));
        });

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let from = window.find(format!("drag-tab-{first}")).bounds().center();
            let first_row = window.find(format!("tab-{second}")).bounds();
            window.drag(from, first_row.origin + point(px(30.), px(6.)), cx);
            assert_eq!(client.read(cx).open_tabs, original);
            window.render_frame(cx);
            window.drag_to(format!("rename-{second}"), format!("tab-{first}"), cx);
            window.drag_to(format!("close-{second}"), format!("tab-{first}"), cx);
            assert_eq!(client.read(cx).open_tabs, original);
            assert_eq!(client.read(cx).active, *first);
            assert!(client.read(cx).modal.is_none());
            client.update(cx, |client, cx| {
                client.tab_shortcut_hint = true;
                cx.notify();
            });
            window.render_frame(cx);
            window.drag_to(format!("drag-tab-{first}"), format!("tab-{second}"), cx);
            assert_eq!(client.read(cx).open_tabs, original);
            client.update(cx, |client, cx| {
                client.tab_shortcut_hint = false;
                cx.notify();
            });
            window.render_frame(cx);
            window.drag_to(format!("drag-tab-{first}"), format!("tab-{second}"), cx);
            assert_eq!(client.read(cx).open_tabs, reordered);
            window.render_frame(cx);
            window.click(format!("close-{second}"), cx);
            client.update(cx, |client, cx| {
                client.select_session(second.clone());
                cx.notify();
            });
            let mut reopened = original.clone();
            reopened.remove(1);
            reopened.push(second.clone());
            assert_eq!(client.read(cx).open_tabs, reopened);
            assert_eq!(client.read(cx).active, *second);
        })
        .unwrap();
        cx.update(|cx| {
            client.read(cx).settings.persisted.save(&path).unwrap();
            let (restored, _) = PersistedState::load(&path).unwrap();
            let server = restored.servers.get("http://127.0.0.1:4096").unwrap();
            assert_eq!(
                server
                    .tabs
                    .iter()
                    .map(|tab| tab.id.as_str())
                    .collect::<Vec<_>>(),
                client
                    .read(cx)
                    .open_tabs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            );
            assert_eq!(server.active.as_deref(), Some(second.as_str()));
        });
    }

    #[gpui_kit::test]
    fn late_confirmation_before_failed_draft_render_cancels_restoration(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("sent", window, cx));
                let session = client.active.clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("response lost".into()),
                    },
                    cx,
                );
                assert!(
                    client
                        .draft_actions
                        .iter()
                        .any(|action| matches!(action, super::DraftAction::Restore { .. }))
                );
                client.handle_live_event(inbox_enqueued(&session, &pending.message_id, "sent"), cx);
                assert!(
                    !client
                        .draft_actions
                        .iter()
                        .any(|action| matches!(action, super::DraftAction::Restore { .. }))
                );
            });
            window.render_frame(cx);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn stale_history_before_post_acceptance_cannot_prune_local_prompt(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                let session = client.active.clone();
                client.loading_messages.insert(session.clone(), None);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("sent", window, cx));
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Ok(()),
                    },
                    cx,
                );
                assert!(client.skip_prune_for_load[&session].contains(&pending.message_id));
                let empty_page = || opencode_gpui::api::MessagePage {
                    messages: vec![],
                    next_cursor: None,
                    queued: Some(vec![]),
                };
                client.handle_live_event(
                    UiEvent::MessagesLoaded {
                        session_id: session.clone(),
                        cursor: None,
                        result: Ok(empty_page()),
                    },
                    cx,
                );
                assert!(client.conversations[&session].has_user_message(&pending.message_id));
                assert!(
                    client.loading_messages.contains_key(&session),
                    "acceptance schedules a fresh post-send history load"
                );
                client.handle_live_event(
                    UiEvent::MessagesLoaded {
                        session_id: session.clone(),
                        cursor: None,
                        result: Ok(empty_page()),
                    },
                    cx,
                );
                assert!(
                    !client.conversations[&session].has_user_message(&pending.message_id),
                    "a fresh authoritative empty snapshot removes a canceled ghost"
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn late_confirmation_never_strips_a_new_draft_sharing_the_old_prefix(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("hi", window, cx));
                client.send_prompt(false, cx);
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let pending = client.pending_prompts[&session].clone();
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("high priority", window, cx));
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("response lost".into()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(
                client.read(cx).composer.read(cx).value().as_ref(),
                "hi\nhigh priority"
            );
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let id = client.failed_prompt_drafts[&session].message_id.clone();
                client.handle_live_event(inbox_enqueued(&session, &id, "hi"), cx);
            });
            window.render_frame(cx);
            assert_eq!(
                client.read(cx).composer.read(cx).value().as_ref(),
                "high priority"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn merged_failed_draft_waits_for_confirmation_then_keeps_new_text(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("A", window, cx));
                client.send_prompt(false, cx);
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let pending = client.pending_prompts[&session].clone();
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("B", window, cx));
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session,
                        result: Err("ambiguous failure".into()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "A\nB");
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let failed_id = client.failed_prompt_drafts[&session].message_id.clone();
                client.send_prompt(false, cx);
                assert!(
                    !client.pending_prompts.contains_key(&session),
                    "must not send A twice"
                );
                client.handle_live_event(inbox_enqueued(&session, &failed_id, "A"), cx);
            });
            window.render_frame(cx);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "B");
            client.update(cx, |client, cx| {
                client.send_prompt(false, cx);
                assert!(client.pending_prompts.contains_key(&client.active));
                assert_eq!(client.pending_prompts[&client.active].text, "B");
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn late_confirmation_preserves_an_edited_restored_draft(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("sent", window, cx));
                let session = client.active.clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session,
                        result: Err("response lost".into()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "sent");
            client.update(cx, |client, cx| {
                let session = client.active.clone();
                let id = client.failed_prompt_drafts[&session].message_id.clone();
                client.composer.update(cx, |input, cx| {
                    input.set_value("sentry priority", window, cx)
                });
                client.handle_live_event(inbox_enqueued(&session, &id, "sent"), cx);
            });
            window.render_frame(cx);
            assert_eq!(
                client.read(cx).composer.read(cx).value().as_ref(),
                "sentry priority"
            );
            client.update(cx, |client, cx| {
                client.send_prompt(false, cx);
                assert_eq!(
                    client.pending_prompts[&client.active].text,
                    "sentry priority"
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn optimistic_prompt_reconciles_with_sse_without_premature_stop(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client.composer.update(cx, |input, cx| {
                    input.set_value("Immediate feedback", window, cx);
                });
                let session = client.active.clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                assert_eq!(
                    client.transcript[&session]
                        .iter()
                        .filter(|row| row.key.message_id == pending.message_id)
                        .count(),
                    1
                );
                assert!(
                    !client.conversations[&session].has_delivered_user_message(&pending.message_id)
                );
                client.stop_active(cx);
                client.update_tab_status(session.clone(), RunStatus::Busy);
                assert!(client.deferred_abort.contains(&session));

                let event = |kind: &str, data: serde_json::Value| {
                    UiEvent::ServerEvent(opencode_gpui::api::ServerEnvelope {
                        directory: Some("/repo".into()),
                        payload: json!({"id": format!("evt_{kind}"), "created": 1234,
                            "type": kind, "data": data}),
                    })
                };
                client.handle_live_event(
                    event(
                        "session.inbox.enqueued",
                        json!({
                            "sessionID": session, "inboxID": pending.message_id,
                            "item": {"type": "user", "payload": {"text": "Immediate feedback"},
                                "delivery": "steer"}
                        }),
                    ),
                    cx,
                );
                assert_eq!(
                    client.transcript[&session]
                        .iter()
                        .filter(|row| row.key.message_id == pending.message_id)
                        .count(),
                    0
                );
                assert!(client.deferred_abort.contains(&session));
                client.handle_live_event(
                    event(
                        "session.inbox.delivered",
                        json!({
                            "sessionID": session, "inboxID": pending.message_id,
                        }),
                    ),
                    cx,
                );
                assert!(
                    client.conversations[&session].has_delivered_user_message(&pending.message_id)
                );
                assert!(!client.deferred_abort.contains(&session));
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("lost HTTP response".into()),
                    },
                    cx,
                );
                assert!(!client.draft_actions.iter().any(|action| matches!(action,
                    super::DraftAction::Restore { session: id, .. } if id == &session)));
                assert_eq!(
                    client.transcript[&session]
                        .iter()
                        .filter(|row| row.key.message_id == pending.message_id)
                        .count(),
                    1
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn replacing_the_composer_before_send_clear_never_discards_new_text(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = opencode_gpui::api::ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("hi", window, cx));
                client.send_prompt(false, cx);
                client.composer.update(cx, |input, cx| {
                    input.set_value("high priority", window, cx);
                });
            });
            window.render_frame(cx);
            assert_eq!(
                client.read(cx).composer.read(cx).value().as_ref(),
                "high priority"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ambiguous_send_failure_keeps_earlier_stop_until_own_delivery(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("slow", window, cx));
                let session = client.active.clone();
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&session].clone();
                client.stop_active(cx);
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: session.clone(),
                        result: Err("timeout".into()),
                    },
                    cx,
                );
                assert!(client.deferred_abort.contains(&session));
                assert!(client.local_busy.contains(&session));
                assert!(!client.can_send(cx));
                let UiEvent::ServerEvent(enqueued) =
                    inbox_enqueued(&session, &pending.message_id, "slow")
                else {
                    unreachable!();
                };
                assert!(
                    client
                        .conversations
                        .get_mut(&session)
                        .unwrap()
                        .apply_event(&enqueued.payload)
                );
                client
                    .conversations
                    .get_mut(&session)
                    .unwrap()
                    .apply_event(&json!({
                        "id": "evt_delivered_late", "created": 2000,
                        "type": "session.inbox.delivered",
                        "data": {"sessionID": session, "inboxID": pending.message_id}
                    }));
                assert!(
                    client.conversations[&session].has_delivered_user_message(&pending.message_id)
                );
                client.update_tab_status(session.clone(), RunStatus::Busy);
                assert!(!client.deferred_abort.contains(&session));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn failed_follow_up_does_not_retire_first_runs_local_busy(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("First", window, cx));
                client.send_prompt(false, cx);
                let pending = client.pending_prompts[&client.active].clone();
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: client.active.clone(),
                        result: Ok(()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("Follow-up", window, cx));
                client.send_prompt(true, cx);
                let pending = client.pending_prompts[&client.active].clone();
                assert_eq!(pending.delivery, Some(protocol::Delivery::Queue));
                client.handle_live_event(
                    UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: client.active.clone(),
                        result: Err("queue failed".into()),
                    },
                    cx,
                );
                assert!(client.local_busy.contains(&client.active));
                assert!(client.is_running(&client.active));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn stop_during_first_pending_post_defers_abort_until_acceptance(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = opencode_gpui::api::ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("Run a task", window, cx));
                client.send_prompt(false, cx);
                assert!(client.is_running(&client.active));
                assert!(client.pending_prompts[&client.active].delivery.is_none());
            });
            window.render_frame(cx);
            let _stop = window.find("stop-run");
            client.update(cx, |client, cx| {
                client.stop_active(cx);
                assert!(client.deferred_abort.contains(&client.active));
                let pending = client.pending_prompts[&client.active].clone();
                client.handle_live_event(
                    super::UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: client.active.clone(),
                        result: Ok(()),
                    },
                    cx,
                );
                assert!(client.deferred_abort.contains(&client.active));
                assert!(client.is_running(&client.active));
                client.update_tab_status(client.active.clone(), RunStatus::Idle);
                assert!(
                    client.is_running(&client.active),
                    "stale Idle must not hide local Busy"
                );
                assert!(client.deferred_abort.contains(&client.active));
                assert!(client.local_busy.contains(&client.active));
                client.update_tab_status(client.active.clone(), RunStatus::Busy);
                assert!(
                    client.deferred_abort.contains(&client.active),
                    "another client's Busy must not abort this prompt before delivery"
                );
                assert!(client.local_busy.contains(&client.active));
                let message = protocol::SessionMessage::from_value(json!({
                    "id": pending.message_id, "type": "user", "time": { "created": 1 },
                    "content": [{ "type": "text", "text": "Run a task" }]
                }));
                client
                    .conversations
                    .get_mut(&client.active)
                    .unwrap()
                    .replace_from_api(&[message], None);
                let active = client.active.clone();
                client.maybe_dispatch_deferred_abort(&active);
                assert!(!client.deferred_abort.contains(&client.active));
                client.update_tab_status(client.active.clone(), RunStatus::Idle);
                assert!(!client.is_running(&client.active));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn failed_prompt_restores_draft_and_reuses_id_only_when_unchanged(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let (api, _receiver, _) = opencode_gpui::api::ApiHandle::preview();
                client.api = Some(api);
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("First draft", window, cx));
                client.send_prompt(false, cx);
                assert!(client.pending_prompts.contains_key(&client.active));
                assert_eq!(client.composer.read(cx).value().as_ref(), "First draft");
            });
            window.render_frame(cx);
            assert_eq!(client.read(cx).composer.read(cx).value().as_ref(), "");
            client.update(cx, |client, cx| {
                let pending = client.pending_prompts[&client.active].clone();
                client.handle_live_event(
                    super::UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: client.active.clone(),
                        result: Err("transient failure".into()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                assert_eq!(client.composer.read(cx).value().as_ref(), "First draft");
                let retry = client.failed_prompt_drafts[&client.active]
                    .message_id
                    .clone();
                client.send_prompt(false, cx);
                assert_eq!(client.pending_prompts[&client.active].message_id, retry);
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                let pending = client.pending_prompts[&client.active].clone();
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("New draft", window, cx));
                client.handle_live_event(
                    super::UiEvent::PromptAccepted {
                        request_id: pending.request_id,
                        session_id: client.active.clone(),
                        result: Err("another failure".into()),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                assert_eq!(
                    client.composer.read(cx).value().as_ref(),
                    "First draft\nNew draft"
                );
                assert!(client.failed_prompt_drafts.contains_key(&client.active));
                client.send_prompt(false, cx);
                assert!(!client.pending_prompts.contains_key(&client.active));
            });
        })
        .unwrap();
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

    #[gpui_kit::test]
    fn composer_send_requires_model_input_and_supported_attachments(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(!client.update(cx, |client, cx| client.can_send(cx)));
            let (api, _, _) = super::ApiHandle::preview();
            client.update(cx, |client, cx| {
                client.api = Some(api);
                client.permissions.clear();
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("Prompt text", window, cx));
            });
            assert!(client.update(cx, |client, cx| client.can_send(cx)));
            client.update(cx, |client, _| {
                for option in &mut client.catalog.models {
                    option.supports_attachments = false;
                }
                client.attachments_draft.push(PathBuf::from("/fake.png"));
            });
            assert!(!client.update(cx, |client, cx| client.can_send(cx)));
            client.update(cx, |client, _| {
                client.attachments_draft.clear();
                client.pending_prompts.insert(
                    client.active.clone(),
                    super::PendingPromptSend {
                        request_id: 1,
                        message_id: "pending".into(),
                        text: String::new(),
                        attachments: vec![],
                        delivery: None,
                        edit_generation: 0,
                    },
                );
            });
            assert!(!client.update(cx, |client, cx| client.can_send(cx)));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn code_copy_is_a_named_button_with_a_clipboard_action(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let (row_index, block_index) = {
                let client = client.read(cx);
                client.transcript[&client.active]
                    .iter()
                    .enumerate()
                    .find_map(|(row_index, row)| {
                        markdown_blocks(&row.body)
                            .iter()
                            .position(|block| matches!(block, MarkdownBlock::Code(_, _)))
                            .map(|block_index| (row_index, block_index))
                    })
                    .expect("preview has a code block")
            };
            let id = format!("copy-code-{row_index}-{block_index}");
            let control = window.find(id.clone());
            assert_eq!(control.role(), Some(Role::Button));
            assert_eq!(control.label(), Some("Copy code"));
            window.click(id, cx);
            let copied = cx.read_from_clipboard().and_then(|item| item.text());
            assert!(copied.is_some_and(|text| text.contains("paperclip_icon")));
            cx.write_to_clipboard(super::ClipboardItem::new_string("cleared".to_owned()));
            window.press("space", cx);
            let copied = cx.read_from_clipboard().and_then(|item| item.text());
            assert!(copied.is_some_and(|text| text.contains("paperclip_icon")));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn composer_controls_have_names_focus_and_keyboard_actions(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            for (id, name) in [
                ("attach-file", "Attach file"),
                ("composer-model", "Choose model"),
                ("composer-level", "Choose reasoning level"),
            ] {
                let control = window.find(id);
                assert_eq!(control.role(), Some(Role::Button));
                assert_eq!(control.label(), Some(name));
            }
            let model_focus = client.read(cx).composer_action_focus[1].clone();
            model_focus.focus(window, cx);
            window.press("enter", cx);
            assert!(client.read(cx).modal == Some(Modal::Model));
            window.press("escape", cx);
            let level_focus = client.read(cx).composer_action_focus[2].clone();
            level_focus.focus(window, cx);
            window.press("space", cx);
            assert!(client.read(cx).modal == Some(Modal::Level));
            window.press("escape", cx);
            let (api, _, _) = super::ApiHandle::preview();
            client.update(cx, |client, cx| {
                client.api = Some(api);
                client.permissions.clear();
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("Keyboard send", window, cx));
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(window.find("send-prompt").role(), Some(Role::Button));
            let send_focus = client.read(cx).composer_action_focus[4].clone();
            send_focus.focus(window, cx);
            window.press("enter", cx);
            assert!(
                client
                    .read(cx)
                    .pending_prompts
                    .contains_key(&client.read(cx).active)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn parked_tray_warns_when_a_new_prompt_would_resume_it(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.update(cx, |client, cx| {
                client.select_session("ses_parked".into());
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(
                client.update(cx, |client, cx| client.resume_warning(cx)),
                None
            );
            client.update(cx, |client, cx| {
                client
                    .composer
                    .update(cx, |input, cx| input.set_value("Next turn", window, cx));
            });
            assert!(
                client
                    .update(cx, |client, cx| client.resume_warning(cx))
                    .is_some()
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn composer_keeps_keyboard_navigation_after_switching_tabs(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.permissions.clear();
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            client.read(cx).composer.focus_handle(cx).focus(window, cx);
            window.press("ctrl-tab", cx);
            assert_eq!(client.read(cx).active, "ses_other");
            assert!(client.read(cx).composer.focus_handle(cx).is_focused(window));
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-1", cx);
            assert_eq!(client.read(cx).active, "ses_preview");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn inactive_row_actions_target_the_clicked_tab(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        let original = cx.update(|cx| client.read(cx).active.clone());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("rename-ses_other", cx);
            assert_eq!(client.read(cx).active, original);
            assert_eq!(client.read(cx).rename_target.as_deref(), Some("ses_other"));
            assert_eq!(
                client.read(cx).rename.read(cx).value().as_ref(),
                "SSH tunnel notes"
            );
            client.update(cx, |client, cx| {
                client.rename.update(cx, |input, cx| {
                    input.set_value("Renamed other", window, cx);
                });
                client.rename_session(cx);
            });
            assert_eq!(client.read(cx).active, original);
            assert_eq!(
                client
                    .read(cx)
                    .sessions
                    .iter()
                    .find(|session| session.id == "ses_other")
                    .unwrap()
                    .title,
                "Renamed other"
            );
            window.render_frame(cx);
            let close_focus = client.read(cx).tab_focus["ses_other"][2].clone();
            close_focus.focus(window, cx);
            window.press("space", cx);
            assert!(!client.read(cx).open_tabs.contains(&"ses_other".to_owned()));
            assert_eq!(client.read(cx).active, original);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn rename_enter_validates_and_updates_the_target_session(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("rename-ses_other", cx);
            window.render_frame(cx);
            assert!(client.read(cx).rename.focus_handle(cx).is_focused(window));
            client.update(cx, |client, cx| {
                client
                    .rename
                    .update(cx, |input, cx| input.set_value("", window, cx));
            });
            window.press("enter", cx);
            assert_eq!(
                client.read(cx).rename_error.as_deref(),
                Some("Enter a session title")
            );
            assert!(client.read(cx).modal == Some(Modal::Rename));
            client.update(cx, |client, cx| {
                client.rename.update(cx, |input, cx| {
                    input.set_value("Renamed through Enter", window, cx)
                });
            });
            window.press("enter", cx);
            assert!(client.read(cx).modal.is_none());
            assert_eq!(
                client
                    .read(cx)
                    .sessions
                    .iter()
                    .find(|session| session.id == "ses_other")
                    .unwrap()
                    .title,
                "Renamed through Enter"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn rename_failure_keeps_the_editor_open_for_retry(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                client.modal = Some(Modal::Rename);
                client.rename_target = Some("ses_other".into());
                client.rename_pending = Some(42);
                client.handle_live_event(
                    super::UiEvent::SessionRenamed {
                        request_id: 42,
                        session_id: "ses_other".into(),
                        result: Err("server rejected the title".into()),
                    },
                    cx,
                );
                assert!(client.modal == Some(Modal::Rename));
                assert!(client.rename_pending.is_none());
                assert_eq!(
                    client.rename_error.as_deref(),
                    Some("server rejected the title")
                );
            });
        });
    }

    #[gpui_kit::test]
    fn busy_to_idle_marks_open_tabs_unread_including_active(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless client window")
        });
        cx.update(|cx| {
            client.update(cx, |client, _| {
                client
                    .statuses
                    .insert(client.active.clone(), RunStatus::Busy);
                client.update_tab_status(client.active.clone(), RunStatus::Idle);
                assert!(client.unread.contains(&client.active));
                client.select_session(client.active.clone());
                assert!(client.unread.contains(&client.active));
                client.statuses.insert("ses_closed".into(), RunStatus::Busy);
                client.update_tab_status("ses_closed".into(), RunStatus::Idle);
                assert!(!client.unread.contains("ses_closed"));
                client.unread.insert("ses_other".into());
                client.select_session("ses_other".into());
                assert!(client.unread.contains("ses_other"));
            });
        });
    }

    #[test]
    fn sticky_tracks_latest_user_past_top_and_hides_flush_row() {
        let users = [(0, px(-120.), px(-25.)), (3, px(210.), px(280.))];
        assert_eq!(sticky_user_index(users, px(0.), px(400.)), Some(0));
        let users = [(0, px(-480.), px(-385.)), (3, px(-20.), px(50.))];
        assert_eq!(sticky_user_index(users, px(0.), px(400.)), Some(3));
        let users = [(0, px(-480.), px(-385.)), (3, px(0.), px(70.))];
        assert_eq!(sticky_user_index(users, px(0.), px(400.)), None);
    }

    #[gpui_kit::test]
    fn long_sticky_prompt_is_capped_and_scrollable(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, None))
            })
            .expect("headless preview window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            assert!(window.try_find("sticky-user").is_some());
        })
        .unwrap();
        cx.update(|cx| {
            client.update(cx, |client, cx| {
                let rows = client.transcript.get_mut(&client.active).unwrap();
                let user = rows
                    .iter_mut()
                    .find(|row| row.role == model::Role::User)
                    .unwrap();
                user.body = "A long prompt with wrapped words and lines. ".repeat(100);
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let sticky = window.find("sticky-user").bounds();
            let body = window.find("sticky-body").bounds();
            assert!(sticky.size.height > px(106.), "long prompt did not grow");
            assert!(
                sticky.size.height <= px(226.),
                "sticky prompt escaped its cap"
            );
            assert!(
                body.size.height <= px(180.),
                "sticky body escaped its scroll cap"
            );
        })
        .unwrap();
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
        assert!(blocks.iter().any(|block| matches!(block, MarkdownBlock::Structured { content, .. } if content == "☑ done")));
        assert!(markdown_blocks(COMPLEX_MARKDOWN_PREVIEW)
            .iter()
            .any(|block| matches!(block, MarkdownBlock::Code(language, code) if language == "rust" && code.contains("paperclip_icon"))));
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
    }

    #[gpui_kit::test]
    fn complex_markdown_measures_and_renders_in_virtual_transcript(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, client) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Client::from_preview(window, cx, Some("complex-markdown".into())))
            })
            .expect("headless transcript window")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            let state = client.read(cx);
            assert!(
                state.transcript[&state.active]
                    .iter()
                    .any(|row| row.kind == model::TranscriptRowKind::Normal
                        && row.body == COMPLEX_MARKDOWN_PREVIEW)
            );
            assert!(state.row_heights[&state.active].borrow().layouts > 0);
        })
        .unwrap();
    }
}
