use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub id: String,
    pub directory: String,
    pub title: String,
    pub time: SessionTime,
    #[serde(default, rename = "parentID")]
    pub parent_id: Option<String>,
    /// The model saved on the session by the server; `None` follows the
    /// server's default model.
    #[serde(default)]
    pub model: Option<SessionModel>,
}

impl Session {
    pub fn model_selection(&self) -> Option<ModelSelection> {
        self.model.as_ref().map(|model| ModelSelection {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            variant: model.variant.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionModel {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default)]
    pub archived: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Project {
    pub worktree: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A model plus optional variant. `model_id` is the catalog ID
/// (`ModelInfo.id`). The field names are persisted in `state.json`, so keep
/// them stable.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl ModelSelection {
    pub fn from_ref(model: &protocol::ModelRef) -> Self {
        Self {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            variant: model.variant.clone(),
        }
    }

    pub fn to_ref(&self) -> protocol::ModelRef {
        protocol::ModelRef {
            id: self.model_id.clone(),
            provider_id: self.provider_id.clone(),
            variant: self.variant.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOption {
    pub provider_id: String,
    /// Catalog ID (`ModelInfo.id`), which is what `ModelRef.id` takes.
    pub model_id: String,
    pub label: String,
    pub variants: Vec<String>,
    pub supports_attachments: bool,
    pub context_limit: Option<u64>,
}

/// One location's model catalog. Only display data is kept: provider
/// `settings`, `headers` and `body` (which carry API keys) never reach it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    pub models: Vec<ModelOption>,
    /// The server's default model (`GET /api/model/default`), else the first
    /// model. Display only: it is never sent to the server.
    pub preferred: Option<ModelSelection>,
}

/// Input modalities that the composer can attach.
const ATTACHMENT_INPUTS: [&str; 4] = ["image", "pdf", "audio", "video"];

impl ModelCatalog {
    /// Builds the catalog from `GET /api/model` and `GET /api/model/default`.
    /// Disabled and deprecated models are left out. The provider ID labels
    /// the provider: display names need `GET /api/provider`, whose entries
    /// also carry provider settings.
    pub fn from_models(
        models: &[protocol::ModelInfo],
        default: Option<&protocol::ModelInfo>,
    ) -> Self {
        let mut options: Vec<ModelOption> = models
            .iter()
            .filter(|model| {
                model.enabled && model.status != Some(protocol::ModelStatus::Deprecated)
            })
            .map(|model| {
                let name = if model.name.trim().is_empty() {
                    model.id.as_str()
                } else {
                    model.name.as_str()
                };
                let mut variants: Vec<String> = model
                    .variants
                    .iter()
                    .map(|variant| variant.id.clone())
                    .collect();
                variants.sort_by_key(|variant| variant_rank(variant));
                variants.dedup();
                ModelOption {
                    provider_id: model.provider_id.clone(),
                    model_id: model.id.clone(),
                    label: format!("{} / {name}", model.provider_id),
                    variants,
                    supports_attachments: ATTACHMENT_INPUTS
                        .iter()
                        .any(|input| model.accepts_input(input)),
                    context_limit: Some(model.limit.context).filter(|limit| *limit > 0),
                }
            })
            .collect();
        options.sort_by_key(|left| left.label.to_lowercase());
        let preferred = default
            .map(|model| ModelSelection {
                provider_id: model.provider_id.clone(),
                model_id: model.id.clone(),
                variant: None,
            })
            .filter(|selection| contains_model(&options, selection))
            .or_else(|| {
                options.first().map(|model| ModelSelection {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                    variant: None,
                })
            });
        Self {
            models: options,
            preferred,
        }
    }

    pub fn find(&self, selection: &ModelSelection) -> Option<&ModelOption> {
        self.models.iter().find(|model| {
            model.provider_id == selection.provider_id && model.model_id == selection.model_id
        })
    }
}

fn contains_model(models: &[ModelOption], selection: &ModelSelection) -> bool {
    models.iter().any(|model| {
        model.provider_id == selection.provider_id && model.model_id == selection.model_id
    })
}

/// The model the composer shows for a session: an explicit pick still in
/// flight, then the model saved on the session, then the catalog's default
/// (the server default, else the first model). Only the first two are server
/// state; the fallback is display only and must never be sent (P5).
pub fn displayed_model(
    pending: Option<&ModelSelection>,
    session: Option<&Session>,
    catalog: &ModelCatalog,
) -> Option<ModelSelection> {
    pending
        .cloned()
        .or_else(|| session.and_then(Session::model_selection))
        .or_else(|| catalog.preferred.clone())
}

/// The switch an explicit pick requests, or `None` when it matches what is
/// already shown. Re-picking a displayed default therefore leaves the session
/// following the server default instead of pinning it.
pub fn model_switch_for_pick(
    displayed: Option<&ModelSelection>,
    picked: ModelSelection,
) -> Option<ModelSelection> {
    (displayed != Some(&picked)).then_some(picked)
}

fn variant_rank(value: &str) -> (usize, String) {
    let rank = match value.to_ascii_lowercase().as_str() {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" | "max" => 4,
        _ => 5,
    };
    (rank, value.to_ascii_lowercase())
}

/// Why the model catalogs must be refetched (R5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogInvalidation {
    /// The event's location; `None` means every loaded catalog.
    pub directory: Option<String>,
    /// `location.shutdown`: the location's instance is gone, so the session
    /// list is refreshed as well.
    pub shutdown: bool,
}

impl CatalogInvalidation {
    /// `payload` is the whole `/api/event` event.
    #[cfg(test)]
    pub fn from_event(payload: &Value) -> Option<Self> {
        let event = protocol::Event::deserialize(payload).ok()?;
        Self::from_kind(&event, &protocol::decode_event(&event))
    }

    pub fn from_kind(event: &protocol::Event, kind: &protocol::EventKind) -> Option<Self> {
        let shutdown = match kind {
            protocol::EventKind::LocationShutdown => true,
            protocol::EventKind::CatalogChanged(
                protocol::CatalogChange::Model
                | protocol::CatalogChange::Provider
                | protocol::CatalogChange::ModelsDev
                | protocol::CatalogChange::Integration
                | protocol::CatalogChange::Credential
                | protocol::CatalogChange::Config,
            ) => false,
            _ => return None,
        };
        Some(Self {
            directory: event
                .directory()
                .filter(|directory| !directory.is_empty())
                .map(str::to_owned),
            shutdown,
        })
    }
}

pub fn format_context_usage(used: u64, limit: u64) -> String {
    format!("{} / {}", compact_tokens(used), compact_tokens(limit))
}

fn compact_tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 100_000 {
        let tenths = (n + 50) / 100;
        if tenths.is_multiple_of(10) {
            format!("{}k", tenths / 10)
        } else {
            format!("{}.{}k", tenths / 10, tenths % 10)
        }
    } else if n < 1_000_000 {
        format!("{}k", (n + 500) / 1000)
    } else if n < 10_000_000 {
        let tenths = (n + 50_000) / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}m", tenths / 10)
        } else {
            format!("{}.{}m", tenths / 10, tenths % 10)
        }
    } else {
        format!("{}m", (n + 500_000) / 1_000_000)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "YOU",
            Self::Assistant => "AGENT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    Text,
    Reasoning,
    Tool,
    File,
}

impl SegmentKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
            Self::Tool => "tool",
            Self::File => "file",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub key: String,
    pub kind: SegmentKind,
    pub text: String,
    pub image_url: Option<String>,
    pub created: u64,
    /// A tool segment's structured state; `text` is always rendered from it
    /// by [`tool_segment`], for history and live events alike.
    pub tool: Option<protocol::ToolCall>,
}

impl Segment {
    fn text(key: String, kind: SegmentKind, text: String, created: u64) -> Self {
        Self {
            key,
            kind,
            text,
            image_url: None,
            created,
            tool: None,
        }
    }
}

/// The source of a note row that later events update in place.
#[derive(Clone, Debug, PartialEq)]
enum NoteSource {
    Shell(protocol::ShellMessage),
    Compaction(protocol::CompactionMessage),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub role: Role,
    pub created: u64,
    segments: Vec<Segment>,
    error: Option<String>,
    context_tokens: Option<u64>,
    /// An inbox item the server has not delivered yet. Queued messages stay
    /// after every delivered one, the way the server appends the entry only
    /// on delivery. Undelivered user prompts show in the tray
    /// ([`Conversation::tray_items`]), not as transcript rows.
    queued: bool,
    /// A waiting item's delivery mode (steer or queue).
    delivery: Option<protocol::Delivery>,
    note: Option<NoteSource>,
}

impl ChatMessage {
    #[cfg(test)]
    pub fn segment_keys(&self) -> Vec<String> {
        self.segments
            .iter()
            .map(|segment| segment.key.clone())
            .collect()
    }

    fn placeholder(id: impl Into<String>, role: Role) -> Self {
        Self {
            id: id.into(),
            role,
            created: 0,
            segments: Vec::new(),
            error: None,
            context_tokens: None,
            queued: false,
            delivery: None,
            note: None,
        }
    }

    /// An undelivered user prompt: a tray item, not a transcript row.
    pub fn in_tray(&self) -> bool {
        self.queued && self.role == Role::User
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn render(&self) -> String {
        let mut blocks = Vec::new();
        for segment in &self.segments {
            if segment.text.trim().is_empty() {
                continue;
            }
            match segment.kind {
                SegmentKind::Text => blocks.push(segment.text.clone()),
                SegmentKind::Reasoning => {
                    blocks.push(format!("Reasoning\n{}", segment.text.trim()))
                }
                SegmentKind::Tool | SegmentKind::File => blocks.push(segment.text.clone()),
            }
        }
        if let Some(error) = &self.error {
            blocks.push(format!("Error: {error}"));
        }
        blocks.join("\n\n")
    }

    /// The message split into GTK's transcript rows: the accumulated text and
    /// images become a row, a reasoning or tool segment becomes its own row,
    /// and the error, if any, closes the message.
    pub fn rows(&self) -> Vec<TranscriptRow> {
        let mut rows = Vec::new();
        let mut blocks: Vec<String> = Vec::new();
        let mut images: Vec<String> = Vec::new();
        let role = self.role;
        let mut push = |body: String, images: Vec<String>, time: u64, kind: TranscriptRowKind| {
            if body.is_empty() && images.is_empty() {
                return;
            }
            rows.push(TranscriptRow {
                role,
                body,
                images,
                time,
                kind,
            });
        };
        for segment in &self.segments {
            if matches!(segment.kind, SegmentKind::Tool | SegmentKind::Reasoning) {
                push(
                    blocks.join("\n\n"),
                    std::mem::take(&mut images),
                    self.created,
                    TranscriptRowKind::Normal,
                );
                blocks = Vec::new();
                if !segment.text.trim().is_empty() {
                    let (body, kind) = match segment.kind {
                        SegmentKind::Reasoning => (
                            format!("Reasoning\n{}", segment.text.trim()),
                            TranscriptRowKind::Reasoning,
                        ),
                        _ => (segment.text.clone(), TranscriptRowKind::Tool),
                    };
                    push(
                        body,
                        Vec::new(),
                        if segment.created == 0 {
                            self.created
                        } else {
                            segment.created
                        },
                        kind,
                    );
                }
                continue;
            }
            if !segment.text.trim().is_empty() {
                blocks.push(segment.text.clone());
            }
            if let Some(url) = &segment.image_url {
                images.push(url.clone());
            }
        }
        push(
            blocks.join("\n\n"),
            std::mem::take(&mut images),
            self.created,
            TranscriptRowKind::Normal,
        );
        if let Some(error) = &self.error {
            push(
                error.clone(),
                Vec::new(),
                self.created,
                TranscriptRowKind::Error,
            );
        }
        rows
    }

    fn transcript_rows(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|row| {
                let kind = match row.kind {
                    TranscriptRowKind::Normal => "",
                    TranscriptRowKind::Reasoning => "reasoning",
                    TranscriptRowKind::Tool => "tool",
                    TranscriptRowKind::Error => "error",
                };
                serde_json::json!({
                    "role": row.role.label(),
                    "body": row.body,
                    "images": row.images,
                    "time": row.time,
                    "kind": kind,
                })
                .to_string()
            })
            .collect()
    }

    fn segment_mut(&mut self, key: &str) -> Option<&mut Segment> {
        self.segments.iter_mut().find(|segment| segment.key == key)
    }
}

/// The kind of a transcript row: GTK gave reasoning, tool and error rows
/// their own widget classes (`message-reasoning`, `message-error-row`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptRowKind {
    Normal,
    Reasoning,
    Tool,
    Error,
}

/// One rendered transcript row (GTK's `TranscriptRow`): a message is split at
/// every reasoning and tool segment, so each gets its own header, timestamp
/// and row styling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptRow {
    pub role: Role,
    pub body: String,
    pub images: Vec<String>,
    pub time: u64,
    pub kind: TranscriptRowKind,
}

/// One undelivered user prompt (a tray row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    /// The inbox ID (the prompt `id`).
    pub id: String,
    pub delivery: protocol::Delivery,
    pub text: String,
    pub attachments: usize,
}

fn item_delivery(item: &protocol::InboxItem) -> Option<protocol::Delivery> {
    match item {
        protocol::InboxItem::User { delivery, .. }
        | protocol::InboxItem::Synthetic { delivery, .. }
        | protocol::InboxItem::Compaction { delivery }
        | protocol::InboxItem::Move { delivery } => *delivery,
        protocol::InboxItem::Unknown => None,
    }
}

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub messages: Vec<ChatMessage>,
    pub next_cursor: Option<String>,
    pub loaded: bool,
}

impl Conversation {
    /// `entries` is one history page in chronological order (oldest first).
    /// Queued (undelivered) rows are not history, so they are kept after it
    /// until [`Conversation::sync_queued`] or live inbox events settle them.
    pub fn replace_from_api(
        &mut self,
        entries: &[protocol::SessionMessage],
        next_cursor: Option<String>,
    ) {
        let queued: Vec<_> = std::mem::take(&mut self.messages)
            .into_iter()
            .filter(|message| message.queued)
            .collect();
        self.messages = entries.iter().filter_map(message_from_entry).collect();
        for message in queued {
            if !self.contains(&message.id) {
                self.messages.push(message);
            }
        }
        self.next_cursor = next_cursor;
        self.loaded = true;
    }

