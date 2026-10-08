//! GPUI Kit shell backed by the v2 transport, or its deterministic preview fixture.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use gpui_kit::base::{Checkbox, CheckboxIndicator, CheckboxState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::text::markdown;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{Icon, Sizable};
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
    server_version: Option<String>,
    conversations: HashMap<String, Conversation>,
    loading_messages: HashMap<String, Option<String>>,
    message_events_during_load: HashMap<String, Vec<protocol::Event>>,
    reload_after_load: HashSet<String>,
    preserve_scroll: Option<(Point<Pixels>, Point<Pixels>)>,
    catalogs: HashMap<String, ModelCatalog>,
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
    #[cfg(test)]
    measurement_probe: Option<(Pixels, Rc<RefCell<Vec<Pixels>>>)>,
    attachments: HashMap<(String, usize, usize), Arc<Image>>,
    catalog: ModelCatalog,
    composer: Entity<TextareaState>,
    composer_placeholder_focused: bool,
    composer_session: String,
    composers: HashMap<String, Entity<TextareaState>>,
    attachments_draft: Vec<PathBuf>,
    attachment_drafts: HashMap<String, Vec<PathBuf>>,
    overlay: Option<String>,
    scroll: ScrollHandle,
    sessions_picker_scroll: ScrollHandle,
    unread: HashSet<String>,
    statuses: HashMap<String, RunStatus>,
    jobs: Vec<JobRow>,
    forms: Forms,
    permissions: Vec<PendingPermission>,
    permission_in_flight: HashSet<String>,
    permission_focus: [FocusHandle; 3],
    permission_presented: bool,
    settings_tab_focus: [FocusHandle; 2],
    child_parents: HashMap<String, String>,
    next_prompt_request_id: u64,
    next_session_request_id: u64,
    next_model_request_id: u64,
    pending_prompts: HashMap<String, (u64, String, Vec<PathBuf>)>,
    tray_in_flight: HashSet<String>,
    clear_accepted_drafts: Vec<(String, String, Vec<PathBuf>)>,
    modal: Option<Modal>,
    rename_target: Option<String>,
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
}

impl SettingsFields {
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
}

// Increment this when the row's typography or layout rules change. The other
// parts of the stamp follow the live width, theme and conversation snapshot.
const TRANSCRIPT_ROW_STYLE_REVISION: u64 = 1;
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

#[derive(Default)]
struct RowHeightCache {
    stamp: Option<RowLayoutStamp>,
    entries: HashMap<TranscriptRowKey, CachedRowHeight>,
    #[cfg(test)]
    layouts: usize,
}

impl RowHeightCache {
    fn missing(&mut self, stamp: RowLayoutStamp, rows: &[TranscriptRow]) -> Vec<usize> {
        if self.stamp != Some(stamp) {
            self.entries.clear();
            self.stamp = Some(stamp);
        }
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| self.height(row).is_none().then_some(index))
            .collect()
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
}

