use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use async_channel::{Receiver, Sender};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, RequestBuilder, Response},
    header::HeaderValue,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    credentials::CloudflareAccessCredentials,
    jobs::{ShellJob, ShellSnapshot},
    model::{ModelCatalog, Project, RunStatus, Session},
    pending::{self, PendingForm, PendingRequest, PendingSnapshot},
    protocol,
};

const MESSAGE_PAGE_SIZE: u32 = 80;
const SESSION_PAGE_SIZE: u32 = 200;
const MAX_SESSION_PAGES: usize = 1_000;
const MAX_ERROR_BODY_CHARS: usize = 500;
const MAX_ATTACHMENT_BYTES: u64 = protocol::MAX_ATTACHMENT_BYTES as u64;
/// Client cap: the `session.inbox.enqueued` echo carries every file as base64.
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 40 * 1024 * 1024;
const UI_EVENT_CAPACITY: usize = 4_096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Prompt uploads get [`REQUEST_TIMEOUT`] plus one second per this many
/// bytes (about 1 Mbit/s), up to [`MAX_PROMPT_TIMEOUT`].
const PROMPT_UPLOAD_BYTES_PER_SECOND: usize = 128 * 1024;
const MAX_PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    pub base_url: String,
    pub username: String,
    pub password: Option<String>,
    pub cloudflare_access: Option<CloudflareAccessCredentials>,
}

#[derive(Debug)]
pub enum Command {
    /// Pending permissions and forms are recovered only where work can be
    /// pending ([`pending::pending_directories`]): the locations of the
    /// open tabs `sessions` and of the sessions the server reports active,
    /// plus `directories` (open prompts and forms).
    Bootstrap {
        sessions: Vec<String>,
        directories: Vec<String>,
    },
    /// Refetches the pending permission and form lists of these locations.
    LoadPending {
        directories: Vec<String>,
    },
    /// Sessions outside the root list (running child sessions, shell owners
    /// and their parents) for the background jobs list.
    LoadSessionInfo {
        session_ids: Vec<String>,
    },
    /// `cursor: None` loads the newest page; `Some` the page before it.
    LoadMessages {
        session_id: String,
        cursor: Option<String>,
    },
    LoadModels {
        directory: String,
    },
    CreateSession {
        request_id: u64,
        directory: String,
        title: Option<String>,
    },
    RenameSession {
        request_id: u64,
        session_id: String,
        title: String,
    },
    /// Saves an explicit model pick on the session. Never sent for a
    /// displayed default.
    SelectModel {
        request_id: u64,
        session_id: String,
        model: protocol::ModelRef,
    },
    /// Carries no model or agent: the session's saved model and the server's
    /// default agent apply. `delivery` is `Some(Queue)` only for a queued
    /// prompt; a steered one uses the server default. `message_id` is the
    /// client-generated prompt `id` (`msg_…`), which also becomes the user
    /// message ID.
    SendPrompt {
        request_id: u64,
        message_id: String,
        session_id: String,
        text: String,
        attachments: Vec<PathBuf>,
        delivery: Option<protocol::Delivery>,
    },
    /// Stop: `POST /interrupt` without `resume`, which parks every waiting
    /// inbox item.
    Abort {
        session_id: String,
    },
    /// Acts on one waiting inbox item (a tray row).
    Inbox {
        session_id: String,
        inbox_id: String,
        request: InboxRequest,
    },
    /// `session_id` is the request's own session, possibly a child session.
    ReplyPermission {
        request_id: String,
        session_id: String,
        decision: protocol::PermissionDecision,
    },
    /// `directory` is required for a `"global"` owner's form.
    CancelForm {
        form_id: String,
        session_id: String,
        directory: Option<String>,
    },
}

/// A permission reply or form cancel the server accepted, or found already
/// settled (by another client or an earlier attempt). For inbox requests,
/// `AlreadyResolved` means the item was no longer waiting in that mode
/// (delivered, cancelled or switched meanwhile), so the client reconciles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    Done,
    AlreadyResolved,
}

/// What a tray row asks of the server for one waiting inbox item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxRequest {
    /// `PATCH .../inbox/{inboxID}` `{delivery}`; switching to steer wakes
    /// an idle session.
    SetDelivery(protocol::Delivery),
    /// `DELETE .../inbox/{inboxID}`.
    Cancel,
    /// Resume on a parked session, acting on the item with the given current
    /// delivery: it wakes the session for every parked item (steers together
    /// in the next turn, then each queued one as its own turn). Only a switch
    /// to steer wakes it that way, so a queued item is `PATCH`ed to steer and
    /// a steer is bounced, `PATCH`ed to queue and back to steer (it keeps its
    /// place: the inbox is ordered by enqueue sequence). The client never
    /// sends `POST /interrupt?resume=true`: it wakes the session for the
    /// parked steers only and leaves queued items parked.
    Resume(protocol::Delivery),
}

#[derive(Debug)]
pub struct Bootstrap {
    pub version: String,
    pub sessions: Vec<Session>,
    pub sessions_complete: bool,
    pub projects: Vec<Project>,
    pub projects_complete: bool,
    pub statuses: HashMap<String, RunStatus>,
    pub statuses_complete: bool,
    pub pending: Vec<PendingRequest>,
    /// Locations whose pending lists were fetched: an open prompt there
    /// missing from `pending` was resolved while disconnected. A failed
    /// list sets `retry_needed` and a warning.
    pub pending_covered: HashSet<String>,
    /// Running shells of the same locations as the pending lists.
    pub shells: ShellSnapshot,
    pub retry_needed: bool,
    pub warnings: Vec<String>,
}

/// One page of history, oldest entry first.
#[derive(Debug)]
pub struct MessagePage {
    pub messages: Vec<protocol::SessionMessage>,
    /// Cursor for the next older page; `None` once history is exhausted.
    pub next_cursor: Option<String>,
    /// Undelivered inbox items, fetched with the newest page only. `None`
    /// when not fetched or the fetch failed: queued rows are then kept as is.
    pub queued: Option<Vec<protocol::InboxEntry>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MessageLoadError {
    /// The server does not know the session (e.g. a saved v1 tab ID), so
    /// the tab is dropped quietly (R2.8).
    SessionNotFound,
    Failed(String),
}

impl MessageLoadError {
    fn from_error(error: anyhow::Error) -> Self {
        if error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<ApiFailure>())
            .any(ApiFailure::is_session_not_found)
        {
            Self::SessionNotFound
        } else {
            Self::Failed(format_error(error))
        }
    }
}