    /// Prepends an older page (chronological order), skipping known entries.
    pub fn prepend_from_api(
        &mut self,
        entries: &[protocol::SessionMessage],
        next_cursor: Option<String>,
    ) {
        let existing: HashMap<_, _> = self
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| (message.id.as_str(), index))
            .collect();
        let mut earlier: Vec<_> = entries
            .iter()
            .filter_map(message_from_entry)
            .filter(|message| !existing.contains_key(message.id.as_str()))
            .collect();
        earlier.append(&mut self.messages);
        self.messages = earlier;
        self.next_cursor = next_cursor;
        self.loaded = true;
    }

    /// Reconciles queued items with `GET /api/session/{id}/inbox` (oldest
    /// first): undelivered user and synthetic items are kept keyed by their
    /// inbox ID (which becomes their message ID on delivery), with the
    /// listed delivery; items no longer listed go away.
    pub fn sync_queued(&mut self, entries: &[protocol::InboxEntry]) {
        let ids: HashSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        self.messages
            .retain(|message| !message.queued || ids.contains(message.id.as_str()));
        for entry in entries {
            if !self.enqueue(&entry.id, &entry.item, millis(entry.time.created))
                && let Some(delivery) = item_delivery(&entry.item)
            {
                self.change_delivery(&entry.id, delivery);
            }
        }
    }

    /// Undelivered user prompts of this session, oldest first: the tray.
    /// Synthetic items (subagent completions) are not in it; they keep
    /// their transcript rows.
    pub fn tray_items(&self) -> Vec<TrayItem> {
        self.messages
            .iter()
            .filter(|message| message.in_tray())
            .map(|message| TrayItem {
                id: message.id.clone(),
                delivery: message.delivery.unwrap_or(protocol::Delivery::Steer),
                text: message
                    .segments
                    .iter()
                    .filter(|segment| segment.kind == SegmentKind::Text)
                    .map(|segment| segment.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                attachments: message
                    .segments
                    .iter()
                    .filter(|segment| segment.kind == SegmentKind::File)
                    .count(),
            })
            .collect()
    }

    #[cfg(test)]
    pub fn rendered_rows(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| !message.in_tray())
            .filter_map(|message| {
                let content = message.render();
                (!content.is_empty()).then(|| format!("{}\n{content}", message.role.label()))
            })
            .collect()
    }

    /// Transcript rows: every message except the tray's undelivered prompts.
    pub fn transcript_rows(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| !message.in_tray())
            .flat_map(ChatMessage::transcript_rows)
            .collect()
    }

    /// A prompt's `id` is its inbox ID and, once delivered, its user message
    /// ID; waiting (tray) and delivered prompts both count.
    pub fn has_user_message(&self, id: &str) -> bool {
        self.messages
            .iter()
            .any(|message| message.role == Role::User && message.id == id)
    }

    /// The prompt with this `id` was delivered (it is a transcript row).
    pub fn has_delivered_user_message(&self, id: &str) -> bool {
        self.messages
            .iter()
            .any(|message| message.role == Role::User && !message.queued && message.id == id)
    }

    pub fn context_tokens(&self) -> Option<u64> {
        self.messages.iter().rev().find_map(|message| {
            (message.role == Role::Assistant)
                .then_some(message.context_tokens)
                .flatten()
        })
    }

    /// Applies one whole `/api/event` event; see [`Conversation::apply`].
    pub fn apply_event(&mut self, payload: &Value) -> bool {
        let Ok(event) = protocol::Event::deserialize(payload) else {
            return false;
        };
        let kind = protocol::decode_event(&event);
        self.apply(&event, &kind)
    }

    /// Folds a live event into the transcript the way the server projects it
    /// into the message list (`core/src/session/message-updater.ts` and the
    /// inbox projector), with the keys of [`message_from_entry`]. Replaying
    /// events onto a snapshot that already reflects them converges on the same
    /// state: `*.ended` and `message.content.updated` replace text, started
    /// events never reset an existing segment, and tool states never regress.
    /// Returns whether anything visible may have changed.
    pub fn apply(&mut self, event: &protocol::Event, kind: &protocol::EventKind) -> bool {
        use protocol::EventKind as Kind;
        let created = event.created.map(millis).unwrap_or(0);
        match kind {
            Kind::InboxEnqueued(data) => self.enqueue(&data.inbox_id, &data.item, created),
            Kind::InboxDelivered(data) => self.deliver(&data.inbox_id, created),
            Kind::InboxCancelled(data) => self.cancel(&data.inbox_id),
            Kind::InboxDeliveryChanged(data) => self.change_delivery(&data.inbox_id, data.delivery),
            Kind::StepStarted(data) => {
                let index = self.ensure_message(&data.assistant_message_id, Role::Assistant);
                let message = &mut self.messages[index];
                // A retried step restarts the same message.
                message.error = None;
                if data.started > 0 {
                    message.created = millis(data.started);
                }
                true
            }
            Kind::StepEnded(data) => {
                let index = self.ensure_message(&data.assistant_message_id, Role::Assistant);
                if let Some(tokens) = usage_tokens(&data.tokens) {
                    self.messages[index].context_tokens = Some(tokens);
                }
                true
            }
            Kind::StepFailed(data) => {
                let index = self.ensure_message(&data.assistant_message_id, Role::Assistant);
                let message = &mut self.messages[index];
                if !is_interrupt(&data.error) {
                    message.error = Some(structured_error_text(&data.error));
                }
                if let Some(tokens) = data.tokens.as_ref().and_then(usage_tokens) {
                    message.context_tokens = Some(tokens);
                }
                true
            }
            Kind::TextStarted(data) => self.start_fragment(
                &data.assistant_message_id,
                SegmentKind::Text,
                data.ordinal,
                0,
            ),
            Kind::TextDelta(data) => self.append_fragment(
                &data.assistant_message_id,
                SegmentKind::Text,
                data.ordinal,
                &data.delta,
            ),
            Kind::TextEnded(data) => self.end_fragment(
                &data.assistant_message_id,
                SegmentKind::Text,
                data.ordinal,
                &data.text,
                0,
            ),
            Kind::ReasoningStarted(data) => self.start_fragment(
                &data.assistant_message_id,
                SegmentKind::Reasoning,
                data.ordinal,
                created,
            ),
            Kind::ReasoningDelta(data) => self.append_fragment(
                &data.assistant_message_id,
                SegmentKind::Reasoning,
                data.ordinal,
                &data.delta,
            ),
            Kind::ReasoningEnded(data) => self.end_fragment(
                &data.assistant_message_id,
                SegmentKind::Reasoning,
                data.ordinal,
                &data.text,
                created,
            ),
            Kind::ToolInputStarted(data) => self.start_tool(data, event.created.unwrap_or(0)),
            Kind::ToolInputDelta(data) => self.update_tool(
                &data.assistant_message_id,
                &data.id,
                |tool| match &mut tool.state {
                    protocol::ToolState::Streaming { input } if !data.delta.is_empty() => {
                        input.push_str(&data.delta);
                        true
                    }
                    _ => false,
                },
            ),
            Kind::ToolInputEnded(data) => self.update_tool(
                &data.assistant_message_id,
                &data.id,
                |tool| match &mut tool.state {
                    protocol::ToolState::Streaming { input } => {
                        input.clone_from(&data.text);
                        true
                    }
                    _ => false,
                },
            ),
            Kind::ToolCalled(data) => {
                self.update_tool(&data.assistant_message_id, &data.id, |tool| {
                    if tool_settled(tool) {
                        return false;
                    }
                    tool.executed = Some(data.executed);
                    tool.time.ran = event.created;
                    tool.state = protocol::ToolState::Running {
                        input: data.input.clone(),
                        metadata: protocol::JsonMap::new(),
                    };
                    true
                })
            }
            Kind::ToolProgress(data) => {
                // Metadata (e.g. a subagent's child session) is not rendered.
                self.update_tool(&data.assistant_message_id, &data.id, |tool| {
                    if let protocol::ToolState::Running { metadata, .. } = &mut tool.state {
                        metadata.clone_from(&data.metadata);
                    }
                    false
                })
            }
            Kind::ToolSuccess(data) => {
                self.update_tool(&data.assistant_message_id, &data.id, |tool| {
                    let Some(input) = tool_input(tool) else {
                        return false;
                    };
                    tool.executed = Some(data.executed || tool.executed == Some(true));
                    tool.time.completed = event.created;
                    tool.state = protocol::ToolState::Completed {
                        input,
                        content: data.content.clone(),
                        metadata: data.metadata.clone(),
                    };
                    true
                })
            }
            Kind::ToolFailed(data) => {
                self.update_tool(&data.assistant_message_id, &data.id, |tool| {
                    let Some(input) = tool_input(tool) else {
                        return false;
                    };
                    tool.executed = Some(data.executed || tool.executed == Some(true));
                    tool.time.completed = event.created;
                    tool.state = protocol::ToolState::Error {
                        input,
                        error: data.error.clone(),
                        content: data.content.clone(),
                        metadata: data.metadata.clone(),
                    };
                    true
                })
            }
            Kind::MessageContentUpdated(data) => {
                let Some(message) = self.messages.iter_mut().find(|message| {
                    message.role == Role::Assistant && message.id == data.message_id
                }) else {
                    return false;
                };
                message.segments = assistant_segments(&data.content);
                true
            }
            Kind::ExecutionFailed(data) => self.execution_failed(event, &data.error, created),
            Kind::Synthetic(data) => self.add_note(
                event,
                created,
                synthetic_text(&data.text, data.description.as_deref()),
            ),
            Kind::SkillActivated(data) => {
                self.add_note(event, created, skill_text(&data.name, &data.id))
            }
            Kind::InstructionsUpdated(data) => {
                let Some(text) = &data.text else {
                    return false;
                };
                // `description` is `Instructions updated: <delta keys>`.
                let keys: Vec<&str> = event
                    .data
                    .get("delta")
                    .and_then(Value::as_object)
                    .map(|delta| delta.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                let description = format!("Instructions updated: {}", keys.join(", "));
                self.add_note(event, created, system_text(Some(&description), text))
            }
            Kind::ShellStarted(data) => {
                let Some(id) = event.projected_message_id() else {
                    return false;
                };
                if self.contains(&id) {
                    return false;
                }
                let shell = protocol::ShellMessage {
                    id,
                    time: protocol::ShellMessageTime {
                        created: event.created.unwrap_or(0),
                        completed: None,
                    },
                    shell_id: data.shell.id.clone(),
                    command: data.shell.command.clone(),
                    status: data.shell.status,
                    exit: None,
                    output: None,
                };
                match shell_message(&shell) {
                    Some(message) => {
                        self.insert_message(message);
                        true
                    }
                    None => false,
                }
            }
            Kind::ShellEnded(data) => {
                let Some(index) = self.messages.iter().position(|message| {
                    matches!(&message.note, Some(NoteSource::Shell(shell)) if shell.shell_id == data.shell.id)
                }) else {
                    return false;
                };
                let Some(NoteSource::Shell(mut shell)) = self.messages[index].note.clone() else {
                    return false;
                };
                shell.status = data.shell.status;
                shell.exit = data.shell.exit.map(Value::from);
                shell.output = Some(data.output.clone());
                shell.time.completed = event.created;
                self.replace_note(index, shell_message(&shell))
            }
            Kind::CompactionStarted(data) => {
                let Some(id) = data
                    .input_id
                    .clone()
                    .or_else(|| event.projected_message_id())
                else {
                    return false;
                };
                if self.contains(&id) {
                    return false;
                }
                let compaction = compaction_entry(
                    id,
                    event.created.unwrap_or(0),
                    protocol::CompactionStatus::Running,
                );
                match compaction_message(&compaction) {
                    Some(message) => {
                        self.insert_message(message);
                        true
                    }
                    None => false,
                }
            }
            Kind::CompactionEnded(data) => {
                if let Some((index, mut compaction)) = self.running_compaction() {
                    compaction.status = protocol::CompactionStatus::Completed;
                    compaction.summary.clone_from(&data.text);
                    compaction.recent.clone_from(&data.recent);
                    return self.replace_note(index, compaction_message(&compaction));
                }
                let Some(id) = event.projected_message_id() else {
                    return false;
                };
                if self.contains(&id) {
                    return false;
                }
                let mut compaction = compaction_entry(
                    id,
                    event.created.unwrap_or(0),
                    protocol::CompactionStatus::Completed,
                );
                compaction.summary.clone_from(&data.text);
                match compaction_message(&compaction) {
                    Some(message) => {
                        self.insert_message(message);
                        true
                    }
                    None => false,
                }
            }
            Kind::CompactionFailed(data) => {
                if let Some((index, mut compaction)) = self.running_compaction() {
                    compaction.status = protocol::CompactionStatus::Failed;
                    compaction.error = Some(data.error.clone());
                    return self.replace_note(index, compaction_message(&compaction));
                }
                let Some(id) = data
                    .input_id
                    .clone()
                    .or_else(|| event.projected_message_id())
                else {
                    return false;
                };
                if self.contains(&id) {
                    return false;
                }
                let mut compaction = compaction_entry(
                    id,
                    event.created.unwrap_or(0),
                    protocol::CompactionStatus::Failed,
                );
                compaction.error = Some(data.error.clone());
                match compaction_message(&compaction) {
                    Some(message) => {
                        self.insert_message(message);
                        true
                    }
                    None => false,
                }
            }
            Kind::SessionAgentSelected(data) => {
                self.add_note(event, created, agent_switch_text(&data.agent))
            }
            Kind::SessionModelSelected(data) => {
                self.add_note(event, created, Some(model_switch_text(&data.model)))
            }
            Kind::SessionMoved(data) => self.add_note(
                event,
                created,
                location_switch_text(&data.location.directory),
            ),
            _ => false,
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.messages.iter().any(|message| message.id == id)
    }

    /// Index of the first queued row, where delivered messages end.
    fn queue_start(&self) -> usize {
        self.messages
            .iter()
            .position(|message| message.queued)
            .unwrap_or(self.messages.len())
    }

    fn insert_message(&mut self, message: ChatMessage) -> usize {
        if message.queued {
            self.messages.push(message);
            return self.messages.len() - 1;
        }
        let index = self.queue_start();
        self.messages.insert(index, message);
        index
    }

    fn ensure_message(&mut self, id: &str, role: Role) -> usize {
        if let Some(index) = self.messages.iter().position(|message| message.id == id) {
            return index;
        }
        self.insert_message(ChatMessage::placeholder(id, role))
    }

    /// `session.inbox.enqueued`, or a row of the inbox list. The row takes
    /// the history projection of the entry the server writes on delivery.
    fn enqueue(&mut self, id: &str, item: &protocol::InboxItem, created: u64) -> bool {
        if self.contains(id) {
            return false;
        }
        let message = match item {
            protocol::InboxItem::User { payload, .. } => Some(user_chat_message(
                id,
                created,
                &payload.text,
                &payload.files,
            )),
            protocol::InboxItem::Synthetic { payload, .. } => note_message(
                id,
                created as protocol::Millis,
                synthetic_text(&payload.text, payload.description.as_deref()),
            ),
            _ => None,
        };
        let Some(mut message) = message else {
            return false;
        };
        message.queued = true;
        message.delivery = item_delivery(item);
        self.insert_message(message);
        true
    }

    /// `session.inbox.delivery.changed`: a waiting item switched mode.
    fn change_delivery(&mut self, id: &str, delivery: protocol::Delivery) -> bool {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.queued && message.id == id)
        else {
            return false;
        };
        let changed = message.delivery != Some(delivery);
        message.delivery = Some(delivery);
        changed
    }

    /// The server appends the entry when it delivers the item, with the
    /// delivery time, so the row moves to the end of the delivered messages.
    fn deliver(&mut self, id: &str, created: u64) -> bool {
        let Some(index) = self
            .messages
            .iter()
            .position(|message| message.queued && message.id == id)
        else {
            return false;
        };
        let mut message = self.messages.remove(index);
        message.queued = false;
        if created > 0 {
            message.created = created;
        }
        self.insert_message(message);
        true
    }

    fn cancel(&mut self, id: &str) -> bool {
        let before = self.messages.len();
        self.messages
            .retain(|message| !(message.queued && message.id == id));
        self.messages.len() != before
    }

    fn start_fragment(
        &mut self,
        message_id: &str,
        kind: SegmentKind,
        ordinal: u32,
        created: u64,
    ) -> bool {
        let key = format!("{}:{ordinal}", kind.prefix());
        let index = self.ensure_message(message_id, Role::Assistant);
        let message = &mut self.messages[index];
        if message.segment_mut(&key).is_some() {
            return false;
        }
        message
            .segments
            .push(Segment::text(key, kind, String::new(), created));
        true
    }

    fn append_fragment(
        &mut self,
        message_id: &str,
        kind: SegmentKind,
        ordinal: u32,
        delta: &str,
    ) -> bool {
        if delta.is_empty() {
            return false;
        }
        let key = format!("{}:{ordinal}", kind.prefix());
        let index = self.ensure_message(message_id, Role::Assistant);
        let message = &mut self.messages[index];
        match message.segment_mut(&key) {
            Some(segment) => segment.text.push_str(delta),
            None => message
                .segments
                .push(Segment::text(key, kind, delta.to_owned(), 0)),
        }
        true
    }

    /// `*.ended` carries the full text, which replaces whatever the deltas
    /// built (a delta replayed onto a fresh snapshot included).
    fn end_fragment(
        &mut self,
        message_id: &str,
        kind: SegmentKind,
        ordinal: u32,
        text: &str,
        created: u64,
    ) -> bool {
        let key = format!("{}:{ordinal}", kind.prefix());
        let index = self.ensure_message(message_id, Role::Assistant);
        let message = &mut self.messages[index];
        match message.segment_mut(&key) {
            Some(segment) if segment.text == text => false,
            Some(segment) => {
                segment.text = text.to_owned();
                true
            }
            None => {
                message
                    .segments
                    .push(Segment::text(key, kind, text.to_owned(), created));
                true
            }
        }
    }

    fn start_tool(&mut self, data: &protocol::ToolInputStarted, created: protocol::Millis) -> bool {
        let index = self.ensure_message(&data.assistant_message_id, Role::Assistant);
        let message = &mut self.messages[index];
        if message.segment_mut(&tool_key(&data.id)).is_some() {
            return false;
        }
        message.segments.push(tool_segment(&protocol::ToolCall {
            id: data.id.clone(),
            name: data.name.clone(),
            executed: None,
            state: protocol::ToolState::Streaming {
                input: String::new(),
            },
            time: protocol::ToolTime {
                created,
                ran: None,
                completed: None,
            },
        }));
        true
    }

    /// Updates the tool `tool_id` of one assistant message and re-renders its
    /// row. Events for a tool this conversation never saw start are ignored,
    /// as the server's projection ignores them.
    fn update_tool(
        &mut self,
        message_id: &str,
        tool_id: &str,
        update: impl FnOnce(&mut protocol::ToolCall) -> bool,
    ) -> bool {
        let Some(segment) = self
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
            .and_then(|message| message.segment_mut(&tool_key(tool_id)))
        else {
            return false;
        };
        let Some(tool) = segment.tool.as_mut() else {
            return false;
        };
        if !update(tool) {
            return false;
        }
        let rendered = tool_segment(tool);
        *segment = rendered;
        true
    }

    /// A failed execution normally repeats the error of its failed step,
    /// which is already on that assistant message. Otherwise (a failure
    /// outside any step) it becomes an error row keyed like the `idle` entry
    /// the server writes for it; history does not render that entry, so the
    /// row lasts until the transcript is reloaded.
    fn execution_failed(
        &mut self,
        event: &protocol::Event,
        error: &protocol::StructuredError,
        created: u64,
    ) -> bool {
        if is_interrupt(error) {
            return false;
        }
        let turn_failed = self
            .messages
            .iter()
            .rev()
            .filter(|message| !message.queued)
            .take_while(|message| message.role != Role::User)
            .any(|message| message.error.is_some());
        if turn_failed {
            return false;
        }
        let Some(id) = event.projected_message_id() else {
            return false;
        };
        let index = self.ensure_message(&id, Role::Assistant);
        let message = &mut self.messages[index];
        if message.created == 0 {
            message.created = created;
        }
        message.error = Some(structured_error_text(error));
        true
    }

    /// A message entry projected from this event (`evt_` → `msg_`).
    fn add_note(&mut self, event: &protocol::Event, created: u64, body: Option<String>) -> bool {
        let Some(id) = event.projected_message_id() else {
            return false;
        };
        if self.contains(&id) {
            return false;
        }
        let Some(message) = note_message(&id, created as protocol::Millis, body) else {
            return false;
        };
        self.insert_message(message);
        true
    }

    fn replace_note(&mut self, index: usize, message: Option<ChatMessage>) -> bool {
        match message {
            Some(message) => self.messages[index] = message,
            None => {
                self.messages.remove(index);
            }
        }
        true
    }

    /// The latest compaction entry, when it is still running.
    fn running_compaction(&self) -> Option<(usize, protocol::CompactionMessage)> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| match &message.note {
                Some(NoteSource::Compaction(compaction)) => Some((index, compaction.clone())),
                _ => None,
            })
            .filter(|(_, compaction)| compaction.status == protocol::CompactionStatus::Running)
    }
}

