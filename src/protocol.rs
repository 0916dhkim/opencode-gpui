//! Typed wire layer for the OpenCode 2.0.8 HTTP API (`/api/...`).
//!
//! Only the surface the GTK client needs is modelled. Every type decodes
//! tolerantly: unknown fields are ignored, optional fields default, and the
//! growing unions (message entries, assistant content, events, statuses) keep
//! an `Unknown`/`Other` fallback so one new shape never fails a whole page.
//!
//! Response shapes differ per endpoint; there is no universal envelope:
//!
//! | endpoint | success | body |
//! |---|---|---|
//! | `GET /api/info` | 200 | bare [`ServerInfo`] |
//! | `GET /api/project` | 200 | bare `Vec<`[`ProjectInfo`]`>` |
//! | `GET /api/session` | 200 | [`Page`]`<`[`SessionInfo`]`>` |
//! | `POST /api/session`, `GET /api/session/{id}` | 200 | [`Data`]`<`[`SessionInfo`]`>` |
//! | `GET /api/session/active` | 200 | [`Data`]`<HashMap<id, `[`ActiveStatus`]`>>` |
//! | `PATCH /api/session/{id}` | 204 | — |
//! | `POST /api/session/{id}/model` | 204 | — |
//! | `POST /api/session/{id}/prompt` | 200 | [`Data`]`<`[`InboxUser`]`>` |
//! | `POST /api/session/{id}/interrupt` | 200 | bare [`Interrupted`] |
//! | `POST /api/session/{id}/background` | 204 | — (no request body or location query) |
//! | `GET /api/session/{id}/inbox` | 200 | [`Data`]`<Vec<`[`InboxEntry`]`>>` |
//! | `PATCH /api/session/{id}/inbox/{inboxID}` | 204 | — |
//! | `DELETE /api/session/{id}/inbox/{inboxID}` | 204 | — |
//! | `GET /api/session/{id}/message` | 200 | [`Page`]`<`[`SessionMessage`]`>` |
//! | `GET /api/model` | 200 | [`Located`]`<Vec<`[`ModelInfo`]`>>` |
//! | `GET /api/model/default` | 200 | [`ModelDefaultResponse`] |
//! | `GET /api/permission/request` | 200 | [`Located`]`<Vec<`[`PermissionRequest`]`>>` |
//! | `GET /api/session/{id}/permission` | 200 | [`Data`]`<Vec<`[`PermissionRequest`]`>>` |
//! | `POST /api/session/{id}/permission/{requestID}/reply` | 204 | — |
//! | `GET /api/form` | 200 | [`Located`]`<Vec<`[`FormInfo`]`>>` |
//! | `GET /api/session/{id}/form` | 200 | [`Data`]`<Vec<`[`FormInfo`]`>>` |
//! | `GET /api/session/{id}/form/{formID}` | 200 | [`Data`]`<`[`FormDetail`]`>` |
//! | `DELETE /api/session/{id}/form/{formID}` | 204 | — |
//! | `GET /api/event` | 200 | `text/event-stream` of [`Event`] |
//!
//! Declared failures carry an [`ApiError`] body (`{"_tag", "message", ...}`).
//!
//! Location-scoped routes (`/api/model`, `/api/model/default`, `/api/form`,
//! `/api/permission/request`, and the `"global"` form owner) read the
//! directory from the deepObject query key `location[directory]`, falling back
//! to the `x-opencode-directory` header and then the server's cwd. A plain
//! `?directory=` is silently ignored there. Use [`location_query`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;
/// Milliseconds since the Unix epoch.
pub type Millis = i64;

pub const API_PREFIX: &str = "/api";
/// Largest decoded attachment the server accepts (`core/src/session/prompt.ts` `MAX_ATTACHMENT_BYTES`).
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
/// Owner used by MCP elicitation forms that have no real session.
pub const GLOBAL_FORM_OWNER: &str = "global";

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}

/// `{data, cursor}` used by the session and message lists.
///
/// The server returns cursors whenever the page is non-empty, so a cursor does
/// not mean more items exist; stop on an empty page.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub cursor: Cursor,
}

impl<T> Page<T> {
    pub fn next_cursor(&self) -> Option<&str> {
        if self.data.is_empty() {
            None
        } else {
            self.cursor.next.as_deref()
        }
    }
}

/// `{data}` used by session get/create, active, prompt, inbox and per-session lists.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Data<T> {
    pub data: T,
}

/// `{location, data}` used by location-scoped lists.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Located<T> {
    #[serde(default)]
    pub location: LocationRef,
    pub data: T,
}

/// `GET /api/model/default`. The server schema is `UndefinedOr(Model.Info)`, so
/// `data` is omitted when no default exists (types.d.ts says `null`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ModelDefaultResponse {
    #[serde(default)]
    pub location: LocationRef,
    #[serde(default)]
    pub data: Option<ModelInfo>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Interrupted {
    pub interrupted: bool,
}

pub type ServerInfoResponse = ServerInfo;
pub type ProjectListResponse = Vec<ProjectInfo>;
pub type SessionListResponse = Page<SessionInfo>;
pub type SessionResponse = Data<SessionInfo>;
pub type SessionActiveResponse = Data<HashMap<String, ActiveStatus>>;
pub type PromptResponse = Data<InboxUser>;
pub type InterruptResponse = Interrupted;
pub type InboxListResponse = Data<Vec<InboxEntry>>;
pub type MessageListResponse = Page<SessionMessage>;
pub type ModelListResponse = Located<Vec<ModelInfo>>;
pub type PermissionRequestListResponse = Located<Vec<PermissionRequest>>;
pub type FormListResponse = Located<Vec<FormInfo>>;
pub type ShellListResponse = Located<Vec<ShellInfo>>;
// Routes the client does not call; decoded by the fixture tests only.
#[cfg(test)]
pub type SessionPermissionListResponse = Data<Vec<PermissionRequest>>;
#[cfg(test)]
pub type SessionFormListResponse = Data<Vec<FormInfo>>;
#[cfg(test)]
pub type FormDetailResponse = Data<FormDetail>;

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(body)
}

/// One recorded HTTP exchange, as stored in `tests/fixtures/v2-2.0.8/<name>.json`.
#[cfg(test)]
#[derive(Clone, Debug, Deserialize)]
pub struct CapturedExchange {
    pub name: String,
    pub request: CapturedRequest,
    pub response: CapturedResponse,
    #[serde(default)]
    pub note: Option<String>,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize)]
pub struct CapturedRequest {
    pub method: String,
    /// e.g. `/api/session/{sessionID}/message`.
    #[serde(rename = "pathTemplate")]
    pub path_template: String,
    /// Raw query string, without `?`.
    #[serde(default)]
    pub query: Option<String>,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize)]
pub struct CapturedResponse {
    pub status: u16,
    /// Parsed JSON body; `null` for empty bodies (204, and the bodiless 401).
    #[serde(default)]
    pub body: Value,
    /// A body that was not JSON, verbatim.
    #[serde(rename = "bodyText", default)]
    pub body_text: Option<String>,
}

#[cfg(test)]
impl CapturedExchange {
    pub fn decode_body<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.response.body)
    }

    pub fn error(&self) -> Option<ApiError> {
        ApiError::deserialize(&self.response.body)
            .ok()
            .filter(|error| error.tag.is_some() || error.message.is_some())
    }
}

// ---------------------------------------------------------------------------
// Shared value types
// ---------------------------------------------------------------------------

/// `LocationRef`; the public variant omits `workspaceID`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct LocationRef {
    #[serde(default)]
    pub directory: String,
    #[serde(rename = "workspaceID", default)]
    pub workspace_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LocationPublicRef {
    pub directory: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Token counts are `Schema.Finite` on the server, so they may be fractional.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct TokenUsage {
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache: CacheUsage,
}

impl TokenUsage {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.reasoning + self.cache.read + self.cache.write
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct CacheUsage {
    pub read: f64,
    pub write: f64,
}

/// `Session.StructuredError`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct StructuredError {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub status: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CreatedTime {
    #[serde(default)]
    pub created: Millis,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Succeeded,
    Failed,
    Interrupted,
    #[serde(other)]
    Unknown,
}

/// How a waiting inbox item is delivered: `steer` (the server default) joins
/// the running turn at its next step; `queue` waits for the run to end and
/// starts a new turn. The client only ever sends `Steer` or `Queue`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Steer,
    Queue,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Server, projects, sessions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub paths: ServerPaths,
}

impl ServerInfo {
    pub fn major_version(&self) -> Option<u32> {
        self.version
            .trim_start_matches('v')
            .split('.')
            .next()?
            .parse()
            .ok()
    }

    pub fn is_v2(&self) -> bool {
        self.major_version() == Some(2)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ServerPaths {
    #[serde(default)]
    pub tmp: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ProjectInfo {
    pub id: String,
    #[serde(default)]
    pub canonical: String,
    #[serde(default)]
    pub vcs: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon: Option<ProjectIcon>,
    #[serde(default)]
    pub time: Option<ProjectTime>,
    #[serde(default)]
    pub sandboxes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ProjectIcon {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "override", default)]
    pub override_: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ProjectTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub updated: Millis,
}

/// Public `Session.Info`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(rename = "projectID", default)]
    pub project_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: TokenUsage,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub time: SessionTime,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subpath: Option<String>,
    #[serde(default)]
    pub location: LocationRef,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

impl SessionInfo {
    pub fn directory(&self) -> &str {
        &self.location.directory
    }

    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct SessionTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub updated: Millis,
    #[serde(default)]
    pub idle: Option<Millis>,
    #[serde(default)]
    pub viewed: Option<Millis>,
    #[serde(default)]
    pub archived: Option<Millis>,
}

/// Entry of `GET /api/session/active`. Absence means no foreground run on this
/// server; background child work may still be running.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ActiveStatus {
    Running,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Inbox (queued input)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct UserPayload {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub files: Vec<StoredFile>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct SyntheticPayload {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

/// `Session.Inbox.User`, returned by prompt. `id` is the prompt `id`, the
/// inbox ID, and the user message ID once delivered.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InboxUser {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub payload: UserPayload,
    #[serde(default)]
    pub delivery: Option<Delivery>,
}

/// `Session.Inbox.Item`, carried by `session.inbox.enqueued`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InboxItem {
    User {
        #[serde(default)]
        payload: UserPayload,
        #[serde(default)]
        delivery: Option<Delivery>,
    },
    Synthetic {
        #[serde(default)]
        payload: SyntheticPayload,
        #[serde(default)]
        delivery: Option<Delivery>,
    },
    Compaction {
        #[serde(default)]
        delivery: Option<Delivery>,
    },
    Move {
        #[serde(default)]
        delivery: Option<Delivery>,
    },
    #[serde(other)]
    Unknown,
}

/// `Session.Inbox.Info`, one row of `GET /api/session/{id}/inbox`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InboxEntry {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(flatten)]
    pub item: InboxItem,
}

// ---------------------------------------------------------------------------
// Message entries
// ---------------------------------------------------------------------------

/// Stored `Prompt.FileAttachment`. Differs from the prompt input shape: the
/// server keeps base64 `data`, a detected `mime` and its `source`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct StoredFile {
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub source: Option<FileSource>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl StoredFile {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FileSource {
    Inline,
    Uri {
        #[serde(default)]
        uri: String,
    },
    #[serde(other)]
    Unknown,
}

/// A `type` this client does not know, or a known one whose body failed to decode.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownEntry {
    pub kind: String,
    pub id: Option<String>,
    pub error: Option<String>,
    pub raw: Value,
}

impl UnknownEntry {
    fn new(kind: String, raw: Value, error: Option<String>) -> Self {
        let id = raw.get("id").and_then(Value::as_str).map(str::to_owned);
        Self {
            kind,
            id,
            error,
            raw,
        }
    }
}

/// `Session.Message.Info`, one entry of the message list, decoded by `type`.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionMessage {
    AgentSwitched(AgentSwitchedMessage),
    ModelSwitched(ModelSwitchedMessage),
    LocationSwitched(LocationSwitchedMessage),
    User(UserMessage),
    Synthetic(NoteMessage),
    System(NoteMessage),
    Skill(SkillMessage),
    Shell(ShellMessage),
    Assistant(AssistantMessage),
    Compaction(CompactionMessage),
    Idle(IdleMessage),
    Unknown(UnknownEntry),
}

impl<'de> Deserialize<'de> for SessionMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

impl SessionMessage {
    pub fn from_value(value: Value) -> Self {
        let kind = type_tag(&value, "type");
        let decoded = match kind.as_str() {
            "agent-switched" => from_ref(&value).map(Self::AgentSwitched),
            "model-switched" => from_ref(&value).map(Self::ModelSwitched),
            "location-switched" => from_ref(&value).map(Self::LocationSwitched),
            "user" => from_ref(&value).map(Self::User),
            "synthetic" => from_ref(&value).map(Self::Synthetic),
            "system" => from_ref(&value).map(Self::System),
            "skill" => from_ref(&value).map(Self::Skill),
            "shell" => from_ref(&value).map(Self::Shell),
            "assistant" => from_ref(&value).map(Self::Assistant),
            "compaction" => from_ref(&value).map(Self::Compaction),
            "idle" => from_ref(&value).map(Self::Idle),
            _ => return Self::Unknown(UnknownEntry::new(kind, value, None)),
        };
        decoded.unwrap_or_else(|error| {
            Self::Unknown(UnknownEntry::new(kind, value, Some(error.to_string())))
        })
    }