impl std::fmt::Display for MessageLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => formatter.write_str("Session not found"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

#[derive(Debug)]
pub struct ServerEnvelope {
    pub directory: Option<String>,
    pub payload: Value,
}

#[derive(Debug)]
pub enum UiEvent {
    Connection {
        connected: bool,
        error: Option<String>,
    },
    Bootstrap(Result<Bootstrap, String>),
    MessagesLoaded {
        session_id: String,
        cursor: Option<String>,
        result: Result<MessagePage, MessageLoadError>,
    },
    ModelsLoaded {
        directory: String,
        result: Result<ModelCatalog, String>,
    },
    SessionCreated {
        request_id: u64,
        result: Result<Session, String>,
    },
    SessionRenamed {
        request_id: u64,
        session_id: String,
        result: Result<Session, String>,
    },
    ModelSelected {
        request_id: u64,
        session_id: String,
        model: protocol::ModelRef,
        result: Result<(), String>,
    },
    PromptAccepted {
        request_id: u64,
        session_id: String,
        result: Result<(), String>,
    },
    Aborted {
        session_id: String,
        result: Result<(), String>,
    },
    InboxSettled {
        session_id: String,
        inbox_id: String,
        request: InboxRequest,
        result: Result<Settled, String>,
    },
    PendingLoaded(PendingSnapshot),
    SessionInfoLoaded(Vec<(String, Result<Session, String>)>),
    PermissionReplied {
        request_id: String,
        result: Result<Settled, String>,
    },
    FormCancelled {
        form_id: String,
        result: Result<Settled, String>,
    },
    ServerEvent(ServerEnvelope),
}

#[derive(Clone)]
pub struct ApiHandle {
    refresh_commands: Sender<Command>,
    interaction_commands: Sender<Command>,
    abort_commands: Sender<Command>,
    urgent_commands: Sender<Command>,
    _lifetime: Arc<ApiLifetime>,
}

struct ApiLifetime {
    alive: Arc<AtomicBool>,
}

impl Drop for ApiLifetime {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl ApiHandle {
    pub fn start(config: ApiConfig) -> Result<(Self, Receiver<UiEvent>, String)> {
        let api = Api::new(config)?;
        let server_key = api.base_url.as_str().trim_end_matches('/').to_owned();
        let (refresh_sender, refresh_receiver) = async_channel::unbounded();
        let (interaction_sender, interaction_receiver) = async_channel::unbounded();
        let (abort_sender, abort_receiver) = async_channel::unbounded();
        let (urgent_sender, urgent_receiver) = async_channel::unbounded();
        let (ui_sender, ui_receiver) = async_channel::bounded(UI_EVENT_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let lifetime = Arc::new(ApiLifetime {
            alive: alive.clone(),
        });

        spawn_command_worker(api.clone(), refresh_receiver, ui_sender.clone());
        spawn_command_worker(api.clone(), interaction_receiver, ui_sender.clone());
        spawn_command_worker(api.clone(), abort_receiver, ui_sender.clone());
        spawn_command_worker(api.clone(), urgent_receiver, ui_sender.clone());
        spawn_event_worker(api, ui_sender, alive);

        Ok((
            Self {
                refresh_commands: refresh_sender,
                interaction_commands: interaction_sender,
                abort_commands: abort_sender,
                urgent_commands: urgent_sender,
                _lifetime: lifetime,
            },
            ui_receiver,
            server_key,
        ))
    }

    pub fn preview() -> (Self, Receiver<UiEvent>, String) {
        let (refresh_sender, refresh_receiver) = async_channel::unbounded();
        let (interaction_sender, interaction_receiver) = async_channel::unbounded();
        let (abort_sender, abort_receiver) = async_channel::unbounded();
        let (urgent_sender, urgent_receiver) = async_channel::unbounded();
        let (ui_sender, ui_receiver) = async_channel::bounded(UI_EVENT_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let lifetime = Arc::new(ApiLifetime {
            alive: alive.clone(),
        });
        let mut preview = crate::preview::State::new();
        if std::env::var("OPENCODE_PREVIEW_HISTORY_PAGES").as_deref() == Ok("1") {
            preview.enable_paged_history();
        }
        let state = Arc::new(Mutex::new(preview));
        spawn_preview_worker(refresh_receiver, ui_sender.clone(), state.clone());
        spawn_preview_worker(interaction_receiver, ui_sender.clone(), state.clone());
        spawn_preview_worker(abort_receiver, ui_sender.clone(), state.clone());
        spawn_preview_worker(urgent_receiver, ui_sender.clone(), state);
        let _ = ui_sender.send_blocking(UiEvent::Connection {
            connected: true,
            error: None,
        });
        (
            Self {
                refresh_commands: refresh_sender,
                interaction_commands: interaction_sender,
                abort_commands: abort_sender,
                urgent_commands: urgent_sender,
                _lifetime: lifetime,
            },
            ui_receiver,
            crate::preview::SERVER_KEY.to_owned(),
        )
    }

    pub fn send(&self, command: Command) {
        let sender = match &command {
            Command::Bootstrap { .. }
            | Command::LoadPending { .. }
            | Command::LoadSessionInfo { .. }
            | Command::LoadMessages { .. }
            | Command::LoadModels { .. } => &self.refresh_commands,
            // One worker, so a model switch lands before a prompt sent after it.
            Command::CreateSession { .. }
            | Command::RenameSession { .. }
            | Command::SelectModel { .. }
            | Command::SendPrompt { .. } => &self.interaction_commands,
            Command::Abort { .. } => &self.abort_commands,
            Command::ReplyPermission { .. }
            | Command::CancelForm { .. }
            | Command::Inbox { .. } => &self.urgent_commands,
        };
        let _ = sender.send_blocking(command);
    }
}

/// The server's mount root: the configured URL with a trailing slash. The
/// base may carry a reverse-proxy mount prefix; API paths add `/api`
/// themselves, so a base that already ends in `/api` would double it.
pub(crate) fn mount_root(base_url: &str) -> Result<Url> {
    let mut base = base_url.trim().to_owned();
    if !base.ends_with('/') {
        base.push('/');
    }
    let mut url = Url::parse(&base).context("invalid OpenCode server URL")?;
    let mount = url.path().trim_end_matches('/');
    let mount = mount.strip_suffix(protocol::API_PREFIX).unwrap_or(mount);
    let mount = format!("{mount}/");
    url.set_path(&mount);
    Ok(url)
}

/// The same canonical mount-root identity used by `ApiHandle::start` for
/// per-server tabs and unread state. A configured terminal `/api` is not a
/// separate server from its mount root.
pub fn server_key(base_url: &str) -> Result<String> {
    Ok(mount_root(base_url)?
        .as_str()
        .trim_end_matches('/')
        .to_owned())
}

/// The server's own web UI, served at its mount root.
pub fn web_ui_url(base_url: &str) -> Result<String> {
    Ok(mount_root(base_url)?.to_string())
}

#[derive(Clone)]
struct Api {
    base_url: Url,
    client: Client,
    event_client: Client,
    username: String,
    password: Option<String>,
    cloudflare_access: Option<CloudflareAccessHeaders>,
}

#[derive(Clone)]
struct CloudflareAccessHeaders {
    client_id: HeaderValue,
    client_secret: HeaderValue,
}

impl Api {
    fn new(config: ApiConfig) -> Result<Self> {
        let base_url = mount_root(&config.base_url)?;
        if !matches!(base_url.scheme(), "http" | "https") {
            bail!("OpenCode server URL must use http or https");
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            bail!("put OpenCode credentials in the username and password options, not the URL");
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            bail!("OpenCode server URL must not contain a query string or fragment");
        }
        let loopback = match base_url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if base_url.scheme() == "http" && !loopback {
            bail!("remote servers require HTTPS; use HTTPS or an SSH tunnel to localhost");
        }
        if config.cloudflare_access.is_some() && base_url.scheme() != "https" {
            bail!("Cloudflare Access credentials require an HTTPS server URL");
        }
        let cloudflare_access = config
            .cloudflare_access
            .map(|credentials| {
                let client_id = HeaderValue::from_str(&credentials.client_id)
                    .context("Cloudflare Access client ID contains invalid characters")?;
                let mut client_secret = HeaderValue::from_str(&credentials.client_secret)
                    .context("Cloudflare Access client secret contains invalid characters")?;
                client_secret.set_sensitive(true);
                Ok::<_, anyhow::Error>(CloudflareAccessHeaders {
                    client_id,
                    client_secret,
                })
            })
            .transpose()?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("opencode-gpui/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize HTTP client")?;
        let event_client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(None)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("opencode-gpui/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize event-stream client")?;
        Ok(Self {
            base_url,
            client,
            event_client,
            username: config.username,
            password: config.password,
            cloudflare_access,
        })
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        let request = self.client.request(method, url);
        self.authenticate(request)
    }

    fn event_request(&self, method: Method, url: Url) -> RequestBuilder {
        let request = self.event_client.request(method, url);
        self.authenticate(request)
    }

    fn authenticate(&self, request: RequestBuilder) -> RequestBuilder {
        let request = match &self.cloudflare_access {
            Some(credentials) => request
                .header("CF-Access-Client-Id", credentials.client_id.clone())
                .header("CF-Access-Client-Secret", credentials.client_secret.clone()),
            None => request,
        };
        match &self.password {
            Some(password) => request.basic_auth(&self.username, Some(password)),
            None => request,
        }
    }

    /// `path` is an API path from the `protocol` helpers (`/api/...`). It is
    /// resolved below the base URL, so a reverse-proxy mount prefix is kept.
    fn url(&self, path: &str, query: &[(String, String)]) -> Result<Url> {
        let mut url = self
            .base_url
            .join(path.trim_start_matches('/'))
            .with_context(|| format!("invalid API path: {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    fn get<T: DeserializeOwned>(&self, path: &str, query: &[(String, String)]) -> Result<T> {
        let response = self
            .request(Method::GET, self.url(path, query)?)
            .send()
            .context("request failed")?;
        decode_json(response)
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        self.send_json_within(method, path, body, REQUEST_TIMEOUT)
    }

    fn send_json_within<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
        timeout: Duration,
    ) -> Result<T> {
        let response = self
            .request(method, self.url(path, &[])?)
            .timeout(timeout)
            .json(body)
            .send()
            .context("request failed")?;
        decode_json(response)
    }

    fn send_empty(&self, method: Method, path: &str, body: &impl Serialize) -> Result<()> {
        let response = self
            .request(method, self.url(path, &[])?)
            .json(body)
            .send()
            .context("request failed")?;
        expect_success(response).map(|_| ())
    }

    /// Settles a pending permission or form. Only a declared "already
    /// resolved" error counts as settled; any other failure, including a
    /// bare 404 from a missing route, is an error.
    fn settle_request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&impl Serialize>,
    ) -> Result<Settled> {
        let request = self.request(method, self.url(path, query)?);
        let request = match body {
            Some(body) => request.json(body),
            None => request,
        };
        let result = request
            .send()
            .context("request failed")
            .and_then(expect_success);
        match result {
            Ok(_) => Ok(Settled::Done),
            Err(error) if is_already_resolved(&error) => Ok(Settled::AlreadyResolved),
            Err(error) => Err(error),
        }
    }

    fn server_info(&self) -> Result<protocol::ServerInfo> {
        const REQUIRES_V2: &str = "this client requires OpenCode 2.x";
        let response = self
            .request(Method::GET, self.url(&protocol::info_path(), &[])?)
            .send()
            .context("request failed")?;
        if response.status() == StatusCode::NOT_FOUND {
            bail!("Server does not provide /api/info; {REQUIRES_V2}");
        }
        let body = expect_success(response)?
            .bytes()
            .context("failed to read server info")?;
        let info: protocol::ServerInfoResponse = protocol::decode(&body)
            .map_err(|_| anyhow!("Server did not return OpenCode server info; {REQUIRES_V2}"))?;
        if !info.is_v2() {
            bail!("Server is OpenCode {}; {REQUIRES_V2}", info.version);
        }
        Ok(info)
    }

    fn bootstrap(
        &self,
        open_sessions: &[String],
        extra_directories: &[String],
    ) -> Result<Bootstrap> {
        let mut warnings = Vec::new();
        let mut retry_needed = false;
        let version = self.server_info()?.version;

        let (projects, projects_complete) =
            match self.get::<protocol::ProjectListResponse>(&protocol::projects_path(), &[]) {
                Ok(projects) => (projects.iter().map(Project::from_info).collect(), true),
                Err(error) => {
                    retry_needed = true;
                    warnings.push(format!("Could not list every project: {error:#}"));
                    (Vec::new(), false)
                }
            };

        let (sessions, incomplete) = self.list_root_sessions()?;
        let sessions_complete = incomplete.is_none();
        if let Some(warning) = incomplete {
            retry_needed = true;
            warnings.push(warning);
        }

        let (statuses, statuses_complete) = match self.load_statuses() {
            Ok(statuses) => (statuses, true),
            Err(error) => {
                retry_needed = true;
                warnings.push(format!("Could not refresh session status: {error:#}"));
                (HashMap::new(), false)
            }
        };

        let directories = pending::pending_directories(
            &sessions,
            open_sessions
                .iter()
                .map(String::as_str)
                .chain(statuses.keys().map(String::as_str)),
            extra_directories.iter().map(String::as_str),
        );
        let pending = self.load_pending(&directories);
        if !pending.complete {
            retry_needed = true;
        }
        warnings.extend(pending.warnings);
        let mut shells = self.load_shells(&directories);
        if !shells.warnings.is_empty() {
            retry_needed = true;
        }
        warnings.append(&mut shells.warnings);

        Ok(Bootstrap {
            version,
            sessions,
            sessions_complete,
            projects,
            projects_complete,
            statuses,
            statuses_complete,
            pending: pending.requests,
            pending_covered: pending.covered,
            shells,
            retry_needed,
            warnings,
        })
    }

    /// Running shell commands of these locations (`location[directory]`,
    /// the same set as the pending lists, so no new location is started).
    /// A command seen in two locations is kept once. A location is covered
    /// once its list loaded; a failed one sets a warning.
    fn load_shells(&self, directories: &[String]) -> ShellSnapshot {
        let mut snapshot = ShellSnapshot::default();
        let mut seen = HashSet::new();
        let directories: BTreeSet<&String> = directories.iter().collect();
        for directory in directories.into_iter().filter(|d| !d.is_empty()) {
            snapshot.queried.insert(directory.clone());
            match self.get::<protocol::ShellListResponse>(
                &protocol::shells_path(),
                &[protocol::location_query(directory)],
            ) {
                Ok(list) => {
                    snapshot.covered.insert(directory.clone());
                    if !list.location.directory.is_empty() {
                        snapshot.covered.insert(list.location.directory.clone());
                    }
                    for info in &list.data {
                        if seen.insert(info.id.clone()) {
                            snapshot.shells.extend(ShellJob::running(info, directory));
                        }
                    }
                }
                Err(error) => snapshot.warnings.push(format!(
                    "Could not list running shell commands in {directory}: {error:#}"
                )),
            }
        }
        snapshot
    }

    /// `GET /api/session/{id}` for each session, in order; failures are per
    /// session.
    fn load_session_info(&self, session_ids: &[String]) -> Vec<(String, Result<Session, String>)> {
        session_ids
            .iter()
            .map(|id| {
                let result = self
                    .get::<protocol::SessionResponse>(&protocol::session_path(id), &[])
                    .map(|response| Session::from_info(&response.data))
                    .map_err(format_error);
                (id.clone(), result)
            })
            .collect()
    }

    /// Pending permissions and forms of these locations. Both lists are
    /// location-scoped (`location[directory]`) and include the requests of
    /// child sessions in that location. A request seen in two locations is
    /// kept once. A location is covered once both of its lists loaded.
    fn load_pending(&self, directories: &[String]) -> PendingSnapshot {
        let mut snapshot = PendingSnapshot {
            complete: true,
            ..PendingSnapshot::default()
        };
        let mut seen = HashSet::new();
        let directories: BTreeSet<&String> = directories.iter().collect();
        for directory in directories.into_iter().filter(|d| !d.is_empty()) {
            let location = [protocol::location_query(directory)];
            let mut covered = vec![directory.clone()];
            let mut listed = 0;
            let resolved = |located: &protocol::LocationRef| {
                if located.directory.is_empty() {
                    directory.clone()
                } else {
                    located.directory.clone()
                }
            };
            match self.get::<protocol::PermissionRequestListResponse>(
                &protocol::permission_requests_path(),
                &location,
            ) {
                Ok(list) => {
                    listed += 1;
                    let directory = resolved(&list.location);
                    covered.push(directory.clone());
                    for request in list.data {
                        if seen.insert(request.id.clone()) {
                            snapshot.requests.push(PendingRequest::Permission {
                                directory: directory.clone(),
                                request,
                            });
                        }
                    }
                }
                Err(error) => {
                    snapshot.complete = false;
                    snapshot.warnings.push(format!(
                        "Could not list pending permissions in {directory}: {error:#}"
                    ));
                }
            }
            match self.get::<protocol::FormListResponse>(&protocol::forms_path(), &location) {
                Ok(list) => {
                    listed += 1;
                    let directory = resolved(&list.location);
                    covered.push(directory.clone());
                    for form in list.data {
                        if seen.insert(form.id.clone()) {
                            snapshot.requests.push(PendingRequest::Form(PendingForm {
                                form,
                                directory: Some(directory.clone()),
                            }));
                        }
                    }
                }
                Err(error) => {
                    snapshot.complete = false;
                    snapshot.warnings.push(format!(
                        "Could not list pending forms in {directory}: {error:#}"
                    ));
                }
            }
            if listed == 2 {
                snapshot.covered.extend(covered);
            }
        }
        snapshot
    }

    /// Every root session across all locations and projects: an unfiltered
    /// `GET /api/session` spans the whole server. Pages run oldest first so a
    /// session updated while paging moves ahead of the cursor instead of
    /// behind it. Returns the sessions and, when the list may be partial, why.
    fn list_root_sessions(&self) -> Result<(Vec<Session>, Option<String>)> {
        let first = protocol::SessionListQuery {
            limit: Some(SESSION_PAGE_SIZE),
            order: Some(protocol::Order::Asc),
            ..protocol::SessionListQuery::roots()
        };
        let mut query = first.clone();
        let mut sessions_by_id: HashMap<String, Session> = HashMap::new();
        let mut seen_cursors = HashSet::new();
        let mut incomplete = None;
        for page_index in 0.. {
            if page_index == MAX_SESSION_PAGES {
                incomplete = Some("The server session list reached its page limit".to_owned());
                break;
            }
            let page = match self
                .get::<protocol::SessionListResponse>(&protocol::sessions_path(), &query.pairs())
            {
                Ok(page) => page,
                Err(error) if page_index == 0 => return Err(error),
                Err(error) => {
                    incomplete = Some(format!("Could not list every session: {error:#}"));
                    break;
                }
            };
            for info in page.data.iter().filter(|info| info.is_root()) {
                let session = Session::from_info(info);
                let replace = sessions_by_id
                    .get(&session.id)
                    .is_none_or(|existing| session.time.updated >= existing.time.updated);
                if replace {
                    sessions_by_id.insert(session.id.clone(), session);
                }
            }
            let Some(cursor) = page.next_cursor() else {
                break;
            };
            if !seen_cursors.insert(cursor.to_owned()) {
                incomplete = Some("The server repeated a session list cursor".to_owned());
                break;
            }
            query = first.with_cursor(cursor);
        }
        let mut sessions: Vec<_> = sessions_by_id.into_values().collect();
        sessions.retain(|session| session.time.archived.is_none());
        sessions.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
        Ok((sessions, incomplete))
    }

    /// `running` (or any future active kind) is busy; absent sessions are idle.
    fn load_statuses(&self) -> Result<HashMap<String, RunStatus>> {
        let active: protocol::SessionActiveResponse =
            self.get(&protocol::sessions_active_path(), &[])?;
        Ok(active
            .data
            .into_keys()
            .map(|session_id| (session_id, RunStatus::Busy))
            .collect())
    }

    /// Pages run newest first (the server default): the first page sends no
    /// `order`, older pages send only `cursor` and `limit`, because the server
    /// rejects `order` with a cursor and the cursor does not keep `limit`.
    /// Each page is reversed into chronological order. A short or empty page
    /// ends history, even though the server returns a cursor for any
    /// non-empty page.
    ///
    /// The newest page also fetches the inbox, *before* the history: an item
    /// delivered in between then shows up in both (and dedupes by ID) rather
    /// than in neither. Events after the inbox fetch are buffered by the UI
    /// and replayed. An inbox failure never fails the load.
    fn load_messages(&self, session_id: &str, cursor: Option<&str>) -> Result<MessagePage> {
        let queued = if cursor.is_none() {
            self.get::<protocol::InboxListResponse>(&protocol::session_inbox_path(session_id), &[])
                .map(|inbox| inbox.data)
                .ok()
        } else {
            None
        };
        let query = protocol::MessageListQuery {
            limit: Some(MESSAGE_PAGE_SIZE),
            order: None,
            cursor: cursor.map(str::to_owned),
        };
        let mut page: protocol::MessageListResponse = self
            .get(&protocol::session_messages_path(session_id), &query.pairs())
            .context("failed to load messages")?;
        let full = page.data.len() >= MESSAGE_PAGE_SIZE as usize;
        let next_cursor = page.next_cursor().filter(|_| full).map(str::to_owned);
        page.data.reverse();
        Ok(MessagePage {
            messages: page.data,
            next_cursor,
            queued,
        })
    }

    /// Both routes are location-scoped, so they take `location[directory]`;
    /// a plain `directory` would be ignored.
    fn load_models(&self, directory: &str) -> Result<ModelCatalog> {
        let location = [protocol::location_query(directory)];
        let models: protocol::ModelListResponse = self
            .get(&protocol::models_path(), &location)
            .context("failed to load models")?;
        let default: protocol::ModelDefaultResponse = self
            .get(&protocol::model_default_path(), &location)
            .context("failed to load the default model")?;
        Ok(ModelCatalog::from_models(
            &models.data,
            default.data.as_ref(),
        ))
    }

    fn select_model(&self, session_id: &str, model: &protocol::ModelRef) -> Result<()> {
        let body = protocol::SwitchModelBody {
            model: model.clone(),
        };
        self.send_empty(
            Method::POST,
            &protocol::session_model_path(session_id),
            &body,
        )
    }

    fn create_session(&self, directory: &str, title: Option<&str>) -> Result<Session> {
        let body = protocol::CreateSessionBody::new(directory, title.map(|t| t.trim().to_owned()));
        let created: protocol::SessionResponse =
            self.send_json(Method::POST, &protocol::sessions_path(), &body)?;
        Ok(Session::from_info(&created.data))
    }

    /// A blank title would make the server regenerate one, so it is refused.
    fn rename_session(&self, session_id: &str, title: &str) -> Result<Session> {
        let body =
            protocol::RenameSessionBody::new(title).context("session title cannot be blank")?;
        let path = protocol::session_path(session_id);
        self.send_empty(Method::PATCH, &path, &body)?;
        let renamed: protocol::SessionResponse = self
            .get(&path, &[])
            .context("renamed the session but could not reload it")?;
        Ok(Session::from_info(&renamed.data))
    }

    /// The response only confirms the prompt entered the session inbox.
    ///
    /// The timeout grows with the upload ([`prompt_timeout`]), so a large
    /// attachment is not cut off and sent again.
    ///
    /// A transport failure after the request may have reached the server is
    /// retried once with the identical body: the server treats a repeated
    /// `id` with the same content as the already-accepted prompt. HTTP error
    /// responses are final.
    fn send_prompt(
        &self,
        session_id: &str,
        message_id: String,
        text: String,
        attachments: &[PathBuf],
        delivery: Option<protocol::Delivery>,
    ) -> Result<protocol::InboxUser> {
        let body = protocol::PromptBody {
            id: message_id,
            text,
            files: encode_attachments(attachments)?,
            delivery: delivery.filter(|delivery| *delivery == protocol::Delivery::Queue),
        };
        let path = protocol::session_prompt_path(session_id);
        let timeout = prompt_timeout(
            body.text.len() + body.files.iter().map(|file| file.uri.len()).sum::<usize>(),
        );
        let send = || self.send_json_within(Method::POST, &path, &body, timeout);
        let accepted: protocol::PromptResponse = match send() {
            Err(error) if is_ambiguous_transport_failure(&error) => {
                send().context("retried after an interrupted send")?
            }
            result => result?,
        };
        Ok(accepted.data)
    }

    fn abort(&self, session_id: &str) -> Result<bool> {
        let response = self
            .request(
                Method::POST,
                self.url(&protocol::session_interrupt_path(session_id), &[])?,
            )
            .send()
            .context("request failed")?;
        let interrupted: protocol::InterruptResponse = decode_json(response)?;
        Ok(interrupted.interrupted)
    }

    /// One tray action on a waiting inbox item. A declared 409 (the item is
    /// no longer waiting in the other mode) or 404 is
    /// [`Settled::AlreadyResolved`]; the caller reconciles with the inbox.
    fn inbox_request(
        &self,
        session_id: &str,
        inbox_id: &str,
        request: InboxRequest,
    ) -> Result<Settled> {
        let item = protocol::session_inbox_item_path(session_id, inbox_id);
        let set = |delivery| {
            self.send_empty(
                Method::PATCH,
                &item,
                &protocol::InboxUpdateBody { delivery },
            )
        };
        let result = match request {
            InboxRequest::SetDelivery(delivery) => set(delivery),
            InboxRequest::Cancel => self
                .request(Method::DELETE, self.url(&item, &[])?)
                .send()
                .context("request failed")
                .and_then(expect_success)
                .map(|_| ()),
            InboxRequest::Resume(protocol::Delivery::Queue) => set(protocol::Delivery::Steer),
            InboxRequest::Resume(_) => {
                set(protocol::Delivery::Queue).and_then(|()| set(protocol::Delivery::Steer))
            }
        };
        match result {
            Ok(()) => Ok(Settled::Done),
            Err(error) if is_stale_inbox_item(&error) => Ok(Settled::AlreadyResolved),
            Err(error) => Err(error),
        }
    }

    fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: protocol::PermissionDecision,
    ) -> Result<Settled> {
        self.settle_request(
            Method::POST,
            &protocol::permission_reply_path(session_id, request_id),
            &[],
            Some(&protocol::PermissionReplyBody {
                decision,
                message: None,
            }),
        )
    }

    /// `DELETE` with no body; only a `"global"` form takes the location.
    fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        directory: Option<&str>,
    ) -> Result<Settled> {
        let query = match directory {
            Some(directory) => protocol::form_location_query(session_id, directory),
            None if session_id == protocol::GLOBAL_FORM_OWNER => {
                bail!("the form's location is unknown, so it cannot be cancelled here")
            }
            None => Vec::new(),
        };
        self.settle_request(
            Method::DELETE,
            &protocol::session_form_path(session_id, form_id),
            &query,
            None::<&()>,
        )
    }
}

fn spawn_preview_worker(
    commands: Receiver<Command>,
    ui: Sender<UiEvent>,
    state: Arc<Mutex<crate::preview::State>>,
) {
    thread::spawn(move || {
        while let Ok(command) = commands.recv_blocking() {
            let history_delay = if matches!(&command, Command::LoadMessages { cursor: None, .. }) {
                std::env::var("OPENCODE_PREVIEW_HISTORY_DELAY_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0)
            } else {
                0
            };
            let (event, server_events) = {
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                let event = state.handle(command);
                (event, state.take_server_events())
            };
            if history_delay > 0 {
                thread::sleep(Duration::from_millis(history_delay));
            }
            if std::iter::once(event)
                .chain(server_events)
                .any(|event| ui.send_blocking(event).is_err())
            {
                break;
            }
        }
    });
}

fn spawn_command_worker(api: Api, commands: Receiver<Command>, ui: Sender<UiEvent>) {
    thread::spawn(move || {
        while let Ok(command) = commands.recv_blocking() {
            let event = match command {
                Command::Bootstrap {
                    sessions,
                    directories,
                } => {
                    UiEvent::Bootstrap(api.bootstrap(&sessions, &directories).map_err(format_error))
                }
                Command::LoadPending { directories } => {
                    UiEvent::PendingLoaded(api.load_pending(&directories))
                }
                Command::LoadSessionInfo { session_ids } => {
                    UiEvent::SessionInfoLoaded(api.load_session_info(&session_ids))
                }
                Command::LoadMessages { session_id, cursor } => {
                    let result = api
                        .load_messages(&session_id, cursor.as_deref())
                        .map_err(MessageLoadError::from_error);
                    UiEvent::MessagesLoaded {
                        session_id,
                        cursor,
                        result,
                    }
                }
                Command::LoadModels { directory } => {
                    let result = api.load_models(&directory).map_err(format_error);
                    UiEvent::ModelsLoaded { directory, result }
                }
                Command::CreateSession {
                    request_id,
                    directory,
                    title,
                } => UiEvent::SessionCreated {
                    request_id,
                    result: api
                        .create_session(&directory, title.as_deref())
                        .map_err(format_error),
                },
                Command::RenameSession {
                    request_id,
                    session_id,
                    title,
                } => UiEvent::SessionRenamed {
                    request_id,
                    result: api
                        .rename_session(&session_id, &title)
                        .map_err(format_error),
                    session_id,
                },
                Command::SelectModel {
                    request_id,
                    session_id,
                    model,
                } => UiEvent::ModelSelected {
                    request_id,
                    result: api.select_model(&session_id, &model).map_err(format_error),
                    session_id,
                    model,
                },
                Command::SendPrompt {
                    request_id,
                    message_id,
                    session_id,
                    text,
                    attachments,
                    delivery,
                } => {
                    let result = api
                        .send_prompt(&session_id, message_id, text, &attachments, delivery)
                        .map(|_| ())
                        .map_err(format_error);
                    UiEvent::PromptAccepted {
                        request_id,
                        session_id,
                        result,
                    }
                }
                Command::Abort { session_id } => {
                    let result = api.abort(&session_id).map(|_| ()).map_err(format_error);
                    UiEvent::Aborted { session_id, result }
                }
                Command::Inbox {
                    session_id,
                    inbox_id,
                    request,
                } => UiEvent::InboxSettled {
                    result: api
                        .inbox_request(&session_id, &inbox_id, request)
                        .map_err(format_error),
                    session_id,
                    inbox_id,
                    request,
                },
                Command::ReplyPermission {
                    request_id,
                    session_id,
                    decision,
                } => {
                    let result = api
                        .reply_permission(&session_id, &request_id, decision)
                        .map_err(format_error);
                    UiEvent::PermissionReplied { request_id, result }
                }
                Command::CancelForm {
                    form_id,
                    session_id,
                    directory,
                } => {
                    let result = api
                        .cancel_form(&session_id, &form_id, directory.as_deref())
                        .map_err(format_error);
                    UiEvent::FormCancelled { form_id, result }
                }
            };
            if ui.send_blocking(event).is_err() {
                break;
            }
        }
    });
}

fn spawn_event_worker(api: Api, ui: Sender<UiEvent>, alive: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut delay = Duration::from_millis(500);
        let mut last_error = None;
        while alive.load(Ordering::Relaxed) {
            let mut connected = false;
            let attempt_started = Instant::now();
            let result = stream_events(&api, &ui, &mut connected);
            if !alive.load(Ordering::Relaxed) {
                return;
            }
            if connected {
                last_error = None;
                if attempt_started.elapsed() >= Duration::from_secs(30) {
                    delay = Duration::from_millis(500);
                }
            }
            let error = result.err().map(format_error);
            if error != last_error {
                let _ = ui.send_blocking(UiEvent::Connection {
                    connected: false,
                    error: error.clone(),
                });
                last_error = error;
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    });
}

fn stream_events(api: &Api, ui: &Sender<UiEvent>, connected: &mut bool) -> Result<()> {
    let response = expect_success(
        api.event_request(Method::GET, api.url(&protocol::events_path(), &[])?)
            .header("Accept", "text/event-stream")
            .send()
            .context("failed to connect event stream")?,
    )?;
    let mut reader = BufReader::new(response);
    let mut decoder = SseDecoder::default();
    let mut line = String::new();

    loop {
        if ui.is_closed() {
            return Ok(());
        }
        line.clear();
        if reader
            .read_line(&mut line)
            .context("failed to read event stream")?
            == 0
        {
            bail!("event stream closed");
        }
        if ui.is_closed() {
            return Ok(());
        }
        let Some(data) = decoder.push(&line) else {
            continue;
        };
        let payload: Value = serde_json::from_str(&data).context("invalid event payload")?;
        match event_frame(payload) {
            EventFrame::Connected => {
                *connected = true;
                let _ = ui.send_blocking(UiEvent::Connection {
                    connected: true,
                    error: None,
                });
            }
            EventFrame::Event(envelope) => {
                ui.send_blocking(UiEvent::ServerEvent(envelope))
                    .map_err(|_| anyhow!("UI event receiver closed"))?;
            }
            EventFrame::Ignored => {}
        }
    }
}

#[derive(Debug)]
enum EventFrame {
    Connected,
    Event(ServerEnvelope),
    Ignored,
}

/// Maps one `/api/event` frame. Consumers get the whole event as the payload
/// and the directory from `location`.
fn event_frame(payload: Value) -> EventFrame {
    let Ok(event) = protocol::Event::deserialize(&payload) else {
        return EventFrame::Ignored;
    };
    if event.type_ == "server.connected" {
        return EventFrame::Connected;
    }
    let directory = event
        .directory()
        .filter(|directory| !directory.is_empty())
        .map(str::to_owned);
    EventFrame::Event(ServerEnvelope { directory, payload })
}

#[derive(Default)]
struct SseDecoder {
    data: Vec<String>,
}

impl SseDecoder {
    fn push(&mut self, line: &str) -> Option<String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data).join("\n"));
        }
        if let Some(value) = line.strip_prefix("data:") {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
        None
    }
}