fn tool_key(id: &str) -> String {
    format!("tool:{id}")
}

/// Completed and failed tools are final; a replayed earlier event must not
/// reopen them.
fn tool_settled(tool: &protocol::ToolCall) -> bool {
    matches!(
        tool.state,
        protocol::ToolState::Completed { .. } | protocol::ToolState::Error { .. }
    )
}

/// The input a terminal tool state keeps, or `None` once the tool settled.
/// A tool whose `tool.called` was missed still settles, with its streamed
/// input parsed.
fn tool_input(tool: &protocol::ToolCall) -> Option<protocol::JsonMap> {
    match &tool.state {
        protocol::ToolState::Running { input, .. } => Some(input.clone()),
        protocol::ToolState::Streaming { input } => {
            Some(serde_json::from_str(input).unwrap_or_default())
        }
        protocol::ToolState::Unknown => Some(protocol::JsonMap::new()),
        protocol::ToolState::Completed { .. } | protocol::ToolState::Error { .. } => None,
    }
}

fn compaction_entry(
    id: String,
    created: protocol::Millis,
    status: protocol::CompactionStatus,
) -> protocol::CompactionMessage {
    protocol::CompactionMessage {
        id,
        time: protocol::CreatedTime { created },
        status,
        reason: None,
        summary: String::new(),
        recent: String::new(),
        error: None,
        model: None,
        cost: None,
        tokens: None,
    }
}

/// Projects one v2 message-list entry into the transcript model, or `None`
/// when the entry renders nothing (`idle`, unknown entries without text).
/// Every entry other than `user` renders as an AGENT message styled like
/// assistant text (R3.4).
///
/// Message and segment keys are shared with live events (R3.5), so a refresh
/// or a replay of events onto fresh history never duplicates rows:
/// - message: the entry `id`. Event-projected entries (switches, synthetic,
///   system, skill, shell, idle) carry the event ID with `evt_` replaced by
///   `msg_` ([`protocol::Event::projected_message_id`]); compaction uses its
///   `inputID` when present; user entries and synthetic entries delivered
///   through the inbox use the inbox `id` (the prompt `id` for prompts);
///   assistant entries use the `assistantMessageID` of their `session.step.*`
///   events.
/// - assistant text and reasoning: `text:{k}` / `reasoning:{k}`, where `k` is
///   the item's index among same-kind items of `content[]`. That is the live
///   `ordinal` of `session.{text,reasoning}.{started,delta,ended}`.
/// - assistant tool: `tool:{id}`, the provider call ID that `session.tool.*`
///   events carry as `data.id`; it is unique only within one message.
/// - user text `text:0`, user files `file:{index}`; the other entries have a
///   single `text:0` segment.
pub fn message_from_entry(entry: &protocol::SessionMessage) -> Option<ChatMessage> {
    use protocol::SessionMessage as Entry;
    match entry {
        Entry::User(message) => Some(user_chat_message(
            &message.id,
            millis(message.time.created),
            &message.text,
            &message.files,
        )),
        Entry::Assistant(message) => Some(assistant_message(message)),
        Entry::Synthetic(note) => note_message(
            &note.id,
            note.time.created,
            synthetic_text(&note.text, note.description.as_deref()),
        ),
        Entry::System(note) => note_message(
            &note.id,
            note.time.created,
            system_text(note.description.as_deref(), &note.text),
        ),
        Entry::Skill(skill) => note_message(
            &skill.id,
            skill.time.created,
            skill_text(&skill.name, &skill.skill),
        ),
        Entry::Shell(shell) => shell_message(shell),
        Entry::Compaction(compaction) => compaction_message(compaction),
        Entry::AgentSwitched(switch) => note_message(
            &switch.id,
            switch.time.created,
            agent_switch_text(&switch.agent),
        ),
        Entry::ModelSwitched(switch) => note_message(
            &switch.id,
            switch.time.created,
            Some(model_switch_text(&switch.model)),
        ),
        Entry::LocationSwitched(switch) => note_message(
            &switch.id,
            switch.time.created,
            location_switch_text(&switch.location.directory),
        ),
        Entry::Idle(_) => None,
        Entry::Unknown(unknown) => {
            let id = unknown.id.as_deref()?;
            let created = unknown
                .raw
                .pointer("/time/created")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let text = unknown
                .raw
                .get("text")
                .and_then(Value::as_str)
                .and_then(non_blank);
            note_message(id, created, text)
        }
    }
}

fn synthetic_text(text: &str, description: Option<&str>) -> Option<String> {
    non_blank(text)
        .or_else(|| description.and_then(non_blank))
        .map(str::to_owned)
}

/// System text is the full instruction set; its description names what changed.
fn system_text(description: Option<&str>, text: &str) -> Option<String> {
    description
        .and_then(non_blank)
        .or_else(|| non_blank(text))
        .map(str::to_owned)
}

/// Skill text is the whole skill body, so only the name is shown.
fn skill_text(name: &str, skill: &str) -> Option<String> {
    non_blank(name)
        .or_else(|| non_blank(skill))
        .map(|name| format!("Skill: {name}"))
}

fn agent_switch_text(agent: &str) -> Option<String> {
    non_blank(agent).map(|agent| format!("Switched agent to {agent}"))
}

fn model_switch_text(model: &protocol::ModelRef) -> String {
    let variant = model
        .variant
        .as_deref()
        .and_then(non_blank)
        .map(|variant| format!(" ({variant})"))
        .unwrap_or_default();
    format!(
        "Switched model to {}/{}{variant}",
        model.provider_id, model.id
    )
}

fn location_switch_text(directory: &str) -> Option<String> {
    non_blank(directory).map(|directory| format!("Moved to {directory}"))
}

/// A user entry, or a queued user inbox item (same payload shape).
fn user_chat_message(
    id: &str,
    created: u64,
    text: &str,
    files: &[protocol::StoredFile],
) -> ChatMessage {
    let mut segments = Vec::new();
    if !text.is_empty() {
        segments.push(Segment::text(
            "text:0".to_owned(),
            SegmentKind::Text,
            text.to_owned(),
            0,
        ));
    }
    for (index, file) in files.iter().enumerate() {
        let name = file
            .name
            .as_deref()
            .and_then(non_blank)
            .unwrap_or("attachment");
        let mime = non_blank(&file.mime).unwrap_or("application/octet-stream");
        let mut segment = Segment::text(
            format!("file:{index}"),
            SegmentKind::File,
            format!("Attached: {name} ({mime})"),
            0,
        );
        segment.image_url =
            (mime.starts_with("image/") && !file.data.is_empty()).then(|| file.data_url());
        segments.push(segment);
    }
    ChatMessage {
        segments,
        created,
        ..ChatMessage::placeholder(id, Role::User)
    }
}

fn assistant_message(message: &protocol::AssistantMessage) -> ChatMessage {
    ChatMessage {
        created: millis(message.time.created),
        segments: assistant_segments(&message.content),
        error: message
            .error
            .as_ref()
            .filter(|error| !is_interrupt(error))
            .map(structured_error_text),
        context_tokens: message.tokens.as_ref().and_then(usage_tokens),
        ..ChatMessage::placeholder(message.id.clone(), Role::Assistant)
    }
}

/// `assistant.content[]`, from history or `session.message.content.updated`.
fn assistant_segments(content: &[protocol::AssistantContent]) -> Vec<Segment> {
    use protocol::AssistantContent as Content;
    let mut segments = Vec::new();
    let mut texts = 0;
    let mut reasonings = 0;
    for item in content {
        match item {
            Content::Text(text) => {
                segments.push(Segment::text(
                    format!("text:{texts}"),
                    SegmentKind::Text,
                    text.text.clone(),
                    0,
                ));
                texts += 1;
            }
            Content::Reasoning(reasoning) => {
                segments.push(Segment::text(
                    format!("reasoning:{reasonings}"),
                    SegmentKind::Reasoning,
                    reasoning.text.clone(),
                    reasoning.time.map(|time| millis(time.created)).unwrap_or(0),
                ));
                reasonings += 1;
            }
            Content::Tool(tool) => segments.push(tool_segment(tool)),
            // Keep ordinals aligned when a known kind fails to decode.
            Content::Unknown(unknown) => match unknown.kind.as_str() {
                "text" => texts += 1,
                "reasoning" => reasonings += 1,
                _ => {}
            },
        }
    }
    segments
}

/// An interrupted step fails with `type: "aborted"`, which is not an error
/// worth showing.
fn is_interrupt(error: &protocol::StructuredError) -> bool {
    error.kind == "aborted"
}

fn structured_error_text(error: &protocol::StructuredError) -> String {
    non_blank(&error.message)
        .or_else(|| non_blank(&error.kind))
        .unwrap_or("Request failed")
        .to_owned()
}

fn usage_tokens(tokens: &protocol::TokenUsage) -> Option<u64> {
    let total = tokens.total();
    (total.is_finite() && total >= 1.0).then(|| total.round() as u64)
}

/// `"{name} · {status}{ — title}{: error}"`, the tool row body.
fn tool_segment(tool: &protocol::ToolCall) -> Segment {
    use protocol::ToolState;
    let empty = protocol::JsonMap::new();
    let streamed: protocol::JsonMap;
    let (status, input, error) = match &tool.state {
        ToolState::Streaming { input } => {
            streamed = serde_json::from_str(input).unwrap_or_default();
            ("running", &streamed, None)
        }
        ToolState::Running { input, .. } => ("running", input, None),
        ToolState::Completed { input, .. } => ("completed", input, None),
        ToolState::Error { input, error, .. } => {
            ("error", input, Some(structured_error_text(error)))
        }
        ToolState::Unknown => ("pending", &empty, None),
    };
    let name = non_blank(&tool.name).unwrap_or("tool");
    let title = tool_title(name, input)
        .map(|title| format!(" — {title}"))
        .unwrap_or_default();
    let detail = error.map(|error| format!(": {error}")).unwrap_or_default();
    Segment {
        key: format!("tool:{}", tool.id),
        kind: SegmentKind::Tool,
        text: format!("{name} · {status}{title}{detail}"),
        image_url: None,
        created: millis(tool.time.created),
        tool: Some(tool.clone()),
    }
}

/// A one-line summary of a tool call from its input, per v2 tool schema
/// (`core/src/tool/plugin/*.ts`); other tools (MCP, plugins) try common keys.
fn tool_title(name: &str, input: &protocol::JsonMap) -> Option<String> {
    let keys: &[&str] = match name {
        "shell" => &["command", "description"],
        "subagent" => &["description", "agent"],
        "read" | "write" | "edit" => &["path"],
        "glob" | "grep" => &["pattern"],
        "webfetch" => &["url"],
        "websearch" => &["query"],
        "skill" => &["id"],
        _ => &[
            "command",
            "description",
            "path",
            "filePath",
            "pattern",
            "query",
            "url",
        ],
    };
    keys.iter().find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .and_then(single_line)
    })
}

fn single_line(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(line)
}