    #[cfg(test)]
    pub fn id(&self) -> Option<&str> {
        Some(match self {
            Self::AgentSwitched(message) => &message.id,
            Self::ModelSwitched(message) => &message.id,
            Self::LocationSwitched(message) => &message.id,
            Self::User(message) => &message.id,
            Self::Synthetic(message) | Self::System(message) => &message.id,
            Self::Skill(message) => &message.id,
            Self::Shell(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::Compaction(message) => &message.id,
            Self::Idle(message) => &message.id,
            Self::Unknown(entry) => return entry.id.as_deref(),
        })
    }

    #[cfg(test)]
    pub fn kind(&self) -> &str {
        match self {
            Self::AgentSwitched(_) => "agent-switched",
            Self::ModelSwitched(_) => "model-switched",
            Self::LocationSwitched(_) => "location-switched",
            Self::User(_) => "user",
            Self::Synthetic(_) => "synthetic",
            Self::System(_) => "system",
            Self::Skill(_) => "skill",
            Self::Shell(_) => "shell",
            Self::Assistant(_) => "assistant",
            Self::Compaction(_) => "compaction",
            Self::Idle(_) => "idle",
            Self::Unknown(entry) => &entry.kind,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AgentSwitchedMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub previous: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelSwitchedMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    pub model: ModelRef,
    #[serde(default)]
    pub previous: Option<ModelRef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct LocationSwitchedMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub location: LocationRef,
    #[serde(rename = "projectID", default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub subpath: Option<String>,
    #[serde(default)]
    pub previous: Option<PreviousLocation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PreviousLocation {
    #[serde(default)]
    pub location: LocationRef,
    #[serde(rename = "projectID", default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub subpath: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct UserMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub files: Vec<StoredFile>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

/// `synthetic` and `system` entries share this shape.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct NoteMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SkillMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    #[serde(default)]
    pub skill: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShellStatus {
    Running,
    Exited,
    Timeout,
    Killed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ShellOutput {
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ShellMessageTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub completed: Option<Millis>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellMessage {
    pub id: String,
    #[serde(default)]
    pub time: ShellMessageTime,
    #[serde(rename = "shellID", default)]
    pub shell_id: String,
    #[serde(default)]
    pub command: String,
    pub status: ShellStatus,
    /// `Schema.Number`: a number, or `"Infinity"`/`"-Infinity"`/`"NaN"`.
    #[serde(default)]
    pub exit: Option<Value>,
    #[serde(default)]
    pub output: Option<ShellOutput>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AssistantTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub streamed: Option<Millis>,
    #[serde(default)]
    pub completed: Option<Millis>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AssistantRetry {
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub at: Millis,
    #[serde(default)]
    pub error: StructuredError,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub id: String,
    #[serde(default)]
    pub time: AssistantTime,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub content: Vec<AssistantContent>,
    #[serde(default)]
    pub finish: Option<String>,
    #[serde(rename = "rawFinish", default)]
    pub raw_finish: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
    #[serde(default)]
    pub error: Option<StructuredError>,
    #[serde(default)]
    pub retry: Option<AssistantRetry>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompactionStatus {
    Running,
    Completed,
    Failed,
    #[serde(other)]
    Unknown,
}

/// The `compaction` entry; the three server variants are merged and told apart by `status`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CompactionMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    pub status: CompactionStatus,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub recent: String,
    #[serde(default)]
    pub error: Option<StructuredError>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct IdleMessage {
    pub id: String,
    #[serde(default)]
    pub time: CreatedTime,
    pub outcome: Outcome,
}

// ---------------------------------------------------------------------------
// Assistant content
// ---------------------------------------------------------------------------

/// One item of `assistant.content[]`, also the payload of
/// `session.message.content.updated`. Text and reasoning items carry no ID;
/// the live `ordinal` is the item's index among items of the same kind.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantContent {
    Text(AssistantText),
    Reasoning(AssistantReasoning),
    Tool(ToolCall),
    Unknown(UnknownEntry),
}

impl<'de> Deserialize<'de> for AssistantContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

impl AssistantContent {
    pub fn from_value(value: Value) -> Self {
        let kind = type_tag(&value, "type");
        let decoded = match kind.as_str() {
            "text" => from_ref(&value).map(Self::Text),
            "reasoning" => from_ref(&value).map(Self::Reasoning),
            "tool" => from_ref(&value).map(Self::Tool),
            _ => return Self::Unknown(UnknownEntry::new(kind, value, None)),
        };
        decoded.unwrap_or_else(|error| {
            Self::Unknown(UnknownEntry::new(kind, value, Some(error.to_string())))
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AssistantText {
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ReasoningTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub completed: Option<Millis>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AssistantReasoning {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub time: Option<ReasoningTime>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ToolTime {
    #[serde(default)]
    pub created: Millis,
    #[serde(default)]
    pub ran: Option<Millis>,
    #[serde(default)]
    pub completed: Option<Millis>,
}

/// `Session.Message.Assistant.Tool`. `id` is the provider call ID; key rows by
/// assistant message ID + `id` (the server matches the latest item with that `id`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub executed: Option<bool>,
    pub state: ToolState,
    #[serde(default)]
    pub time: ToolTime,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    /// Raw, still-streaming JSON input text.
    Streaming {
        #[serde(default)]
        input: String,
    },
    Running {
        #[serde(default)]
        input: JsonMap,
        #[serde(default)]
        metadata: JsonMap,
    },
    Completed {
        #[serde(default)]
        input: JsonMap,
        #[serde(default)]
        content: Vec<ToolContent>,
        #[serde(default)]
        metadata: Option<JsonMap>,
    },
    Error {
        #[serde(default)]
        input: JsonMap,
        #[serde(default)]
        error: StructuredError,
        #[serde(default)]
        content: Vec<ToolContent>,
        #[serde(default)]
        metadata: Option<JsonMap>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    Text {
        #[serde(default)]
        text: String,
    },
    File {
        #[serde(default)]
        uri: String,
        #[serde(default)]
        mime: String,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Alpha,
    Beta,
    Deprecated,
    Active,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

impl Default for ModelCapabilities {
    /// Mirrors `Model.Capabilities.default()` in `schema/src/model.ts`.
    fn default() -> Self {
        Self {
            tools: true,
            input: vec!["text".into(), "image".into()],
            output: vec!["text".into()],
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ModelVariant {
    pub id: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ModelLimit {
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub input: Option<u64>,
    #[serde(default)]
    pub output: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// Catalog ID; use this for [`ModelRef::id`], not `modelID`.
    pub id: String,
    /// Upstream provider model name.
    #[serde(rename = "modelID", default)]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub status: Option<ModelStatus>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub variants: Vec<ModelVariant>,
    #[serde(default)]
    pub limit: ModelLimit,
}

impl ModelInfo {
    pub fn accepts_input(&self, modality: &str) -> bool {
        self.capabilities.input.iter().any(|item| item == modality)
    }
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Permissions and forms
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PermissionSource {
    Tool {
        #[serde(rename = "messageID", default)]
        message_id: String,
        #[serde(default)]
        id: String,
    },
    #[serde(other)]
    Unknown,
}

/// `Permission.Request`; also the `permission.asked` event payload. `session_id`
/// may be a child session absent from the root session list; reply through it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub save: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
    #[serde(default)]
    pub source: Option<PermissionSource>,
    #[serde(default)]
    pub message: Option<String>,
}

impl PermissionRequest {
    pub fn offers_always(&self) -> bool {
        self.save.as_ref().is_some_and(|save| !save.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

/// `Form.Info`. GTK only shows a notice with Cancel, so fields stay raw JSON.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FormInfo {
    pub id: String,
    /// A session ID, or [`GLOBAL_FORM_OWNER`] for MCP elicitations.
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
    #[serde(default)]
    pub fields: Vec<Value>,
}

impl FormInfo {
    pub fn is_global(&self) -> bool {
        self.session_id == GLOBAL_FORM_OWNER
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FormDetail {
    #[serde(flatten)]
    pub info: FormInfo,
    pub state: FormState,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum FormState {
    Pending,
    Answered {
        #[serde(default)]
        answer: JsonMap,
    },
    Cancelled,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Body of declared 4xx/5xx responses: an Effect `TaggedError` encoded as
/// `{"_tag": "...", "message": "...", ...fields}`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ApiError {
    #[serde(rename = "_tag", default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 400; schema rejections carry `kind`, attachment errors `field: "files"`.
    InvalidRequest,
    /// 400.
    InvalidCursor,
    /// 401.
    Unauthorized,
    /// 403.
    Forbidden,
    /// 404.
    SessionNotFound,
    /// 404.
    MessageNotFound,
    /// 404; also when the request belongs to another session.
    PermissionNotFound,
    /// 404; also when the form belongs to another session.
    FormNotFound,
    /// 409.
    FormAlreadySettled,
    /// 400.
    FormInvalidAnswer,
    /// 409; e.g. a prompt `id` reused for a different record.
    Conflict,
    /// 409.
    SessionBusy,
    /// 503.
    ServiceUnavailable,
    /// 500; carries a server log `ref`.
    Unknown,
    Other,
}

impl ApiError {
    pub fn kind(&self) -> ApiErrorKind {
        match self.tag.as_deref() {
            Some("InvalidRequestError") => ApiErrorKind::InvalidRequest,
            Some("InvalidCursorError") => ApiErrorKind::InvalidCursor,
            Some("UnauthorizedError") => ApiErrorKind::Unauthorized,
            Some("ForbiddenError") => ApiErrorKind::Forbidden,
            Some("SessionNotFoundError") => ApiErrorKind::SessionNotFound,
            Some("MessageNotFoundError") => ApiErrorKind::MessageNotFound,
            Some("PermissionNotFoundError") => ApiErrorKind::PermissionNotFound,
            Some("FormNotFoundError") => ApiErrorKind::FormNotFound,
            Some("FormAlreadySettledError") => ApiErrorKind::FormAlreadySettled,
            Some("FormInvalidAnswerError") => ApiErrorKind::FormInvalidAnswer,
            Some("ConflictError") => ApiErrorKind::Conflict,
            Some("SessionBusyError") => ApiErrorKind::SessionBusy,
            Some("ServiceUnavailableError") => ApiErrorKind::ServiceUnavailable,
            Some("UnknownError") => ApiErrorKind::Unknown,
            _ => ApiErrorKind::Other,
        }
    }

    /// The target request/form is already gone; reconcile instead of failing.
    pub fn is_already_resolved(&self) -> bool {
        matches!(
            self.kind(),
            ApiErrorKind::PermissionNotFound
                | ApiErrorKind::FormNotFound
                | ApiErrorKind::FormAlreadySettled
        )
    }

    #[cfg(test)]
    pub fn field(&self) -> Option<&str> {
        self.extra.get("field").and_then(Value::as_str)
    }

    pub fn display_message(&self) -> &str {
        self.message
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or("Request failed")
    }
}

pub fn decode_error_body(body: &[u8]) -> Option<ApiError> {
    let error: ApiError = serde_json::from_slice(body).ok()?;
    (error.tag.is_some() || error.message.is_some()).then_some(error)
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Durable log position of durable events.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Durable {
    #[serde(rename = "aggregateID", default)]
    pub aggregate_id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub version: u32,
}

/// One event from `GET /api/event`.
///
/// SSE framing (`server/src/handlers/event.ts`, `server/src/event-feed.ts`):
/// every event is a single `data: <compact JSON>\n\n` frame with no `event:`,
/// `id:` or `retry:` fields; `: heartbeat\n\n` comments arrive every 15 s. The
/// first frame is `server.connected` with only `{id, type, data}` (no
/// `created`). There is no replay and `Last-Event-ID` is ignored; a consumer
/// more than 4096 frames behind has its stream failed. Frames are the
/// in-process payload passed to `JSON.stringify`, not schema-encoded.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Event {
    pub id: String,
    #[serde(default)]
    pub created: Option<Millis>,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub location: Option<LocationRef>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub durable: Option<Durable>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

impl Event {
    pub fn directory(&self) -> Option<&str> {
        self.location
            .as_ref()
            .map(|location| location.directory.as_str())
    }

    /// ID of the message entry this event creates, for events projected with
    /// `SessionMessage.ID.fromEvent` (switches, synthetic, system, skill, shell,
    /// idle, and compaction without `inputID`).
    pub fn projected_message_id(&self) -> Option<String> {
        message_id_from_event_id(&self.id)
    }
}

#[cfg(test)]
pub fn parse_event(json: &str) -> Result<Event, serde_json::Error> {
    serde_json::from_str(json)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionRef {
    #[serde(rename = "sessionID")]
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SessionCreated {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "projectID", default)]
    pub project_id: String,
    #[serde(default)]
    pub location: LocationRef,
    #[serde(default)]
    pub subpath: Option<String>,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionRenamed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionModelSelected {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub model: ModelRef,
    #[serde(default)]
    pub previous: Option<ModelRef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionAgentSelected {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub previous: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionMoved {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub location: LocationRef,
    #[serde(rename = "projectID", default)]
    pub project_id: String,
    #[serde(default)]
    pub subpath: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionViewed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub idle: Millis,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SessionUsage {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: TokenUsage,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InboxEnqueued {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
    pub item: InboxItem,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InboxRef {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InboxDeliveryChanged {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
    pub delivery: Delivery,
}

/// `session.{text,reasoning}.started`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FragmentStarted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: u32,
}

/// `session.{text,reasoning}.delta`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FragmentDelta {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: u32,
    #[serde(default)]
    pub delta: String,
}

/// `session.{text,reasoning}.ended`; `text` is the full final text.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FragmentEnded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: u32,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ToolInputStarted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ToolInputDelta {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub delta: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ToolInputEnded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ToolCalled {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub input: JsonMap,
    #[serde(default)]
    pub executed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ToolProgress {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub metadata: JsonMap,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ToolSuccess {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
    #[serde(default)]
    pub executed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ToolFailed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    #[serde(default)]
    pub error: StructuredError,
    #[serde(default)]
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
    #[serde(default)]
    pub executed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct StepStarted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub started: Millis,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct StepEnded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    #[serde(default)]
    pub finish: String,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: TokenUsage,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct StepFailed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    #[serde(default)]
    pub error: StructuredError,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ExecutionFailed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub error: StructuredError,
}

/// `reason` is `user`, `shutdown`, `superseded` or `inactivity`. A `shutdown`
/// interruption records no idle entry; the resumed run continues the turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ExecutionInterrupted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RetryScheduled {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub at: Millis,
    #[serde(default)]
    pub error: StructuredError,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct MessageContentUpdated {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(default)]
    pub content: Vec<AssistantContent>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SyntheticAdded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SkillActivated {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub text: String,
}

/// Produces a `system` entry only when `text` is present.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InstructionsUpdated {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellInfo {
    pub id: String,
    pub status: ShellStatus,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub exit: Option<f64>,
    #[serde(default)]
    pub metadata: JsonMap,
    #[serde(default)]
    pub time: ShellTime,
}

impl ShellInfo {
    /// The session that started the command (`metadata.sessionID`, set by
    /// the `shell` tool; possibly a child session).
    pub fn session_id(&self) -> Option<&str> {
        self.metadata
            .get("sessionID")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ShellTime {
    #[serde(default)]
    pub started: Millis,
    #[serde(default)]
    pub completed: Option<Millis>,
}

/// `shell.created`: `data.info` is the new command, normally `running`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellCreated {
    pub info: ShellInfo,
}

/// `shell.exited`: the command reached a terminal status.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellExited {
    pub id: String,
    #[serde(default)]
    pub exit: Option<f64>,
    pub status: ShellStatus,
}

/// `shell.deleted`: the command and its output were removed.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ShellRef {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellStarted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub shell: ShellInfo,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShellEnded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub shell: ShellInfo,
    #[serde(default)]
    pub output: ShellOutput,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CompactionStarted {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub recent: String,
    #[serde(rename = "inputID", default)]
    pub input_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CompactionDelta {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CompactionEnded {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub recent: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CompactionFailed {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub error: StructuredError,
    #[serde(rename = "inputID", default)]
    pub input_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry {
        #[serde(default)]
        attempt: u32,
        #[serde(default)]
        message: String,
        #[serde(default)]
        next: Millis,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SessionStatusChanged {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PermissionReplied {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
    /// `once`, `always` or `reject`.
    #[serde(default)]
    pub reply: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FormCreated {
    pub form: FormInfo,
}

/// `form.replied` and `form.cancelled` (the latter has no `answer`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FormSettled {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub answer: Option<JsonMap>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorktreeUpdated {
    #[serde(rename = "projectID")]
    pub project_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorktreeResolved {
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub previous: String,
}

/// Events that only mean "refetch the model/agent/config catalogs".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogChange {
    Provider,
    Model,
    ModelsDev,
    Agent,
    Config,
    Integration,
    Credential,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    ServerConnected,
    SessionCreated(SessionCreated),
    SessionRenamed(SessionRenamed),
    SessionDeleted(SessionRef),
    SessionModelSelected(SessionModelSelected),
    SessionAgentSelected(SessionAgentSelected),
    SessionMoved(SessionMoved),
    SessionViewed(SessionViewed),
    SessionUsageUpdated(SessionUsage),
    InboxEnqueued(InboxEnqueued),
    InboxDelivered(InboxRef),
    InboxCancelled(InboxRef),
    InboxDeliveryChanged(InboxDeliveryChanged),
    TextStarted(FragmentStarted),
    TextDelta(FragmentDelta),
    TextEnded(FragmentEnded),
    ReasoningStarted(FragmentStarted),
    ReasoningDelta(FragmentDelta),
    ReasoningEnded(FragmentEnded),
    ToolInputStarted(ToolInputStarted),
    ToolInputDelta(ToolInputDelta),
    ToolInputEnded(ToolInputEnded),
    ToolCalled(ToolCalled),
    ToolProgress(ToolProgress),
    ToolSuccess(ToolSuccess),
    ToolFailed(ToolFailed),
    StepStarted(StepStarted),
    StepEnded(StepEnded),
    StepFailed(StepFailed),
    ExecutionStarted(SessionRef),
    ExecutionSucceeded(SessionRef),
    ExecutionFailed(ExecutionFailed),
    ExecutionInterrupted(ExecutionInterrupted),
    RetryScheduled(RetryScheduled),
    MessageContentUpdated(MessageContentUpdated),
    Synthetic(SyntheticAdded),
    SkillActivated(SkillActivated),
    InstructionsUpdated(InstructionsUpdated),
    ShellStarted(ShellStarted),
    ShellEnded(ShellEnded),
    /// `shell.created|exited|deleted`: location-scoped shell commands
    /// (`/api/shell`), not session-scoped.
    ShellCreated(ShellCreated),
    ShellExited(ShellExited),
    ShellDeleted(ShellRef),
    CompactionStarted(CompactionStarted),
    CompactionDelta(CompactionDelta),
    CompactionEnded(CompactionEnded),
    CompactionFailed(CompactionFailed),
    SessionStatus(SessionStatusChanged),
    SessionIdle(SessionRef),
    PermissionAsked(PermissionRequest),
    PermissionReplied(PermissionReplied),
    FormCreated(FormCreated),
    FormReplied(FormSettled),
    FormCancelled(FormSettled),
    LocationShutdown,
    ProjectUpdated(ProjectInfo),
    WorktreeUpdated(WorktreeUpdated),
    WorktreeResolved(WorktreeResolved),
    CatalogChanged(CatalogChange),
    /// An event type this client ignores.
    Other(String),
    /// A handled type whose `data` did not decode.
    Malformed {
        type_: String,
        error: String,
    },
}

pub fn decode_event(event: &Event) -> EventKind {
    fn typed<T: DeserializeOwned>(event: &Event, wrap: impl FnOnce(T) -> EventKind) -> EventKind {
        match T::deserialize(&event.data) {
            Ok(data) => wrap(data),
            Err(error) => EventKind::Malformed {
                type_: event.type_.clone(),
                error: error.to_string(),
            },
        }
    }

    match event.type_.as_str() {
        "server.connected" => EventKind::ServerConnected,
        "session.created" => typed(event, EventKind::SessionCreated),
        "session.renamed" => typed(event, EventKind::SessionRenamed),
        "session.deleted" => typed(event, EventKind::SessionDeleted),
        "session.model.selected" => typed(event, EventKind::SessionModelSelected),
        "session.agent.selected" => typed(event, EventKind::SessionAgentSelected),
        "session.moved" => typed(event, EventKind::SessionMoved),
        "session.viewed" => typed(event, EventKind::SessionViewed),
        "session.usage.updated" => typed(event, EventKind::SessionUsageUpdated),
        "session.inbox.enqueued" => typed(event, EventKind::InboxEnqueued),
        "session.inbox.delivered" => typed(event, EventKind::InboxDelivered),
        "session.inbox.cancelled" => typed(event, EventKind::InboxCancelled),
        "session.inbox.delivery.changed" => typed(event, EventKind::InboxDeliveryChanged),
        "session.text.started" => typed(event, EventKind::TextStarted),
        "session.text.delta" => typed(event, EventKind::TextDelta),
        "session.text.ended" => typed(event, EventKind::TextEnded),
        "session.reasoning.started" => typed(event, EventKind::ReasoningStarted),
        "session.reasoning.delta" => typed(event, EventKind::ReasoningDelta),
        "session.reasoning.ended" => typed(event, EventKind::ReasoningEnded),
        "session.tool.input.started" => typed(event, EventKind::ToolInputStarted),
        "session.tool.input.delta" => typed(event, EventKind::ToolInputDelta),
        "session.tool.input.ended" => typed(event, EventKind::ToolInputEnded),
        "session.tool.called" => typed(event, EventKind::ToolCalled),
        "session.tool.progress" => typed(event, EventKind::ToolProgress),
        "session.tool.success" => typed(event, EventKind::ToolSuccess),
        "session.tool.failed" => typed(event, EventKind::ToolFailed),
        "session.step.started" => typed(event, EventKind::StepStarted),
        "session.step.ended" => typed(event, EventKind::StepEnded),
        "session.step.failed" => typed(event, EventKind::StepFailed),
        "session.execution.started" => typed(event, EventKind::ExecutionStarted),
        "session.execution.succeeded" => typed(event, EventKind::ExecutionSucceeded),
        "session.execution.failed" => typed(event, EventKind::ExecutionFailed),
        "session.execution.interrupted" => typed(event, EventKind::ExecutionInterrupted),
        "session.retry.scheduled" => typed(event, EventKind::RetryScheduled),
        "session.message.content.updated" => typed(event, EventKind::MessageContentUpdated),
        "session.synthetic" => typed(event, EventKind::Synthetic),
        "session.skill.activated" => typed(event, EventKind::SkillActivated),
        "session.instructions.updated" => typed(event, EventKind::InstructionsUpdated),
        "session.shell.started" => typed(event, EventKind::ShellStarted),
        "session.shell.ended" => typed(event, EventKind::ShellEnded),
        "shell.created" => typed(event, EventKind::ShellCreated),
        "shell.exited" => typed(event, EventKind::ShellExited),
        "shell.deleted" => typed(event, EventKind::ShellDeleted),
        "session.compaction.started" => typed(event, EventKind::CompactionStarted),
        "session.compaction.delta" => typed(event, EventKind::CompactionDelta),
        "session.compaction.ended" => typed(event, EventKind::CompactionEnded),
        "session.compaction.failed" => typed(event, EventKind::CompactionFailed),
        "session.status" => typed(event, EventKind::SessionStatus),
        "session.idle" => typed(event, EventKind::SessionIdle),
        "permission.asked" => typed(event, EventKind::PermissionAsked),
        "permission.replied" => typed(event, EventKind::PermissionReplied),
        "form.created" => typed(event, EventKind::FormCreated),
        "form.replied" => typed(event, EventKind::FormReplied),
        "form.cancelled" => typed(event, EventKind::FormCancelled),
        "location.shutdown" => EventKind::LocationShutdown,
        "project.updated" => typed(event, EventKind::ProjectUpdated),
        "worktree.updated" => typed(event, EventKind::WorktreeUpdated),
        "worktree.resolved" => typed(event, EventKind::WorktreeResolved),
        "provider.updated" => EventKind::CatalogChanged(CatalogChange::Provider),
        "model.updated" => EventKind::CatalogChanged(CatalogChange::Model),
        "models-dev.refreshed" => EventKind::CatalogChanged(CatalogChange::ModelsDev),
        "agent.updated" => EventKind::CatalogChanged(CatalogChange::Agent),
        "config.updated" => EventKind::CatalogChanged(CatalogChange::Config),
        "integration.updated" => EventKind::CatalogChanged(CatalogChange::Integration),
        "credential.updated" | "credential.switched" => {
            EventKind::CatalogChanged(CatalogChange::Credential)
        }
        other => EventKind::Other(other.to_owned()),
    }
}

impl EventKind {
    /// Session the event belongs to, when it is session-scoped.
    pub fn session_id(&self) -> Option<&str> {
        Some(match self {
            Self::SessionCreated(data) => &data.session_id,
            Self::SessionRenamed(data) => &data.session_id,
            Self::SessionDeleted(data)
            | Self::ExecutionStarted(data)
            | Self::ExecutionSucceeded(data)
            | Self::SessionIdle(data) => &data.session_id,
            Self::SessionModelSelected(data) => &data.session_id,
            Self::SessionAgentSelected(data) => &data.session_id,
            Self::SessionMoved(data) => &data.session_id,
            Self::SessionViewed(data) => &data.session_id,
            Self::SessionUsageUpdated(data) => &data.session_id,
            Self::InboxEnqueued(data) => &data.session_id,
            Self::InboxDelivered(data) | Self::InboxCancelled(data) => &data.session_id,
            Self::InboxDeliveryChanged(data) => &data.session_id,
            Self::TextStarted(data) | Self::ReasoningStarted(data) => &data.session_id,
            Self::TextDelta(data) | Self::ReasoningDelta(data) => &data.session_id,
            Self::TextEnded(data) | Self::ReasoningEnded(data) => &data.session_id,
            Self::ToolInputStarted(data) => &data.session_id,
            Self::ToolInputDelta(data) => &data.session_id,
            Self::ToolInputEnded(data) => &data.session_id,
            Self::ToolCalled(data) => &data.session_id,
            Self::ToolProgress(data) => &data.session_id,
            Self::ToolSuccess(data) => &data.session_id,
            Self::ToolFailed(data) => &data.session_id,
            Self::StepStarted(data) => &data.session_id,
            Self::StepEnded(data) => &data.session_id,
            Self::StepFailed(data) => &data.session_id,
            Self::ExecutionFailed(data) => &data.session_id,
            Self::ExecutionInterrupted(data) => &data.session_id,
            Self::RetryScheduled(data) => &data.session_id,
            Self::MessageContentUpdated(data) => &data.session_id,
            Self::Synthetic(data) => &data.session_id,
            Self::SkillActivated(data) => &data.session_id,
            Self::InstructionsUpdated(data) => &data.session_id,
            Self::ShellStarted(data) => &data.session_id,
            Self::ShellEnded(data) => &data.session_id,
            Self::CompactionStarted(data) => &data.session_id,
            Self::CompactionDelta(data) => &data.session_id,
            Self::CompactionEnded(data) => &data.session_id,
            Self::CompactionFailed(data) => &data.session_id,
            Self::SessionStatus(data) => &data.session_id,
            Self::PermissionAsked(data) => &data.session_id,
            Self::PermissionReplied(data) => &data.session_id,
            Self::FormCreated(data) => &data.form.session_id,
            Self::FormReplied(data) | Self::FormCancelled(data) => &data.session_id,
            Self::ServerConnected
            | Self::LocationShutdown
            | Self::ShellCreated(_)
            | Self::ShellExited(_)
            | Self::ShellDeleted(_)
            | Self::ProjectUpdated(_)
            | Self::WorktreeUpdated(_)
            | Self::WorktreeResolved(_)
            | Self::CatalogChanged(_)
            | Self::Other(_)
            | Self::Malformed { .. } => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// `POST /api/session`. Never carries `agent`; `model` is left to the server.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CreateSessionBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationPublicRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl CreateSessionBody {
    pub fn new(directory: impl Into<String>, title: Option<String>) -> Self {
        Self {
            location: Some(LocationPublicRef {
                directory: directory.into(),
            }),
            title: title.filter(|title| !title.trim().is_empty()),
        }
    }
}

/// `PATCH /api/session/{id}`. An empty title asks the server to regenerate
/// one, so [`RenameSessionBody::new`] refuses blank titles.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RenameSessionBody {
    pub title: String,
}

impl RenameSessionBody {
    pub fn new(title: &str) -> Option<Self> {
        let title = title.trim();
        (!title.is_empty()).then(|| Self {
            title: title.to_owned(),
        })
    }
}

/// `PromptInput.FileAttachment`. Send `data:` URIs only; `file:` URIs resolve on the server host.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PromptFile {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl PromptFile {
    /// The server requires canonical padded base64 and re-detects the MIME
    /// type from the bytes, ignoring the data URL's declared type.
    pub fn from_bytes(bytes: &[u8], mime: &str, name: Option<String>) -> Self {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self {
            uri: format!("data:{mime};base64,{data}"),
            name,
        }
    }
}

/// `POST /api/session/{id}/prompt`. Never carries `agent`, `agents` or `resume`.
///
/// `id` must start with `msg_`. Re-posting the same `id` reconciles with the
/// already-admitted input instead of duplicating it. `delivery` is only ever
/// `"queue"`; a steered prompt omits it (the server default).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PromptBody {
    pub id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PromptFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

/// `PATCH /api/session/{id}/inbox/{inboxID}` (204). A conditional switch to
/// the other mode: 409 when the item is no longer waiting or already has
/// this mode. Switching to `steer` wakes the session.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InboxUpdateBody {
    pub delivery: Delivery,
}

/// `POST /api/session/{id}/model`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SwitchModelBody {
    pub model: ModelRef,
}

/// `POST /api/session/{sessionID}/permission/{requestID}/reply`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PermissionReplyBody {
    pub decision: PermissionDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Builds a `msg_` ID in the server's ascending format: 12 hex digits of
/// `timestamp_ms * 0x1000 + counter`, then 14 base62 characters.
pub fn message_id_at(timestamp_ms: u64, counter: u16, entropy: u64) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let value = (u128::from(timestamp_ms) << 12) + u128::from(counter);
    let mut id = String::with_capacity(30);
    id.push_str("msg_");
    for index in 0..6 {
        let byte = (value >> (40 - 8 * index)) & 0xff;
        let _ = write!(id, "{byte:02x}");
    }
    let mut state = entropy;
    for _ in 0..14 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        id.push(CHARS[((state >> 33) % 62) as usize] as char);
    }
    id
}

/// Like the server's `Identifier.ascending`: calls within one millisecond bump
/// the counter, and a clock stepping backwards never yields a smaller ID.
pub fn new_message_id() -> String {
    static LAST: std::sync::Mutex<(u64, u16)> = std::sync::Mutex::new((0, 0));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let (timestamp, counter) = {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now > last.0 {
            *last = (now, 1);
        } else if last.1 >= 0xfff {
            *last = (last.0 + 1, 1);
        } else {
            last.1 += 1;
        }
        *last
    };
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(timestamp);
    hasher.write_u16(counter);
    message_id_at(timestamp, counter, hasher.finish())
}

/// `SessionMessage.ID.fromEvent`: `evt_X` becomes `msg_X`.
pub fn message_id_from_event_id(event_id: &str) -> Option<String> {
    event_id
        .strip_prefix("evt_")
        .map(|rest| format!("msg_{rest}"))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[cfg(test)]
pub struct EndpointSpec {
    pub name: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    pub success_status: u16,
}

#[cfg(test)]
const fn endpoint(
    name: &'static str,
    method: &'static str,
    path: &'static str,
    success_status: u16,
) -> EndpointSpec {
    EndpointSpec {
        name,
        method,
        path,
        success_status,
    }
}

/// Endpoints the client uses (plus the per-session list and form routes the
/// fixtures cover), with their success status (from `client.js`).
#[cfg(test)]
pub const ENDPOINTS: &[EndpointSpec] = &[
    endpoint("server.info", "GET", "/api/info", 200),
    endpoint("project.list", "GET", "/api/project", 200),
    endpoint("session.list", "GET", "/api/session", 200),
    endpoint("session.create", "POST", "/api/session", 200),
    endpoint("session.active", "GET", "/api/session/active", 200),
    endpoint("session.get", "GET", "/api/session/{sessionID}", 200),
    endpoint("session.update", "PATCH", "/api/session/{sessionID}", 204),
    endpoint(
        "session.switchModel",
        "POST",
        "/api/session/{sessionID}/model",
        204,
    ),
    endpoint(
        "session.prompt",
        "POST",
        "/api/session/{sessionID}/prompt",
        200,
    ),
    endpoint(
        "session.interrupt",
        "POST",
        "/api/session/{sessionID}/interrupt",
        200,
    ),
    endpoint(
        "session.inbox.list",
        "GET",
        "/api/session/{sessionID}/inbox",
        200,
    ),
    endpoint(
        "session.inbox.update",
        "PATCH",
        "/api/session/{sessionID}/inbox/{inboxID}",
        204,
    ),
    endpoint(
        "session.inbox.cancel",
        "DELETE",
        "/api/session/{sessionID}/inbox/{inboxID}",
        204,
    ),
    endpoint(
        "message.list",
        "GET",
        "/api/session/{sessionID}/message",
        200,
    ),
    endpoint("model.list", "GET", "/api/model", 200),
    endpoint("model.default", "GET", "/api/model/default", 200),
    endpoint(
        "permission.request.list",
        "GET",
        "/api/permission/request",
        200,
    ),
    endpoint(
        "session.permission.list",
        "GET",
        "/api/session/{sessionID}/permission",
        200,
    ),
    endpoint(
        "session.permission.reply",
        "POST",
        "/api/session/{sessionID}/permission/{requestID}/reply",
        204,
    ),
    endpoint("form.list", "GET", "/api/form", 200),
    endpoint(
        "session.form.list",
        "GET",
        "/api/session/{sessionID}/form",
        200,
    ),
    endpoint(
        "session.form.get",
        "GET",
        "/api/session/{sessionID}/form/{formID}",
        200,
    ),
    endpoint(
        "session.form.cancel",
        "DELETE",
        "/api/session/{sessionID}/form/{formID}",
        204,
    ),
    endpoint("event.subscribe", "GET", "/api/event", 200),
];

#[cfg(test)]
pub fn endpoint_spec(name: &str) -> Option<&'static EndpointSpec> {
    ENDPOINTS.iter().find(|spec| spec.name == name)
}

/// Percent-encodes one path segment like JavaScript `encodeURIComponent`.
pub fn encode_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub fn info_path() -> String {
    format!("{API_PREFIX}/info")
}

pub fn projects_path() -> String {
    format!("{API_PREFIX}/project")
}

pub fn sessions_path() -> String {
    format!("{API_PREFIX}/session")
}

pub fn sessions_active_path() -> String {
    format!("{API_PREFIX}/session/active")
}

pub fn session_path(session_id: &str) -> String {
    format!("{API_PREFIX}/session/{}", encode_segment(session_id))
}

pub fn session_model_path(session_id: &str) -> String {
    format!("{}/model", session_path(session_id))
}

pub fn session_prompt_path(session_id: &str) -> String {
    format!("{}/prompt", session_path(session_id))
}

pub fn session_interrupt_path(session_id: &str) -> String {
    format!("{}/interrupt", session_path(session_id))
}

pub fn session_background_path(session_id: &str) -> String {
    format!("{}/background", session_path(session_id))
}

pub fn session_inbox_path(session_id: &str) -> String {
    format!("{}/inbox", session_path(session_id))
}

/// One waiting inbox item: `PATCH` switches its delivery, `DELETE` cancels it
/// (204 even when it was delivered meanwhile).
pub fn session_inbox_item_path(session_id: &str, inbox_id: &str) -> String {
    format!(
        "{}/inbox/{}",
        session_path(session_id),
        encode_segment(inbox_id)
    )
}

pub fn session_messages_path(session_id: &str) -> String {
    format!("{}/message", session_path(session_id))
}

/// `session_id` must be the request's own `sessionID` (possibly a child session).
pub fn permission_reply_path(session_id: &str, request_id: &str) -> String {
    format!(
        "{}/permission/{}/reply",
        session_path(session_id),
        encode_segment(request_id)
    )
}

pub fn permission_requests_path() -> String {
    format!("{API_PREFIX}/permission/request")
}

pub fn forms_path() -> String {
    format!("{API_PREFIX}/form")
}

/// Get (`GET`) or cancel (`DELETE`) one form. For the `"global"` owner also
/// send [`form_location_query`].
pub fn session_form_path(session_id: &str, form_id: &str) -> String {
    format!(
        "{}/form/{}",
        session_path(session_id),
        encode_segment(form_id)
    )
}

/// Running shell commands of one location (`location[directory]`).
pub fn shells_path() -> String {
    format!("{API_PREFIX}/shell")
}

pub fn models_path() -> String {
    format!("{API_PREFIX}/model")
}

pub fn model_default_path() -> String {
    format!("{API_PREFIX}/model/default")
}

pub fn events_path() -> String {
    format!("{API_PREFIX}/event")
}

pub type QueryPairs = Vec<(String, String)>;

/// `?a=b&c=d`, or an empty string when there are no pairs.
#[cfg(test)]
pub fn query_string(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut serializer = url::form_urlencoded::Serializer::for_suffix(String::from("?"), 1);
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// The deepObject location selector read by location-scoped routes.
pub fn location_query(directory: &str) -> (String, String) {
    ("location[directory]".to_owned(), directory.to_owned())
}

/// Location for a form route: only the `"global"` owner resolves its location
/// from the query; real sessions resolve it from the session.
pub fn form_location_query(session_id: &str, directory: &str) -> QueryPairs {
    if session_id == GLOBAL_FORM_OWNER {
        vec![location_query(directory)]
    } else {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ParentFilter {
    #[default]
    Any,
    /// `parentID=null`: root sessions only.
    Root,
}

/// `GET /api/session` query. A cursor encodes the original filters and order,
/// so a cursor page sends only `cursor` and `limit`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionListQuery {
    pub limit: Option<u32>,
    pub order: Option<Order>,
    pub parent: ParentFilter,
    pub directory: Option<String>,
    pub project: Option<String>,
    pub cursor: Option<String>,
}

impl SessionListQuery {
    pub fn roots() -> Self {
        Self {
            parent: ParentFilter::Root,
            ..Self::default()
        }
    }

    pub fn with_cursor(&self, cursor: &str) -> Self {
        Self {
            cursor: Some(cursor.to_owned()),
            ..self.clone()
        }
    }

    pub fn pairs(&self) -> QueryPairs {
        let mut pairs = QueryPairs::new();
        if let Some(limit) = self.limit {
            pairs.push(("limit".into(), limit.max(1).to_string()));
        }
        if let Some(cursor) = &self.cursor {
            pairs.push(("cursor".into(), cursor.clone()));
            return pairs;
        }
        if let Some(order) = self.order {
            pairs.push(("order".into(), order.as_str().into()));
        }
        match &self.parent {
            ParentFilter::Any => {}
            ParentFilter::Root => pairs.push(("parentID".into(), "null".into())),
        }
        if let Some(directory) = &self.directory {
            pairs.push(("directory".into(), directory.clone()));
        }
        if let Some(project) = &self.project {
            pairs.push(("project".into(), project.clone()));
        }
        pairs
    }
}

/// `GET /api/session/{id}/message` query. Default order is newest first; the
/// server rejects `order` together with `cursor`, so a cursor drops `order`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageListQuery {
    pub limit: Option<u32>,
    pub order: Option<Order>,
    pub cursor: Option<String>,
}

impl MessageListQuery {
    pub const MAX_LIMIT: u32 = 200;

    pub fn pairs(&self) -> QueryPairs {
        let mut pairs = QueryPairs::new();
        if let Some(limit) = self.limit {
            pairs.push(("limit".into(), limit.clamp(1, Self::MAX_LIMIT).to_string()));
        }
        match &self.cursor {
            Some(cursor) => pairs.push(("cursor".into(), cursor.clone())),
            None => {
                if let Some(order) = self.order {
                    pairs.push(("order".into(), order.as_str().into()));
                }
            }
        }
        pairs
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn type_tag(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn from_ref<T: DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
    T::deserialize(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn from<T: DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("decode")
    }

    fn event(value: Value) -> EventKind {
        decode_event(&from::<Event>(value))
    }

    #[test]
    fn server_info_is_a_bare_object() {
        let info: ServerInfo = from(json!({
            "version": "2.0.8",
            "pid": 4242,
            "urls": ["http://127.0.0.1:4096"],
            "paths": { "tmp": "/tmp/opencode" },
            "added": true
        }));
        assert_eq!(info.version, "2.0.8");
        assert_eq!(info.paths.tmp, "/tmp/opencode");
        assert!(info.is_v2());
        let v1: ServerInfo = from(json!({ "version": "1.18.29" }));
        assert!(!v1.is_v2());
    }

    #[test]
    fn project_list_is_a_bare_array() {
        let projects: ProjectListResponse = from(json!([
            {
                "id": "prj_1",
                "canonical": "/repo",
                "vcs": "git",
                "name": "Repo",
                "icon": { "color": "blue", "override": "R" },
                "time": { "created": 1, "updated": 2 },
                "sandboxes": ["/repo-wt"]
            },
            { "id": "prj_2", "canonical": "/other", "time": { "created": 1, "updated": 1 }, "sandboxes": [] }
        ]));
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name.as_deref(), Some("Repo"));
        assert_eq!(
            projects[0].icon.as_ref().unwrap().override_.as_deref(),
            Some("R")
        );
        assert_eq!(projects[0].sandboxes, ["/repo-wt"]);
        assert_eq!(projects[1].name, None);
    }

    fn session_json(id: &str) -> Value {
        json!({
            "id": id,
            "projectID": "prj_1",
            "agent": "build",
            "model": { "id": "gpt-6-sol", "providerID": "openai", "variant": "high" },
            "cost": 0.25,
            "tokens": { "input": 10, "output": 5, "reasoning": 1.5, "cache": { "read": 2, "write": 0 } },
            "outcome": "succeeded",
            "time": { "created": 100, "updated": 200, "idle": 190 },
            "title": "Fix bug",
            "location": { "directory": "/repo" }
        })
    }

    #[test]
    fn session_list_is_a_cursor_page() {
        let page: SessionListResponse = from(json!({
            "data": [session_json("ses_1"), { "id": "ses_2", "parentID": "ses_1", "projectID": "prj_1",
                "cost": 0, "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "time": { "created": 1, "updated": 1 }, "location": { "directory": "/repo" }, "outcome": "paused" }],
            "cursor": { "previous": "cHJldg", "next": null }
        }));
        let first = &page.data[0];
        assert_eq!(first.directory(), "/repo");
        assert_eq!(
            first.model,
            Some(ModelRef {
                id: "gpt-6-sol".into(),
                provider_id: "openai".into(),
                variant: Some("high".into())
            })
        );
        assert_eq!(first.tokens.total(), 18.5);
        assert_eq!(first.time.idle, Some(190));
        assert_eq!(first.outcome, Some(Outcome::Succeeded));
        assert!(first.is_root());
        let child = &page.data[1];
        assert_eq!(child.parent_id.as_deref(), Some("ses_1"));
        assert_eq!(child.title, None);
        assert_eq!(child.outcome, Some(Outcome::Unknown));
        assert_eq!(page.next_cursor(), None);

        let empty: SessionListResponse = from(json!({ "data": [], "cursor": {} }));
        assert!(empty.data.is_empty());
        assert_eq!(empty.next_cursor(), None);
    }

    #[test]
    fn session_get_and_active_use_data_envelopes() {
        let session: SessionResponse = from(json!({ "data": session_json("ses_1") }));
        assert_eq!(session.data.title.as_deref(), Some("Fix bug"));
        let active: SessionActiveResponse = from(json!({
            "data": { "ses_1": { "type": "running" }, "ses_2": { "type": "draining" } }
        }));
        assert_eq!(active.data["ses_1"], ActiveStatus::Running);
        assert_eq!(active.data["ses_2"], ActiveStatus::Unknown);
    }

    #[test]
    fn prompt_returns_the_queued_inbox_user() {
        let response: PromptResponse = from(json!({
            "data": {
                "id": "msg_01",
                "sessionID": "ses_1",
                "time": { "created": 5 },
                "type": "user",
                "payload": {
                    "text": "hi",
                    "files": [{ "data": "aGk=", "mime": "text/plain", "source": { "type": "inline" }, "name": "a.txt" }]
                },
                "delivery": "steer"
            }
        }));
        assert_eq!(response.data.id, "msg_01");
        assert_eq!(response.data.delivery, Some(Delivery::Steer));
        assert_eq!(
            response.data.payload.files[0].source,
            Some(FileSource::Inline)
        );
    }

    #[test]
    fn inbox_list_flattens_tagged_items() {
        let inbox: InboxListResponse = from(json!({
            "data": [
                { "id": "msg_1", "sessionID": "ses_1", "time": { "created": 1 }, "type": "user",
                  "payload": { "text": "queued" }, "delivery": "queue" },
                { "id": "msg_2", "sessionID": "ses_1", "time": { "created": 2 }, "type": "compaction",
                  "payload": {}, "delivery": "steer" },
                { "id": "msg_3", "sessionID": "ses_1", "time": { "created": 3 }, "type": "teleport",
                  "payload": {}, "delivery": "steer" }
            ]
        }));
        assert!(matches!(
            &inbox.data[0].item,
            InboxItem::User { payload, delivery: Some(Delivery::Queue) } if payload.text == "queued"
        ));
        assert!(matches!(inbox.data[1].item, InboxItem::Compaction { .. }));
        assert_eq!(inbox.data[2].item, InboxItem::Unknown);
        assert_eq!(inbox.data[2].id, "msg_3");
    }

    #[test]
    fn interrupt_is_a_bare_flag() {
        let response: InterruptResponse = from(json!({ "interrupted": false }));
        assert!(!response.interrupted);
    }

    fn message_page() -> Value {
        json!({
            "data": [
                { "id": "msg_a", "type": "agent-switched", "time": { "created": 1 }, "agent": "build", "previous": "plan" },
                { "id": "msg_b", "type": "model-switched", "time": { "created": 2 },
                  "model": { "id": "m", "providerID": "p" }, "previous": { "id": "o", "providerID": "p" } },
                { "id": "msg_c", "type": "location-switched", "time": { "created": 3 },
                  "location": { "directory": "/b" }, "projectID": "prj_1", "previous": null },
                { "id": "msg_d", "type": "user", "time": { "created": 4 }, "text": "look",
                  "files": [{ "data": "iVBORw0KGgo=", "mime": "image/png", "source": { "type": "uri", "uri": "file:///x.png" }, "name": "x.png" }] },
                { "id": "msg_e", "type": "synthetic", "time": { "created": 5 }, "text": "subagent done", "description": "Background" },
                { "id": "msg_f", "type": "system", "time": { "created": 6 }, "text": "rules", "description": "Instructions updated: a" },
                { "id": "msg_g", "type": "skill", "time": { "created": 7 }, "skill": "sk_1", "name": "review", "text": "body" },
                { "id": "msg_h", "type": "shell", "time": { "created": 8, "completed": 9 }, "shellID": "sh_1",
                  "command": "ls", "status": "exited", "exit": 0,
                  "output": { "output": "a\n", "cursor": 2, "size": 2, "truncated": false } },
                { "id": "msg_i", "type": "assistant", "time": { "created": 10, "completed": 12 }, "agent": "build",
                  "model": { "id": "m", "providerID": "p" },
                  "content": [
                      { "type": "reasoning", "text": "think", "time": { "created": 10, "completed": 11 } },
                      { "type": "text", "text": "Hello", "state": { "itemId": "x" } },
                      { "type": "tool", "id": "call_1", "name": "bash", "executed": true,
                        "state": { "status": "completed", "input": { "command": "ls" },
                                   "content": [{ "type": "text", "text": "ok" }, { "type": "file", "uri": "data:,", "mime": "text/plain" }],
                                   "metadata": { "exit": 0 } },
                        "time": { "created": 10, "ran": 11, "completed": 12 } },
                      { "type": "tool", "id": "call_2", "name": "read",
                        "state": { "status": "error", "input": {}, "error": { "type": "tool", "message": "nope" } },
                        "time": { "created": 10 } },
                      { "type": "tool", "id": "call_3", "name": "edit", "state": { "status": "streaming", "input": "{\"pa" },
                        "time": { "created": 10 } },
                      { "type": "tool", "id": "call_4", "name": "subagent",
                        "state": { "status": "running", "input": { "prompt": "go" }, "metadata": { "sessionID": "ses_child" } },
                        "time": { "created": 10, "ran": 11 } },
                      { "type": "tool", "id": "call_5", "name": "x", "state": { "status": "paused" }, "time": { "created": 10 } },
                      { "type": "image", "uri": "data:," }
                  ],
                  "finish": "stop", "cost": 0.01,
                  "tokens": { "input": 1, "output": 2, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                  "error": { "type": "provider", "message": "rate limited", "status": 429 },
                  "retry": { "attempt": 1, "at": 13, "error": { "type": "provider", "message": "429" } } },
                { "id": "msg_j", "type": "compaction", "time": { "created": 14 }, "status": "completed", "reason": "auto",
                  "summary": "short", "recent": "msg_i" },
                { "id": "msg_k", "type": "compaction", "time": { "created": 15 }, "status": "failed", "reason": "manual",
                  "error": { "type": "x", "message": "boom" } },
                { "id": "msg_l", "type": "idle", "time": { "created": 16 }, "outcome": "interrupted" },
                { "id": "msg_m", "type": "hologram", "time": { "created": 17 } },
                { "id": "msg_n", "type": "model-switched", "time": { "created": 18 } }
            ],
            "cursor": { "previous": "p", "next": "n" }
        })
    }

    #[test]
    fn message_page_decodes_every_entry_type() {
        let page: MessageListResponse = from(message_page());
        assert_eq!(page.next_cursor(), Some("n"));
        let kinds: Vec<&str> = page.data.iter().map(SessionMessage::kind).collect();
        assert_eq!(
            kinds,
            [
                "agent-switched",
                "model-switched",
                "location-switched",
                "user",
                "synthetic",
                "system",
                "skill",
                "shell",
                "assistant",
                "compaction",
                "compaction",
                "idle",
                "hologram",
                "model-switched"
            ]
        );
        let ids: Vec<Option<&str>> = page.data.iter().map(SessionMessage::id).collect();
        assert_eq!(ids[12], Some("msg_m"));

        let SessionMessage::AgentSwitched(agent) = &page.data[0] else {
            panic!()
        };
        assert_eq!(agent.previous.as_deref(), Some("plan"));
        let SessionMessage::LocationSwitched(location) = &page.data[2] else {
            panic!()
        };
        assert_eq!(location.location.directory, "/b");
        assert_eq!(location.previous, None);
        let SessionMessage::User(user) = &page.data[3] else {
            panic!()
        };
        assert_eq!(
            user.files[0].data_url(),
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert_eq!(
            user.files[0].source,
            Some(FileSource::Uri {
                uri: "file:///x.png".into()
            })
        );
        let SessionMessage::Synthetic(synthetic) = &page.data[4] else {
            panic!()
        };
        assert_eq!(synthetic.text, "subagent done");
        let SessionMessage::Shell(shell) = &page.data[7] else {
            panic!()
        };
        assert_eq!(shell.status, ShellStatus::Exited);
        assert_eq!(shell.exit, Some(json!(0)));
        assert_eq!(shell.output.as_ref().unwrap().output, "a\n");
        let SessionMessage::Compaction(failed) = &page.data[10] else {
            panic!()
        };
        assert_eq!(failed.status, CompactionStatus::Failed);
        assert_eq!(failed.error.as_ref().unwrap().message, "boom");
        let SessionMessage::Idle(idle) = &page.data[11] else {
            panic!()
        };
        assert_eq!(idle.outcome, Outcome::Interrupted);

        let SessionMessage::Unknown(unknown) = &page.data[12] else {
            panic!()
        };
        assert_eq!(unknown.error, None);
        let SessionMessage::Unknown(malformed) = &page.data[13] else {
            panic!()
        };
        assert_eq!(malformed.kind, "model-switched");
        assert!(malformed.error.as_deref().unwrap().contains("model"));
    }

    #[test]
    fn assistant_content_decodes_tool_states() {
        let page: MessageListResponse = from(message_page());
        let SessionMessage::Assistant(assistant) = &page.data[8] else {
            panic!()
        };
        assert_eq!(assistant.agent, "build");
        assert_eq!(assistant.finish.as_deref(), Some("stop"));
        assert_eq!(assistant.error.as_ref().unwrap().status, Some(429));
        assert_eq!(assistant.retry.as_ref().unwrap().attempt, 1);
        let content = &assistant.content;
        assert!(
            matches!(&content[0], AssistantContent::Reasoning(r) if r.text == "think" && r.time.unwrap().completed == Some(11))
        );
        assert!(matches!(&content[1], AssistantContent::Text(t) if t.text == "Hello"));
        let AssistantContent::Tool(done) = &content[2] else {
            panic!()
        };
        assert_eq!(done.id, "call_1");
        assert_eq!(done.executed, Some(true));
        assert_eq!(done.time.ran, Some(11));
        let ToolState::Completed {
            input,
            content: output,
            metadata,
        } = &done.state
        else {
            panic!()
        };
        assert_eq!(input["command"], "ls");
        assert_eq!(output[0], ToolContent::Text { text: "ok".into() });
        assert!(matches!(&output[1], ToolContent::File { name: None, .. }));
        assert_eq!(metadata.as_ref().unwrap()["exit"], 0);
        let AssistantContent::Tool(failed) = &content[3] else {
            panic!()
        };
        assert!(matches!(&failed.state, ToolState::Error { error, .. } if error.message == "nope"));
        let AssistantContent::Tool(streaming) = &content[4] else {
            panic!()
        };
        assert_eq!(
            streaming.state,
            ToolState::Streaming {
                input: "{\"pa".into()
            }
        );
        let AssistantContent::Tool(running) = &content[5] else {
            panic!()
        };
        assert!(
            matches!(&running.state, ToolState::Running { metadata, .. } if metadata["sessionID"] == "ses_child")
        );
        let AssistantContent::Tool(unknown_state) = &content[6] else {
            panic!()
        };
        assert_eq!(unknown_state.state, ToolState::Unknown);
        assert!(matches!(&content[7], AssistantContent::Unknown(entry) if entry.kind == "image"));
    }

    fn model_json() -> Value {
        json!({
            "id": "gpt-6-sol",
            "modelID": "gpt-6-sol-2026-09-01",
            "providerID": "openai",
            "name": "GPT-6 Sol",
            "family": "gpt-6",
            "capabilities": { "tools": true, "input": ["text", "image", "pdf"], "output": ["text"] },
            "variants": [{ "id": "low" }, { "id": "high", "settings": { "effort": "high" } }],
            "time": { "released": 1 },
            "cost": [{ "input": 1, "output": 2, "cache": { "read": 0, "write": 0 } }],
            "status": "active",
            "enabled": true,
            "limit": { "context": 400000, "output": 128000 }
        })
    }

    #[test]
    fn model_list_is_located() {
        let models: ModelListResponse = from(json!({
            "location": { "directory": "/repo" },
            "data": [model_json(), { "id": "old", "modelID": "old", "providerID": "p", "name": "Old",
                "capabilities": { "tools": false, "input": ["text"], "output": ["text"] }, "variants": [],
                "time": { "released": 0 }, "cost": [], "status": "retired", "enabled": false,
                "limit": { "context": 8000, "input": 6000, "output": 2000 } }]
        }));
        assert_eq!(models.location.directory, "/repo");
        let model = &models.data[0];
        assert_eq!(model.status, Some(ModelStatus::Active));
        assert!(model.accepts_input("pdf"));
        assert_eq!(model.limit.context, 400000);
        assert_eq!(model.variants[1].id, "high");
        assert_eq!(model.id, "gpt-6-sol");
        let old = &models.data[1];
        assert!(!old.enabled);
        assert_eq!(old.status, Some(ModelStatus::Unknown));
        assert_eq!(old.limit.input, Some(6000));
    }

    #[test]
    fn model_default_may_be_missing_or_null() {
        let present: ModelDefaultResponse =
            from(json!({ "location": { "directory": "/repo" }, "data": model_json() }));
        assert_eq!(present.data.unwrap().provider_id, "openai");
        let omitted: ModelDefaultResponse = from(json!({ "location": { "directory": "/repo" } }));
        assert_eq!(omitted.data, None);
        let null: ModelDefaultResponse =
            from(json!({ "location": { "directory": "/repo" }, "data": null }));
        assert_eq!(null.data, None);
    }

    fn permission_json() -> Value {
        json!({
            "id": "per_1",
            "sessionID": "ses_child",
            "action": "bash",
            "resources": ["rm -rf build"],
            "save": ["rm *"],
            "metadata": { "command": "rm -rf build" },
            "source": { "type": "tool", "messageID": "msg_i", "id": "call_1" },
            "message": "Run command?"
        })
    }

    #[test]
    fn permission_lists_have_located_and_data_shapes() {
        let located: PermissionRequestListResponse =
            from(json!({ "location": { "directory": "/repo" }, "data": [permission_json()] }));
        let request = &located.data[0];
        assert_eq!(request.session_id, "ses_child");
        assert!(request.offers_always());
        assert_eq!(
            request.source,
            Some(PermissionSource::Tool {
                message_id: "msg_i".into(),
                id: "call_1".into()
            })
        );
        let per_session: SessionPermissionListResponse = from(json!({
            "data": [{ "id": "per_2", "sessionID": "ses_1", "action": "read", "resources": ["/etc"],
                       "source": { "type": "hook" } }]
        }));
        assert!(!per_session.data[0].offers_always());
        assert_eq!(per_session.data[0].source, Some(PermissionSource::Unknown));
    }

    fn form_json() -> Value {
        json!({
            "id": "frm_1",
            "sessionID": "global",
            "title": "Slack wants your workspace",
            "fields": [
                { "key": "workspace", "type": "string", "options": [{ "value": "w1", "label": "One" }] },
                { "key": "go", "type": "external", "url": "https://example.com" }
            ]
        })
    }

    #[test]
    fn forms_decode_title_owner_and_state() {
        let list: FormListResponse =
            from(json!({ "location": { "directory": "/repo" }, "data": [form_json()] }));
        assert!(list.data[0].is_global());
        assert_eq!(list.data[0].fields.len(), 2);
        let per_session: SessionFormListResponse = from(json!({ "data": [] }));
        assert!(per_session.data.is_empty());
        let mut detail = form_json();
        detail["state"] = json!({ "status": "answered", "answer": { "workspace": "w1" } });
        let detail: FormDetailResponse = from(json!({ "data": detail }));
        assert_eq!(detail.data.info.title, "Slack wants your workspace");
        assert!(
            matches!(&detail.data.state, FormState::Answered { answer } if answer["workspace"] == "w1")
        );
        let mut pending = form_json();
        pending["state"] = json!({ "status": "expired" });
        let pending: FormDetail = from(pending);
        assert_eq!(pending.state, FormState::Unknown);
    }

    #[test]
    fn error_bodies_are_tagged() {
        let error = decode_error_body(
            br#"{"_tag":"PermissionNotFoundError","requestID":"per_1","message":"Permission request not found: per_1"}"#,
        )
        .unwrap();
        assert_eq!(error.kind(), ApiErrorKind::PermissionNotFound);
        assert!(error.is_already_resolved());
        assert_eq!(error.extra["requestID"], "per_1");
        let invalid = decode_error_body(
            br#"{"_tag":"InvalidRequestError","message":"Attachment exceeds","field":"files"}"#,
        )
        .unwrap();
        assert_eq!(invalid.kind(), ApiErrorKind::InvalidRequest);
        assert_eq!(invalid.field(), Some("files"));
        assert!(!invalid.is_already_resolved());
        let unauthorized = decode_error_body(
            br#"{"_tag":"UnauthorizedError","message":"Authentication required"}"#,
        )
        .unwrap();
        assert_eq!(unauthorized.kind(), ApiErrorKind::Unauthorized);
        assert_eq!(
            decode_error_body(br#"{"_tag":"BrandNewError"}"#)
                .unwrap()
                .kind(),
            ApiErrorKind::Other
        );
        assert!(decode_error_body(b"<html>Bad gateway</html>").is_none());
        assert!(decode_error_body(b"{}").is_none());
    }

    fn envelope(kind: &str, data: Value) -> Value {
        json!({
            "id": "evt_0123456789abABCDEFGHIJKLMN",
            "created": 1_700_000_000_000_i64,
            "type": kind,
            "location": { "directory": "/repo", "workspaceID": "wrk_1" },
            "durable": { "aggregateID": "ses_1", "seq": 7, "version": 1 },
            "data": data
        })
    }

    #[test]
    fn event_envelope_and_connected_frame() {
        let connected =
            parse_event(r#"{"id":"evt_1","type":"server.connected","data":{}}"#).unwrap();
        assert_eq!(connected.created, None);
        assert_eq!(decode_event(&connected), EventKind::ServerConnected);
        let ev: Event = from(envelope(
            "session.renamed",
            json!({ "sessionID": "ses_1", "title": "T" }),
        ));
        assert_eq!(ev.directory(), Some("/repo"));
        assert_eq!(ev.durable.as_ref().unwrap().seq, 7);
        assert_eq!(
            ev.projected_message_id().as_deref(),
            Some("msg_0123456789abABCDEFGHIJKLMN")
        );
        assert_eq!(
            decode_event(&ev),
            EventKind::SessionRenamed(SessionRenamed {
                session_id: "ses_1".into(),
                title: "T".into()
            })
        );
    }

    #[test]
    fn unknown_and_malformed_events_are_tolerated() {
        assert_eq!(
            event(envelope("pty.created", json!({ "info": {} }))),
            EventKind::Other("pty.created".into())
        );
        assert_eq!(
            event(envelope(
                "session.teleported",
                json!({ "sessionID": "ses_1" })
            )),
            EventKind::Other("session.teleported".into())
        );
        let malformed = event(envelope(
            "session.text.delta",
            json!({ "sessionID": "ses_1" }),
        ));
        assert!(
            matches!(malformed, EventKind::Malformed { ref type_, .. } if type_ == "session.text.delta")
        );
        assert_eq!(malformed.session_id(), None);
    }

    #[test]
    fn session_events_decode() {
        let created = event(envelope(
            "session.created",
            json!({ "sessionID": "ses_2", "projectID": "prj_1", "location": { "directory": "/repo" },
                    "parentID": "ses_1", "slug": "brave-otter", "version": "2.0.8" }),
        ));
        let EventKind::SessionCreated(created) = created else {
            panic!()
        };
        assert_eq!(created.parent_id.as_deref(), Some("ses_1"));
        assert_eq!(created.location.directory, "/repo");

        assert_eq!(
            event(envelope("session.deleted", json!({ "sessionID": "ses_1" }))).session_id(),
            Some("ses_1")
        );
        let EventKind::SessionModelSelected(model) = event(envelope(
            "session.model.selected",
            json!({ "sessionID": "ses_1", "model": { "id": "m", "providerID": "p", "variant": "high" } }),
        )) else {
            panic!()
        };
        assert_eq!(model.model.variant.as_deref(), Some("high"));
        assert!(matches!(
            event(envelope("session.agent.selected", json!({ "sessionID": "ses_1", "agent": "plan" }))),
            EventKind::SessionAgentSelected(SessionAgentSelected { ref agent, .. }) if agent == "plan"
        ));
        assert!(matches!(
            event(envelope("session.moved", json!({ "sessionID": "ses_1", "location": { "directory": "/b" }, "projectID": "prj_2" }))),
            EventKind::SessionMoved(SessionMoved { ref location, .. }) if location.directory == "/b"
        ));
        assert!(matches!(
            event(envelope(
                "session.viewed",
                json!({ "sessionID": "ses_1", "idle": 55 })
            )),
            EventKind::SessionViewed(SessionViewed { idle: 55, .. })
        ));
        assert!(matches!(
            event(envelope("session.usage.updated", json!({ "sessionID": "ses_1", "cost": 1.5,
                "tokens": { "input": 1, "output": 1, "reasoning": 0, "cache": { "read": 0, "write": 0 } } }))),
            EventKind::SessionUsageUpdated(SessionUsage { cost, .. }) if cost == 1.5
        ));
    }

    #[test]
    fn inbox_events_decode() {
        let EventKind::InboxEnqueued(enqueued) = event(envelope(
            "session.inbox.enqueued",
            json!({ "sessionID": "ses_1", "inboxID": "msg_1",
                    "item": { "type": "user", "payload": { "text": "hi" }, "delivery": "steer" } }),
        )) else {
            panic!()
        };
        assert_eq!(enqueued.inbox_id, "msg_1");
        assert!(
            matches!(enqueued.item, InboxItem::User { ref payload, .. } if payload.text == "hi")
        );
        for kind in ["session.inbox.delivered", "session.inbox.cancelled"] {
            let decoded = event(envelope(
                kind,
                json!({ "sessionID": "ses_1", "inboxID": "msg_1" }),
            ));
            assert_eq!(decoded.session_id(), Some("ses_1"));
            assert!(!matches!(
                decoded,
                EventKind::Malformed { .. } | EventKind::Other(_)
            ));
        }
        assert!(matches!(
            event(envelope(
                "session.inbox.delivery.changed",
                json!({ "sessionID": "ses_1", "inboxID": "msg_1", "delivery": "queue" })
            )),
            EventKind::InboxDeliveryChanged(InboxDeliveryChanged {
                delivery: Delivery::Queue,
                ..
            })
        ));
    }

    #[test]
    fn streaming_events_decode() {
        let base = |extra: Value| {
            let mut data = json!({ "sessionID": "ses_1", "assistantMessageID": "msg_i" });
            data.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            data
        };
        assert!(matches!(
            event(envelope(
                "session.text.started",
                base(json!({ "ordinal": 0 }))
            )),
            EventKind::TextStarted(FragmentStarted { ordinal: 0, .. })
        ));
        assert!(matches!(
            event(envelope("session.text.delta", base(json!({ "ordinal": 0, "delta": "He" })))),
            EventKind::TextDelta(FragmentDelta { ref delta, .. }) if delta == "He"
        ));
        assert!(matches!(
            event(envelope("session.text.ended", base(json!({ "ordinal": 0, "text": "Hello", "state": {} })))),
            EventKind::TextEnded(FragmentEnded { ref text, .. }) if text == "Hello"
        ));
        assert!(matches!(
            event(envelope(
                "session.reasoning.started",
                base(json!({ "ordinal": 1 }))
            )),
            EventKind::ReasoningStarted(FragmentStarted { ordinal: 1, .. })
        ));
        assert!(matches!(
            event(envelope(
                "session.reasoning.delta",
                base(json!({ "ordinal": 1, "delta": "t" }))
            )),
            EventKind::ReasoningDelta(_)
        ));
        assert!(matches!(
            event(envelope(
                "session.reasoning.ended",
                base(json!({ "ordinal": 1, "text": "think" }))
            )),
            EventKind::ReasoningEnded(_)
        ));
        assert!(matches!(
            event(envelope("session.tool.input.started", base(json!({ "id": "call_1", "name": "bash" })))),
            EventKind::ToolInputStarted(ToolInputStarted { ref id, ref name, .. }) if id == "call_1" && name == "bash"
        ));
        assert!(matches!(
            event(envelope(
                "session.tool.input.delta",
                base(json!({ "id": "call_1", "delta": "{\"" }))
            )),
            EventKind::ToolInputDelta(_)
        ));
        assert!(matches!(
            event(envelope(
                "session.tool.input.ended",
                base(json!({ "id": "call_1", "text": "{}" }))
            )),
            EventKind::ToolInputEnded(_)
        ));
        assert!(matches!(
            event(envelope("session.tool.called", base(json!({ "id": "call_1", "input": { "command": "ls" }, "executed": false })))),
            EventKind::ToolCalled(ToolCalled { ref input, executed: false, .. }) if input["command"] == "ls"
        ));
        assert!(matches!(
            event(envelope("session.tool.progress", base(json!({ "id": "call_1", "metadata": { "sessionID": "ses_c" } })))),
            EventKind::ToolProgress(ToolProgress { ref metadata, .. }) if metadata["sessionID"] == "ses_c"
        ));
        assert!(matches!(
            event(envelope("session.tool.success", base(json!({ "id": "call_1", "content": [{ "type": "text", "text": "ok" }], "executed": true })))),
            EventKind::ToolSuccess(ToolSuccess { ref content, executed: true, .. }) if content.len() == 1
        ));
        assert!(matches!(
            event(envelope("session.tool.failed", base(json!({ "id": "call_2", "error": { "type": "tool", "message": "bad" }, "executed": true })))),
            EventKind::ToolFailed(ToolFailed { ref error, .. }) if error.message == "bad"
        ));
        let content_updated = event(envelope(
            "session.message.content.updated",
            json!({ "sessionID": "ses_1", "messageID": "msg_i",
                    "content": [{ "type": "text", "text": "replaced" }, { "type": "sticker" }] }),
        ));
        let EventKind::MessageContentUpdated(updated) = content_updated else {
            panic!()
        };
        assert_eq!(updated.message_id, "msg_i");
        assert!(matches!(&updated.content[1], AssistantContent::Unknown(_)));
    }

    #[test]
    fn step_execution_and_retry_events_decode() {
        let tokens =
            json!({ "input": 1, "output": 2, "reasoning": 0, "cache": { "read": 0, "write": 0 } });
        assert!(matches!(
            event(envelope(
                "session.step.started",
                json!({ "sessionID": "ses_1", "assistantMessageID": "msg_i",
                "agent": "build", "model": { "id": "m", "providerID": "p" }, "started": 9 })
            )),
            EventKind::StepStarted(StepStarted { started: 9, .. })
        ));
        assert!(matches!(
            event(envelope("session.step.ended", json!({ "sessionID": "ses_1", "assistantMessageID": "msg_i",
                "finish": "tool-calls", "cost": 0.5, "tokens": tokens }))),
            EventKind::StepEnded(StepEnded { ref finish, .. }) if finish == "tool-calls"
        ));
        assert!(matches!(
            event(envelope(
                "session.step.failed",
                json!({ "sessionID": "ses_1", "assistantMessageID": "msg_i",
                "error": { "type": "provider", "message": "x" } })
            )),
            EventKind::StepFailed(StepFailed { cost: None, .. })
        ));
        assert!(matches!(
            event(envelope(
                "session.execution.started",
                json!({ "sessionID": "ses_1" })
            )),
            EventKind::ExecutionStarted(_)
        ));
        assert!(matches!(
            event(envelope(
                "session.execution.succeeded",
                json!({ "sessionID": "ses_1" })
            )),
            EventKind::ExecutionSucceeded(_)
        ));
        assert!(matches!(
            event(envelope("session.execution.failed", json!({ "sessionID": "ses_1", "error": { "type": "x", "message": "y" } }))),
            EventKind::ExecutionFailed(ExecutionFailed { ref error, .. }) if error.message == "y"
        ));
        assert!(matches!(
            event(envelope("session.execution.interrupted", json!({ "sessionID": "ses_1", "reason": "user" }))),
            EventKind::ExecutionInterrupted(ExecutionInterrupted { ref reason, .. }) if reason == "user"
        ));
        assert!(matches!(
            event(envelope(
                "session.retry.scheduled",
                json!({ "sessionID": "ses_1", "assistantMessageID": "msg_i",
                "attempt": 2, "at": 99, "error": { "type": "provider", "message": "overloaded", "status": 529 } })
            )),
            EventKind::RetryScheduled(RetryScheduled {
                attempt: 2,
                at: 99,
                ..
            })
        ));
        assert!(matches!(
            event(envelope(
                "session.status",
                json!({ "sessionID": "ses_1", "status": { "type": "retry", "attempt": 1, "message": "m", "next": 5 } })
            )),
            EventKind::SessionStatus(SessionStatusChanged {
                status: SessionStatus::Retry { attempt: 1, .. },
                ..
            })
        ));
        assert!(matches!(
            event(envelope("session.idle", json!({ "sessionID": "ses_1" }))),
            EventKind::SessionIdle(_)
        ));
    }

    #[test]
    fn transcript_note_events_decode() {
        assert!(matches!(
            event(envelope("session.synthetic", json!({ "sessionID": "ses_1", "text": "done", "metadata": { "a": 1 } }))),
            EventKind::Synthetic(SyntheticAdded { ref text, .. }) if text == "done"
        ));
        assert!(matches!(
            event(envelope(
                "session.skill.activated",
                json!({ "sessionID": "ses_1", "id": "sk", "name": "n", "text": "t" })
            )),
            EventKind::SkillActivated(_)
        ));
        assert!(matches!(
            event(envelope(
                "session.instructions.updated",
                json!({ "sessionID": "ses_1", "delta": { "a": "removed" } })
            )),
            EventKind::InstructionsUpdated(InstructionsUpdated { text: None, .. })
        ));
        let shell = json!({ "id": "sh_1", "status": "exited", "command": "ls", "cwd": "/repo", "shell": "zsh",
                            "file": "/tmp/out", "exit": 0, "metadata": {}, "time": { "started": 1, "completed": 2 } });
        assert!(matches!(
            event(envelope("session.shell.started", json!({ "sessionID": "ses_1", "shell": shell.clone() }))),
            EventKind::ShellStarted(ShellStarted { ref shell, .. }) if shell.id == "sh_1"
        ));
        assert!(matches!(
            event(envelope("session.shell.ended", json!({ "sessionID": "ses_1", "shell": shell,
                "output": { "output": "a", "cursor": 1, "size": 1, "truncated": false } }))),
            EventKind::ShellEnded(ShellEnded { ref output, .. }) if output.output == "a"
        ));
        assert!(matches!(
            event(envelope("session.compaction.started", json!({ "sessionID": "ses_1", "reason": "auto", "recent": "msg_1", "inputID": "msg_c" }))),
            EventKind::CompactionStarted(CompactionStarted { ref input_id, .. }) if input_id.as_deref() == Some("msg_c")
        ));
        assert!(matches!(
            event(envelope(
                "session.compaction.delta",
                json!({ "sessionID": "ses_1", "text": "su" })
            )),
            EventKind::CompactionDelta(_)
        ));
        assert!(matches!(
            event(envelope("session.compaction.ended", json!({ "sessionID": "ses_1", "reason": "auto", "text": "summary", "recent": "msg_1" }))),
            EventKind::CompactionEnded(CompactionEnded { ref text, .. }) if text == "summary"
        ));
        assert!(matches!(
            event(envelope(
                "session.compaction.failed",
                json!({ "sessionID": "ses_1", "reason": "manual", "error": { "type": "x", "message": "y" } })
            )),
            EventKind::CompactionFailed(_)
        ));
    }

    #[test]
    fn permission_form_and_invalidation_events_decode() {
        assert!(matches!(
            event(envelope("permission.asked", permission_json())),
            EventKind::PermissionAsked(PermissionRequest { ref id, .. }) if id == "per_1"
        ));
        assert!(matches!(
            event(envelope("permission.replied", json!({ "sessionID": "ses_1", "requestID": "per_1", "reply": "always" }))),
            EventKind::PermissionReplied(PermissionReplied { ref reply, .. }) if reply == "always"
        ));
        let created = event(envelope("form.created", json!({ "form": form_json() })));
        assert_eq!(created.session_id(), Some("global"));
        assert!(matches!(
            event(envelope(
                "form.replied",
                json!({ "id": "frm_1", "sessionID": "ses_1", "answer": { "k": true } })
            )),
            EventKind::FormReplied(FormSettled {
                answer: Some(_),
                ..
            })
        ));
        assert!(matches!(
            event(envelope(
                "form.cancelled",
                json!({ "id": "frm_1", "sessionID": "ses_1" })
            )),
            EventKind::FormCancelled(FormSettled { answer: None, .. })
        ));
        assert_eq!(
            event(envelope("location.shutdown", json!({}))),
            EventKind::LocationShutdown
        );
        assert!(matches!(
            event(envelope("project.updated", json!({ "id": "prj_1", "canonical": "/repo", "time": { "created": 1, "updated": 2 }, "sandboxes": [] }))),
            EventKind::ProjectUpdated(ProjectInfo { ref canonical, .. }) if canonical == "/repo"
        ));
        assert!(matches!(
            event(envelope(
                "worktree.updated",
                json!({ "projectID": "prj_1" })
            )),
            EventKind::WorktreeUpdated(_)
        ));
        assert!(matches!(
            event(envelope(
                "worktree.resolved",
                json!({ "projectID": "prj_1", "directory": "/a", "previous": "/b" })
            )),
            EventKind::WorktreeResolved(_)
        ));
        for (kind, change) in [
            ("provider.updated", CatalogChange::Provider),
            ("model.updated", CatalogChange::Model),
            ("models-dev.refreshed", CatalogChange::ModelsDev),
            ("agent.updated", CatalogChange::Agent),
            ("config.updated", CatalogChange::Config),
            ("integration.updated", CatalogChange::Integration),
            ("credential.updated", CatalogChange::Credential),
            ("credential.switched", CatalogChange::Credential),
        ] {
            assert_eq!(
                event(envelope(kind, json!({}))),
                EventKind::CatalogChanged(change)
            );
        }
    }

    #[test]
    fn request_bodies_serialize_exactly() {
        fn body<T: Serialize>(value: &T) -> Value {
            serde_json::to_value(value).unwrap()
        }
        assert_eq!(
            body(&CreateSessionBody::new("/repo", Some("Fix".into()))),
            json!({ "location": { "directory": "/repo" }, "title": "Fix" })
        );
        assert_eq!(
            body(&CreateSessionBody::new("/repo", Some("  ".into()))),
            json!({ "location": { "directory": "/repo" } })
        );
        assert_eq!(
            body(&RenameSessionBody::new(" New name ").unwrap()),
            json!({ "title": "New name" })
        );
        assert_eq!(RenameSessionBody::new("   "), None);
        assert_eq!(
            body(&PromptBody {
                id: "msg_1".into(),
                text: "hi".into(),
                files: vec![],
                delivery: None,
            }),
            json!({ "id": "msg_1", "text": "hi" }),
            "a steered prompt omits delivery (the server default)"
        );
        assert_eq!(
            body(&PromptBody {
                id: "msg_1".into(),
                text: "later".into(),
                files: vec![],
                delivery: Some(Delivery::Queue),
            }),
            json!({ "id": "msg_1", "text": "later", "delivery": "queue" })
        );
        assert_eq!(
            body(&InboxUpdateBody {
                delivery: Delivery::Steer
            }),
            json!({ "delivery": "steer" })
        );
        assert_eq!(
            body(&InboxUpdateBody {
                delivery: Delivery::Queue
            }),
            json!({ "delivery": "queue" })
        );
        assert_eq!(
            body(&PromptBody {
                id: "msg_1".into(),
                text: "see".into(),
                files: vec![
                    PromptFile::from_bytes(b"hi", "text/plain", Some("a.txt".into())),
                    PromptFile::from_bytes(&[0xff, 0xfe], "application/octet-stream", None),
                ],
                delivery: None,
            }),
            json!({
                "id": "msg_1",
                "text": "see",
                "files": [
                    { "uri": "data:text/plain;base64,aGk=", "name": "a.txt" },
                    { "uri": "data:application/octet-stream;base64,//4=" }
                ]
            })
        );
        assert_eq!(
            body(&SwitchModelBody {
                model: ModelRef {
                    id: "gpt-6-sol".into(),
                    provider_id: "openai".into(),
                    variant: None
                }
            }),
            json!({ "model": { "id": "gpt-6-sol", "providerID": "openai" } })
        );
        assert_eq!(
            body(&PermissionReplyBody {
                decision: PermissionDecision::Always,
                message: None
            }),
            json!({ "decision": "always" })
        );
        assert_eq!(
            body(&PermissionReplyBody {
                decision: PermissionDecision::Reject,
                message: Some("no".into())
            }),
            json!({ "decision": "reject", "message": "no" })
        );
    }

    #[test]
    fn route_helpers_encode_ids_under_api() {
        assert_eq!(info_path(), "/api/info");
        assert_eq!(projects_path(), "/api/project");
        assert_eq!(sessions_active_path(), "/api/session/active");
        assert_eq!(session_path("ses_1"), "/api/session/ses_1");
        assert_eq!(session_path("a/b c?"), "/api/session/a%2Fb%20c%3F");
        assert_eq!(session_messages_path("ses_1"), "/api/session/ses_1/message");
        assert_eq!(session_prompt_path("ses_1"), "/api/session/ses_1/prompt");
        assert_eq!(
            session_interrupt_path("ses_1"),
            "/api/session/ses_1/interrupt"
        );
        assert_eq!(session_model_path("ses_1"), "/api/session/ses_1/model");
        assert_eq!(
            session_inbox_item_path("ses_1", "msg_1"),
            "/api/session/ses_1/inbox/msg_1"
        );
        assert_eq!(
            permission_reply_path("ses_child", "per_1"),
            "/api/session/ses_child/permission/per_1/reply"
        );
        assert_eq!(
            session_form_path("global", "frm_1"),
            "/api/session/global/form/frm_1"
        );
        assert_eq!(models_path(), "/api/model");
        assert_eq!(model_default_path(), "/api/model/default");
        assert_eq!(permission_requests_path(), "/api/permission/request");
        assert_eq!(forms_path(), "/api/form");
        assert_eq!(events_path(), "/api/event");
        assert_eq!(encode_segment("é"), "%C3%A9");
    }

    #[test]
    fn endpoint_table_marks_no_content_routes() {
        let no_content: Vec<&str> = ENDPOINTS
            .iter()
            .filter(|spec| spec.success_status == 204)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            no_content,
            [
                "session.update",
                "session.switchModel",
                "session.inbox.update",
                "session.inbox.cancel",
                "session.permission.reply",
                "session.form.cancel"
            ]
        );
        assert_eq!(endpoint_spec("session.prompt").unwrap().success_status, 200);
        assert_eq!(endpoint_spec("session.update").unwrap().method, "PATCH");
        assert!(ENDPOINTS.iter().all(|spec| spec.path.starts_with("/api/")));
    }

    #[test]
    fn query_builders_follow_server_rules() {
        assert_eq!(
            query_string(&[location_query("/Users/me/my repo")]),
            "?location%5Bdirectory%5D=%2FUsers%2Fme%2Fmy+repo"
        );
        assert_eq!(query_string(&[]), "");
        assert_eq!(form_location_query("ses_1", "/repo"), QueryPairs::new());
        assert_eq!(
            form_location_query("global", "/repo"),
            vec![location_query("/repo")]
        );

        let roots = SessionListQuery {
            limit: Some(100),
            ..SessionListQuery::roots()
        };
        assert_eq!(query_string(&roots.pairs()), "?limit=100&parentID=null");
        let scoped = SessionListQuery {
            directory: Some("/repo".into()),
            order: Some(Order::Asc),
            ..SessionListQuery::roots()
        };
        assert_eq!(
            query_string(&scoped.pairs()),
            "?order=asc&parentID=null&directory=%2Frepo"
        );
        assert_eq!(
            query_string(&scoped.with_cursor("abc").pairs()),
            "?cursor=abc"
        );

        let first = MessageListQuery {
            limit: Some(500),
            order: Some(Order::Asc),
            cursor: None,
        };
        assert_eq!(query_string(&first.pairs()), "?limit=200&order=asc");
        let next = MessageListQuery {
            cursor: Some("eyJ".into()),
            ..first
        };
        assert_eq!(query_string(&next.pairs()), "?limit=200&cursor=eyJ");
    }

    #[test]
    fn message_ids_match_the_server_format() {
        let id = message_id_at(1_700_000_000_000, 1, 42);
        assert!(id.starts_with("msg_"));
        assert_eq!(id.len(), 4 + 12 + 14);
        let expected_time = format!("{:012x}", (1_700_000_000_000_u128 << 12) + 1);
        assert_eq!(&id[4..16], &expected_time[expected_time.len() - 12..]);
        assert!(id[16..].bytes().all(|byte| byte.is_ascii_alphanumeric()));
        assert!(message_id_at(1_700_000_000_001, 1, 42) > id);
        let fresh = new_message_id();
        assert!(fresh.starts_with("msg_") && fresh.len() == 30);
        let successive: Vec<String> = (0..5_000).map(|_| new_message_id()).collect();
        assert!(successive.iter().all(|id| id.starts_with("msg_")
            && id.len() == 30
            && id[4..16].bytes().all(|byte| byte.is_ascii_hexdigit())));
        assert!(
            std::iter::once(&fresh)
                .chain(&successive)
                .collect::<Vec<_>>()
                .windows(2)
                .all(|pair| pair[0][..16] < pair[1][..16]),
            "time/counter prefixes strictly ascend, even within one millisecond"
        );
        assert_eq!(
            message_id_from_event_id("evt_abc").as_deref(),
            Some("msg_abc")
        );
        assert_eq!(message_id_from_event_id("abc"), None);
    }

    #[test]
    fn captured_exchange_wrapper_decodes_bodies_and_errors() {
        let exchange: CapturedExchange = from(json!({
            "name": "session.messages.page1",
            "request": {
                "method": "GET",
                "path": "/api/session/ses_1/message",
                "pathTemplate": "/api/session/{sessionID}/message",
                "query": "limit=2",
                "body": null
            },
            "response": {
                "status": 200,
                "headers": { "content-type": "application/json" },
                "body": message_page()
            }
        }));
        assert_eq!(exchange.request.query.as_deref(), Some("limit=2"));
        let page: MessageListResponse = exchange.decode_body().unwrap();
        assert_eq!(page.data.len(), 14);
        let failure: CapturedExchange = from(json!({
            "name": "reply.gone",
            "request": {
                "method": "POST",
                "path": "/api/session/ses_1/permission/per_1/reply",
                "pathTemplate": "/api/session/{sessionID}/permission/{requestID}/reply",
                "query": null,
                "body": { "decision": "once" }
            },
            "response": {
                "status": 404,
                "headers": {},
                "body": { "_tag": "PermissionNotFoundError", "requestID": "per_1", "message": "gone" }
            },
            "note": "stale"
        }));
        assert!(failure.error().unwrap().is_already_resolved());
        assert_eq!(failure.note.as_deref(), Some("stale"));
        let empty: CapturedExchange = from(json!({
            "name": "session.rename",
            "request": { "method": "PATCH", "path": "/api/session/ses_1", "pathTemplate": "/api/session/{sessionID}" },
            "response": { "status": 204, "headers": {}, "body": null }
        }));
        assert_eq!(empty.response.body, Value::Null);
        assert!(empty.error().is_none());
    }

    fn fixture_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-2.0.8")
    }

    fn captured_exchanges() -> Vec<CapturedExchange> {
        let mut paths: Vec<_> = std::fs::read_dir(fixture_dir())
            .expect("fixture directory")
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "json")
                    && path.file_name().is_some_and(|name| name != "index.json")
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|path| {
                let text = std::fs::read_to_string(path).unwrap();
                serde_json::from_str(&text)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            })
            .collect()
    }

    fn strict<T: DeserializeOwned>(exchange: &CapturedExchange) -> T {
        exchange
            .decode_body()
            .unwrap_or_else(|error| panic!("{}: {error}", exchange.name))
    }

    /// Tolerant decoding would hide a renamed field or tag, so the known
    /// unions must not fall back to their `Unknown` arms on real data.
    fn assert_message_page_is_fully_typed(name: &str, page: &MessageListResponse) {
        for entry in &page.data {
            assert!(
                !matches!(entry, SessionMessage::Unknown(_)),
                "{name}: {entry:?}"
            );
            match entry {
                SessionMessage::Assistant(message) => {
                    for item in &message.content {
                        match item {
                            AssistantContent::Unknown(unknown) => panic!("{name}: {unknown:?}"),
                            AssistantContent::Tool(tool) => {
                                assert!(!matches!(tool.state, ToolState::Unknown), "{name}");
                                assert!(!tool.name.is_empty() && tool.time.created > 0, "{name}");
                            }
                            _ => {}
                        }
                    }
                    assert!(message.time.created > 0, "{name}");
                }
                SessionMessage::User(message) => {
                    for file in &message.files {
                        assert!(!file.data.is_empty() && !file.mime.is_empty(), "{name}");
                        assert_eq!(file.source, Some(FileSource::Inline), "{name}");
                    }
                }
                SessionMessage::Idle(message) => {
                    assert_ne!(message.outcome, Outcome::Unknown, "{name}")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn every_captured_http_fixture_decodes_with_its_typed_response() {
        let exchanges = captured_exchanges();
        assert!(exchanges.len() >= 72, "{}", exchanges.len());
        let mut message_pages = 0;
        for exchange in &exchanges {
            let name = exchange.name.as_str();
            let status = exchange.response.status;
            let body = &exchange.response.body;
            assert!(exchange.response.body_text.is_none(), "{name}");
            if !(200..300).contains(&status) {
                if status == 401 {
                    assert!(body.is_null(), "{name}: 401 has no body");
                    continue;
                }
                let error = exchange.error().unwrap_or_else(|| panic!("{name}: {body}"));
                assert!(error.message.is_some(), "{name}");
                let expected = match status {
                    400 => [ApiErrorKind::InvalidRequest, ApiErrorKind::InvalidCursor].as_slice(),
                    404 => &[ApiErrorKind::SessionNotFound],
                    409 => &[ApiErrorKind::FormAlreadySettled],
                    _ => panic!("{name}: unexpected status {status}"),
                };
                assert!(expected.contains(&error.kind()), "{name}: {error:?}");
                continue;
            }
            if status == 204 {
                assert!(body.is_null(), "{name}");
                continue;
            }
            let method = exchange.request.method.as_str();
            match (method, exchange.request.path_template.as_str()) {
                ("GET", "/api/info") => assert!(strict::<ServerInfoResponse>(exchange).is_v2()),
                ("GET", "/api/project") => {
                    let projects = strict::<ProjectListResponse>(exchange);
                    assert!(projects.iter().all(|project| !project.canonical.is_empty()));
                }
                ("GET", "/api/session") => {
                    let page = strict::<SessionListResponse>(exchange);
                    assert!(
                        page.data
                            .iter()
                            .all(|session| !session.directory().is_empty()
                                && session.time.created > 0)
                    );
                }
                ("POST", "/api/session") | ("GET", "/api/session/{sessionID}") => {
                    let session = strict::<SessionResponse>(exchange).data;
                    assert!(!session.directory().is_empty(), "{name}");
                }
                ("GET", "/api/session/active") => {
                    let active = strict::<SessionActiveResponse>(exchange);
                    assert!(active.data.values().all(|s| *s == ActiveStatus::Running));
                }
                ("POST", "/api/session/{sessionID}/prompt") => {
                    let prompt = strict::<PromptResponse>(exchange).data;
                    assert!(prompt.id.starts_with("msg_"), "{name}");
                }
                ("POST", "/api/session/{sessionID}/interrupt") => {
                    strict::<InterruptResponse>(exchange);
                }
                ("GET", "/api/session/{sessionID}/inbox") => {
                    let inbox = strict::<InboxListResponse>(exchange);
                    assert!(
                        inbox
                            .data
                            .iter()
                            .all(|entry| !matches!(entry.item, InboxItem::Unknown))
                    );
                }
                ("GET", "/api/session/{sessionID}/message") => {
                    let page = strict::<MessageListResponse>(exchange);
                    assert_message_page_is_fully_typed(name, &page);
                    message_pages += 1;
                }
                ("GET", "/api/model") => {
                    let models = strict::<ModelListResponse>(exchange);
                    assert!(
                        models
                            .data
                            .iter()
                            .all(|model| model.status != Some(ModelStatus::Unknown))
                    );
                }
                ("GET", "/api/model/default") => {
                    strict::<ModelDefaultResponse>(exchange);
                }
                ("GET", "/api/permission/request") => {
                    let requests = strict::<PermissionRequestListResponse>(exchange);
                    assert!(
                        requests
                            .data
                            .iter()
                            .all(|request| !request.action.is_empty()
                                && !matches!(request.source, Some(PermissionSource::Unknown)))
                    );
                }
                ("GET", "/api/session/{sessionID}/permission") => {
                    strict::<SessionPermissionListResponse>(exchange);
                }
                ("GET", "/api/session/{sessionID}/permission/{requestID}") => {
                    let request = strict::<Data<PermissionRequest>>(exchange).data;
                    assert!(request.offers_always(), "{name}");
                }
                ("GET", "/api/form") => {
                    strict::<FormListResponse>(exchange);
                }
                ("GET", "/api/session/{sessionID}/form") => {
                    strict::<SessionFormListResponse>(exchange);
                }
                ("POST", "/api/session/{sessionID}/form") => {
                    let form = strict::<Data<FormInfo>>(exchange).data;
                    assert!(!form.title.is_empty() && !form.fields.is_empty(), "{name}");
                }
                ("GET", "/api/session/{sessionID}/form/{formID}") => {
                    let form = strict::<FormDetailResponse>(exchange).data;
                    assert_eq!(form.state, FormState::Pending, "{name}");
                }
                (method, template) => panic!("{name}: unmapped fixture {method} {template}"),
            }
        }
        assert!(message_pages >= 19, "{message_pages}");
    }

    /// Event types seen in the captures that the client deliberately ignores.
    const IGNORED_CAPTURED_EVENTS: &[&str] = &["session.step.streamed"];

    #[test]
    fn every_captured_event_decodes_without_malformed_payloads() {
        let mut paths: Vec<_> = std::fs::read_dir(fixture_dir().join("events"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        paths.sort();
        assert!(paths.len() >= 14);
        let mut decoded = 0;
        for path in paths {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            for (index, line) in text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .enumerate()
            {
                let event =
                    parse_event(line).unwrap_or_else(|error| panic!("{file}:{index}: {error}"));
                let at = format!("{file}:{index} {}", event.type_);
                match decode_event(&event) {
                    EventKind::Malformed { type_, error } => panic!("{at}: {type_}: {error}"),
                    EventKind::Other(type_) => {
                        assert!(IGNORED_CAPTURED_EVENTS.contains(&type_.as_str()), "{at}")
                    }
                    EventKind::ServerConnected => assert_eq!(event.created, None, "{at}"),
                    EventKind::InboxEnqueued(data) => {
                        assert!(!matches!(data.item, InboxItem::Unknown), "{at}")
                    }
                    EventKind::TextEnded(data) | EventKind::ReasoningEnded(data) => {
                        assert!(!data.text.is_empty(), "{at}")
                    }
                    EventKind::ToolInputStarted(data) => assert!(!data.name.is_empty(), "{at}"),
                    EventKind::ToolInputEnded(data) => assert!(!data.text.is_empty(), "{at}"),
                    EventKind::ToolCalled(data) => assert!(!data.input.is_empty(), "{at}"),
                    EventKind::ToolSuccess(data) => assert!(!data.content.is_empty(), "{at}"),
                    EventKind::StepStarted(data) => assert!(data.started > 0, "{at}"),
                    EventKind::StepEnded(data) => assert!(data.tokens.total() > 0.0, "{at}"),
                    EventKind::StepFailed(data) => assert!(!data.error.kind.is_empty(), "{at}"),
                    EventKind::ExecutionFailed(data) => {
                        assert!(!data.error.message.is_empty(), "{at}")
                    }
                    EventKind::ExecutionInterrupted(data) => {
                        assert!(!data.reason.is_empty(), "{at}")
                    }
                    EventKind::RetryScheduled(data) => {
                        assert!(data.attempt > 0 && !data.error.message.is_empty(), "{at}")
                    }
                    EventKind::PermissionAsked(data) => assert!(
                        !data.action.is_empty()
                            && !matches!(data.source, Some(PermissionSource::Unknown)),
                        "{at}"
                    ),
                    EventKind::PermissionReplied(data) => assert!(!data.reply.is_empty(), "{at}"),
                    EventKind::FormCreated(data) => assert!(!data.form.title.is_empty(), "{at}"),
                    EventKind::ShellCreated(data) => assert!(
                        data.info.status == ShellStatus::Running
                            && data.info.session_id().is_some()
                            && data.info.time.started > 0,
                        "{at}"
                    ),
                    EventKind::ShellExited(data) => assert!(!data.id.is_empty(), "{at}"),
                    _ => {}
                }
                if !matches!(event.type_.as_str(), "server.connected") {
                    assert!(event.created.is_some(), "{at}");
                }
                decoded += 1;
            }
        }
        assert!(decoded > 300, "{decoded}");
    }
}