fn attachment_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_owned())
}

/// Adds one attachment's decoded size to `total`, enforcing the server's
/// per-file limit and the client's total cap.
fn add_attachment_size(path: &Path, size: u64, total: &mut u64) -> Result<()> {
    if size > MAX_ATTACHMENT_BYTES {
        bail!("{} is larger than 20 MiB", attachment_name(path));
    }
    *total += size;
    if *total > MAX_TOTAL_ATTACHMENT_BYTES {
        bail!("attachments are larger than 40 MiB in total");
    }
    Ok(())
}

/// Checks attachments by metadata only, so the UI can reject a draft before
/// sending it. [`encode_attachments`] repeats the checks on the bytes it reads.
pub fn check_attachments(paths: &[PathBuf]) -> Result<()> {
    let mut total = 0;
    for path in paths {
        let metadata = fs::metadata(path)
            .with_context(|| format!("failed to inspect attachment {}", path.display()))?;
        if !metadata.is_file() {
            bail!("attachment {} is not a regular file", path.display());
        }
        add_attachment_size(path, metadata.len(), &mut total)?;
    }
    Ok(())
}

/// Reads local files into `data:` URIs; `file:` URIs would resolve on the
/// server host. The MIME type is a hint: the server detects it from the bytes.
fn encode_attachments(paths: &[PathBuf]) -> Result<Vec<protocol::PromptFile>> {
    check_attachments(paths)?;
    let mut total = 0;
    paths
        .iter()
        .map(|path| {
            let file = File::open(path)
                .with_context(|| format!("failed to open attachment {}", path.display()))?;
            if !file
                .metadata()
                .with_context(|| format!("failed to inspect attachment {}", path.display()))?
                .is_file()
            {
                bail!("attachment {} is not a regular file", path.display());
            }
            let mut bytes = Vec::new();
            file.take(MAX_ATTACHMENT_BYTES + 1)
                .read_to_end(&mut bytes)
                .with_context(|| format!("failed to read attachment {}", path.display()))?;
            add_attachment_size(path, bytes.len() as u64, &mut total)?;
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Ok(protocol::PromptFile::from_bytes(
                &bytes,
                mime.as_ref(),
                Some(attachment_name(path)),
            ))
        })
        .collect()
}