fn non_blank(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

/// A non-user entry shown as one AGENT text segment.
fn note_message(
    id: &str,
    created: protocol::Millis,
    body: Option<impl Into<String>>,
) -> Option<ChatMessage> {
    let text = body?.into();
    Some(ChatMessage {
        created: millis(created),
        segments: vec![Segment::text(
            "text:0".to_owned(),
            SegmentKind::Text,
            text,
            0,
        )],
        ..ChatMessage::placeholder(id, Role::Assistant)
    })
}

/// A shell entry; `session.shell.ended` updates it through its source.
fn shell_message(shell: &protocol::ShellMessage) -> Option<ChatMessage> {
    let mut message = note_message(&shell.id, shell.time.created, shell_body(shell))?;
    message.note = Some(NoteSource::Shell(shell.clone()));
    Some(message)
}

/// A compaction entry; its ended/failed events update it through its source.
fn compaction_message(compaction: &protocol::CompactionMessage) -> Option<ChatMessage> {
    let mut message = note_message(
        &compaction.id,
        compaction.time.created,
        compaction_body(compaction),
    )?;
    message.note = Some(NoteSource::Compaction(compaction.clone()));
    Some(message)
}

/// `$ command` and its output as a code block, plus how it ended if abnormal.
fn shell_body(shell: &protocol::ShellMessage) -> Option<String> {
    let command = shell.command.trim();
    let output = shell
        .output
        .as_ref()
        .map(|output| output.output.trim_end())
        .filter(|output| !output.is_empty());
    if command.is_empty() && output.is_none() {
        return None;
    }
    let mut text = format!("$ {command}");
    if let Some(output) = output {
        text.push('\n');
        text.push_str(output);
    }
    let mut body = code_block(&text);
    match shell.status {
        protocol::ShellStatus::Timeout => body.push_str("\n\nTimed out"),
        protocol::ShellStatus::Killed => body.push_str("\n\nKilled"),
        _ => {}
    }
    Some(body)
}

/// A fenced block whose fence is longer than any backtick run inside.
fn code_block(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{text}\n{fence}")
}

fn compaction_body(compaction: &protocol::CompactionMessage) -> Option<String> {
    use protocol::CompactionStatus;
    let summary = non_blank(&compaction.summary).map(str::to_owned);
    match compaction.status {
        CompactionStatus::Running => Some("Compacting the conversation…".to_owned()),
        CompactionStatus::Completed => {
            summary.or_else(|| Some("Compacted the conversation.".to_owned()))
        }
        CompactionStatus::Failed => Some(
            match compaction
                .error
                .as_ref()
                .and_then(|error| non_blank(&error.message).or_else(|| non_blank(&error.kind)))
            {
                Some(error) => format!("Compaction failed: {error}"),
                None => "Compaction failed.".to_owned(),
            },
        ),
        CompactionStatus::Unknown => summary,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Idle,
    Busy,
    Retry { message: String, attempt: u32 },
}

impl RunStatus {
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Busy | Self::Retry { .. })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The run status an event implies. 2.0.8 never publishes `session.status`
/// or `session.idle`; `session.execution.*` brackets each run and a step
/// start after a scheduled retry means the run is going again. A `shutdown`
/// interruption keeps the run claimed (the restarted server resumes it), so
/// it changes nothing; the reconnect bootstrap resyncs the status.
pub fn run_status_change(kind: &protocol::EventKind) -> Option<(String, RunStatus)> {
    use protocol::EventKind as Kind;
    let (session_id, status) = match kind {
        Kind::ExecutionStarted(data) => (&data.session_id, RunStatus::Busy),
        Kind::StepStarted(data) => (&data.session_id, RunStatus::Busy),
        Kind::RetryScheduled(data) => (
            &data.session_id,
            RunStatus::Retry {
                message: retry_message(&data.error, data.attempt),
                attempt: data.attempt,
            },
        ),
        Kind::ExecutionSucceeded(data) => (&data.session_id, RunStatus::Idle),
        Kind::ExecutionFailed(data) => (&data.session_id, RunStatus::Idle),
        Kind::ExecutionInterrupted(data) if data.reason != "shutdown" => {
            (&data.session_id, RunStatus::Idle)
        }
        _ => return None,
    };
    Some((session_id.clone(), status))
}

/// [`run_status_change`] for a whole `/api/event` event.
pub fn event_run_status(payload: &Value) -> Option<(String, RunStatus)> {
    let event = protocol::Event::deserialize(payload).ok()?;
    run_status_change(&protocol::decode_event(&event))
}

fn retry_message(error: &protocol::StructuredError, attempt: u32) -> String {
    let retrying = format!("Retrying (attempt {attempt})…");
    match non_blank(&error.message).or_else(|| non_blank(&error.kind)) {
        Some(reason) => format!("{}. {retrying}", reason.trim().trim_end_matches('.')),
        None => retrying,
    }
}

/// `/debug …` composer commands, which exercise the transcript and status
/// UI by feeding synthetic v2 events through the live reducers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugCommand {
    Error,
    Retry,
    Idle,
}

impl DebugCommand {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/debug error" => Some(Self::Error),
            "/debug retry" => Some(Self::Retry),
            "/debug clear" | "/debug idle" => Some(Self::Idle),
            _ => None,
        }
    }

    /// Whole `/api/event` events, in order. Apply each to the conversation
    /// and its [`event_run_status`] to the session.
    pub fn events(self, session_id: &str, now: u64) -> Vec<Value> {
        let event = |kind: &str, data: Value| {
            let id = protocol::new_message_id().replacen("msg_", "evt_", 1);
            json!({ "id": id, "created": now, "type": kind, "data": data })
        };
        let message_id = protocol::new_message_id();
        match self {
            Self::Error => {
                let error = json!({
                    "type": "provider.api",
                    "message": "AI_APICallError: Not Found (404)",
                    "status": 404
                });
                vec![
                    event(
                        "session.step.started",
                        json!({ "sessionID": session_id, "assistantMessageID": message_id,
                                "agent": "build", "started": now }),
                    ),
                    event(
                        "session.step.failed",
                        json!({ "sessionID": session_id, "assistantMessageID": message_id,
                                "error": error }),
                    ),
                    event(
                        "session.execution.failed",
                        json!({ "sessionID": session_id, "error": error }),
                    ),
                ]
            }
            Self::Retry => vec![event(
                "session.retry.scheduled",
                json!({
                    "sessionID": session_id,
                    "assistantMessageID": message_id,
                    "attempt": 1,
                    "at": now + 4_000,
                    "error": { "type": "provider.rate_limit", "message": "Rate limited upstream", "status": 429 }
                }),
            )],
            Self::Idle => vec![event(
                "session.execution.succeeded",
                json!({ "sessionID": session_id }),
            )],
        }
    }
}

const UNTITLED_SESSION: &str = "Untitled session";

fn millis(value: protocol::Millis) -> u64 {
    value.max(0) as u64
}

fn display_title(title: Option<&str>) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(UNTITLED_SESSION)
        .to_owned()
}

impl Session {
    /// Never copies `agent`: the client always uses the server's default agent.
    pub fn from_info(info: &protocol::SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            directory: info.directory().to_owned(),
            title: display_title(info.title.as_deref()),
            time: SessionTime {
                created: millis(info.time.created),
                updated: millis(info.time.updated),
                archived: info.time.archived.map(|archived| archived as f64),
            },
            parent_id: info.parent_id.clone(),
            model: info.model.as_ref().map(SessionModel::from_ref),
        }
    }

    /// `session.created` carries creation fields only; its time is the event's `created`.
    fn from_created(data: &protocol::SessionCreated, created: u64) -> Self {
        Self {
            id: data.session_id.clone(),
            directory: data.location.directory.clone(),
            title: display_title(data.title.as_deref()),
            time: SessionTime {
                created,
                updated: created,
                archived: None,
            },
            parent_id: data.parent_id.clone(),
            model: data.model.as_ref().map(SessionModel::from_ref),
        }
    }
}

impl SessionModel {
    fn from_ref(model: &protocol::ModelRef) -> Self {
        Self {
            id: model.id.clone(),
            provider_id: model.provider_id.clone(),
            variant: model.variant.clone(),
        }
    }

    pub fn from_selection(selection: &ModelSelection) -> Self {
        Self {
            id: selection.model_id.clone(),
            provider_id: selection.provider_id.clone(),
            variant: selection.variant.clone(),
        }
    }
}

impl Project {
    pub fn from_info(info: &protocol::ProjectInfo) -> Self {
        Self {
            worktree: info.canonical.clone(),
            name: info
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
        }
    }
}

/// A change to the root-session list carried by a v2 `session.*` event.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionChange {
    Created(Session),
    Renamed {
        id: String,
        title: String,
    },
    Moved {
        id: String,
        directory: String,
    },
    /// `session.model.selected`, from this client or another one.
    ModelSelected {
        id: String,
        model: SessionModel,
    },
    Deleted(String),
}

impl SessionChange {
    /// `payload` is the whole `/api/event` event (`{id, created, type, location?, data}`).
    #[cfg(test)]
    pub fn from_event(payload: &Value) -> Option<Self> {
        let event = protocol::Event::deserialize(payload).ok()?;
        Self::from_kind(&event, &protocol::decode_event(&event))
    }

    pub fn from_kind(event: &protocol::Event, kind: &protocol::EventKind) -> Option<Self> {
        match kind {
            protocol::EventKind::SessionCreated(data) => Some(Self::Created(
                Session::from_created(data, event.created.map(millis).unwrap_or(0)),
            )),
            protocol::EventKind::SessionRenamed(data) => Some(Self::Renamed {
                id: data.session_id.clone(),
                title: display_title(Some(&data.title)),
            }),
            protocol::EventKind::SessionMoved(data) if !data.location.directory.is_empty() => {
                Some(Self::Moved {
                    id: data.session_id.clone(),
                    directory: data.location.directory.clone(),
                })
            }
            protocol::EventKind::SessionModelSelected(data) => Some(Self::ModelSelected {
                model: SessionModel::from_ref(&data.model),
                id: data.session_id.clone(),
            }),
            protocol::EventKind::SessionDeleted(data) => {
                Some(Self::Deleted(data.session_id.clone()))
            }
            _ => None,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Created(session) => &session.id,
            Self::Renamed { id, .. }
            | Self::Moved { id, .. }
            | Self::ModelSelected { id, .. }
            | Self::Deleted(id) => id,
        }
    }