/// Detached rows are laid out at a definite width during GPUI's layout phase,
/// never prepainted, painted, or inserted into the transcript scroll area.
/// `layout_as_root` panics if called from the view's render method instead.
struct TranscriptMeasurementProbe {
    rows: Vec<(TranscriptRowKey, u64, AnyElement)>,
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

fn markdown_blocks(source: &str) -> Vec<MarkdownBlock> {
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
                MarkdownBlock::Paragraph(text) => markdown(text).into_any_element(),
                MarkdownBlock::List(items) => {
                    let mut list = div().flex().flex_col().gap(px(10.));
                    for item in items {
                        list = list.child(
                            div()
                                .flex()
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
                                    div()
                                        .id(format!("copy-code-{row_index}-{block_index}"))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        }))
                                        .child("▣"),
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
            };
            content = content.child(element);
        }
        content.into_any_element()
    }

    fn from_live(window: &mut Window, cx: &mut Context<Self>, args: &Args) -> Self {
        let (state, state_warning) = match persist::load_with_legacy(&persist::default_path()) {
            Ok(loaded) => loaded,
            Err(error) => (PersistedState::default(), Some(error.to_string())),
        };
        let server = args
            .server
            .clone()
            .unwrap_or(state.connection.server.clone());
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
            connection_status: "Connecting".into(),
            disconnected: false,
            server_version: None,
            conversations: HashMap::new(),
            loading_messages: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            preserve_scroll: None,
            catalogs: HashMap::new(),
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
            row_heights: HashMap::new(),
            #[cfg(test)]
            measurement_probe: None,
            attachments: HashMap::new(),
            catalog: ModelCatalog::default(),
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            }),
            composer_placeholder_focused: false,
            composer_session: String::new(),
            composers: HashMap::new(),
            attachments_draft: Vec::new(),
            attachment_drafts: HashMap::new(),
            overlay: None,
            scroll,
            sessions_picker_scroll: ScrollHandle::new(),
            unread: HashSet::new(),
            statuses: HashMap::new(),
            jobs: Vec::new(),
            forms: Forms::default(),
            permissions: Vec::new(),
            permission_in_flight: HashSet::new(),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: false,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            child_parents: HashMap::new(),
            next_prompt_request_id: 0,
            next_session_request_id: 0,
            next_model_request_id: 0,
            pending_prompts: HashMap::new(),
            tray_in_flight: HashSet::new(),
            clear_accepted_drafts: Vec::new(),
            modal: None,
            rename_target: None,
            picker_highlight: None,
            search: cx.new(|cx| InputState::new(window, cx).placeholder("Search models (fuzzy)…")),
            rename: cx.new(|cx| InputState::new(window, cx)),
        };
        cx.subscribe(&client.composer, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { secondary, shift } = event
                && !shift
            {
                this.send_prompt(*secondary, cx);
            }
        })
        .detach();
        cx.subscribe(
            &client.search,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.picker_highlight = None;
                    if this.modal == Some(Modal::Sessions) {
                        this.sessions_picker_scroll
                            .set_offset(point(px(0.), px(0.)));
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.accept_modal_choice(cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(
            &client.settings.session_search,
            |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
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
            self.active = id.clone();
        }
        self.scroll.scroll_to_bottom();
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
            if !self.catalogs.contains_key(&session.directory) {
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
        let key = self
            .settings
            .current
            .base_url
            .trim_end_matches('/')
            .to_owned();
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
        self.statuses.insert(id, status);
    }

    fn close_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        self.tab_drop_target = None;
        let index = self.open_tabs.iter().position(|tab| tab == id);
        self.open_tabs.retain(|tab| tab != id);
        self.tab_focus.remove(id);
        self.conversations.remove(id);
        self.loading_messages.remove(id);
        self.message_events_during_load.remove(id);
        self.reload_after_load.remove(id);
        self.transcript.remove(id);
        self.row_heights.remove(id);
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
        self.clear_accepted_drafts
            .retain(|(session, _, _)| session != id);
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
            cx.subscribe(&composer, |this, _, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { secondary, shift } = event
                    && !shift
                {
                    this.send_prompt(*secondary, cx);
                }
            })
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
            self.permission_focus[0].clone()
        } else {
            self.composer.focus_handle(cx)
        };
        focus.focus(window, cx);
        window.on_next_frame(move |window, cx| focus.focus(window, cx));
    }

    fn selected_model(&self) -> Option<ModelSelection> {
        self.sessions
            .iter()
            .find(|session| session.id == self.active)
            .and_then(Session::model_selection)
            .or_else(|| self.catalog.preferred.clone())
    }

    fn choose_model(&mut self, model: protocol::ModelRef, cx: &mut Context<Self>) {
        if self.active.is_empty() {
            return;
        }
        if let Some(api) = &self.api {
            self.next_model_request_id += 1;
            api.send(Command::SelectModel {
                request_id: self.next_model_request_id,
                session_id: self.active.clone(),
                model,
            });
        } else if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == self.active)
        {
            session.model = Some(SessionModel {
                id: model.id,
                provider_id: model.provider_id,
                variant: model.variant,
            });
        }
        self.modal = None;
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
        cx.notify();
    }

    fn rename_session(&mut self, cx: &mut Context<Self>) {
        let title = self.rename.read(cx).value().trim().to_owned();
        let id = self.rename_target.as_deref().unwrap_or(&self.active);
        if title.is_empty() || id.is_empty() {
            return;
        }
        if let Some(api) = &self.api {
            self.next_session_request_id += 1;
            api.send(Command::RenameSession {
                request_id: self.next_session_request_id,
                session_id: id.to_owned(),
                title,
            });
        } else if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.title = title;
        }
        self.rename_target = None;
        self.modal = None;
        cx.notify();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.preview_api {
            self.modal = None;
            cx.notify();
            return;
        }
        match self.apply_settings_inner(cx) {
            Ok(()) => {
                self.settings.error = None;
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
        // Open the new connection before writing any credential under its identity.
        let (api, receiver, key) = ApiHandle::start(config.clone())?;
        if let Some(token) = &cloudflare_access {
            credentials::save(&server, token)?;
        } else if old.cloudflare_access.is_some() && old.base_url.trim_end_matches('/') == server {
            credentials::remove(&old.base_url)?;
        }
        let (stored, warning) =
            credentials::apply_password_change(&SystemKeyring, &server, &username, &password_plan);
        if let Some(warning) = &warning {
            log::warn!("{warning}");
        }
        let mut persisted = self.settings.persisted.clone();
        persisted.connection = ConnectionSettings {
            server: server.clone(),
            username,
            cloudflare_access: cloudflare_access.is_some(),
            basic_auth_in_keyring: stored,
        };
        persisted.save(&persist::default_path())?;
        self.connection_generation += 1;
        let generation = self.connection_generation;
        self.api = Some(api);
        self.settings.persisted = persisted.clone();
        self.settings.current = config;
        self.connection_status = warning.map_or_else(
            || "Connecting".into(),
            |warning| format!("Connecting · {warning}"),
        );
        self.disconnected = false;
        self.sessions.clear();
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
        self.clear_accepted_drafts.clear();
        self.transcript.clear();
        self.row_heights.clear();
        self.conversations.clear();
        self.loading_messages.clear();
        self.message_events_during_load.clear();
        self.reload_after_load.clear();
        self.preserve_scroll = None;
        self.catalogs.clear();
        self.statuses.clear();
        self.forms.clear();
        self.permissions.clear();
        self.permission_in_flight.clear();
        self.permission_presented = false;
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
                }
                let placeholder = match modal {
                    Modal::Model => "Search models (fuzzy)…",
                    Modal::Level => "Search levels (fuzzy)…",
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
                self.settings.server.focus_handle(cx).focus(window, cx);
                self.settings
                    .server
                    .update(cx, |input, cx| input.select_all(window, cx));
            }
        }
        cx.notify();
    }

    fn modal_choice_count(&self, cx: &Context<Self>) -> usize {
        let query = self.search.read(cx).value().to_lowercase();
        match self.modal {
            Some(Modal::Sessions) => filter_tab_sessions(&self.sessions, &query).len(),
            Some(Modal::NewSession) => self
                .projects
                .iter()
                .filter(|project| {
                    project.worktree.to_lowercase().contains(&query)
                        || project
                            .name
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .count(),
            Some(Modal::Model) => self
                .catalog
                .models
                .iter()
                .filter(|option| {
                    option.label.to_lowercase().contains(&query)
                        || option.model_id.to_lowercase().contains(&query)
                })
                .take(7)
                .count(),
            Some(Modal::Level) => {
                let variants = self
                    .selected_model()
                    .as_ref()
                    .and_then(|selection| self.catalog.find(selection))
                    .map(|option| option.variants.as_slice())
                    .unwrap_or_default();
                std::iter::once("Default")
                    .chain(variants.iter().map(String::as_str))
                    .filter(|level| level.to_lowercase().contains(&query))
                    .count()
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
                let directory = self
                    .projects
                    .iter()
                    .filter(|project| {
                        project.worktree.to_lowercase().contains(&query)
                            || project
                                .name
                                .as_deref()
                                .unwrap_or_default()
                                .to_lowercase()
                                .contains(&query)
                    })
                    .nth(index)
                    .map(|project| project.worktree.clone());
                if let Some(directory) = directory {
                    self.create_session(directory, cx);
                }
            }
            Some(Modal::Model) => {
                let choice = self
                    .catalog
                    .models
                    .iter()
                    .filter(|option| {
                        option.label.to_lowercase().contains(&query)
                            || option.model_id.to_lowercase().contains(&query)
                    })
                    .take(7)
                    .nth(index)
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
                let variant = std::iter::once(None)
                    .chain(variant.iter().cloned().map(Some))
                    .filter(|variant| {
                        variant
                            .as_deref()
                            .unwrap_or("Default")
                            .to_lowercase()
                            .contains(&query)
                    })
                    .nth(index);
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
            if self.modal.take().is_some() {
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
        let text = self.composer.read(cx).value().to_string();
        if (text.trim().is_empty() && self.attachments_draft.is_empty())
            || self.active.is_empty()
            || self.composer_session != self.active
            || self.pending_prompts.contains_key(&self.active)
            || self.visible_permission().is_some()
        {
            return;
        }
        let Some(api) = &self.api else {
            return;
        };
        self.next_prompt_request_id += 1;
        let request_id = self.next_prompt_request_id;
        let delivery = self
            .statuses
            .get(&self.active)
            .is_some_and(RunStatus::is_busy)
            .then_some(if queue {
                protocol::Delivery::Queue
            } else {
                protocol::Delivery::Steer
            });
        api.send(Command::SendPrompt {
            request_id,
            message_id: protocol::new_message_id(),
            session_id: self.active.clone(),
            text: text.clone(),
            attachments: self.attachments_draft.clone(),
            delivery,
        });
        self.pending_prompts.insert(
            self.active.clone(),
            (request_id, text, self.attachments_draft.clone()),
        );
        cx.notify();
    }

    fn update_transcript(&mut self, session_id: &str) {
        let Some(conversation) = self.conversations.get(session_id) else {
            return;
        };
        let rows: Vec<TranscriptRow> = conversation
            .messages
            .iter()
            .filter(|message| !message.in_tray())
            .flat_map(|message| message.rows())
            .collect();
        self.attachments.retain(|(id, _, _), _| id != session_id);
        for (row_index, row) in rows.iter().enumerate() {
            for (image_index, url) in row.images.iter().enumerate() {
                if let Some(image) = inline_image(url) {
                    self.attachments
                        .insert((session_id.to_owned(), row_index, image_index), image);
                }
            }
        }
        self.transcript.insert(session_id.to_owned(), rows);
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
                // The stream reconnects independently of request workers; resync after outages.
                if self.disconnected {
                    self.refresh_open_tabs = true;
                    self.request_bootstrap();
                    self.disconnected = false;
                }
            }
            UiEvent::Connection {
                connected: false,
                error: _,
            } => {
                self.disconnected = true;
                self.connection_status = "Disconnected · reconnecting".into();
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
                if data.statuses_complete {
                    self.statuses = data.statuses;
                } else {
                    self.statuses.extend(data.statuses);
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
                if let Some(id) = desired {
                    self.select_session(id);
                } else {
                    self.active.clear();
                    self.catalog = ModelCatalog::default();
                }
                self.bootstrapped = true;
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
                                let conversation =
                                    self.conversations.entry(session_id.clone()).or_default();
                                if cursor.is_some() {
                                    if self.active == session_id {
                                        self.preserve_scroll =
                                            Some((self.scroll.offset(), self.scroll.max_offset()));
                                    }
                                    conversation.prepend_from_api(&page.messages, page.next_cursor);
                                } else {
                                    conversation.replace_from_api(&page.messages, page.next_cursor);
                                    if let Some(queued) = page.queued {
                                        conversation.sync_queued(&queued);
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
                            }
                            Err(MessageLoadError::SessionNotFound) => {
                                self.close_tab(&session_id, cx);
                            }
                            Err(error) => {
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
                result: Err(error), ..
            } => self.connection_status = format!("Models failed: {error}"),
            UiEvent::SessionCreated { result, .. } => match result {
                Ok(session) => {
                    let id = session.id.clone();
                    self.sessions.push(session);
                    self.select_session(id);
                }
                Err(error) => self.connection_status = format!("Create failed: {error}"),
            },
            UiEvent::SessionRenamed {
                session_id, result, ..
            } => match result {
                Ok(session) => {
                    if let Some(existing) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        *existing = session;
                    }
                    self.persist_tabs();
                }
                Err(error) => self.connection_status = format!("Rename failed: {error}"),
            },
            UiEvent::ModelSelected {
                session_id,
                model,
                result,
                ..
            } => match result {
                Ok(()) => {
                    if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        session.model = Some(SessionModel {
                            id: model.id,
                            provider_id: model.provider_id,
                            variant: model.variant,
                        });
                    }
                }
                Err(error) => self.connection_status = format!("Model change failed: {error}"),
            },
            UiEvent::PromptAccepted {
                request_id,
                session_id,
                result,
            } => {
                if self
                    .pending_prompts
                    .get(&session_id)
                    .is_some_and(|(pending, _, _)| *pending == request_id)
                    && let Some((_, text, attachments)) = self.pending_prompts.remove(&session_id)
                {
                    match result {
                        Ok(()) => self
                            .clear_accepted_drafts
                            .push((session_id, text, attachments)),
                        Err(error) => self.connection_status = format!("Send failed: {error}"),
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
                            self.open_tabs.retain(|tab| tab != &id);
                            self.conversations.remove(&id);
                            self.transcript.remove(&id);
                            self.row_heights.remove(&id);
                            self.composers.remove(&id);
                            self.attachment_drafts.remove(&id);
                            self.pending_prompts.remove(&id);
                            self.clear_accepted_drafts
                                .retain(|(session, _, _)| session != &id);
                            self.unread.remove(&id);
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
                        self.update_transcript(id);
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
                    if let Some(model::CatalogInvalidation {
                        directory: Some(directory),
                        ..
                    }) = model::CatalogInvalidation::from_kind(&event, &kind)
                    {
                        self.catalogs.remove(&directory);
                        if let Some(api) = &self.api {
                            api.send(Command::LoadModels { directory });
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
                transcript.insert(
                    session.id.clone(),
                    conversation
                        .messages
                        .iter()
                        .filter(|message| !message.in_tray())
                        .flat_map(|message| message.rows())
                        .collect(),
                );
                conversations.insert(session.id.clone(), conversation);
            }
        }
        let attachments = transcript
            .iter()
            .flat_map(|(session_id, rows)| {
                rows.iter().enumerate().flat_map(move |(row_index, row)| {
                    row.images
                        .iter()
                        .enumerate()
                        .filter_map(move |(image_index, url)| {
                            inline_image(url)
                                .map(|image| ((session_id.clone(), row_index, image_index), image))
                        })
                })
            })
            .collect();
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
        scroll.scroll_to_bottom();
        let after_first_layout = scroll.clone();
        window.on_next_frame(move |window, _| {
            after_first_layout.scroll_to_bottom();
            window.refresh();
        });
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
            server_version: None,
            conversations,
            loading_messages: HashMap::new(),
            message_events_during_load: HashMap::new(),
            reload_after_load: HashSet::new(),
            preserve_scroll: None,
            catalogs: HashMap::new(),
            running_jobs,
            sessions,
            open_tabs: server.tabs.iter().map(|tab| tab.id.clone()).collect(),
            tab_focus: HashMap::new(),
            tab_shortcut_hint: false,
            tab_drop_target: None,
            bootstrapped: true,
            projects,
            active,
            transcript,
            row_heights: HashMap::new(),
            #[cfg(test)]
            measurement_probe: None,
            attachments,
            catalog,
            composer: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
                    .submit_on_enter(true)
                    .placeholder("Ask OpenCode anything…")
            }),
            composer_placeholder_focused: false,
            composer_session: String::new(),
            composers: HashMap::new(),
            attachments_draft: Vec::new(),
            attachment_drafts: HashMap::new(),
            overlay: if modal.is_none() { overlay } else { None },
            scroll,
            sessions_picker_scroll: ScrollHandle::new(),
            unread: server.unread,
            statuses: bootstrap.statuses,
            jobs,
            forms,
            permissions,
            permission_in_flight: HashSet::new(),
            permission_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            permission_presented: false,
            settings_tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            child_parents: HashMap::new(),
            next_prompt_request_id: 0,
            next_session_request_id: 0,
            next_model_request_id: 0,
            pending_prompts: HashMap::new(),
            tray_in_flight: HashSet::new(),
            clear_accepted_drafts: Vec::new(),
            modal,
            rename_target: None,
            picker_highlight: None,
            search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(match modal {
                    Some(Modal::Model) => "Search models (fuzzy)…",
                    Some(Modal::Level) => "Search levels (fuzzy)…",
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
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.accept_modal_choice(cx),
                _ => {}
            },
        )
        .detach();
        if settings_sessions {
            client.settings.tab = SettingsTab::Sessions;
        }
        cx.subscribe(
            &client.settings.session_search,
            |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
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
                        self.tone(0xa3a9a8, 0x899097)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.rename_target = Some(rename_id.clone());
                        this.show_modal(Modal::Rename, window, cx);
                    }))
                    .child("✎"),
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
                        self.tone(0xa3a9a8, 0x899097)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab(&close_id, cx);
                        this.focus_selected_composer(window, cx);
                    }))
                    .child("×"),
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
                div()
                    .id("new-session")
                    .h(px(39.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .text_color(self.tone(0x667078, 0x92999f))
                    .cursor_pointer()
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
                        div()
                            .id("footer-tabs")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_modal(Modal::Sessions, window, cx);
                            }))
                            .child("≡  Tabs"),
                    )
                    .child(
                        div()
                            .id("footer-settings")
                            .cursor_pointer()
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
                        .p(px(12.))
                        .border_l_4()
                        .border_color(self.tone(0xcf222e, 0xf85149))
                        .bg(self.tone(0xf8eae7, 0x271a1b))
                        .child(row.body.clone()),
                )
            }
            TranscriptRowKind::Normal if !user => {
                body = body.child(self.markdown_body(&row.body, index, cx))
            }
            _ => body = body.child(row.body.clone()),
        }
        for (image_index, image) in row.images.iter().enumerate() {
            let source = self
                .attachments
                .get(&(self.active.clone(), index, image_index));
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
            .gap(px(10.))
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
        let element = self.message_row(row, index, cx);
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
        let missing = cache.borrow_mut().missing(stamp, transcript);
        #[cfg(test)]
        let missing = if heights.is_some() {
            (0..transcript.len()).collect::<Vec<_>>()
        } else {
            missing
        };
        if missing.is_empty() {
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
            stamp,
            cache,
            #[cfg(test)]
            heights,
            spacer: div().size(px(0.)),
        })
    }

    fn form_notice(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let notice = self.forms.notice(Some(&self.active), &HashMap::new())?;
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
                div()
                    .text_color(self.tone(0x4d5354, 0xc4c8ca))
                    .child("Open web UI"),
            );
        if let Some(target) = notice.cancel {
            bar = bar.child(
                div()
                    .id("cancel-form")
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(api) = &this.api {
                            api.send(Command::CancelForm {
                                form_id: target.form_id.clone(),
                                session_id: target.session_id.clone(),
                                directory: target.directory.clone(),
                            });
                        }
                        cx.notify();
                    }))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(self.tone(0xc8c3ba, 0x353b40))
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
            details = details.child(
                div()
                    .text_size(px(11.))
                    .text_color(self.tone(0x626764, 0x9da4aa))
                    .child(metadata),
            );
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
            actions = actions.child(
                div()
                    .id(format!("permission-{label}-{}", request.id))
                    .role(Role::Button)
                    .aria_label(label)
                    .test_support()
                    .track_focus(&self.permission_focus[index])
                    .focus_visible(|style| style.border_color(self.tone(0x2356a8, 0x78baff)))
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
            .mx(px(16.))
            .mb(px(17.))
            .p(px(16.))
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

    fn chat(&self, cx: &Context<Self>) -> AnyElement {
        // GTK reserves a permanent 14px gutter for the transcript scrollbar.
        let mut rows = div()
            .id("transcript")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .w_full()
            .pr(px(TRANSCRIPT_SCROLLBAR_GUTTER))
            .flex()
            .flex_col();
        if let Some(cursor) = self
            .conversations
            .get(&self.active)
            .and_then(|conversation| conversation.next_cursor.clone())
        {
            let id = self.active.clone();
            let loading = self.loading_messages.contains_key(&id);
            rows = rows.child(
                div()
                    .id("load-earlier")
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
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.load_earlier(&id, &cursor, cx);
                            }))
                    })
                    .child(if loading {
                        "Loading earlier messages…"
                    } else {
                        "Load earlier messages"
                    }),
            );
        }
        let sticky = self.sticky_user_row().map(|row| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right(px(TRANSCRIPT_SCROLLBAR_GUTTER))
                .h(px(106.))
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
                .child(row.body.clone())
                .into_any_element()
        });
        if let Some(transcript) = self.transcript.get(&self.active) {
            for (index, row) in transcript.iter().enumerate() {
                rows = rows.child(self.message(row, index, cx));
            }
        }
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(rows)
            .when_some(sticky, |view, sticky| view.child(sticky))
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    fn sticky_user_row(&self) -> Option<&TranscriptRow> {
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
        let users = transcript.iter().enumerate().filter_map(|(index, row)| {
            if row.role.label() != "YOU" {
                return None;
            }
            let bounds = self.scroll.bounds_for_item(index + first_child)?;
            // GPUI keeps child bounds in unscrolled content coordinates.
            let row_top = bounds.origin.y + self.scroll.offset().y;
            Some((index, row_top, row_top + bounds.size.height))
        });
        sticky_user_index(users, top, bottom).and_then(|index| transcript.get(index))
    }

    fn tray_view(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let items = self.conversations.get(&self.active)?.tray_items();
        let rows = tray::tray_rows(&items, None, None, &self.tray_in_flight);
        if rows.is_empty() {
            return None;
        }
        let running = self
            .statuses
            .get(&self.active)
            .is_some_and(RunStatus::is_busy);
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
                                div()
                                    .id("resume-tray")
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.act_on_tray(id.clone(), request, cx);
                                    }))
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
                        div()
                            .id(format!("switch-waiting-{id}"))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act_on_tray(id.clone(), request, cx);
                            }))
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
                        div()
                            .id(format!("cancel-waiting-{id}"))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act_on_tray(id.clone(), request, cx);
                            }))
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

    fn composer(&self, cx: &Context<Self>) -> AnyElement {
        let selected_model = self.selected_model();
        let model = selected_model
            .as_ref()
            .and_then(|preferred| {
                self.catalog.models.iter().find(|model| {
                    model.provider_id == preferred.provider_id
                        && model.model_id == preferred.model_id
                })
            })
            .or_else(|| self.catalog.models.first());
        let context = model
            .and_then(|option| option.context_limit)
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
        let model = model.map_or("Choose model".to_owned(), |model| model.label.clone());
        let running = self
            .statuses
            .get(&self.active)
            .is_some_and(RunStatus::is_busy);
        let mut files = div().px(px(13.)).flex().gap(px(7.));
        for (index, path) in self.attachments_draft.iter().enumerate() {
            let target = path.clone();
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
                    .child(label)
                    .child(
                        div()
                            .id(format!("remove-attachment-{index}"))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.attachments_draft.retain(|path| path != &target);
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            );
        }
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
                    .when(running, |footer| {
                        footer.child(
                            div()
                                .id("stop-run")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(api) = &this.api {
                                        api.send(Command::Abort {
                                            session_id: this.active.clone(),
                                        });
                                    }
                                    cx.notify();
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
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send_prompt(false, cx);
                            }))
                            .w(px(32.))
                            .h(px(32.))
                            .rounded_full()
                            .bg(self.tone(0xc59535, 0xd29b52))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(0x17130e))
                            .child(
                                Icon::default()
                                    .data(include_bytes!("icons/send.svg"))
                                    .with_size(px(22.)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn settings_sessions_body(&self, cx: &Context<Self>) -> AnyElement {
        let query = self.settings.session_search.read(cx).value().to_lowercase();
        let mut rows = div().px(px(21.)).flex().flex_col().gap(px(7.));
        let mut shown = 0;
        for session in self
            .sessions
            .iter()
            .filter(|session| {
                query.is_empty()
                    || session.title.to_lowercase().contains(&query)
                    || session.directory.to_lowercase().contains(&query)
            })
            .take(50)
        {
            shown += 1;
            let id = session.id.clone();
            let open = self.open_tabs.contains(&id);
            rows = rows.child(
                div()
                    .id(format!("settings-session-{id}"))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(id.clone());
                        this.modal = None;
                        cx.notify();
                    }))
                    .px(px(14.))
                    .py(px(8.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(self.tone(
                        if open { 0xcbbba4 } else { 0xded8cb },
                        if open { 0x3c4a5f } else { 0x242a34 },
                    ))
                    .bg(self.tone(
                        if open { 0xf3ede3 } else { 0xfffdfa },
                        if open { 0x222b38 } else { 0x1a1f26 },
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
                    .mt(px(14.))
                    .mb(px(12.))
                    .h(px(50.))
                    .px(px(9.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(self.tone(0x4f99ee, 0x3b82f6))
                    .child("⌕")
                    .child(Input::new(&self.settings.session_search).appearance(false)),
            )
            .child(
                div()
                    .id("settings-sessions-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(rows),
            )
            .into_any_element()
    }

    fn modal_view(&self, cx: &Context<Self>) -> Option<AnyElement> {
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
            .on_click(cx.listener(|this, _, _, cx| {
                this.modal = None;
                cx.notify();
            }));
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
                    let query = self.search.read(cx).value().to_lowercase();
                    for (index, project) in self
                        .projects
                        .iter()
                        .filter(|project| {
                            project.worktree.to_lowercase().contains(&query)
                                || project
                                    .name
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&query)
                        })
                        .enumerate()
                    {
                        let directory = project.worktree.clone();
                        let label = project.name.clone().unwrap_or_else(|| {
                            project
                                .worktree
                                .rsplit('/')
                                .next()
                                .unwrap_or("repo")
                                .to_owned()
                        });
                        card = card.child(
                            div()
                                .id(format!("project-choice-{}", project.worktree))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.create_session(directory.clone(), cx);
                                }))
                                .h(px(44.))
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
                                        .child(project.worktree.clone()),
                                ),
                        );
                    }
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
                                .child(self.active.clone()),
                        )
                        .child("▣"),
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
                                    this.modal = None;
                                    cx.notify();
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
                    .gap(px(9.))
                    .bg(self.tone(0xfffdfa, 0x15181b));
                for (index, (label, input)) in fields.into_iter().enumerate() {
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
                            .h(px(34.))
                            .px(px(9.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(self.tone(
                                if index == 0 { 0x4f99ee } else { 0xd3cec5 },
                                if index == 0 { 0x3b82f6 } else { 0x262c36 },
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
                    self.settings_sessions_body(cx)
                };
                div()
                    .w(px(820.))
                    .h(px(674.))
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
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.settings.tab = SettingsTab::Connection;
                                        this.settings.server.focus_handle(cx).focus(window, cx);
                                        cx.notify();
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
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.settings.tab = SettingsTab::Sessions;
                                        this.settings
                                            .session_search
                                            .focus_handle(cx)
                                            .focus(window, cx);
                                        cx.notify();
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
                                    .child("127.0.0.1")
                                    .child("opencode-gpui v0.1.0"),
                            ),
                    )
                    .child(
                        div().w(px(540.)).flex().flex_col().child(content).child(
                            div()
                                .h(px(56.))
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
                                        .on_click(cx.listener(|this, _, _, cx| {
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
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.apply_settings(cx);
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
                let mut list = div()
                    .w(px(if model { 368. } else { 268. }))
                    .h(px(if model { 178. } else { 204. }))
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
                            .child("⌕")
                            .child(Input::new(&self.search).appearance(false)),
                    );
                if model {
                    let query = self.search.read(cx).value().to_lowercase();
                    for option in self
                        .catalog
                        .models
                        .iter()
                        .filter(|option| {
                            option.label.to_lowercase().contains(&query)
                                || option.model_id.to_lowercase().contains(&query)
                        })
                        .take(7)
                        .enumerate()
                    {
                        let (index, option) = option;
                        let selected = self.selected_model().as_ref().is_some_and(|preferred| {
                            preferred.provider_id == option.provider_id
                                && preferred.model_id == option.model_id
                        });
                        let selection = protocol::ModelRef {
                            id: option.model_id.clone(),
                            provider_id: option.provider_id.clone(),
                            variant: None,
                        };
                        list = list.child(
                            div()
                                .id(format!("pick-model-{}", option.model_id))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_model(selection.clone(), cx);
                                }))
                                .h(px(52.))
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
                                        .child(option.label.clone())
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
                                .child(if selected { "✓" } else { "" }),
                        );
                    }
                } else {
                    let chosen = self.selected_model();
                    let variants = chosen
                        .as_ref()
                        .and_then(|selection| self.catalog.find(selection))
                        .map(|option| option.variants.clone())
                        .unwrap_or_default();
                    let query = self.search.read(cx).value().to_lowercase();
                    for (index, variant) in std::iter::once(None)
                        .chain(variants.into_iter().map(Some))
                        .filter(|variant| {
                            variant
                                .as_deref()
                                .unwrap_or("Default")
                                .to_lowercase()
                                .contains(&query)
                        })
                        .enumerate()
                    {
                        let level = variant.as_deref().unwrap_or("Default");
                        let selected = chosen
                            .as_ref()
                            .is_some_and(|selection| selection.variant == variant);
                        let model_ref = chosen.as_ref().map(|selection| protocol::ModelRef {
                            id: selection.model_id.clone(),
                            provider_id: selection.provider_id.clone(),
                            variant: variant.clone(),
                        });
                        list = list.child(
                            div()
                                .id(format!("pick-level-{level}"))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(model) = &model_ref {
                                        this.choose_model(model.clone(), cx);
                                    }
                                }))
                                .h(px(33.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .bg(self.tone(
                                    if selected || self.picker_highlight == Some(index) {
                                        0x3584df
                                    } else {
                                        0xfffdfa
                                    },
                                    if selected || self.picker_highlight == Some(index) {
                                        0x2563eb
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
                list.into_any_element()
            }
        };
        let backdrop = if matches!(modal, Modal::Model | Modal::Level) {
            backdrop
                .items_start()
                .justify_end()
                .pl(px(if modal == Modal::Model { 225. } else { 379. }))
                .pb(px(77.))
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
        for id in &self.open_tabs {
            self.tab_focus
                .entry(id.clone())
                .or_insert_with(|| std::array::from_fn(|_| cx.focus_handle().tab_stop(true)));
        }
        if let Some((old_offset, old_max)) = self.preserve_scroll.take() {
            let scroll = self.scroll.clone();
            window.on_next_frame(move |window, _| {
                let new_max = scroll.max_offset();
                scroll.set_offset(point(old_offset.x, old_offset.y - (new_max.y - old_max.y)));
                window.refresh();
            });
        }
        let mut deferred = Vec::new();
        for (id, text, attachments) in std::mem::take(&mut self.clear_accepted_drafts) {
            if self.active == id {
                if self.composer.read(cx).value().as_ref() == text {
                    self.composer.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                    });
                }
                if self.attachments_draft == attachments {
                    self.attachments_draft.clear();
                }
            } else {
                deferred.push((id, text, attachments));
            }
        }
        self.clear_accepted_drafts = deferred;
        let row_probe = self.row_measurement_probe(window, cx);
        let permission_visible = self.modal.is_none() && self.visible_permission().is_some();
        if permission_visible && !self.permission_presented {
            self.permission_presented = true;
            let focus = self.permission_focus[0].clone();
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        } else if !permission_visible && self.permission_presented {
            self.permission_presented = false;
            if self.modal.is_none() {
                let focus = self.composer.focus_handle(cx);
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
        }
        let composer_slot = self
            .permission_card(cx)
            .unwrap_or_else(|| self.composer(cx));
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
                            .child(self.chat(cx))
                            .when_some(self.overlay.as_ref(), |view, overlay| {
                                view.child(overlay.clone())
                            })
                            .when_some(self.working_pill(), |view, pill| view.child(pill))
                            .when_some(self.form_notice(cx), |view, notice| view.child(notice))
                            .when_some(self.tray_view(cx), |view, tray| view.child(tray))
                            .child(composer_slot),
                    ),
            )
            .when_some(self.modal_view(cx), |view, modal| view.child(modal));
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
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    use super::{
        Client, MarkdownBlock, Modal, RowHeightCache, RowLayoutStamp, SESSION_PICKER_LIMIT,
        TRANSCRIPT_ROW_STYLE_REVISION, TabAttention, Theme, ThemeMode, filter_tab_sessions,
        fuzzy_score, inline_image, markdown_blocks, model, reorder_tab_ids, sticky_user_index,
        tab_indicator, tab_number_key,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AppContext, Bounds, Focusable, Role, TestAppContext, WindowBounds, WindowOptions, point,
        px, size,
    };
    use opencode_gpui::model::RunStatus;
    use opencode_gpui::persist::PersistedState;

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
                    client
                        .attachments
                        .retain(|(session, _, _), _| session != &client.active);
                    client.attachments.insert(
                        (client.active.clone(), 2, 0),
                        inline_image(&rows[2].images[0]).expect("fixture image decodes"),
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
            window.click("permission-Deny-per_preview", cx);
            assert!(window.try_find("permission-Deny-per_preview").is_none());
        })
        .unwrap();
        cx.update(|cx| assert!(client.read(cx).permissions.is_empty()));
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
            window.press("enter", cx);
            assert_eq!(
                window.find("settings-tab-connection").selected(),
                Some(true)
            );
            let sessions_focus = client.read(cx).settings_tab_focus[1].clone();
            sessions_focus.focus(window, cx);
            window.render_frame(cx);
            window.press("space", cx);
            assert_eq!(window.find("settings-tab-sessions").selected(), Some(true));
        })
        .unwrap();
        cx.update(|cx| assert!(!client.read(cx).settings.remember_password));
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
            assert!(client.read(cx).permission_focus[0].is_focused(window));
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
}