/// Time allowed for a prompt request whose body is about `payload_bytes`.
fn prompt_timeout(payload_bytes: usize) -> Duration {
    let upload = (payload_bytes / PROMPT_UPLOAD_BYTES_PER_SECOND) as u64;
    (REQUEST_TIMEOUT + Duration::from_secs(upload)).min(MAX_PROMPT_TIMEOUT)
}

/// The request may have reached the server but no complete response came
/// back (timeout, reset, truncated body). A failure to connect is not
/// ambiguous, and neither is any HTTP error response.
fn is_ambiguous_transport_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|error| {
            !error.is_connect()
                && error.status().is_none()
                && (error.is_timeout()
                    || error.is_request()
                    || error.is_body()
                    || error.is_decode())
        })
}

/// A non-success HTTP response. Declared v2 failures carry a typed body.
#[derive(Debug)]
pub struct ApiFailure {
    pub status: StatusCode,
    pub error: Option<protocol::ApiError>,
    pub body: String,
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            Some(error) => formatter.write_str(error.display_message()),
            None if self.body.is_empty() => write!(formatter, "server returned {}", self.status),
            None => write!(formatter, "server returned {}: {}", self.status, self.body),
        }
    }
}

impl std::error::Error for ApiFailure {}

/// A declared `PermissionNotFoundError`, `FormNotFoundError` or
/// `FormAlreadySettledError`.
fn is_already_resolved(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ApiFailure>())
        .any(|failure| {
            failure
                .error
                .as_ref()
                .is_some_and(protocol::ApiError::is_already_resolved)
        })
}

/// A declared 409 or 404 on an inbox item: it was delivered, cancelled or
/// switched meanwhile. A bare 404 (a proxy, a missing route) is an error.
fn is_stale_inbox_item(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ApiFailure>())
        .any(|failure| {
            failure.error.is_some()
                && matches!(failure.status, StatusCode::CONFLICT | StatusCode::NOT_FOUND)
        })
}

impl ApiFailure {
    /// A declared `SessionNotFoundError`; a bare 404 (e.g. from a proxy) is not.
    pub fn is_session_not_found(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.kind() == protocol::ApiErrorKind::SessionNotFound)
    }
}

fn expect_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.bytes().unwrap_or_default();
    let error = protocol::decode_error_body(&body);
    let text = String::from_utf8_lossy(&body);
    let text = text.trim();
    let body = match text.char_indices().nth(MAX_ERROR_BODY_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    };
    Err(ApiFailure {
        status,
        error,
        body,
    }
    .into())
}

fn decode_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    let body = expect_success(response)?
        .bytes()
        .context("failed to read the server response")?;
    protocol::decode(&body).context("server returned an unexpected response")
}