    /// Applies the change to a root-session list and reports whether it changed.
    /// A creation never replaces a known session, whose data is at least as new;
    /// child sessions are ignored; renames and moves of unknown sessions are no-ops.
    pub fn apply(&self, sessions: &mut Vec<Session>) -> bool {
        match self {
            Self::Created(session) => {
                if session.parent_id.is_some()
                    || sessions.iter().any(|existing| existing.id == session.id)
                {
                    return false;
                }
                sessions.push(session.clone());
                true
            }
            Self::Renamed { id, title } => sessions
                .iter_mut()
                .find(|session| &session.id == id && &session.title != title)
                .map(|session| session.title.clone_from(title))
                .is_some(),
            Self::Moved { id, directory } => sessions
                .iter_mut()
                .find(|session| &session.id == id && &session.directory != directory)
                .map(|session| session.directory.clone_from(directory))
                .is_some(),
            Self::ModelSelected { id, model } => sessions
                .iter_mut()
                .find(|session| &session.id == id && session.model.as_ref() != Some(model))
                .map(|session| session.model = Some(model.clone()))
                .is_some(),
            Self::Deleted(id) => {
                let before = sessions.len();
                sessions.retain(|session| &session.id != id);
                sessions.len() != before
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture_body(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/v2-2.0.8/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str::<Value>(&text).unwrap()["response"]["body"].take()
    }

    fn selection(provider: &str, model: &str, variant: Option<&str>) -> ModelSelection {
        ModelSelection {
            provider_id: provider.into(),
            model_id: model.into(),
            variant: variant.map(str::to_owned),
        }
    }

    #[test]
    fn catalog_builds_from_the_captured_model_routes() {
        let list: protocol::ModelListResponse =
            serde_json::from_value(fixture_body("model.list")).unwrap();
        let default: protocol::ModelDefaultResponse =
            serde_json::from_value(fixture_body("model.default")).unwrap();
        let catalog = ModelCatalog::from_models(&list.data, default.data.as_ref());

        assert_eq!(
            catalog.models,
            [
                ModelOption {
                    provider_id: "mock".into(),
                    model_id: "mock-model".into(),
                    label: "mock / Mock Model".into(),
                    variants: vec!["low".into(), "medium".into(), "high".into()],
                    supports_attachments: true,
                    context_limit: Some(128_000),
                },
                ModelOption {
                    provider_id: "mock".into(),
                    model_id: "mock-model-alt".into(),
                    label: "mock / Mock Model Alt".into(),
                    variants: vec!["low".into(), "high".into()],
                    supports_attachments: false,
                    context_limit: Some(32_000),
                },
            ]
        );
        assert_eq!(
            catalog.preferred,
            Some(selection("mock", "mock-model", None))
        );
        let debug = format!("{catalog:?}");
        assert!(
            fixture_body("model.list")
                .to_string()
                .contains("dummy-not-a-secret")
        );
        assert!(!debug.contains("dummy-not-a-secret"), "{debug}");
        assert!(!debug.contains("baseURL") && !debug.contains("reasoning"));
    }

    #[test]
    fn catalog_uses_catalog_ids_and_skips_disabled_and_deprecated_models() {
        let models: Vec<protocol::ModelInfo> = serde_json::from_value(json!([
            {
                "id": "gpt-6-fast", "modelID": "gpt-6", "providerID": "openai", "name": "GPT 6 Fast",
                "settings": { "apiKey": "sk-hidden" },
                "headers": { "authorization": "Bearer sk-hidden" },
                "capabilities": { "tools": true, "input": ["text", "pdf"], "output": ["text"] },
                "variants": [
                    { "id": "xhigh", "settings": {} }, { "id": "Minimal" }, { "id": "custom" },
                    { "id": "medium", "body": { "k": "sk-hidden" } }
                ],
                "limit": { "context": 400000, "output": 1 }, "status": "active", "enabled": true
            },
            { "id": "old", "modelID": "old", "providerID": "openai", "name": "Old", "status": "deprecated" },
            { "id": "off", "modelID": "off", "providerID": "openai", "name": "Off", "enabled": false },
            {
                "id": "text-only", "modelID": "t", "providerID": "anthropic", "name": "",
                "capabilities": { "input": ["text"] }, "limit": { "context": 0 }, "status": "beta"
            },
            { "id": "defaults", "modelID": "d", "providerID": "zed", "name": "Defaults" }
        ]))
        .unwrap();
        let catalog = ModelCatalog::from_models(&models, Some(&models[0]));

        let ids: Vec<_> = catalog.models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(
            ids,
            ["text-only", "gpt-6-fast", "defaults"],
            "sorted by label"
        );
        let text_only = &catalog.models[0];
        assert_eq!(text_only.label, "anthropic / text-only");
        assert!(!text_only.supports_attachments);
        assert_eq!(text_only.context_limit, None);
        let fast = &catalog.models[1];
        assert_eq!(fast.variants, ["Minimal", "medium", "xhigh", "custom"]);
        assert!(fast.supports_attachments, "pdf input accepts attachments");
        assert_eq!(fast.context_limit, Some(400_000));
        assert!(
            catalog.models[2].supports_attachments,
            "missing capabilities default to text and image"
        );
        assert_eq!(
            catalog.preferred,
            Some(selection("openai", "gpt-6-fast", None)),
            "the default uses ModelInfo.id, not modelID"
        );
        assert!(!format!("{catalog:?}").contains("sk-hidden"));

        // A disabled or missing default falls back to the first model.
        let fallback = ModelCatalog::from_models(&models, Some(&models[2]));
        assert_eq!(
            fallback.preferred,
            Some(selection("anthropic", "text-only", None))
        );
        assert_eq!(
            ModelCatalog::from_models(&models, None).preferred,
            fallback.preferred
        );
        assert_eq!(
            ModelCatalog::from_models(&[], None),
            ModelCatalog::default()
        );
    }

    #[test]
    fn displayed_model_prefers_a_pending_pick_then_the_session_model() {
        let catalog = ModelCatalog {
            models: Vec::new(),
            preferred: Some(selection("p", "default", None)),
        };
        let mut session = listed("ses_a", "A");
        assert_eq!(
            displayed_model(None, Some(&session), &catalog),
            Some(selection("p", "default", None)),
            "a session without a model shows the server default"
        );
        session.model = Some(SessionModel {
            id: "saved".into(),
            provider_id: "p".into(),
            variant: Some("high".into()),
        });
        assert_eq!(
            displayed_model(None, Some(&session), &catalog),
            Some(selection("p", "saved", Some("high")))
        );
        let pending = selection("q", "picked", None);
        assert_eq!(
            displayed_model(Some(&pending), Some(&session), &catalog),
            Some(pending.clone())
        );
        assert_eq!(displayed_model(None, None, &ModelCatalog::default()), None);
    }

    #[test]
    fn only_a_pick_that_changes_the_display_requests_a_switch() {
        let shown_default = selection("p", "default", None);
        assert_eq!(
            model_switch_for_pick(Some(&shown_default), shown_default.clone()),
            None,
            "re-picking the displayed default keeps following the server default"
        );
        assert_eq!(
            model_switch_for_pick(
                Some(&shown_default),
                selection("p", "default", Some("high"))
            ),
            Some(selection("p", "default", Some("high")))
        );
        assert_eq!(
            model_switch_for_pick(Some(&shown_default), selection("p", "other", None)),
            Some(selection("p", "other", None))
        );
        assert_eq!(
            model_switch_for_pick(None, shown_default.clone()),
            Some(shown_default)
        );
        assert_eq!(
            selection("p", "m", Some("low")).to_ref(),
            protocol::ModelRef {
                id: "m".into(),
                provider_id: "p".into(),
                variant: Some("low".into()),
            }
        );
    }

    #[test]
    fn catalog_invalidation_events_carry_their_location() {
        let event = |event_type: &str, location: Option<&str>| {
            let mut event = json!({ "id": "evt_1", "created": 1, "type": event_type, "data": {} });
            if let Some(directory) = location {
                event["location"] = json!({ "directory": directory });
            }
            event
        };
        for event_type in [
            "model.updated",
            "provider.updated",
            "integration.updated",
            "credential.updated",
            "credential.switched",
            "config.updated",
            "models-dev.refreshed",
        ] {
            assert_eq!(
                CatalogInvalidation::from_event(&event(event_type, Some("/work"))),
                Some(CatalogInvalidation {
                    directory: Some("/work".into()),
                    shutdown: false,
                }),
                "{event_type}"
            );
        }
        assert_eq!(
            CatalogInvalidation::from_event(&event("model.updated", None)),
            Some(CatalogInvalidation {
                directory: None,
                shutdown: false,
            })
        );
        assert_eq!(
            CatalogInvalidation::from_event(&event("location.shutdown", Some("/work"))),
            Some(CatalogInvalidation {
                directory: Some("/work".into()),
                shutdown: true,
            })
        );
        for ignored in [
            "agent.updated",
            "session.renamed",
            "server.instance.disposed",
        ] {
            assert_eq!(CatalogInvalidation::from_event(&event(ignored, None)), None);
        }
    }

    /// A captured `GET /api/session/{id}/message` page, reversed into
    /// chronological order the way the client does.
    fn fixture_entries(name: &str) -> Vec<protocol::SessionMessage> {
        let path = format!(
            "{}/tests/fixtures/v2-2.0.8/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let exchange: protocol::CapturedExchange = serde_json::from_str(&text).unwrap();
        let mut page: protocol::MessageListResponse = exchange.decode_body().unwrap();
        page.data.reverse();
        page.data
    }

    fn history(name: &str) -> Conversation {
        let mut conversation = Conversation::default();
        conversation.replace_from_api(&fixture_entries(name), None);
        conversation
    }

    fn row_values(conversation: &Conversation) -> Vec<Value> {
        conversation
            .transcript_rows()
            .iter()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect()
    }

    fn entries(values: Vec<Value>) -> Vec<protocol::SessionMessage> {
        values
            .into_iter()
            .map(protocol::SessionMessage::from_value)
            .collect()
    }

    #[test]
    fn transcript_rows_include_image_file_parts() {
        let conversation = history("session.messages.attachment");
        let rows = row_values(&conversation);
        assert_eq!(rows.len(), 2, "idle entries render nothing: {rows:?}");
        assert_eq!(rows[0]["role"], "YOU");
        assert_eq!(
            rows[0]["body"],
            "Describe the attached image. [[scenario:text]]\n\nAttached: pixel.png (image/png)"
        );
        let image = rows[0]["images"][0].as_str().unwrap();
        assert!(
            image.starts_with("data:image/png;base64,iVBORw0KGgo"),
            "{image}"
        );
        assert_eq!(rows[0]["images"].as_array().unwrap().len(), 1);
        assert_eq!(rows[1]["role"], "AGENT");
        assert_eq!(rows[1]["body"], "Hello from the mock provider.");

        let other = entries(vec![json!({
            "id": "msg_u", "type": "user", "time": { "created": 7 }, "text": "",
            "files": [
                { "data": "JVBERi0=", "mime": "application/pdf", "source": { "type": "inline" }, "name": "spec.pdf" },
                { "data": "AA==", "mime": "image/gif", "source": { "type": "inline" } }
            ]
        })]);
        let mut conversation = Conversation::default();
        conversation.replace_from_api(&other, None);
        let row = &row_values(&conversation)[0];
        assert_eq!(
            row["body"],
            "Attached: spec.pdf (application/pdf)\n\nAttached: attachment (image/gif)"
        );
        assert_eq!(row["images"], json!(["data:image/gif;base64,AA=="]));
        assert_eq!(row["time"], 7);
    }

    #[test]
    fn transcript_rows_split_tool_calls_with_their_own_times() {
        let entries = fixture_entries("session.messages.tools");
        let tool_times: Vec<i64> = entries
            .iter()
            .filter_map(|entry| match entry {
                protocol::SessionMessage::Assistant(message) => Some(message),
                _ => None,
            })
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                protocol::AssistantContent::Tool(tool) => Some(tool.time.created),
                _ => None,
            })
            .collect();
        let mut conversation = Conversation::default();
        conversation.replace_from_api(&entries, None);
        let rows = row_values(&conversation);
        let bodies: Vec<_> = rows
            .iter()
            .map(|row| row["body"].as_str().unwrap())
            .collect();
        assert_eq!(
            bodies,
            [
                "Read two things at once. [[scenario:tools]]",
                "Running two tools at once.",
                "read · completed — README.md",
                "glob · completed — **/*.txt",
                "Both tool calls finished.",
            ]
        );
        assert_eq!(rows[0]["role"], "YOU");
        assert!(rows[1..].iter().all(|row| row["role"] == "AGENT"));
        assert_eq!(rows[2]["kind"], "tool");
        assert_eq!(rows[3]["kind"], "tool");
        assert_eq!(rows[2]["time"], tool_times[0]);
        assert_eq!(rows[3]["time"], tool_times[1]);
        assert_ne!(
            rows[1]["time"], rows[2]["time"],
            "text uses the message time"
        );

        let keys: Vec<_> = conversation.messages[1]
            .segments
            .iter()
            .map(|segment| segment.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["text:0", "tool:call_mock_read", "tool:call_mock_glob"]
        );
    }

    #[test]
    fn transcript_rows_split_reasoning_from_replies() {
        let conversation = history("session.messages.reasoning");
        let rows = row_values(&conversation);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1]["body"],
            "Reasoning\nLet me think about this carefully."
        );
        assert_eq!(rows[1]["kind"], "reasoning");
        assert_eq!(
            rows[1]["time"], 1_790_289_087_309_u64,
            "reasoning time.created"
        );
        assert_eq!(rows[2]["body"], "After thinking, the answer is 42.");
        assert_eq!(rows[2]["kind"], "");
        let keys: Vec<_> = conversation.messages[1]
            .segments
            .iter()
            .map(|segment| segment.key.as_str())
            .collect();
        assert_eq!(keys, ["reasoning:0", "text:0"]);
    }

    #[test]
    fn segment_keys_follow_per_kind_ordinals_and_tool_ids() {
        let conversation = {
            let mut conversation = Conversation::default();
            conversation.replace_from_api(
                &entries(vec![json!({
                    "id": "msg_a", "type": "assistant", "time": { "created": 1 }, "agent": "build",
                    "content": [
                        { "type": "reasoning", "text": "r0" },
                        { "type": "text", "text": "t0" },
                        { "type": "tool", "id": "call_1", "name": "shell", "time": { "created": 2 },
                          "state": { "status": "streaming", "input": "{\"command\":\"ls -la\"}" } },
                        { "type": "hologram" },
                        { "type": "reasoning", "text": "r1" },
                        { "type": "text", "text": "t1" },
                        { "type": "tool", "id": "call_2", "name": "subagent", "time": { "created": 3 },
                          "state": { "status": "error", "input": { "description": "Dig in", "agent": "general" },
                                     "error": { "type": "tool", "message": "child failed" } } }
                    ]
                })]),
                None,
            );
            conversation
        };
        let segments: Vec<_> = conversation.messages[0]
            .segments
            .iter()
            .map(|segment| (segment.key.as_str(), segment.text.as_str()))
            .collect();
        assert_eq!(
            segments,
            [
                ("reasoning:0", "r0"),
                ("text:0", "t0"),
                ("tool:call_1", "shell · running — ls -la"),
                ("reasoning:1", "r1"),
                ("text:1", "t1"),
                ("tool:call_2", "subagent · error — Dig in: child failed"),
            ]
        );

        // A live delta keyed by ordinal lands on the history segment.
        let mut conversation = conversation;
        assert!(conversation.apply_event(&json!({
            "id": "evt_1", "created": 9, "type": "session.text.delta",
            "data": { "sessionID": "ses_1", "assistantMessageID": "msg_a", "ordinal": 1, "delta": "!" }
        })));
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].segments[4].text, "t1!");
    }

    #[test]
    fn captured_scenarios_project_into_sensible_rows() {
        let expect = |name: &str, expected: &[(&str, &str, &str)]| {
            let rows = row_values(&history(name));
            let actual: Vec<_> = rows
                .iter()
                .map(|row| {
                    (
                        row["role"].as_str().unwrap().to_owned(),
                        row["kind"].as_str().unwrap().to_owned(),
                        row["body"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            let expected: Vec<_> = expected
                .iter()
                .map(|(role, kind, body)| {
                    ((*role).to_owned(), (*kind).to_owned(), (*body).to_owned())
                })
                .collect();
            assert_eq!(actual, expected, "{name}");
        };
        expect(
            "session.messages.text",
            &[
                ("YOU", "", "Say hello. [[scenario:text]]"),
                ("AGENT", "", "Hello from the mock provider."),
            ],
        );
        expect(
            "session.messages.permission",
            &[
                ("YOU", "", "Run a shell command. [[scenario:permission]]"),
                ("AGENT", "tool", "shell · completed — echo permission-probe"),
                ("AGENT", "", "The shell command completed."),
            ],
        );
        expect(
            "session.messages.subagent-permission",
            &[
                (
                    "YOU",
                    "",
                    "Delegate a shell command. [[scenario:subagent-permission]]",
                ),
                (
                    "AGENT",
                    "tool",
                    "subagent · completed — Mock child shell task",
                ),
                ("AGENT", "", "The child subagent finished."),
            ],
        );
        expect(
            "session.messages.error",
            &[
                ("YOU", "", "Fail please. [[scenario:error]]"),
                ("AGENT", "error", "Mock provider exploded"),
            ],
        );
        expect(
            "session.messages.retry",
            &[
                ("YOU", "", "Fail once then recover. [[scenario:retry]]"),
                ("AGENT", "", "Recovered after a retry."),
            ],
        );
        // The interrupted step fails with `aborted`; only its partial text shows.
        expect(
            "session.messages.interrupt",
            &[
                ("YOU", "", "Stream slowly. [[scenario:slow]]"),
                ("AGENT", "", "slow-0 slow-1 slow-2 "),
            ],
        );

        // A background subagent completes later as a synthetic entry, which
        // renders like assistant text.
        let conversation = history("session.messages.subagent");
        let rows = row_values(&conversation);
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[0]["role"], "YOU");
        assert_eq!(rows[1]["body"], "subagent · completed — Mock child task");
        assert_eq!(rows[1]["kind"], "tool");
        assert_eq!(rows[2]["body"], "Launched a background subagent.");
        assert!(
            rows[3]["body"]
                .as_str()
                .unwrap()
                .contains("Hello from the child subagent.")
        );
        assert_eq!(
            (&rows[3]["role"], &rows[3]["kind"]),
            (&json!("AGENT"), &json!(""))
        );
        assert_eq!(rows[4]["body"], "Hello from the mock provider.");
        assert!(
            conversation
                .messages
                .iter()
                .all(|message| message.id.starts_with("msg_"))
        );
    }

    #[test]
    fn non_user_entries_render_as_agent_text() {
        let mut conversation = Conversation::default();
        conversation.replace_from_api(
            &entries(vec![
                json!({ "id": "msg_1", "type": "model-switched", "time": { "created": 1 },
                        "model": { "id": "gpt-6", "providerID": "openai", "variant": "high" } }),
                json!({ "id": "msg_2", "type": "agent-switched", "time": { "created": 2 }, "agent": "plan" }),
                json!({ "id": "msg_3", "type": "location-switched", "time": { "created": 3 },
                        "location": { "directory": "/work/b" } }),
                json!({ "id": "msg_4", "type": "system", "time": { "created": 4 },
                        "text": "<all instructions>", "description": "Instructions updated: AGENTS.md" }),
                json!({ "id": "msg_5", "type": "skill", "time": { "created": 5 },
                        "skill": "pdf", "name": "PDF tools", "text": "# Long skill body" }),
                json!({ "id": "msg_6", "type": "shell", "time": { "created": 6, "completed": 7 },
                        "shellID": "sh_1", "command": "ls", "status": "exited", "exit": 0,
                        "output": { "output": "a\nb\n", "cursor": 4, "size": 4, "truncated": false } }),
                json!({ "id": "msg_7", "type": "shell", "time": { "created": 8 },
                        "shellID": "sh_2", "command": "sleep 99", "status": "timeout" }),
                json!({ "id": "msg_8", "type": "compaction", "time": { "created": 9 },
                        "status": "completed", "summary": "Earlier work summary." }),
                json!({ "id": "msg_9", "type": "compaction", "time": { "created": 10 }, "status": "running" }),
                json!({ "id": "msg_10", "type": "compaction", "time": { "created": 11 }, "status": "failed",
                        "error": { "type": "provider", "message": "too long" } }),
                json!({ "id": "msg_11", "type": "synthetic", "time": { "created": 12 }, "text": "Continue." }),
                json!({ "id": "msg_12", "type": "idle", "time": { "created": 13 }, "outcome": "succeeded" }),
                json!({ "id": "msg_13", "type": "teleported", "time": { "created": 14 }, "text": "Beamed up" }),
                json!({ "id": "msg_14", "type": "teleported", "time": { "created": 15 } }),
            ]),
            None,
        );
        let rows = row_values(&conversation);
        let bodies: Vec<_> = rows
            .iter()
            .map(|row| row["body"].as_str().unwrap())
            .collect();
        assert_eq!(
            bodies,
            [
                "Switched model to openai/gpt-6 (high)",
                "Switched agent to plan",
                "Moved to /work/b",
                "Instructions updated: AGENTS.md",
                "Skill: PDF tools",
                "```\n$ ls\na\nb\n```",
                "```\n$ sleep 99\n```\n\nTimed out",
                "Earlier work summary.",
                "Compacting the conversation…",
                "Compaction failed: too long",
                "Continue.",
                "Beamed up",
            ]
        );
        assert!(
            rows.iter().all(|row| row["role"] == "AGENT"
                && row["kind"] == ""
                && row["images"] == json!([]))
        );
        assert_eq!(rows[0]["time"], 1);
        assert_eq!(rows[11]["time"], 14);
        assert_eq!(conversation.context_tokens(), None);
    }

    #[test]
    fn shell_output_fence_outgrows_backticks_inside() {
        assert_eq!(code_block("a ``` b"), "````\na ``` b\n````");
        assert_eq!(code_block("plain"), "```\nplain\n```");
    }

    #[test]
    fn paged_history_prepends_older_entries_once() {
        let pages: Vec<_> = (1..=4)
            .map(|index| fixture_entries(&format!("session.messages.page{index}")))
            .collect();
        let mut conversation = Conversation::default();
        conversation.replace_from_api(&pages[0], Some("c1".into()));
        for (index, page) in pages.iter().enumerate().skip(1) {
            conversation.prepend_from_api(page, Some(format!("c{}", index + 1)));
        }
        conversation.prepend_from_api(&pages[1], None);
        assert_eq!(conversation.next_cursor, None);
        let rows = row_values(&conversation);
        let users: Vec<_> = rows
            .iter()
            .filter(|row| row["role"] == "YOU")
            .map(|row| row["body"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(users.len(), 4, "{users:?}");
        let mut sorted = users.clone();
        sorted.sort();
        assert_eq!(users, sorted, "oldest turn first: {users:?}");
        let times: Vec<_> = rows
            .iter()
            .map(|row| row["time"].as_u64().unwrap())
            .collect();
        assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "{times:?}");
    }

    #[test]
    fn transcript_rows_emits_error_row_for_failed_message() {
        let msg = ChatMessage {
            created: 100,
            error: Some("AI_APICallError: Not Found".into()),
            ..ChatMessage::placeholder("msg_err", Role::Assistant)
        };
        let rows = msg.transcript_rows();
        assert_eq!(rows.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!(parsed["kind"], "error");
        assert_eq!(parsed["body"], "AI_APICallError: Not Found");
        assert_eq!(parsed["role"], "AGENT");
    }

    #[test]
    fn context_usage_uses_the_latest_assistant_window_tokens() {
        let mut conversation = Conversation::default();
        conversation.replace_from_api(
            &entries(vec![
                json!({ "id": "msg_user", "type": "user", "time": { "created": 1 }, "text": "hi" }),
                json!({ "id": "msg_old", "type": "assistant", "time": { "created": 2 }, "agent": "build",
                        "content": [], "tokens": { "input": 800, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } } }),
                json!({ "id": "msg_new", "type": "assistant", "time": { "created": 3 }, "agent": "build",
                        "content": [], "tokens": { "input": 124, "output": 40, "reasoning": 10.4, "cache": { "read": 48000, "write": 200 } } }),
                json!({ "id": "msg_streaming", "type": "assistant", "time": { "created": 4 }, "agent": "build", "content": [] }),
                json!({ "id": "msg_idle", "type": "idle", "time": { "created": 5 }, "outcome": "succeeded" }),
                json!({ "id": "msg_switch", "type": "model-switched", "time": { "created": 6 },
                        "model": { "id": "m", "providerID": "p" } }),
            ]),
            None,
        );
        assert_eq!(conversation.context_tokens(), Some(48_374));
        assert!(conversation.apply_event(&live(
            "session.step.ended",
            json!({ "assistantMessageID": "msg_new", "finish": "stop", "cost": 0,
                    "tokens": { "input": 200, "output": 50, "reasoning": 0, "cache": { "read": 50000, "write": 0 } } }),
        )));
        assert_eq!(conversation.context_tokens(), Some(50_250));
        assert_eq!(history("session.messages.long").context_tokens(), Some(342));
    }

    // ---------------------------------------------------------------------
    // Live events (CP-009)
    // ---------------------------------------------------------------------

    fn scenario_session(key: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/v2-2.0.8/index.json"
        );
        let index: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        index["sessions"][key]
            .as_str()
            .unwrap_or_else(|| panic!("no session {key}"))
            .to_owned()
    }

    fn scenario_events(name: &str) -> Vec<Value> {
        let path = format!(
            "{}/tests/fixtures/v2-2.0.8/events/{name}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{path}: {error}"))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Feeds a scenario's events for one session through the real reducer,
    /// routed by `sessionID` the way the UI routes them (many carry no
    /// `location`).
    fn replay_events(conversation: &mut Conversation, events: &[Value], session_id: &str) {
        for payload in events {
            let event = protocol::Event::deserialize(payload).unwrap();
            let kind = protocol::decode_event(&event);
            assert!(
                !matches!(kind, protocol::EventKind::Malformed { .. }),
                "{kind:?}"
            );
            if kind.session_id() == Some(session_id) {
                conversation.apply(&event, &kind);
            }
        }
    }

    /// The message list captured after a scenario, oldest first. The history
    /// scenario was captured as newest-first pages of three.
    fn captured_history(fixtures: &[&str]) -> Conversation {
        let mut conversation = Conversation::default();
        let entries: Vec<_> = fixtures
            .iter()
            .rev()
            .flat_map(|name| fixture_entries(name))
            .collect();
        conversation.replace_from_api(&entries, None);
        conversation
    }

    fn message_shape(conversation: &Conversation) -> Vec<(String, Vec<String>)> {
        conversation
            .messages
            .iter()
            .map(|message| {
                (
                    message.id.clone(),
                    message
                        .segments
                        .iter()
                        .map(|segment| segment.key.clone())
                        .collect(),
                )
            })
            .collect()
    }

    /// (events file, session key in `index.json`, message-list fixtures
    /// captured after the scenario, newest page first).
    const SCENARIOS: &[(&str, &str, &[&str])] = &[
        ("text", "text", &["session.messages.text"]),
        ("reasoning", "reasoning", &["session.messages.reasoning"]),
        ("tools", "tools", &["session.messages.tools"]),
        ("long", "long", &["session.messages.long"]),
        ("error", "error", &["session.messages.error"]),
        ("retry", "retry", &["session.messages.retry"]),
        ("interrupt", "interrupt", &["session.messages.interrupt"]),
        ("attachment", "attachment", &["session.messages.attachment"]),
        ("permission", "permission", &["session.messages.permission"]),
        ("subagent", "subagent", &["session.messages.subagent"]),
        (
            "subagent",
            "subagent-child",
            &["session.messages.subagent-child"],
        ),
        (
            "subagent-permission",
            "subagent-permission",
            &["session.messages.subagent-permission"],
        ),
        (
            "subagent-permission",
            "subagent-permission-child",
            &["session.messages.subagent-permission-child"],
        ),
        (
            "history",
            "history",
            &[
                "session.messages.page1",
                "session.messages.page2",
                "session.messages.page3",
                "session.messages.page4",
            ],
        ),
    ];

    /// Acceptance: every captured scenario, streamed live from an empty
    /// transcript, ends in exactly the rows (order, role, kind, body, images,
    /// time), message IDs, segment keys and context usage that a reload of
    /// the message list captured afterwards produces.
    #[test]
    fn live_scenarios_end_exactly_where_history_does() {
        for (events, key, fixtures) in SCENARIOS {
            let session_id = scenario_session(key);
            let mut live = Conversation::default();
            replay_events(&mut live, &scenario_events(events), &session_id);
            let reloaded = captured_history(fixtures);
            assert!(!reloaded.messages.is_empty(), "{key}");
            assert_eq!(
                row_values(&live),
                row_values(&reloaded),
                "{key}: live rows differ from history"
            );
            assert_eq!(message_shape(&live), message_shape(&reloaded), "{key}");
            assert_eq!(live.context_tokens(), reloaded.context_tokens(), "{key}");
            assert!(live.messages.iter().all(|message| !message.queued), "{key}");
        }
    }

    /// `message_events_during_load`: events that arrive while a transcript
    /// loads are replayed onto the fresh snapshot, which already reflects
    /// some or all of them. Nothing may duplicate.
    #[test]
    fn replaying_scenarios_onto_their_final_snapshot_changes_nothing() {
        for (events, key, fixtures) in SCENARIOS {
            let session_id = scenario_session(key);
            let mut conversation = captured_history(fixtures);
            let before = row_values(&conversation);
            replay_events(&mut conversation, &scenario_events(events), &session_id);
            assert_eq!(row_values(&conversation), before, "{key}");
            assert_eq!(
                message_shape(&conversation),
                message_shape(&captured_history(fixtures)),
                "{key}"
            );
        }
    }

    /// A reconnect reload lands mid-scenario: the snapshot holds a prefix of
    /// the events, the buffer holds the rest plus some already reflected.
    #[test]
    fn replaying_onto_a_mid_scenario_snapshot_converges() {
        for (events, key, fixtures) in SCENARIOS {
            let session_id = scenario_session(key);
            let events = scenario_events(events);
            let reloaded = captured_history(fixtures);
            for split in [events.len() / 3, events.len() / 2, events.len() * 2 / 3] {
                // Snapshot: the live projection of the first `split` events,
                // standing in for the server's message list at that moment.
                let mut snapshot = Conversation::default();
                replay_events(&mut snapshot, &events[..split], &session_id);
                // Buffer starts a few events before the snapshot.
                let overlap = split.saturating_sub(4);
                replay_events(&mut snapshot, &events[overlap..], &session_id);
                assert_eq!(
                    row_values(&snapshot),
                    row_values(&reloaded),
                    "{key} split {split}"
                );
            }
        }
    }

    fn live(kind: &str, mut data: Value) -> Value {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if data.get("sessionID").is_none() {
            data["sessionID"] = json!("ses_1");
        }
        json!({ "id": format!("evt_{n:026}"), "created": 1_000 + n, "type": kind, "data": data })
    }

    fn tool_rows(conversation: &Conversation) -> Vec<String> {
        row_values(conversation)
            .into_iter()
            .filter(|row| row["kind"] == "tool")
            .map(|row| row["body"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn concurrent_tools_in_one_message_are_distinct_rows() {
        let mut conversation = Conversation::default();
        for (kind, data) in [
            (
                "session.step.started",
                json!({ "assistantMessageID": "msg_a", "agent": "build", "started": 5 }),
            ),
            (
                "session.tool.input.started",
                json!({ "assistantMessageID": "msg_a", "id": "call_1", "name": "shell" }),
            ),
            (
                "session.tool.input.started",
                json!({ "assistantMessageID": "msg_a", "id": "call_2", "name": "read" }),
            ),
            (
                "session.tool.input.delta",
                json!({ "assistantMessageID": "msg_a", "id": "call_1", "delta": "{\"command\":" }),
            ),
            (
                "session.tool.input.delta",
                json!({ "assistantMessageID": "msg_a", "id": "call_1", "delta": "\"ls\"}" }),
            ),
            (
                "session.tool.input.ended",
                json!({ "assistantMessageID": "msg_a", "id": "call_2", "text": "{\"path\":\"a.rs\"}" }),
            ),
        ] {
            conversation.apply_event(&live(kind, data));
        }
        assert_eq!(
            tool_rows(&conversation),
            ["shell · running — ls", "read · running — a.rs"]
        );
        for (kind, data) in [
            (
                "session.tool.called",
                json!({ "assistantMessageID": "msg_a", "id": "call_2", "input": { "path": "a.rs" } }),
            ),
            (
                "session.tool.progress",
                json!({ "assistantMessageID": "msg_a", "id": "call_2", "metadata": { "lines": 3 } }),
            ),
            (
                "session.tool.success",
                json!({ "assistantMessageID": "msg_a", "id": "call_2",
                                              "content": [{ "type": "text", "text": "fn main" }] }),
            ),
            (
                "session.tool.called",
                json!({ "assistantMessageID": "msg_a", "id": "call_1", "input": { "command": "ls" } }),
            ),
            (
                "session.tool.failed",
                json!({ "assistantMessageID": "msg_a", "id": "call_1",
                                             "error": { "type": "tool", "message": "exit 2" } }),
            ),
            // Late or replayed events never reopen a settled tool.
            (
                "session.tool.called",
                json!({ "assistantMessageID": "msg_a", "id": "call_2", "input": { "path": "b.rs" } }),
            ),
            (
                "session.tool.input.ended",
                json!({ "assistantMessageID": "msg_a", "id": "call_1", "text": "{}" }),
            ),
        ] {
            conversation.apply_event(&live(kind, data));
        }
        assert_eq!(
            tool_rows(&conversation),
            ["shell · error — ls: exit 2", "read · completed — a.rs"]
        );
        let tool = conversation.messages[0].segments[1].tool.as_ref().unwrap();
        assert!(matches!(tool.state, protocol::ToolState::Completed { .. }));
    }

    #[test]
    fn the_same_tool_id_in_two_messages_stays_two_rows() {
        let mut conversation = Conversation::default();
        for message in ["msg_a", "msg_b"] {
            conversation.apply_event(&live(
                "session.step.started",
                json!({ "assistantMessageID": message, "agent": "build", "started": 1 }),
            ));
            conversation.apply_event(&live(
                "session.tool.input.started",
                json!({ "assistantMessageID": message, "id": "call_mock_read", "name": "read" }),
            ));
        }
        conversation.apply_event(&live(
            "session.tool.called",
            json!({ "assistantMessageID": "msg_b", "id": "call_mock_read", "input": { "path": "b" } }),
        ));
        assert_eq!(
            tool_rows(&conversation),
            ["read · running", "read · running — b"]
        );
        // A tool this conversation never saw start is ignored, like the server does.
        assert!(!conversation.apply_event(&live(
            "session.tool.success",
            json!({ "assistantMessageID": "msg_a", "id": "call_unknown", "content": [] }),
        )));
    }

    #[test]
    fn ended_text_replaces_duplicated_deltas() {
        let mut conversation = history("session.messages.text");
        let message = conversation.messages[1].id.clone();
        // A delta buffered during the load lands on the finished snapshot text.
        conversation.apply_event(&live(
            "session.text.delta",
            json!({ "assistantMessageID": message, "ordinal": 0, "delta": "Hello from the mock provider." }),
        ));
        assert_eq!(
            conversation.messages[1].segments[0].text,
            "Hello from the mock provider.Hello from the mock provider."
        );
        assert!(conversation.apply_event(&live(
            "session.text.ended",
            json!({ "assistantMessageID": message, "ordinal": 0, "text": "Hello from the mock provider." }),
        )));
        assert_eq!(
            row_values(&conversation),
            row_values(&history("session.messages.text"))
        );

        // Reasoning started/delta/ended keep their own ordinal space.
        let mut conversation = Conversation::default();
        for (kind, data) in [
            (
                "session.step.started",
                json!({ "assistantMessageID": "msg_a", "agent": "build", "started": 1 }),
            ),
            (
                "session.reasoning.started",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0 }),
            ),
            (
                "session.text.started",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0 }),
            ),
            (
                "session.reasoning.delta",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0, "delta": "hm" }),
            ),
            (
                "session.text.delta",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0, "delta": "hel" }),
            ),
            (
                "session.text.delta",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0, "delta": "lo" }),
            ),
        ] {
            conversation.apply_event(&live(kind, data));
        }
        assert_eq!(
            conversation.rendered_rows(),
            ["AGENT\nReasoning\nhm\n\nhello"]
        );
        assert!(
            !conversation.apply_event(&live(
                "session.text.started",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0 }),
            )),
            "a replayed start never resets the segment"
        );
        assert_eq!(
            conversation.rendered_rows(),
            ["AGENT\nReasoning\nhm\n\nhello"]
        );
    }

    #[test]
    fn content_updated_replaces_the_message_with_the_server_version() {
        let mut conversation = history("session.messages.tools");
        let message = conversation.messages[1].id.clone();
        assert!(conversation.apply_event(&live(
            "session.message.content.updated",
            json!({
                "messageID": message,
                "content": [
                    { "type": "text", "text": "Rewritten." },
                    { "type": "tool", "id": "call_mock_glob", "name": "glob", "time": { "created": 7 },
                      "state": { "status": "completed", "input": { "pattern": "*.md" }, "content": [] } }
                ]
            }),
        )));
        let bodies: Vec<_> = row_values(&conversation)
            .into_iter()
            .map(|row| row["body"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            bodies,
            [
                "Read two things at once. [[scenario:tools]]",
                "Rewritten.",
                "glob · completed — *.md",
                "Both tool calls finished.",
            ]
        );
        // Tool events after the update act on the server's structured state.
        assert!(!conversation.apply_event(&live(
            "session.tool.called",
            json!({ "assistantMessageID": message, "id": "call_mock_glob", "input": {} }),
        )));
        assert!(!conversation.apply_event(&live(
            "session.message.content.updated",
            json!({ "messageID": "msg_unknown", "content": [] }),
        )));
    }

    #[test]
    fn queued_prompts_show_until_delivered_or_cancelled() {
        let mut conversation = Conversation::default();
        let enqueue = |id: &str, text: &str| {
            live(
                "session.inbox.enqueued",
                json!({ "inboxID": id, "item": { "type": "user", "payload": { "text": text }, "delivery": "steer" } }),
            )
        };
        for payload in [
            enqueue("msg_1", "first"),
            live("session.inbox.delivered", json!({ "inboxID": "msg_1" })),
            live(
                "session.step.started",
                json!({ "assistantMessageID": "msg_a", "agent": "build", "started": 1 }),
            ),
            enqueue("msg_2", "steer me"),
            enqueue("msg_3", "and me"),
            // A later step still lands before the undelivered prompts.
            live(
                "session.step.started",
                json!({ "assistantMessageID": "msg_b", "agent": "build", "started": 2 }),
            ),
            live(
                "session.text.started",
                json!({ "assistantMessageID": "msg_b", "ordinal": 0 }),
            ),
            live(
                "session.text.ended",
                json!({ "assistantMessageID": "msg_b", "ordinal": 0, "text": "working" }),
            ),
        ] {
            conversation.apply_event(&payload);
        }
        let ids = |conversation: &Conversation| -> Vec<String> {
            conversation.messages.iter().map(|m| m.id.clone()).collect()
        };
        assert_eq!(
            ids(&conversation),
            ["msg_1", "msg_a", "msg_b", "msg_2", "msg_3"]
        );
        assert!(conversation.has_user_message("msg_2"));
        assert!(!conversation.has_delivered_user_message("msg_2"));
        let bodies: Vec<String> = row_values(&conversation)
            .iter()
            .map(|row| row["body"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            bodies,
            ["first", "working"],
            "waiting prompts are tray items, not transcript rows"
        );
        let tray: Vec<String> = conversation
            .tray_items()
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(tray, ["msg_2", "msg_3"]);

        let delivered = live("session.inbox.delivered", json!({ "inboxID": "msg_3" }));
        let delivered_at = delivered["created"].as_u64().unwrap();
        assert!(conversation.apply_event(&delivered));
        assert_eq!(
            ids(&conversation),
            ["msg_1", "msg_a", "msg_b", "msg_3", "msg_2"]
        );
        assert_eq!(
            conversation.messages[3].created, delivered_at,
            "history uses the delivery time"
        );
        assert!(conversation.has_delivered_user_message("msg_3"));
        assert_eq!(row_values(&conversation).last().unwrap()["body"], "and me");
        assert_eq!(conversation.tray_items().len(), 1);

        // Only an undelivered prompt can be cancelled.
        assert!(!conversation.apply_event(&live(
            "session.inbox.cancelled",
            json!({ "inboxID": "msg_3" })
        )));
        assert!(conversation.apply_event(&live(
            "session.inbox.cancelled",
            json!({ "inboxID": "msg_2" })
        )));
        assert_eq!(ids(&conversation), ["msg_1", "msg_a", "msg_b", "msg_3"]);
        assert!(
            !conversation.apply_event(&enqueue("msg_3", "echo")),
            "never duplicates a known row"
        );
    }

    #[test]
    fn the_tray_follows_enqueue_delivery_changes_cancel_and_sync() {
        let mut conversation = Conversation::default();
        let enqueue = |id: &str, item: Value| {
            live(
                "session.inbox.enqueued",
                json!({ "inboxID": id, "item": item }),
            )
        };
        let file = json!({ "data": "aGk=", "mime": "text/plain", "name": "a.txt" });
        for payload in [
            enqueue(
                "msg_s",
                json!({ "type": "user", "payload": { "text": "and keep the 30 s cap\non the backoff" }, "delivery": "steer" }),
            ),
            enqueue(
                "msg_q",
                json!({ "type": "user", "payload": { "text": "then update the changelog", "files": [file.clone(), file] }, "delivery": "queue" }),
            ),
            enqueue(
                "msg_syn",
                json!({ "type": "synthetic", "payload": { "text": "The background subagent finished." }, "delivery": "steer" }),
            ),
        ] {
            assert!(conversation.apply_event(&payload));
        }
        let tray = conversation.tray_items();
        assert_eq!(
            tray,
            [
                TrayItem {
                    id: "msg_s".into(),
                    delivery: protocol::Delivery::Steer,
                    text: "and keep the 30 s cap\non the backoff".into(),
                    attachments: 0,
                },
                TrayItem {
                    id: "msg_q".into(),
                    delivery: protocol::Delivery::Queue,
                    text: "then update the changelog".into(),
                    attachments: 2,
                },
            ],
            "oldest first; synthetic items are not in the tray"
        );
        let rows = row_values(&conversation);
        assert_eq!(rows.len(), 1, "the synthetic item keeps its transcript row");
        assert_eq!(rows[0]["role"], "AGENT");

        let switch = |id: &str, delivery: &str| {
            live(
                "session.inbox.delivery.changed",
                json!({ "inboxID": id, "delivery": delivery }),
            )
        };
        assert!(conversation.apply_event(&switch("msg_q", "steer")));
        assert!(
            !conversation.apply_event(&switch("msg_q", "steer")),
            "no change"
        );
        assert!(!conversation.apply_event(&switch("msg_unknown", "queue")));
        assert_eq!(
            conversation.tray_items()[1].delivery,
            protocol::Delivery::Steer
        );

        // A reload's inbox list rebuilds the tray, deliveries included.
        let listed: protocol::InboxListResponse = serde_json::from_value(json!({ "data": [
            { "id": "msg_s", "sessionID": "ses_1", "time": { "created": 1 }, "type": "user",
              "payload": { "text": "and keep the 30 s cap\non the backoff" }, "delivery": "queue" },
            { "id": "msg_new", "sessionID": "ses_1", "time": { "created": 2 }, "type": "user",
              "payload": { "text": "from another client" }, "delivery": "steer" }
        ] }))
        .unwrap();
        conversation.sync_queued(&listed.data);
        let tray: Vec<_> = conversation
            .tray_items()
            .into_iter()
            .map(|item| (item.id, item.delivery))
            .collect();
        assert_eq!(
            tray,
            [
                ("msg_s".to_owned(), protocol::Delivery::Queue),
                ("msg_new".to_owned(), protocol::Delivery::Steer)
            ]
        );
        assert!(row_values(&conversation).is_empty(), "msg_syn is gone too");

        assert!(conversation.apply_event(&live(
            "session.inbox.delivered",
            json!({ "inboxID": "msg_new" })
        )));
        assert!(conversation.apply_event(&live(
            "session.inbox.cancelled",
            json!({ "inboxID": "msg_s" })
        )));
        assert!(conversation.tray_items().is_empty());
        assert_eq!(
            row_values(&conversation)[0]["body"],
            "from another client",
            "a delivered prompt moves into the transcript"
        );
    }

    #[test]
    fn a_reload_keeps_parked_prompts_from_the_inbox() {
        // After an interrupt the steered follow-up stays in the inbox.
        let inbox: protocol::InboxListResponse =
            serde_json::from_value(fixture_body("session.inbox.list.afterInterrupt")).unwrap();
        let mut conversation = history("session.messages.interrupt");
        conversation.sync_queued(&inbox.data);
        let rows = row_values(&conversation);
        assert_eq!(
            rows,
            row_values(&history("session.messages.interrupt")),
            "a parked prompt is a tray item, not a row"
        );
        let parked = inbox.data[0].id.clone();
        assert_eq!(
            conversation.tray_items(),
            [TrayItem {
                id: parked.clone(),
                delivery: protocol::Delivery::Steer,
                text: "Follow-up sent while busy. [[scenario:text]]".into(),
                attachments: 0,
            }]
        );
        assert!(conversation.has_user_message(&parked));

        // A plain reload keeps the local queued item; the inbox list settles it.
        let mut reloaded = conversation.clone();
        reloaded.replace_from_api(&fixture_entries("session.messages.interrupt"), None);
        assert_eq!(row_values(&reloaded), rows);
        assert_eq!(reloaded.tray_items(), conversation.tray_items());
        reloaded.sync_queued(&[]);
        assert!(!reloaded.has_user_message(&parked));
        assert!(reloaded.tray_items().is_empty());

        // The captured cancellation removes it live.
        let session_id = scenario_session("interrupt");
        let events = scenario_events("interrupt");
        let cancel: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "session.inbox.cancelled")
            .cloned()
            .collect();
        assert!(cancel[0].get("location").is_none(), "routed by sessionID");
        replay_events(&mut conversation, &cancel, &session_id);
        assert_eq!(
            row_values(&conversation),
            row_values(&history("session.messages.interrupt"))
        );
    }

    #[test]
    fn step_errors_show_once_and_interrupts_not_at_all() {
        let mut conversation = Conversation::default();
        for (kind, data) in [
            (
                "session.step.started",
                json!({ "assistantMessageID": "msg_a", "agent": "build", "started": 1 }),
            ),
            (
                "session.text.started",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0 }),
            ),
            (
                "session.text.ended",
                json!({ "assistantMessageID": "msg_a", "ordinal": 0, "text": "partial" }),
            ),
            (
                "session.step.failed",
                json!({ "assistantMessageID": "msg_a",
                                            "error": { "type": "aborted", "message": "Step interrupted" } }),
            ),
            ("session.execution.interrupted", json!({ "reason": "user" })),
            (
                "session.execution.failed",
                json!({ "error": { "type": "aborted", "message": "Aborted" } }),
            ),
        ] {
            conversation.apply_event(&live(kind, data));
        }
        assert_eq!(conversation.rendered_rows(), ["AGENT\npartial"]);

        // A step failure shows on its message; the execution failure repeating it adds nothing.
        let error = json!({ "type": "provider.internal", "message": "boom", "status": 500 });
        conversation.apply_event(&live(
            "session.inbox.enqueued",
            json!({ "inboxID": "msg_u", "item": { "type": "user", "payload": { "text": "again" } } }),
        ));
        conversation.apply_event(&live(
            "session.inbox.delivered",
            json!({ "inboxID": "msg_u" }),
        ));
        conversation.apply_event(&live(
            "session.step.started",
            json!({ "assistantMessageID": "msg_b", "agent": "build", "started": 2 }),
        ));
        conversation.apply_event(&live(
            "session.step.failed",
            json!({ "assistantMessageID": "msg_b", "error": error }),
        ));
        conversation.apply_event(&live("session.execution.failed", json!({ "error": error })));
        assert_eq!(
            conversation.rendered_rows(),
            ["AGENT\npartial", "YOU\nagain", "AGENT\nError: boom"]
        );

        // A failure outside any step becomes its own error row, once per event.
        let failed = live(
            "session.execution.failed",
            json!({ "error": { "type": "config", "message": "no model" } }),
        );
        conversation.apply_event(&live(
            "session.inbox.enqueued",
            json!({ "inboxID": "msg_v", "item": { "type": "user", "payload": { "text": "third" } } }),
        ));
        conversation.apply_event(&live(
            "session.inbox.delivered",
            json!({ "inboxID": "msg_v" }),
        ));
        assert!(conversation.apply_event(&failed));
        assert!(!conversation.apply_event(&failed));
        assert_eq!(
            conversation.rendered_rows().last().map(String::as_str),
            Some("AGENT\nError: no model")
        );
        assert_eq!(
            conversation.messages.last().unwrap().id,
            protocol::message_id_from_event_id(failed["id"].as_str().unwrap()).unwrap(),
            "keyed like the idle entry the server writes"
        );

        // A retried step clears the previous attempt's error.
        conversation.apply_event(&live(
            "session.step.started",
            json!({ "assistantMessageID": "msg_b", "agent": "build", "started": 3 }),
        ));
        assert!(
            conversation
                .messages
                .iter()
                .find(|m| m.id == "msg_b")
                .unwrap()
                .error
                .is_none()
        );
    }

    #[test]
    fn event_projected_notes_use_history_keys_and_text() {
        let mut conversation = Conversation::default();
        let model = live(
            "session.model.selected",
            json!({ "model": { "id": "gpt-6", "providerID": "openai", "variant": "high" } }),
        );
        let shell = live(
            "session.shell.started",
            json!({ "shell": { "id": "sh_1", "status": "running", "command": "ls", "cwd": "/w" } }),
        );
        for payload in [
            model.clone(),
            live("session.agent.selected", json!({ "agent": "plan" })),
            live(
                "session.moved",
                json!({ "location": { "directory": "/work/b" }, "projectID": "p" }),
            ),
            live("session.synthetic", json!({ "text": "Continue." })),
            live(
                "session.skill.activated",
                json!({ "id": "pdf", "name": "PDF tools", "text": "# body" }),
            ),
            live(
                "session.instructions.updated",
                json!({ "delta": { "AGENTS.md": "sha" } }),
            ),
            live(
                "session.instructions.updated",
                json!({ "text": "<all>", "delta": { "AGENTS.md": "sha" } }),
            ),
            shell.clone(),
            live(
                "session.shell.ended",
                json!({ "shell": { "id": "sh_1", "status": "timeout", "command": "ls", "cwd": "/w" },
                                                "output": { "output": "a\n", "cursor": 2, "size": 2, "truncated": false } }),
            ),
            live(
                "session.compaction.started",
                json!({ "reason": "auto", "inputID": "msg_compact" }),
            ),
            live("session.compaction.delta", json!({ "text": "partial" })),
            live(
                "session.compaction.ended",
                json!({ "reason": "auto", "text": "Earlier work summary." }),
            ),
            live(
                "session.compaction.failed",
                json!({ "reason": "auto", "error": { "type": "provider", "message": "too long" } }),
            ),
            live("session.execution.succeeded", json!({})),
            live(
                "session.usage.updated",
                json!({ "cost": 0, "tokens": { "input": 1, "output": 1, "reasoning": 0, "cache": { "read": 0, "write": 0 } } }),
            ),
            live("shell.created", json!({ "info": { "id": "sh_2" } })),
            live("pty.created", json!({})),
            live("project.updated", json!({ "id": "p", "canonical": "/w" })),
        ] {
            conversation.apply_event(&payload);
        }
        let bodies: Vec<_> = row_values(&conversation)
            .into_iter()
            .map(|row| row["body"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            bodies,
            [
                "Switched model to openai/gpt-6 (high)",
                "Switched agent to plan",
                "Moved to /work/b",
                "Continue.",
                "Skill: PDF tools",
                "Instructions updated: AGENTS.md",
                "```\n$ ls\na\n```\n\nTimed out",
                "Earlier work summary.",
                "Compaction failed: too long",
            ]
        );
        assert_eq!(
            conversation.messages[0].id,
            protocol::message_id_from_event_id(model["id"].as_str().unwrap()).unwrap()
        );
        assert_eq!(
            conversation.messages[6].id,
            protocol::message_id_from_event_id(shell["id"].as_str().unwrap()).unwrap()
        );
        assert_eq!(
            conversation.messages[7].id, "msg_compact",
            "compaction uses inputID"
        );
        assert!(
            row_values(&conversation)
                .iter()
                .all(|row| row["role"] == "AGENT" && row["kind"] == "")
        );
    }

    #[test]
    fn session_events_without_location_route_by_session_id() {
        for name in ["text", "error", "interrupt", "subagent"] {
            for payload in scenario_events(name) {
                let event = protocol::Event::deserialize(&payload).unwrap();
                let kind = protocol::decode_event(&event);
                if event.type_.starts_with("session.")
                    && !matches!(kind, protocol::EventKind::Other(_))
                {
                    assert!(
                        kind.session_id().is_some_and(|id| id.starts_with("ses_")),
                        "{name}: {}",
                        event.type_
                    );
                }
            }
        }
        let started = scenario_events("text")
            .into_iter()
            .find(|event| event["type"] == "session.execution.started")
            .unwrap();
        assert!(started.get("location").is_none());
        assert_eq!(
            event_run_status(&started),
            Some((scenario_session("text"), RunStatus::Busy))
        );
    }

    #[test]
    fn run_status_follows_execution_and_retry_events() {
        let status = |kind: &str, data: Value| event_run_status(&live(kind, data)).map(|(_, s)| s);
        assert_eq!(
            status("session.execution.started", json!({})),
            Some(RunStatus::Busy)
        );
        assert_eq!(
            status(
                "session.step.started",
                json!({ "assistantMessageID": "msg_a", "started": 1 })
            ),
            Some(RunStatus::Busy)
        );
        for (kind, data) in [
            ("session.execution.succeeded", json!({})),
            (
                "session.execution.failed",
                json!({ "error": { "type": "x", "message": "y" } }),
            ),
            ("session.execution.interrupted", json!({ "reason": "user" })),
        ] {
            assert_eq!(status(kind, data), Some(RunStatus::Idle), "{kind}");
        }
        assert_eq!(
            status(
                "session.execution.interrupted",
                json!({ "reason": "shutdown" })
            ),
            None,
            "a shutdown keeps the run for the restarted server"
        );
        for ignored in [
            "session.status",
            "session.idle",
            "session.error",
            "session.usage.updated",
        ] {
            assert_eq!(
                status(ignored, json!({ "status": { "type": "idle" } })),
                None,
                "{ignored}"
            );
        }

        let retry = scenario_events("retry")
            .into_iter()
            .find(|event| event["type"] == "session.retry.scheduled")
            .unwrap();
        assert_eq!(
            event_run_status(&retry),
            Some((
                scenario_session("retry"),
                RunStatus::Retry {
                    message: "Mock provider temporarily unavailable. Retrying (attempt 2)…".into(),
                    attempt: 2,
                }
            ))
        );
        // The captured retry run goes busy → retry → busy → idle.
        let statuses: Vec<_> = scenario_events("retry")
            .iter()
            .filter_map(event_run_status)
            .map(|(_, status)| match status {
                RunStatus::Idle => "idle",
                RunStatus::Busy => "busy",
                RunStatus::Retry { .. } => "retry",
            })
            .collect();
        assert_eq!(statuses, ["busy", "busy", "retry", "busy", "idle"]);
    }

    #[test]
    fn debug_commands_feed_v2_events_through_the_reducers() {
        assert_eq!(
            DebugCommand::parse(" /debug error "),
            Some(DebugCommand::Error)
        );
        assert_eq!(
            DebugCommand::parse("/debug retry"),
            Some(DebugCommand::Retry)
        );
        assert_eq!(
            DebugCommand::parse("/debug clear"),
            Some(DebugCommand::Idle)
        );
        assert_eq!(DebugCommand::parse("/debug idle"), Some(DebugCommand::Idle));
        assert_eq!(DebugCommand::parse("/debug"), None);

        let mut conversation = history("session.messages.text");
        let mut statuses = Vec::new();
        for command in [DebugCommand::Error, DebugCommand::Retry, DebugCommand::Idle] {
            for payload in command.events("ses_1", 42) {
                let event = protocol::Event::deserialize(&payload).unwrap();
                let kind = protocol::decode_event(&event);
                assert!(
                    !matches!(
                        kind,
                        protocol::EventKind::Malformed { .. } | protocol::EventKind::Other(_)
                    ),
                    "{payload}"
                );
                assert_eq!(kind.session_id(), Some("ses_1"));
                assert!(event.id.starts_with("evt_"));
                conversation.apply(&event, &kind);
                statuses.extend(run_status_change(&kind).map(|(_, status)| status));
            }
        }
        let rows = row_values(&conversation);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[2]["kind"], "error");
        assert_eq!(rows[2]["body"], "AI_APICallError: Not Found (404)");
        assert_eq!(rows[2]["time"], 42);
        assert!(
            matches!(
                statuses.as_slice(),
                [
                    RunStatus::Busy,
                    RunStatus::Idle,
                    RunStatus::Retry { attempt: 1, .. },
                    RunStatus::Idle
                ]
            ),
            "{statuses:?}"
        );
    }

    #[test]
    fn compact_context_usage_matches_the_composer_label() {
        assert_eq!(format_context_usage(0, 200_000), "0 / 200k");
        assert_eq!(format_context_usage(12_400, 200_000), "12.4k / 200k");
        assert_eq!(format_context_usage(999, 8_192), "999 / 8.2k");
        assert_eq!(format_context_usage(1_500_000, 2_000_000), "1.5m / 2m");
    }

    #[test]
    fn running_tool_extracts_command_from_input() {
        let tool: protocol::ToolCall = serde_json::from_value(json!({
            "type": "tool",
            "id": "call_1",
            "name": "shell",
            "time": { "created": 5 },
            "state": {
                "status": "running",
                "input": {
                    "command": "pnpm exec playwright test\n  e2e/tests/desktop/apps20-ad-resizer.spec.ts",
                    "description": "Run the resizer spec"
                },
                "metadata": {}
            }
        }))
        .unwrap();
        let segment = tool_segment(&tool);
        assert_eq!(
            segment.text,
            "shell · running — pnpm exec playwright test e2e/tests/desktop/apps20-ad-resizer.spec.ts"
        );
        assert_eq!((segment.key.as_str(), segment.created), ("tool:call_1", 5));
    }

    #[test]
    fn tool_titles_follow_the_v2_tool_inputs() {
        let title = |name: &str, input: Value| tool_title(name, input.as_object().unwrap());
        assert_eq!(
            title("shell", json!({ "description": "d" })).as_deref(),
            Some("d")
        );
        assert_eq!(
            title(
                "subagent",
                json!({ "agent": "general", "description": "Mock task", "prompt": "p" })
            )
            .as_deref(),
            Some("Mock task")
        );
        for name in ["read", "write", "edit"] {
            assert_eq!(
                title(name, json!({ "path": "src/a.rs" })).as_deref(),
                Some("src/a.rs")
            );
        }
        assert_eq!(
            title("grep", json!({ "pattern": "fn main", "path": "src" })).as_deref(),
            Some("fn main")
        );
        assert_eq!(
            title("glob", json!({ "pattern": "**/*.rs" })).as_deref(),
            Some("**/*.rs")
        );
        assert_eq!(
            title(
                "webfetch",
                json!({ "url": "https://x.dev", "format": "markdown" })
            )
            .as_deref(),
            Some("https://x.dev")
        );
        assert_eq!(
            title("websearch", json!({ "query": "gtk4" })).as_deref(),
            Some("gtk4")
        );
        assert_eq!(
            title("skill", json!({ "id": "pdf" })).as_deref(),
            Some("pdf")
        );
        assert_eq!(
            title("kagi_search", json!({ "query": "rust" })).as_deref(),
            Some("rust")
        );
        assert_eq!(
            title("patch", json!({ "patchText": "*** Begin Patch" })),
            None
        );
        assert_eq!(title("shell", json!({ "command": "  " })), None);
    }

    fn session_info(value: Value) -> protocol::SessionInfo {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn session_info_maps_to_a_session() {
        let session = Session::from_info(&session_info(json!({
            "id": "ses_a",
            "parentID": "ses_parent",
            "projectID": "prj",
            "agent": "build",
            "model": { "id": "gpt-6", "providerID": "openai", "variant": "high" },
            "time": { "created": 10, "updated": 20, "archived": 30 },
            "title": "Fix the build",
            "location": { "directory": "/work/a" }
        })));
        assert_eq!(
            session,
            Session {
                id: "ses_a".into(),
                directory: "/work/a".into(),
                title: "Fix the build".into(),
                time: SessionTime {
                    created: 10,
                    updated: 20,
                    archived: Some(30.0),
                },
                parent_id: Some("ses_parent".into()),
                model: Some(SessionModel {
                    id: "gpt-6".into(),
                    provider_id: "openai".into(),
                    variant: Some("high".into()),
                }),
            }
        );
    }

    #[test]
    fn untitled_session_info_gets_a_stable_title() {
        for title in [json!(null), json!(""), json!("   ")] {
            let session = Session::from_info(&session_info(json!({
                "id": "ses_a",
                "time": { "created": 1, "updated": 2 },
                "title": title,
                "location": { "directory": "/work" }
            })));
            assert_eq!(session.title, "Untitled session");
            assert_eq!(session.parent_id, None);
            assert_eq!(session.model, None);
            assert_eq!(session.time.archived, None);
        }
    }

    #[test]
    fn project_info_maps_canonical_to_worktree() {
        let project: protocol::ProjectInfo = serde_json::from_value(json!({
            "id": "prj",
            "canonical": "/work/a",
            "name": "A",
            "sandboxes": ["/work/a-sandbox"]
        }))
        .unwrap();
        assert_eq!(
            Project::from_info(&project),
            Project {
                worktree: "/work/a".into(),
                name: Some("A".into()),
            }
        );
        let unnamed: protocol::ProjectInfo =
            serde_json::from_value(json!({ "id": "prj", "canonical": "/work/b", "name": "" }))
                .unwrap();
        assert_eq!(Project::from_info(&unnamed).name, None);
    }

    fn session_event(event_type: &str, data: Value) -> Value {
        json!({
            "id": "evt_1",
            "created": 500,
            "type": event_type,
            "location": { "directory": "/work" },
            "data": data
        })
    }

    fn listed(id: &str, title: &str) -> Session {
        Session {
            id: id.into(),
            directory: "/work".into(),
            title: title.into(),
            time: SessionTime {
                created: 1,
                updated: 900,
                archived: None,
            },
            parent_id: None,
            model: None,
        }
    }

    #[test]
    fn session_created_event_builds_a_root_session_from_creation_fields() {
        let change = SessionChange::from_event(&session_event(
            "session.created",
            json!({
                "sessionID": "ses_new",
                "slug": "misty-garden",
                "version": "2.0.8",
                "projectID": "prj",
                "location": { "directory": "/work/new" },
                "subpath": ""
            }),
        ))
        .unwrap();
        let SessionChange::Created(session) = &change else {
            panic!("unexpected {change:?}");
        };
        assert_eq!(session.id, "ses_new");
        assert_eq!(session.directory, "/work/new");
        assert_eq!(session.title, "Untitled session");
        assert_eq!((session.time.created, session.time.updated), (500, 500));

        let mut sessions = vec![listed("ses_a", "A")];
        assert!(change.apply(&mut sessions));
        assert_eq!(sessions.len(), 2);
        assert!(!change.apply(&mut sessions), "a creation is idempotent");

        let mut known = vec![listed("ses_new", "Server title")];
        assert!(!change.apply(&mut known), "never replaces a known session");
        assert_eq!(known[0].title, "Server title");

        let child = SessionChange::from_event(&session_event(
            "session.created",
            json!({
                "sessionID": "ses_child",
                "parentID": "ses_a",
                "location": { "directory": "/work" }
            }),
        ))
        .unwrap();
        let mut sessions = vec![listed("ses_a", "A")];
        assert!(!child.apply(&mut sessions));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn session_rename_move_and_delete_events_update_the_list() {
        let mut sessions = vec![listed("ses_a", "A"), listed("ses_b", "B")];

        let renamed = SessionChange::from_event(&session_event(
            "session.renamed",
            json!({ "sessionID": "ses_a", "title": "Renamed" }),
        ))
        .unwrap();
        assert_eq!(renamed.session_id(), "ses_a");
        assert!(renamed.apply(&mut sessions));
        assert_eq!(sessions[0].title, "Renamed");
        assert!(!renamed.apply(&mut sessions));

        let moved = SessionChange::from_event(&session_event(
            "session.moved",
            json!({
                "sessionID": "ses_b",
                "location": { "directory": "/work/elsewhere" },
                "projectID": "prj"
            }),
        ))
        .unwrap();
        assert!(moved.apply(&mut sessions));
        assert_eq!(sessions[1].directory, "/work/elsewhere");

        let unknown = SessionChange::Renamed {
            id: "ses_missing".into(),
            title: "X".into(),
        };
        assert!(!unknown.apply(&mut sessions));

        let deleted = SessionChange::from_event(&session_event(
            "session.deleted",
            json!({ "sessionID": "ses_a" }),
        ))
        .unwrap();
        assert_eq!(deleted, SessionChange::Deleted("ses_a".into()));
        assert!(deleted.apply(&mut sessions));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_b");

        for ignored in [
            session_event("session.model.selected", json!({ "sessionID": "ses_b" })),
            session_event("session.renamed", json!({})),
            json!({ "type": "session.deleted", "data": { "sessionID": "ses_b" } }),
        ] {
            assert_eq!(SessionChange::from_event(&ignored), None, "{ignored}");
        }
    }

    #[test]
    fn session_model_selected_updates_the_saved_model() {
        let mut sessions = vec![listed("ses_a", "A")];
        let selected = SessionChange::from_event(&session_event(
            "session.model.selected",
            json!({
                "sessionID": "ses_a",
                "model": { "id": "alt", "providerID": "mock", "variant": "high" },
                "previous": { "id": "base", "providerID": "mock" }
            }),
        ))
        .unwrap();
        assert_eq!(selected.session_id(), "ses_a");
        assert!(selected.apply(&mut sessions));
        assert_eq!(
            sessions[0].model_selection(),
            Some(selection("mock", "alt", Some("high")))
        );
        assert!(!selected.apply(&mut sessions), "an echo changes nothing");
        assert!(
            !SessionChange::ModelSelected {
                id: "ses_missing".into(),
                model: SessionModel::from_selection(&selection("mock", "alt", None)),
            }
            .apply(&mut sessions)
        );
        assert_eq!(
            SessionChange::from_event(&session_event(
                "session.agent.selected",
                json!({ "sessionID": "ses_a", "agent": "plan" }),
            )),
            None,
            "agent selections are tolerated and ignored"
        );
    }

    #[test]
    fn captured_session_lifecycle_events_decode() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/v2-2.0.8/events/session-lifecycle.jsonl"
        );
        let text = std::fs::read_to_string(path).expect("captured session-lifecycle events");
        let mut sessions = Vec::new();
        let mut created = 0;
        let mut renamed = 0;
        let mut model_selected = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let payload: Value = serde_json::from_str(line).unwrap();
            match SessionChange::from_event(&payload) {
                Some(change @ SessionChange::Created(_)) => {
                    created += 1;
                    assert!(change.apply(&mut sessions));
                }
                Some(change @ SessionChange::Renamed { .. }) => {
                    renamed += 1;
                    assert!(change.apply(&mut sessions));
                }
                Some(change @ SessionChange::ModelSelected { .. }) => {
                    model_selected += 1;
                    assert!(change.apply(&mut sessions));
                }
                Some(other) => panic!("unexpected {other:?}"),
                None => {}
            }
        }
        assert!(created > 0 && renamed > 0 && model_selected > 0);
        let switched: protocol::SessionResponse =
            serde_json::from_value(fixture_body("session.get.modelSwitched")).unwrap();
        let switched = Session::from_info(&switched.data);
        assert_eq!(
            sessions
                .iter()
                .find(|session| session.id == switched.id)
                .and_then(|session| session.model.as_ref()),
            switched.model.as_ref(),
            "the event and the refetched session agree on the model"
        );
        assert!(
            sessions
                .iter()
                .all(|session| !session.directory.is_empty() && session.time.created > 0)
        );
        assert!(
            sessions
                .iter()
                .any(|session| session.title != "Untitled session")
        );
    }
}