/// Includes the cause chain, e.g. "request failed: <transport error>".
fn format_error(error: impl std::fmt::Display) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
    };

    use serde_json::json;

    use super::*;

    #[test]
    fn preview_handle_answers_without_a_server() {
        let (api, events, key) = ApiHandle::preview();
        assert_eq!(key, crate::preview::SERVER_KEY);
        api.send(Command::Bootstrap {
            sessions: Vec::new(),
            directories: Vec::new(),
        });
        api.send(Command::LoadModels {
            directory: "/repo".into(),
        });
        let mut saw_bootstrap = false;
        let mut saw_models = false;
        for _ in 0..16 {
            match events.recv_blocking() {
                Ok(UiEvent::Connection {
                    connected: true, ..
                }) => {}
                Ok(UiEvent::Bootstrap(Ok(bootstrap))) => {
                    assert_eq!(bootstrap.version, "preview");
                    assert!(!bootstrap.sessions.is_empty());
                    saw_bootstrap = true;
                }
                Ok(UiEvent::ModelsLoaded {
                    result: Ok(catalog),
                    ..
                }) => {
                    assert!(
                        catalog
                            .models
                            .iter()
                            .any(|model| model.supports_attachments)
                    );
                    saw_models = true;
                }
                Ok(_) => {}
                Err(_) => panic!("preview event channel closed"),
            }
            if saw_bootstrap && saw_models {
                return;
            }
        }
        panic!("missing preview bootstrap or models");
    }

    fn config(base_url: String, password: Option<&str>) -> ApiConfig {
        ApiConfig {
            base_url,
            username: "opencode".into(),
            password: password.map(str::to_owned),
            cloudflare_access: None,
        }
    }

    struct HttpRequest {
        line: String,
        body: String,
    }

    impl HttpRequest {
        /// Request target, e.g. `/api/session?limit=200`.
        fn target(&self) -> &str {
            self.line.split(' ').nth(1).unwrap_or_default()
        }

        fn path(&self) -> &str {
            self.target().split('?').next().unwrap_or_default()
        }

        fn query(&self) -> HashMap<String, String> {
            let query = self.target().split_once('?').map_or("", |(_, query)| query);
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap()
        }
    }

    fn read_http_request(stream: &TcpStream) -> HttpRequest {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        HttpRequest {
            line: request_line.trim_end().to_owned(),
            body: String::from_utf8(body).unwrap(),
        }
    }

    type Requests = Arc<Mutex<Vec<HttpRequest>>>;

    /// Answers exactly `count` requests with `respond(request) -> (status, body)`.
    fn serve(
        count: usize,
        respond: impl Fn(&HttpRequest) -> (u16, String) + Send + 'static,
    ) -> (String, Requests, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Requests = Arc::default();
        let captured = requests.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&stream);
                let (status, body) = respond(&request);
                captured.lock().unwrap().push(request);
                write!(
                    stream,
                    "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), requests, server)
    }

    fn ok(body: Value) -> (u16, String) {
        (200, body.to_string())
    }

    fn session_json(id: &str, directory: &str, title: &str, updated: i64) -> Value {
        json!({
            "id": id,
            "projectID": "prj",
            "time": { "created": 1, "updated": updated },
            "title": title,
            "location": { "directory": directory }
        })
    }

    #[test]
    fn decoder_joins_multiline_data_and_ignores_comments() {
        let mut decoder = SseDecoder::default();
        assert_eq!(decoder.push(": keepalive\n"), None);
        assert_eq!(decoder.push("data: {\"hello\":\n"), None);
        assert_eq!(decoder.push("data: \"world\"}\n"), None);
        assert_eq!(decoder.push("\n"), Some("{\"hello\":\n\"world\"}".into()));
    }

    #[test]
    fn v2_frames_map_to_connection_and_located_envelopes() {
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for line in [
            "data: {\"id\":\"evt_0\",\"type\":\"server.connected\",\"data\":{}}\n",
            "\n",
            ": heartbeat\n",
            "\n",
            "data: {\"id\":\"evt_1\",\"created\":5,\"type\":\"session.renamed\",\"location\":{\"directory\":\"/work\"},\"data\":{\"sessionID\":\"ses_a\",\"title\":\"T\"}}\n",
            "\n",
            "data: {\"id\":\"evt_2\",\"created\":6,\"type\":\"session.execution.started\",\"data\":{\"sessionID\":\"ses_a\"}}\n",
            "\n",
            "data: {\"unexpected\":true}\n",
            "\n",
        ] {
            if let Some(data) = decoder.push(line) {
                frames.push(event_frame(serde_json::from_str(&data).unwrap()));
            }
        }
        assert_eq!(frames.len(), 4);
        assert!(matches!(frames[0], EventFrame::Connected));
        let EventFrame::Event(renamed) = &frames[1] else {
            panic!("unexpected {:?}", frames[1]);
        };
        assert_eq!(renamed.directory.as_deref(), Some("/work"));
        assert_eq!(renamed.payload["data"]["title"], "T");
        let EventFrame::Event(started) = &frames[2] else {
            panic!("unexpected {:?}", frames[2]);
        };
        assert_eq!(started.directory, None);
        assert!(matches!(frames[3], EventFrame::Ignored));
    }

    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn decode_base64(data: &str) -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap()
    }

    #[test]
    fn attachments_become_canonical_padded_data_uris() {
        let directory = tempfile::tempdir().unwrap();
        let one = directory.path().join("one.txt");
        let two = directory.path().join("two.bin");
        let image = directory.path().join("a.png");
        fs::write(&one, b"h").unwrap();
        fs::write(&two, b"hi").unwrap();
        fs::write(&image, decode_base64(PIXEL)).unwrap();

        let files = encode_attachments(&[one, two, image]).unwrap();

        assert_eq!(
            files,
            vec![
                protocol::PromptFile {
                    uri: "data:text/plain;base64,aA==".into(),
                    name: Some("one.txt".into()),
                },
                protocol::PromptFile {
                    uri: "data:application/octet-stream;base64,aGk=".into(),
                    name: Some("two.bin".into()),
                },
                protocol::PromptFile {
                    uri: format!("data:image/png;base64,{PIXEL}"),
                    name: Some("a.png".into()),
                },
            ]
        );
        assert!(files.iter().all(|file| !file.uri.starts_with("file:")));
    }

    #[test]
    fn attachment_limits_are_checked_before_anything_is_read() {
        let directory = tempfile::tempdir().unwrap();
        let sparse = |name: &str, size: u64| {
            let path = directory.path().join(name);
            File::create(&path).unwrap().set_len(size).unwrap();
            path
        };
        let limit = sparse("limit.bin", MAX_ATTACHMENT_BYTES);
        let over = sparse("over.bin", MAX_ATTACHMENT_BYTES + 1);
        let error = |paths: &[PathBuf]| format_error(check_attachments(paths).unwrap_err());

        assert!(check_attachments(std::slice::from_ref(&limit)).is_ok());
        assert_eq!(
            error(std::slice::from_ref(&over)),
            "over.bin is larger than 20 MiB"
        );
        assert_eq!(
            format_error(encode_attachments(&[over]).unwrap_err()),
            "over.bin is larger than 20 MiB"
        );

        let chunk = 14 * 1024 * 1024;
        let chunks: Vec<_> = (0..3)
            .map(|index| sparse(&format!("chunk{index}.bin"), chunk))
            .collect();
        assert!(check_attachments(&chunks[..2]).is_ok());
        assert_eq!(
            error(&chunks),
            "attachments are larger than 40 MiB in total"
        );
        assert_eq!(
            format_error(encode_attachments(&chunks).unwrap_err()),
            "attachments are larger than 40 MiB in total"
        );

        let mut total = 0;
        assert!(add_attachment_size(&limit, MAX_ATTACHMENT_BYTES, &mut total).is_ok());
        assert!(add_attachment_size(&limit, MAX_ATTACHMENT_BYTES, &mut total).is_ok());
        assert!(add_attachment_size(&limit, 1, &mut total).is_err());

        let folder = directory.path().join("folder");
        fs::create_dir(&folder).unwrap();
        assert!(error(&[folder]).ends_with("is not a regular file"));
        assert!(
            error(&[directory.path().join("missing.png")])
                .starts_with("failed to inspect attachment")
        );
    }

    fn inbox_user(id: &str) -> Value {
        json!({
            "data": {
                "id": id,
                "sessionID": "ses_a",
                "time": { "created": 1 },
                "type": "user",
                "payload": { "text": "hello" },
                "delivery": "steer"
            }
        })
    }

    #[test]
    fn prompt_posts_the_id_text_and_data_files_only() {
        let captured = fixture("session.prompt.attachment");
        let message_id = captured["data"]["id"].as_str().unwrap().to_owned();
        let (base, requests, server) = serve(3, move |request| {
            assert_eq!(request.path(), "/api/session/ses_a/prompt");
            if request.json().get("files").is_some() {
                ok(captured.clone())
            } else {
                ok(inbox_user("msg_0d58b846f00123EB3tzqlo6tYt"))
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("a.png");
        fs::write(&image, decode_base64(PIXEL)).unwrap();
        let api = Api::new(config(base, Some("secret"))).unwrap();

        let accepted = api
            .send_prompt(
                "ses_a",
                message_id.clone(),
                "Describe the attached image.".into(),
                &[image],
                Some(protocol::Delivery::Steer),
            )
            .unwrap();
        let text_only = api
            .send_prompt(
                "ses_a",
                "msg_0d58b846f00123EB3tzqlo6tYt".into(),
                "Say hello.".into(),
                &[],
                None,
            )
            .unwrap();
        api.send_prompt(
            "ses_a",
            "msg_0d58b846f00123EB3tzqlo6tYt".into(),
            "Then this.".into(),
            &[],
            Some(protocol::Delivery::Queue),
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(accepted.id, message_id);
        assert_eq!(accepted.payload.files[0].mime, "image/png");
        assert_eq!(accepted.delivery, Some(protocol::Delivery::Steer));
        assert_eq!(text_only.id, "msg_0d58b846f00123EB3tzqlo6tYt");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line, "POST /api/session/ses_a/prompt HTTP/1.1");
        assert_eq!(
            requests[0].json(),
            json!({
                "id": message_id,
                "text": "Describe the attached image.",
                "files": [{ "uri": format!("data:image/png;base64,{PIXEL}"), "name": "a.png" }]
            }),
            "no agent, model or resume; a steer omits delivery"
        );
        assert_eq!(
            requests[1].json(),
            json!({ "id": "msg_0d58b846f00123EB3tzqlo6tYt", "text": "Say hello." })
        );
        assert_eq!(
            requests[2].json(),
            json!({ "id": "msg_0d58b846f00123EB3tzqlo6tYt", "text": "Then this.", "delivery": "queue" }),
            "only a queued prompt carries delivery"
        );
    }

    #[test]
    fn inbox_requests_use_their_routes_and_treat_conflicts_as_resolved() {
        let (base, requests, server) = serve(9, |request| {
            match (request.line.split(' ').next().unwrap(), request.target()) {
                ("PATCH", "/api/session/ses_a/inbox/msg_gone") => (
                    409,
                    json!({
                        "_tag": "ConflictError",
                        "message": "Pending input cannot change to steer: msg_gone",
                        "resource": "msg_gone"
                    })
                    .to_string(),
                ),
                ("DELETE", "/api/session/ses_a/inbox/msg_proxy") => (404, String::new()),
                _ => (204, String::new()),
            }
        });
        let api = Api::new(config(base, None)).unwrap();
        let steer = InboxRequest::SetDelivery(protocol::Delivery::Steer);
        assert_eq!(
            api.inbox_request("ses_a", "msg_1", steer).unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.inbox_request(
                "ses_a",
                "msg_1",
                InboxRequest::SetDelivery(protocol::Delivery::Queue)
            )
            .unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.inbox_request("ses_a", "msg_gone", steer).unwrap(),
            Settled::AlreadyResolved,
            "delivered, cancelled or switched meanwhile"
        );
        assert_eq!(
            api.inbox_request("ses_a", "msg_1", InboxRequest::Cancel)
                .unwrap(),
            Settled::Done
        );
        assert!(
            api.inbox_request("ses_a", "msg_proxy", InboxRequest::Cancel)
                .is_err(),
            "a bare 404 is an error, not a resolved item"
        );
        // Resume: a queued item is steered; a steer is bounced (queue, then
        // steer). Never `interrupt?resume=true`.
        assert_eq!(
            api.inbox_request(
                "ses_a",
                "msg_q",
                InboxRequest::Resume(protocol::Delivery::Queue)
            )
            .unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.inbox_request(
                "ses_a",
                "msg_s",
                InboxRequest::Resume(protocol::Delivery::Steer)
            )
            .unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.inbox_request(
                "ses_a",
                "msg_gone",
                InboxRequest::Resume(protocol::Delivery::Steer)
            )
            .unwrap(),
            Settled::AlreadyResolved,
            "a bounce that finds the item gone stops at the first 409"
        );
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        let lines: Vec<&str> = requests
            .iter()
            .map(|request| request.line.as_str())
            .collect();
        assert_eq!(
            lines,
            [
                "PATCH /api/session/ses_a/inbox/msg_1 HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_1 HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_gone HTTP/1.1",
                "DELETE /api/session/ses_a/inbox/msg_1 HTTP/1.1",
                "DELETE /api/session/ses_a/inbox/msg_proxy HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_q HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_s HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_s HTTP/1.1",
                "PATCH /api/session/ses_a/inbox/msg_gone HTTP/1.1",
            ]
        );
        let bodies: Vec<Value> = [0, 1, 5, 6, 7, 8]
            .into_iter()
            .map(|index| requests[index].json())
            .collect();
        let delivery = |delivery: &str| json!({ "delivery": delivery });
        assert_eq!(
            bodies,
            [
                delivery("steer"),
                delivery("queue"),
                delivery("steer"),
                delivery("queue"),
                delivery("steer"),
                delivery("queue"),
            ]
        );
        assert!(requests[3].body.is_empty());
    }

    /// Accepts `hang_ups` connections and closes each after reading the whole
    /// request, then answers one more with `answer` if given. Returns the
    /// request bodies and the listener.
    fn hang_up_server(
        hang_ups: usize,
        answer: Option<Value>,
    ) -> (String, std::thread::JoinHandle<(Vec<String>, TcpListener)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..hang_ups {
                let (stream, _) = listener.accept().unwrap();
                bodies.push(read_http_request(&stream).body);
            }
            if let Some(answer) = answer {
                let (mut stream, _) = listener.accept().unwrap();
                bodies.push(read_http_request(&stream).body);
                let body = answer.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            (bodies, listener)
        });
        (base, server)
    }

    #[test]
    fn prompt_timeout_grows_with_the_upload_up_to_five_minutes() {
        assert_eq!(prompt_timeout(0), REQUEST_TIMEOUT);
        assert_eq!(prompt_timeout(10_000), REQUEST_TIMEOUT);
        assert_eq!(
            prompt_timeout(4 * 1024 * 1024),
            REQUEST_TIMEOUT + Duration::from_secs(32)
        );
        // 40 MiB of files is about 54 MiB of base64.
        assert_eq!(prompt_timeout(54 * 1024 * 1024), MAX_PROMPT_TIMEOUT);
    }

    #[test]
    fn an_interrupted_prompt_is_retried_once_with_the_same_body() {
        let id = "msg_0d58b846f00123EB3tzqlo6tYt";
        let (base, server) = hang_up_server(1, Some(inbox_user(id)));
        let api = Api::new(config(base, None)).unwrap();
        let accepted = api
            .send_prompt("ses_a", id.into(), "hello".into(), &[], None)
            .unwrap();
        let (bodies, _) = server.join().unwrap();
        assert_eq!(accepted.id, id);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(
            serde_json::from_str::<Value>(&bodies[0]).unwrap(),
            json!({ "id": id, "text": "hello" })
        );
    }

    #[test]
    fn a_prompt_is_retried_at_most_once() {
        let (base, server) = hang_up_server(2, None);
        let api = Api::new(config(base, None)).unwrap();
        let error = api
            .send_prompt("ses_a", "msg_x".into(), "hello".into(), &[], None)
            .unwrap_err();
        let (bodies, listener) = server.join().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(
            format_error(&error).starts_with("retried after an interrupted send"),
            "{error:#}"
        );
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no third attempt");
    }

    #[test]
    fn prompt_error_responses_are_surfaced_without_a_retry() {
        let (base, requests, server) = serve(2, |request| {
            if request.json()["text"] == "reuse" {
                (
                    409,
                    json!({
                        "_tag": "ConflictError",
                        "message": "Prompt msg_a conflicts with an existing prompt"
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({
                        "_tag": "InvalidRequestError",
                        "message": "Attachment exceeds the 20971520 byte limit: a.png",
                        "field": "files"
                    })
                    .to_string(),
                )
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("a.png");
        fs::write(&image, decode_base64(PIXEL)).unwrap();
        let api = Api::new(config(base, None)).unwrap();

        let files_error = api
            .send_prompt("ses_a", "msg_a".into(), "look".into(), &[image], None)
            .unwrap_err();
        let conflict = api
            .send_prompt("ses_a", "msg_a".into(), "reuse".into(), &[], None)
            .unwrap_err();
        server.join().unwrap();

        let failure = files_error.downcast_ref::<ApiFailure>().unwrap();
        assert_eq!(failure.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            failure.error.as_ref().and_then(protocol::ApiError::field),
            Some("files")
        );
        assert_eq!(
            format_error(files_error),
            "Attachment exceeds the 20971520 byte limit: a.png"
        );
        assert_eq!(
            format_error(conflict),
            "Prompt msg_a conflicts with an existing prompt"
        );
        assert_eq!(requests.lock().unwrap().len(), 2, "one request per prompt");
    }

    #[test]
    fn interrupt_decodes_the_interrupted_flag() {
        let running = fixture("session.interrupt");
        let idle = fixture("session.interrupt.idle");
        let (base, requests, server) = serve(2, move |request| match request.path() {
            "/api/session/ses_run/interrupt" => ok(running.clone()),
            _ => ok(idle.clone()),
        });
        let api = Api::new(config(base, None)).unwrap();
        assert!(api.abort("ses_run").unwrap());
        assert!(!api.abort("ses_idle").unwrap());
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].line,
            "POST /api/session/ses_run/interrupt HTTP/1.1"
        );
        assert!(requests[0].body.is_empty());
    }

    #[test]
    fn rejects_credentials_in_urls_and_remote_cleartext_http() {
        assert!(Api::new(config("http://alice:secret@127.0.0.1:4096".into(), None)).is_err());
        assert!(Api::new(config("http://example.com:4096".into(), Some("secret"))).is_err());
        assert!(Api::new(config("http://example.com:4096".into(), None)).is_err());
        assert!(Api::new(config("http://127.0.0.1:4096".into(), Some("secret"))).is_ok());
        assert!(Api::new(config("https://example.com:4096".into(), Some("secret"))).is_ok());

        let mut cleartext_cloudflare = config("http://127.0.0.1:4096".into(), None);
        cleartext_cloudflare.cloudflare_access = Some(
            CloudflareAccessCredentials::new("client.access".into(), "secret".into()).unwrap(),
        );
        assert!(Api::new(cleartext_cloudflare).is_err());
    }

    #[test]
    fn the_web_ui_opens_at_the_mount_root() {
        for (base, expected) in [
            ("https://host", "https://host/"),
            (" https://host/api ", "https://host/"),
            ("https://host/api/", "https://host/"),
            ("https://host/prefix/api", "https://host/prefix/"),
            ("https://host/myapi", "https://host/myapi/"),
        ] {
            assert_eq!(web_ui_url(base).unwrap(), expected, "{base}");
        }
        assert!(web_ui_url("not a url").is_err());
    }

    #[test]
    fn api_urls_keep_the_mount_prefix_without_doubling_api() {
        for (base, expected) in [
            ("https://host", "https://host/api/info"),
            ("https://host/", "https://host/api/info"),
            ("https://host/prefix", "https://host/prefix/api/info"),
            ("https://host/prefix/", "https://host/prefix/api/info"),
            ("https://host/prefix/api", "https://host/prefix/api/info"),
            ("https://host/api/", "https://host/api/info"),
            ("https://host/myapi", "https://host/myapi/api/info"),
        ] {
            let api = Api::new(config(base.into(), None)).unwrap();
            assert_eq!(
                api.url(&protocol::info_path(), &[]).unwrap().as_str(),
                expected,
                "{base}"
            );
        }
        let api = Api::new(config("https://host/prefix".into(), None)).unwrap();
        let url = api
            .url(
                &protocol::session_path("ses a/b"),
                &[("limit".into(), "5".into())],
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://host/prefix/api/session/ses%20a%2Fb?limit=5"
        );
    }

    #[test]
    fn cloudflare_access_headers_cover_api_and_event_requests() {
        let mut config = config("https://opencode.example.com".into(), None);
        config.cloudflare_access = Some(
            CloudflareAccessCredentials::new("client.access".into(), "secret".into()).unwrap(),
        );
        let api = Api::new(config).unwrap();

        for (request, path) in [
            (
                api.request(Method::GET, api.url(&protocol::info_path(), &[]).unwrap()),
                "/api/info",
            ),
            (
                api.event_request(Method::GET, api.url(&protocol::events_path(), &[]).unwrap()),
                "/api/event",
            ),
        ] {
            let request = request.build().unwrap();
            assert_eq!(request.url().path(), path);
            assert_eq!(request.headers()["CF-Access-Client-Id"], "client.access");
            assert_eq!(request.headers()["CF-Access-Client-Secret"], "secret");
        }
    }

    #[test]
    fn permission_replies_post_the_decision_to_the_request_session() {
        let captured = serde_json::from_str::<Value>(
            &fs::read_to_string(format!(
                "{}/tests/fixtures/v2-2.0.8/session.permission.reply.child.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let (base, requests, server) = serve(4, |request| match request.path() {
            "/api/session/ses_child/permission/per_1/reply" => (204, String::new()),
            "/api/session/ses_a/permission/per_gone/reply" => (
                404,
                json!({
                    "_tag": "PermissionNotFoundError",
                    "requestID": "per_gone",
                    "message": "Permission request not found: per_gone"
                })
                .to_string(),
            ),
            // A route the server does not have: a bare 404 is not "resolved".
            "/api/session/ses_a/permission/per_route/reply" => (404, String::new()),
            _ => (
                400,
                json!({ "_tag": "InvalidRequestError", "message": "Expected decision" })
                    .to_string(),
            ),
        });
        let api = Api::new(config(base, None)).unwrap();
        use protocol::PermissionDecision::{Always, Once, Reject};

        assert_eq!(
            api.reply_permission("ses_child", "per_1", Once).unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.reply_permission("ses_a", "per_gone", Reject).unwrap(),
            Settled::AlreadyResolved
        );
        let route = api
            .reply_permission("ses_a", "per_route", Always)
            .unwrap_err();
        let invalid = api.reply_permission("ses_a", "per_bad", Once).unwrap_err();
        server.join().unwrap();

        assert_eq!(format_error(route), "server returned 404 Not Found");
        assert_eq!(format_error(invalid), "Expected decision");
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].line,
            "POST /api/session/ses_child/permission/per_1/reply HTTP/1.1"
        );
        assert_eq!(requests[0].json(), json!({ "decision": "once" }));
        assert_eq!(requests[0].json(), captured["request"]["body"]);
        assert_eq!(requests[1].json(), json!({ "decision": "reject" }));
        assert_eq!(requests[2].json(), json!({ "decision": "always" }));
        assert!(
            requests
                .iter()
                .all(|request| !request.target().contains('?'))
        );
    }

    #[test]
    fn form_cancel_deletes_and_treats_settled_forms_as_resolved() {
        let settled = fixture("session.form.cancel.again").to_string();
        let (base, requests, server) = serve(5, move |request| {
            match (request.path(), request.query().is_empty()) {
                ("/api/session/ses_a/form/frm_1", true) => (204, String::new()),
                ("/api/session/ses_a/form/frm_done", true) => (409, settled.clone()),
                ("/api/session/global/form/frm_mcp", false) => (204, String::new()),
                ("/api/session/ses_a/form/frm_gone", true) => (
                    404,
                    json!({ "_tag": "FormNotFoundError", "id": "frm_gone", "message": "gone" })
                        .to_string(),
                ),
                _ => (503, String::new()),
            }
        });
        let api = Api::new(config(base, None)).unwrap();

        assert_eq!(
            api.cancel_form("ses_a", "frm_1", Some("/repo")).unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.cancel_form("ses_a", "frm_done", None).unwrap(),
            Settled::AlreadyResolved
        );
        assert_eq!(
            api.cancel_form("global", "frm_mcp", Some("/my repo"))
                .unwrap(),
            Settled::Done
        );
        assert_eq!(
            api.cancel_form("ses_a", "frm_gone", Some("/repo")).unwrap(),
            Settled::AlreadyResolved
        );
        let unavailable = api
            .cancel_form("ses_a", "frm_x", Some("/repo"))
            .unwrap_err();
        assert!(api.cancel_form("global", "frm_mcp", None).is_err());
        server.join().unwrap();

        assert_eq!(
            format_error(unavailable),
            "server returned 503 Service Unavailable"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5, "no request without the global location");
        assert_eq!(
            requests[0].line,
            "DELETE /api/session/ses_a/form/frm_1 HTTP/1.1"
        );
        assert!(requests.iter().all(|request| request.body.is_empty()));
        assert_eq!(
            requests[2].query(),
            HashMap::from([("location[directory]".to_owned(), "/my repo".to_owned())])
        );
        assert!(
            requests
                .iter()
                .filter(|request| !request.path().contains("/global/"))
                .all(|request| request.query().is_empty())
        );
    }

    #[test]
    fn does_not_follow_api_redirects() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target.local_addr().unwrap();
        target.set_nonblocking(true).unwrap();
        let target_seen = Arc::new(AtomicBool::new(false));
        let seen = target_seen.clone();
        let target_server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match target.accept() {
                    Ok((mut stream, _)) => {
                        seen.store(true, Ordering::Relaxed);
                        read_http_request(&stream);
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("target server failed: {error}"),
                }
            }
        });

        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_address = redirect.local_addr().unwrap();
        let redirect_server = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            read_http_request(&stream);
            write!(
                stream,
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let api = Api::new(config(format!("http://{redirect_address}"), Some("secret"))).unwrap();
        assert!(api.server_info().is_err());
        redirect_server.join().unwrap();
        target_server.join().unwrap();
        assert!(!target_seen.load(Ordering::Relaxed));
    }

    #[test]
    fn bootstrap_pages_root_sessions_across_locations() {
        let (base, requests, server) = serve(12, |request| {
            let query = request.query();
            let location = query
                .get("location[directory]")
                .cloned()
                .unwrap_or_default();
            let shell = |id: &str, status: &str, owner: &str| {
                json!({
                    "id": id, "status": status, "command": "pnpm dev\nmore", "cwd": location,
                    "shell": "/bin/bash", "file": "/tmp/out", "metadata": { "sessionID": owner },
                    "time": { "started": 1_000 }
                })
            };
            match (request.path(), query.get("cursor").map(String::as_str)) {
                ("/api/shell", _) if location == "/a" => ok(json!({
                    "location": { "directory": location },
                    "data": [shell("sh_dev", "running", "ses_a"), shell("sh_done", "exited", "ses_a")]
                })),
                // The same command seen from a second location is kept once.
                ("/api/shell", _) => ok(json!({
                    "location": { "directory": location },
                    "data": [shell("sh_dev", "running", "ses_a"), shell("sh_child", "running", "ses_child_of_c")]
                })),
                ("/api/permission/request", _) if location == "/elsewhere" => ok(json!({
                    "location": { "directory": location },
                    "data": [{
                        "id": "per_child", "sessionID": "ses_child_of_c", "action": "shell",
                        "resources": ["ls"]
                    }]
                })),
                ("/api/form", _) if location == "/a" => ok(json!({
                    "location": { "directory": location },
                    "data": [{ "id": "frm_a", "sessionID": "ses_a", "title": "Pick" }]
                })),
                ("/api/permission/request" | "/api/form", _) => {
                    ok(json!({ "location": { "directory": location }, "data": [] }))
                }
                ("/api/info", _) => ok(json!({ "version": "2.0.8", "pid": 1 })),
                ("/api/project", _) => ok(json!([
                    { "id": "prj_a", "canonical": "/a", "name": "A", "sandboxes": [] },
                    { "id": "prj_b", "canonical": "/b" }
                ])),
                ("/api/session", None) => ok(json!({
                    "data": [
                        session_json("ses_a", "/a", "A", 10),
                        session_json("ses_b", "/b", "", 20)
                    ],
                    "cursor": { "previous": "p1", "next": "c1" }
                })),
                ("/api/session", Some("c1")) => ok(json!({
                    "data": [
                        session_json("ses_c", "/elsewhere", "C", 30),
                        session_json("ses_a", "/a", "A renamed", 40),
                        {
                            "id": "ses_archived",
                            "time": { "created": 1, "updated": 50, "archived": 60 },
                            "location": { "directory": "/a" }
                        }
                    ],
                    "cursor": { "next": "c2" }
                })),
                ("/api/session", Some("c2")) => ok(json!({ "data": [], "cursor": {} })),
                ("/api/session/active", _) => ok(json!({
                    "data": { "ses_c": { "type": "running" } }
                })),
                _ => panic!("unexpected request: {}", request.line),
            }
        });

        let api = Api::new(config(base, Some("secret"))).unwrap();
        let bootstrap = api
            .bootstrap(&["ses_a".into(), "ses_gone".into()], &[])
            .unwrap();
        server.join().unwrap();

        assert_eq!(bootstrap.version, "2.0.8");
        let ids: Vec<_> = bootstrap
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, ["ses_a", "ses_c", "ses_b"], "newest first");
        assert_eq!(bootstrap.sessions[0].title, "A renamed");
        assert_eq!(bootstrap.sessions[1].directory, "/elsewhere");
        assert_eq!(bootstrap.sessions[2].title, "Untitled session");
        assert_eq!(
            bootstrap.projects,
            [
                Project {
                    worktree: "/a".into(),
                    name: Some("A".into()),
                },
                Project {
                    worktree: "/b".into(),
                    name: None,
                },
            ]
        );
        assert_eq!(
            bootstrap.statuses,
            HashMap::from([("ses_c".to_owned(), RunStatus::Busy)])
        );
        assert!(bootstrap.sessions_complete);
        assert!(bootstrap.statuses_complete);
        assert_eq!(
            bootstrap.pending,
            [
                PendingRequest::Form(PendingForm {
                    form: serde_json::from_value(
                        json!({ "id": "frm_a", "sessionID": "ses_a", "title": "Pick" })
                    )
                    .unwrap(),
                    directory: Some("/a".into()),
                }),
                PendingRequest::Permission {
                    directory: "/elsewhere".into(),
                    request: serde_json::from_value(json!({
                        "id": "per_child", "sessionID": "ses_child_of_c", "action": "shell",
                        "resources": ["ls"]
                    }))
                    .unwrap(),
                },
            ],
            "a child session's request is recovered even though the child is not listed"
        );
        assert_eq!(
            bootstrap.pending_covered,
            HashSet::from(["/a".to_owned(), "/elsewhere".to_owned()])
        );
        assert!(!bootstrap.retry_needed);
        assert!(bootstrap.warnings.is_empty());
        let shells: Vec<_> = bootstrap
            .shells
            .shells
            .iter()
            .map(|shell| {
                (
                    shell.id.as_str(),
                    shell.directory.as_str(),
                    shell.session_id.as_deref(),
                    shell.started,
                )
            })
            .collect();
        assert_eq!(
            shells,
            [
                ("sh_dev", "/a", Some("ses_a"), 1_000),
                ("sh_child", "/elsewhere", Some("ses_child_of_c"), 1_000)
            ],
            "running only, deduped by ID"
        );
        assert_eq!(bootstrap.shells.covered, bootstrap.pending_covered);

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].path(), "/api/info");
        let pages: Vec<_> = requests
            .iter()
            .filter(|request| request.path() == "/api/session")
            .map(HttpRequest::query)
            .collect();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].get("parentID").map(String::as_str), Some("null"));
        assert_eq!(pages[0].get("order").map(String::as_str), Some("asc"));
        assert!(
            pages
                .iter()
                .all(|query| query.get("limit") == Some(&SESSION_PAGE_SIZE.to_string()))
        );
        assert_eq!(pages[1].get("cursor").map(String::as_str), Some("c1"));
        assert_eq!(pages[2].get("cursor").map(String::as_str), Some("c2"));
        let mut pending: Vec<_> = requests
            .iter()
            .filter(|request| {
                matches!(
                    request.path(),
                    "/api/permission/request" | "/api/form" | "/api/shell"
                )
            })
            .map(|request| {
                let query = request.query();
                assert_eq!(query.len(), 1, "{}", request.line);
                (
                    request.path().to_owned(),
                    query["location[directory]"].clone(),
                )
            })
            .collect();
        pending.sort();
        let expected: Vec<_> = ["/api/form", "/api/permission/request", "/api/shell"]
            .into_iter()
            .flat_map(|path| ["/a", "/elsewhere"].map(|dir| (path.to_owned(), dir.to_owned())))
            .collect();
        assert_eq!(
            pending, expected,
            "the open tab's and the active session's locations only, never project /b or idle ses_b's"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.query().contains_key("directory"))
        );
    }

    #[test]
    fn bootstrap_reports_a_partial_session_list() {
        let (base, _, server) = serve(8, |request| {
            match (request.path(), request.query().contains_key("cursor")) {
                ("/api/shell", _) => (503, String::new()),
                ("/api/permission/request", _) => ok(json!({
                    "location": { "directory": "/a" },
                    "data": [{ "id": "per_1", "sessionID": "ses_a" }]
                })),
                ("/api/form", _) => (503, String::new()),
                ("/api/info", _) => ok(json!({ "version": "2.0.8" })),
                ("/api/project", _) => (
                    500,
                    "{\"_tag\":\"UnknownError\",\"message\":\"boom\"}".into(),
                ),
                ("/api/session", false) => ok(json!({
                    "data": [session_json("ses_a", "/a", "A", 10)],
                    "cursor": { "next": "c1" }
                })),
                ("/api/session", true) => (503, "unavailable".into()),
                ("/api/session/active", _) => (503, String::new()),
                _ => panic!("unexpected request: {}", request.line),
            }
        });
        let api = Api::new(config(base, None)).unwrap();
        let bootstrap = api.bootstrap(&["ses_a".into()], &[]).unwrap();
        server.join().unwrap();

        assert_eq!(bootstrap.sessions.len(), 1);
        assert!(!bootstrap.sessions_complete);
        assert!(!bootstrap.statuses_complete);
        assert_eq!(bootstrap.pending.len(), 1, "what did load is still shown");
        assert!(
            bootstrap.pending_covered.is_empty(),
            "/a's form list failed, so nothing there may be dismissed"
        );
        assert!(bootstrap.retry_needed);
        assert!(bootstrap.projects.is_empty());
        assert_eq!(
            bootstrap.warnings,
            [
                "Could not list every project: boom",
                "Could not list every session: server returned 503 Service Unavailable: unavailable",
                "Could not refresh session status: server returned 503 Service Unavailable",
                "Could not list pending forms in /a: server returned 503 Service Unavailable",
                "Could not list running shell commands in /a: server returned 503 Service Unavailable",
            ]
        );
        assert!(bootstrap.shells.queried.contains("/a"));
        assert!(
            bootstrap.shells.covered.is_empty(),
            "a failed location keeps its shells"
        );
    }

    #[test]
    fn bootstrap_rejects_servers_that_are_not_opencode_2() {
        for (status, body, expected) in [
            (
                200,
                json!({ "version": "1.18.15" }).to_string(),
                "Server is OpenCode 1.18.15; this client requires OpenCode 2.x",
            ),
            (
                404,
                String::new(),
                "Server does not provide /api/info; this client requires OpenCode 2.x",
            ),
            (
                200,
                "<!doctype html>".to_owned(),
                "Server did not return OpenCode server info; this client requires OpenCode 2.x",
            ),
        ] {
            let (base, _, server) = serve(1, move |_| (status, body.clone()));
            let api = Api::new(config(base, None)).unwrap();
            let error = api.bootstrap(&[], &[]).unwrap_err();
            server.join().unwrap();
            assert_eq!(format_error(error), expected);
        }

        let (base, _, server) = serve(1, |_| (401, String::new()));
        let api = Api::new(config(base, None)).unwrap();
        let error = api.bootstrap(&[], &[]).unwrap_err();
        server.join().unwrap();
        assert_eq!(format_error(error), "server returned 401 Unauthorized");
    }

    #[test]
    fn create_session_posts_location_and_title_only() {
        let (base, requests, server) = serve(1, |_| {
            ok(json!({ "data": session_json("ses_new", "/a", "Scratch", 7) }))
        });
        let api = Api::new(config(base, None)).unwrap();
        let session = api.create_session("/a", Some("  Scratch  ")).unwrap();
        server.join().unwrap();

        assert_eq!(session.id, "ses_new");
        assert_eq!(session.directory, "/a");
        assert_eq!(session.title, "Scratch");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line, "POST /api/session HTTP/1.1");
        assert_eq!(
            requests[0].json(),
            json!({ "location": { "directory": "/a" }, "title": "Scratch" })
        );
    }

    #[test]
    fn model_routes_send_the_location_query() {
        let list = fixture("model.list");
        let default = fixture("model.default");
        let (base, requests, server) = serve(2, move |request| match request.path() {
            "/api/model" => ok(list.clone()),
            "/api/model/default" => ok(default.clone()),
            _ => panic!("unexpected request: {}", request.line),
        });
        let api = Api::new(config(base, None)).unwrap();
        let catalog = api.load_models("/Users/me/my repo").unwrap();
        server.join().unwrap();

        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog
                .preferred
                .as_ref()
                .map(|model| model.model_id.as_str()),
            Some("mock-model")
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request.line.starts_with("GET "), "{}", request.line);
            let query = request.query();
            assert_eq!(
                query.get("location[directory]").map(String::as_str),
                Some("/Users/me/my repo")
            );
            assert!(!query.contains_key("directory"), "{}", request.line);
            assert_eq!(query.len(), 1, "{}", request.line);
        }
    }

    #[test]
    fn missing_default_model_falls_back_to_the_first_model() {
        let (base, _, server) = serve(2, |request| match request.path() {
            "/api/model" => ok(fixture("model.list")),
            "/api/model/default" => ok(json!({ "location": { "directory": "/repo" } })),
            _ => panic!("unexpected request: {}", request.line),
        });
        let api = Api::new(config(base, None)).unwrap();
        let catalog = api.load_models("/repo").unwrap();
        server.join().unwrap();
        assert_eq!(
            catalog.preferred.map(|model| model.model_id),
            Some("mock-model".to_owned())
        );
    }

    #[test]
    fn select_model_posts_the_switch_body() {
        let captured = serde_json::from_str::<Value>(
            &fs::read_to_string(format!(
                "{}/tests/fixtures/v2-2.0.8/session.model.switch.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let (base, requests, server) = serve(2, |request| match request.path() {
            "/api/session/ses_a/model" => (204, String::new()),
            _ => (
                400,
                json!({ "_tag": "BadRequest", "message": "Unknown model" }).to_string(),
            ),
        });
        let api = Api::new(config(base, None)).unwrap();
        let model = protocol::ModelRef {
            id: "mock-model-alt".into(),
            provider_id: "mock".into(),
            variant: Some("high".into()),
        };
        api.select_model("ses_a", &model).unwrap();
        let error = api
            .select_model(
                "ses_b",
                &protocol::ModelRef {
                    variant: None,
                    ..model.clone()
                },
            )
            .unwrap_err();
        server.join().unwrap();

        assert_eq!(format_error(error), "Unknown model");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line, "POST /api/session/ses_a/model HTTP/1.1");
        assert_eq!(
            requests[0].json(),
            json!({ "model": { "id": "mock-model-alt", "providerID": "mock", "variant": "high" } })
        );
        assert_eq!(requests[0].json(), captured["request"]["body"]);
        assert_eq!(
            requests[1].json(),
            json!({ "model": { "id": "mock-model-alt", "providerID": "mock" } }),
            "no variant means the model's default variant"
        );
    }

    #[test]
    fn preview_saves_a_model_pick() {
        let mut state = crate::preview::State::new();
        let model = protocol::ModelRef {
            id: "claude-sonnet-4.6".into(),
            provider_id: "anthropic".into(),
            variant: None,
        };
        let UiEvent::ModelSelected {
            request_id: 7,
            result: Ok(()),
            model: echoed,
            ..
        } = state.handle(Command::SelectModel {
            request_id: 7,
            session_id: "ses_preview".into(),
            model: model.clone(),
        })
        else {
            panic!("preview model switch failed");
        };
        assert_eq!(echoed, model);
        let UiEvent::Bootstrap(Ok(bootstrap)) = state.handle(Command::Bootstrap {
            sessions: Vec::new(),
            directories: Vec::new(),
        }) else {
            panic!("preview bootstrap failed");
        };
        let session = bootstrap
            .sessions
            .iter()
            .find(|session| session.id == "ses_preview")
            .unwrap();
        assert_eq!(
            session
                .model_selection()
                .map(|selection| selection.to_ref()),
            Some(model)
        );
    }

    #[test]
    fn rename_session_patches_then_reloads() {
        let (base, requests, server) = serve(2, |request| match request.line.split(' ').next() {
            Some("PATCH") => (204, String::new()),
            Some("GET") => ok(json!({ "data": session_json("ses_a", "/a", "New title", 9) })),
            _ => panic!("unexpected request: {}", request.line),
        });
        let api = Api::new(config(base, None)).unwrap();
        let session = api.rename_session("ses_a", " New title ").unwrap();
        server.join().unwrap();

        assert_eq!(session.title, "New title");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line, "PATCH /api/session/ses_a HTTP/1.1");
        assert_eq!(requests[0].json(), json!({ "title": "New title" }));
        assert_eq!(requests[1].line, "GET /api/session/ses_a HTTP/1.1");
    }

    #[test]
    fn blank_rename_is_refused_before_any_request() {
        let api = Api::new(config("http://127.0.0.1:9".into(), None)).unwrap();
        let error = api.rename_session("ses_a", "   ").unwrap_err();
        assert_eq!(format_error(error), "session title cannot be blank");
    }

    #[test]
    fn error_bodies_surface_the_server_message() {
        let (base, _, server) = serve(2, |request| match request.line.split(' ').next() {
            Some("PATCH") => (
                404,
                json!({
                    "_tag": "SessionNotFoundError",
                    "sessionID": "ses_gone",
                    "message": "Session not found: ses_gone"
                })
                .to_string(),
            ),
            _ => (502, "bad gateway".into()),
        });
        let api = Api::new(config(base, None)).unwrap();
        let error = api.rename_session("ses_gone", "Title").unwrap_err();
        let failure = error.downcast_ref::<ApiFailure>().unwrap();
        assert_eq!(failure.status, StatusCode::NOT_FOUND);
        assert_eq!(
            failure.error.as_ref().map(protocol::ApiError::kind),
            Some(protocol::ApiErrorKind::SessionNotFound)
        );
        assert_eq!(format_error(error), "Session not found: ses_gone");

        let error = api.create_session("/a", None).unwrap_err();
        server.join().unwrap();
        assert_eq!(
            format_error(error),
            "server returned 502 Bad Gateway: bad gateway"
        );
    }

    #[test]
    fn job_sessions_and_shells_load_per_item() {
        let child = fixture("session.get.child");
        let active = fixture("session.active.running");
        let (base, requests, server) = serve(5, move |request| {
            let location = request.query().get("location[directory]").cloned();
            match (request.path(), location.as_deref()) {
                ("/api/session/ses_f2a7444aaffemNHKVCr3zoKjZZ", None) => ok(child.clone()),
                ("/api/session/ses_gone", None) => (
                    404,
                    json!({ "_tag": "SessionNotFoundError", "sessionID": "ses_gone",
                            "message": "Session not found: ses_gone" })
                    .to_string(),
                ),
                ("/api/session/active", None) => ok(active.clone()),
                ("/api/shell", Some("/ok")) => ok(json!({
                    "location": { "directory": "/ok" },
                    "data": [{ "id": "sh_1", "status": "running", "command": "sleep 9",
                               "cwd": "/ok", "shell": "sh", "file": "/tmp/f", "metadata": {},
                               "time": { "started": 5 } }]
                })),
                ("/api/shell", Some("/broken")) => (500, String::new()),
                _ => panic!("unexpected request: {}", request.line),
            }
        });
        let api = Api::new(config(base, None)).unwrap();
        let results =
            api.load_session_info(&["ses_f2a7444aaffemNHKVCr3zoKjZZ".into(), "ses_gone".into()]);
        let statuses = api.load_statuses().unwrap();
        let shells = api.load_shells(&["/ok".into(), "/broken".into(), String::new()]);
        server.join().unwrap();

        let child = results[0].1.as_ref().unwrap();
        assert_eq!(child.title, "Mock child task");
        assert_eq!(
            child.parent_id.as_deref(),
            Some("ses_f2a7444ccffe7yJDIJFaA9PlRV")
        );
        assert_eq!(child.time.created, 1_790_289_099_609);
        assert_eq!(results[1].0, "ses_gone");
        assert!(
            results[1]
                .1
                .as_ref()
                .unwrap_err()
                .contains("Session not found")
        );
        assert_eq!(
            statuses,
            HashMap::from([("ses_f2a743e02ffe9b0pzkusFTSsVL".to_owned(), RunStatus::Busy)])
        );
        assert_eq!(
            shells.shells,
            [ShellJob {
                id: "sh_1".into(),
                directory: "/ok".into(),
                command: "sleep 9".into(),
                session_id: None,
                started: 5,
            }]
        );
        assert_eq!(
            shells.queried,
            BTreeSet::from(["/broken".to_owned(), "/ok".to_owned()])
        );
        assert_eq!(shells.covered, HashSet::from(["/ok".to_owned()]));
        assert_eq!(shells.warnings.len(), 1, "{:?}", shells.warnings);
        let requests = requests.lock().unwrap();
        for request in requests
            .iter()
            .filter(|request| request.path() == "/api/shell")
        {
            assert_eq!(
                request.query().keys().collect::<Vec<_>>(),
                ["location[directory]"],
                "{}",
                request.line
            );
        }
    }

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/v2-2.0.8/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str::<Value>(&text).unwrap()["response"]["body"].take()
    }

    #[test]
    fn captured_fixtures_bootstrap_through_the_client() {
        let info = fixture("info");
        let projects = fixture("project.list");
        let page1 = fixture("session.list.root.page1");
        let page2 = fixture("session.list.root.page2");
        let active = fixture("session.active.running");
        let page1_next = page1["cursor"]["next"].as_str().unwrap().to_owned();
        let expected_sessions = page1["data"]
            .as_array()
            .unwrap()
            .iter()
            .chain(page2["data"].as_array().unwrap())
            .map(|session| session["id"].as_str().unwrap())
            .collect::<HashSet<_>>()
            .len();
        let running = active["data"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();

        let child_permissions = fixture("permission.request.list.child");
        let forms = fixture("form.list");
        let (base, _, server) = serve(9, move |request| {
            let cursor = request.query().get("cursor").cloned();
            let workspace = request
                .query()
                .get("location[directory]")
                .is_some_and(|dir| dir == "/state/workspace");
            match (request.path(), cursor) {
                // Only the running session's location; never project /state/home.
                ("/api/permission/request", _) if workspace => ok(child_permissions.clone()),
                ("/api/form", _) if workspace => ok(forms.clone()),
                ("/api/shell", _) if workspace => ok(json!({
                    "location": { "directory": "/state/workspace" }, "data": []
                })),
                ("/api/info", _) => ok(info.clone()),
                ("/api/project", _) => ok(projects.clone()),
                ("/api/session", None) => ok(page1.clone()),
                ("/api/session", Some(cursor)) if cursor == page1_next => ok(page2.clone()),
                ("/api/session", Some(_)) => ok(json!({ "data": [] })),
                ("/api/session/active", _) => ok(active.clone()),
                _ => panic!("unexpected request: {}", request.line),
            }
        });
        let api = Api::new(config(base, Some("secret"))).unwrap();
        let bootstrap = api.bootstrap(&[], &[]).unwrap();
        server.join().unwrap();

        assert_eq!(bootstrap.version, "2.0.8");
        assert!(bootstrap.sessions_complete && bootstrap.statuses_complete);
        assert!(bootstrap.warnings.is_empty(), "{:?}", bootstrap.warnings);
        assert_eq!(bootstrap.sessions.len(), expected_sessions);
        assert!(bootstrap.sessions.iter().all(|session| {
            session.directory.starts_with('/')
                && !session.title.is_empty()
                && session.parent_id.is_none()
                && session.time.updated >= session.time.created
        }));
        assert!(
            bootstrap
                .projects
                .iter()
                .all(|project| project.worktree.starts_with('/'))
        );
        assert_eq!(bootstrap.statuses.get(&running), Some(&RunStatus::Busy));
        assert_eq!(
            bootstrap.pending_covered,
            HashSet::from(["/state/workspace".to_owned()])
        );
        let ids: Vec<_> = bootstrap.pending.iter().map(PendingRequest::id).collect();
        assert_eq!(
            ids,
            [
                "per_0d58bc8dd001QchLXJK02UCCAr",
                "frm_0d58bede7001Oe4MviTBLpPVWZ"
            ]
        );
        let PendingRequest::Permission { directory, request } = &bootstrap.pending[0] else {
            panic!("unexpected {:?}", bootstrap.pending[0]);
        };
        assert_eq!(directory, "/state/workspace");
        assert!(
            bootstrap
                .sessions
                .iter()
                .all(|session| session.id != request.session_id),
            "the request belongs to a child session outside the root list"
        );

        for name in ["session.get", "session.create", "session.get.renamed"] {
            let response: protocol::SessionResponse =
                serde_json::from_value(fixture(name)).unwrap();
            let session = Session::from_info(&response.data);
            assert!(session.directory.starts_with('/'), "{name}");
        }
    }

    fn user_entry(index: usize) -> Value {
        json!({
            "id": format!("msg_{index:04}"),
            "type": "user",
            "time": { "created": index },
            "text": format!("turn {index}")
        })
    }

    /// Entries `from..to`, newest first as the server sends them.
    fn newest_first(from: usize, to: usize) -> Vec<Value> {
        (from..to).rev().map(user_entry).collect()
    }

    #[test]
    fn message_history_pages_newest_first_and_reverses_each_page() {
        let size = MESSAGE_PAGE_SIZE as usize;
        let queued = fixture("session.inbox.list.afterInterrupt");
        let (base, requests, server) = serve(4, move |request| {
            if request.path() == "/api/session/ses_a/inbox" {
                return ok(queued.clone());
            }
            assert_eq!(request.path(), "/api/session/ses_a/message");
            match request.query().get("cursor").map(String::as_str) {
                None => ok(json!({
                    "data": newest_first(size + 5, 2 * size + 5),
                    "cursor": { "previous": "p0", "next": "c1" }
                })),
                Some("c1") => ok(json!({
                    "data": newest_first(5, size + 5),
                    "cursor": { "previous": "p1", "next": "c2" }
                })),
                Some("c2") => {
                    ok(json!({ "data": [], "cursor": { "previous": null, "next": null } }))
                }
                Some(other) => panic!("unexpected cursor {other}"),
            }
        });
        let api = Api::new(config(base, None)).unwrap();
        let first = api.load_messages("ses_a", None).unwrap();
        let second = api
            .load_messages("ses_a", first.next_cursor.as_deref())
            .unwrap();
        let last = api
            .load_messages("ses_a", second.next_cursor.as_deref())
            .unwrap();
        server.join().unwrap();

        assert_eq!(first.next_cursor.as_deref(), Some("c1"));
        assert_eq!(second.next_cursor.as_deref(), Some("c2"));
        let queued = first.queued.as_deref().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, "msg_0d58bd03f001dhaJLZcYj9eH2D");
        assert!(
            second.queued.is_none() && last.queued.is_none(),
            "only the newest page"
        );
        assert!(last.messages.is_empty());
        assert_eq!(last.next_cursor, None, "the empty final page ends history");
        let ids = |page: &MessagePage| -> Vec<String> {
            page.messages
                .iter()
                .map(|entry| entry.id().unwrap().to_owned())
                .collect()
        };
        assert_eq!(ids(&first).first().unwrap(), "msg_0085");
        assert_eq!(ids(&first).last().unwrap(), "msg_0164");
        assert!(ids(&second).windows(2).all(|pair| pair[0] < pair[1]));

        let mut conversation = crate::model::Conversation::default();
        conversation.replace_from_api(&first.messages, first.next_cursor.clone());
        conversation.prepend_from_api(&second.messages, second.next_cursor.clone());
        conversation.prepend_from_api(&last.messages, last.next_cursor.clone());
        assert_eq!(conversation.next_cursor, None);
        let bodies: Vec<String> = conversation
            .transcript_rows()
            .iter()
            .map(|row| {
                serde_json::from_str::<Value>(row).unwrap()["body"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let expected: Vec<String> = (5..2 * size + 5)
            .map(|index| format!("turn {index}"))
            .collect();
        assert_eq!(
            bodies, expected,
            "chronological after reversing and prepending"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].path(),
            "/api/session/ses_a/inbox",
            "the inbox is fetched before the history"
        );
        let queries: Vec<_> = requests[1..].iter().map(HttpRequest::query).collect();
        assert_eq!(queries[0].get("limit"), Some(&size.to_string()));
        assert!(!queries[0].contains_key("cursor"));
        for (query, cursor) in queries[1..].iter().zip(["c1", "c2"]) {
            assert_eq!(query.get("cursor").map(String::as_str), Some(cursor));
            assert_eq!(query.get("limit"), Some(&size.to_string()));
        }
        assert!(queries.iter().all(|query| !query.contains_key("order")
            && !query.contains_key("before")
            && !query.contains_key("directory")));
    }

    #[test]
    fn a_short_message_page_ends_history() {
        let (base, _, server) = serve(2, |request| {
            if request.path().ends_with("/inbox") {
                // An inbox failure never fails the history load.
                return (503, String::new());
            }
            ok(json!({ "data": newest_first(0, 3), "cursor": { "previous": "p", "next": "n" } }))
        });
        let api = Api::new(config(base, None)).unwrap();
        let page = api.load_messages("ses_a", None).unwrap();
        server.join().unwrap();
        assert!(page.queued.is_none());
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].id(), Some("msg_0000"));
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn only_a_declared_missing_session_is_dropped() {
        let not_found = fixture("error.404.session").to_string();
        let (base, _, server) = serve(6, move |request| match request.path() {
            "/api/session/ses_gone/message" => (404, not_found.clone()),
            "/api/session/ses_proxy/message" => (404, "not found".into()),
            _ => (503, String::new()),
        });
        let api = Api::new(config(base, None)).unwrap();
        let load = |id: &str| {
            api.load_messages(id, None)
                .map_err(MessageLoadError::from_error)
                .unwrap_err()
        };
        assert_eq!(load("ses_gone"), MessageLoadError::SessionNotFound);
        assert_eq!(
            load("ses_proxy"),
            MessageLoadError::Failed(
                "failed to load messages: server returned 404 Not Found: not found".into()
            )
        );
        assert!(matches!(load("ses_busy"), MessageLoadError::Failed(_)));
        server.join().unwrap();
    }

    #[test]
    fn preview_pages_typed_history() {
        let mut state = crate::preview::State::new();
        let UiEvent::MessagesLoaded {
            result: Ok(page),
            cursor: None,
            ..
        } = state.handle(Command::LoadMessages {
            session_id: "ses_preview".into(),
            cursor: None,
        })
        else {
            panic!("preview history failed");
        };
        assert!(
            page.messages
                .iter()
                .any(|entry| matches!(entry, protocol::SessionMessage::Assistant(_)))
        );
    }
}
