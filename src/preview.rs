use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::{
    api::{Bootstrap, Command, InboxRequest, MessagePage, ServerEnvelope, Settled, UiEvent},
    jobs::{ShellJob, ShellSnapshot},
    model::{ModelCatalog, Project, RunStatus, Session, SessionModel},
    pending::{PendingForm, PendingRequest, PendingSnapshot},
    persist::{PersistedTab, ServerState},
    protocol,
};

pub const SERVER_KEY: &str = "preview://opencode-gpui";

const DIRECTORY: &str = "/repo";
const ACTIVE_ID: &str = "ses_preview";
const OTHER_ID: &str = "ses_other";
/// A running session with a steered and a queued message waiting.
const RUNNING_ID: &str = "ses_running";
/// A stopped session whose waiting messages are parked.
const PARKED_ID: &str = "ses_parked";
/// A running background subagent of the active session.
const CHILD_ID: &str = "ses_preview_child";
/// An idle, read session with nothing running.
const IDLE_ID: &str = "ses_idle";
const CREATED: u64 = 1_704_067_200_000;

pub fn server_state() -> ServerState {
    ServerState {
        tabs: vec![
            PersistedTab {
                id: ACTIVE_ID.into(),
                directory: DIRECTORY.into(),
                title: "Fix the attach clip padding".into(),
            },
            PersistedTab {
                id: OTHER_ID.into(),
                directory: DIRECTORY.into(),
                title: "SSH tunnel notes".into(),
            },
            PersistedTab {
                id: RUNNING_ID.into(),
                directory: DIRECTORY.into(),
                title: "Refactor the retry logic".into(),
            },
            PersistedTab {
                id: PARKED_ID.into(),
                directory: DIRECTORY.into(),
                title: "Stopped with parked messages".into(),
            },
            PersistedTab {
                id: IDLE_ID.into(),
                directory: DIRECTORY.into(),
                title: "Release checklist".into(),
            },
        ],
        active: Some(ACTIVE_ID.into()),
        selections: HashMap::new(),
        // With the canned jobs, every tab indicator shows: the active tab's
        // grey gear (jobs only), the other tab's blue gear (jobs, unread),
        // the running tab's orange gear, the parked tab's blue dot (unread)
        // and the idle tab's grey dot.
        unread: HashSet::from([OTHER_ID.to_owned(), PARKED_ID.to_owned()]),
        busy: HashSet::new(),
    }
}

pub struct State {
    sessions: Vec<Session>,
    /// v2 message-list entries per session, oldest first.
    messages: HashMap<String, Vec<protocol::SessionMessage>>,
    next_id: u64,
    /// Canned `/api/event` events produced by the last command.
    server_events: Vec<Value>,
    /// Pending permissions and forms, until answered or cancelled.
    pending: Vec<PendingRequest>,
    /// Undelivered prompts per session, oldest first.
    inbox: HashMap<String, Vec<Waiting>>,
    /// Sessions with a (canned, never ending) run.
    busy: HashSet<String>,
    paged_history: bool,
}

/// One undelivered prompt.
#[derive(Clone, Debug)]
struct Waiting {
    id: String,
    text: String,
    delivery: protocol::Delivery,
    created: u64,
}

impl State {
    pub fn new() -> Self {
        let mut messages = HashMap::new();
        messages.insert(ACTIVE_ID.to_owned(), active_messages());
        messages.insert(OTHER_ID.to_owned(), other_messages());
        messages.insert(RUNNING_ID.to_owned(), running_messages());
        messages.insert(PARKED_ID.to_owned(), parked_messages());
        let waiting = |id: &str, text: &str, delivery, created| Waiting {
            id: id.into(),
            text: text.into(),
            delivery,
            created,
        };
        let mut inbox = HashMap::new();
        inbox.insert(
            RUNNING_ID.to_owned(),
            vec![
                waiting(
                    "msg_running_steer",
                    "and keep the 30 s cap on the backoff",
                    protocol::Delivery::Steer,
                    CREATED + 70_000,
                ),
                waiting(
                    "msg_running_queue",
                    "then update the changelog",
                    protocol::Delivery::Queue,
                    CREATED + 75_000,
                ),
                waiting(
                    "msg_running_review",
                    "open a PR for review",
                    protocol::Delivery::Queue,
                    CREATED + 76_000,
                ),
            ],
        );
        inbox.insert(
            PARKED_ID.to_owned(),
            vec![
                waiting(
                    "msg_parked_steer",
                    "and keep the 30 s cap on the backoff",
                    protocol::Delivery::Steer,
                    CREATED + 70_000,
                ),
                waiting(
                    "msg_parked_queue",
                    "then update the changelog",
                    protocol::Delivery::Queue,
                    CREATED + 75_000,
                ),
                waiting(
                    "msg_parked_review",
                    "open a PR for review",
                    protocol::Delivery::Queue,
                    CREATED + 76_000,
                ),
            ],
        );
        Self {
            sessions: vec![
                active_session(),
                other_session(),
                session_info(
                    RUNNING_ID,
                    DIRECTORY,
                    Some("Refactor the retry logic"),
                    CREATED - 7_200_000,
                    CREATED - 7_100_000,
                ),
                session_info(
                    PARKED_ID,
                    DIRECTORY,
                    Some("Stopped with parked messages"),
                    CREATED - 10_800_000,
                    CREATED - 10_700_000,
                ),
                session_info(
                    IDLE_ID,
                    DIRECTORY,
                    Some("Release checklist"),
                    CREATED - 14_400_000,
                    CREATED - 14_300_000,
                ),
            ],
            messages,
            next_id: 1,
            server_events: Vec::new(),
            pending: canned_pending(),
            inbox,
            busy: HashSet::from([RUNNING_ID.to_owned()]),
            paged_history: false,
        }
    }

    /// Exercise older-history loading without changing the normal screenshot fixture.
    pub fn enable_paged_history(&mut self) {
        self.paged_history = true;
    }

    pub fn handle(&mut self, command: Command) -> UiEvent {
        match command {
            Command::Bootstrap { .. } => UiEvent::Bootstrap(Ok(self.bootstrap())),
            Command::LoadPending { .. } => UiEvent::PendingLoaded(PendingSnapshot {
                requests: self.pending.clone(),
                complete: true,
                covered: HashSet::from([DIRECTORY.to_owned()]),
                warnings: Vec::new(),
            }),
            Command::LoadSessionInfo { session_ids } => UiEvent::SessionInfoLoaded(
                session_ids
                    .into_iter()
                    .map(|id| {
                        let result = if id == CHILD_ID {
                            Ok(child_session())
                        } else {
                            Err(format!("Session not found: {id}"))
                        };
                        (id, result)
                    })
                    .collect(),
            ),
            Command::LoadMessages { session_id, cursor } => UiEvent::MessagesLoaded {
                result: Ok(self.message_page(&session_id, cursor.as_deref())),
                session_id,
                cursor,
            },
            Command::LoadModels { directory } => UiEvent::ModelsLoaded {
                directory,
                result: Ok(catalog()),
            },
            Command::CreateSession {
                request_id,
                directory,
                title,
            } => {
                let session = self.create_session(directory, title);
                UiEvent::SessionCreated {
                    request_id,
                    result: Ok(session),
                }
            }
            Command::RenameSession {
                request_id,
                session_id,
                title,
            } => {
                let result = self.rename_session(&session_id, title);
                UiEvent::SessionRenamed {
                    request_id,
                    session_id,
                    result,
                }
            }
            Command::SelectModel {
                request_id,
                session_id,
                model,
            } => {
                let result = self.select_model(&session_id, &model);
                UiEvent::ModelSelected {
                    request_id,
                    session_id,
                    model,
                    result,
                }
            }
            Command::SendPrompt {
                request_id,
                message_id,
                session_id,
                text,
                delivery,
                ..
            } => {
                self.prompt(&session_id, message_id, text, delivery);
                UiEvent::PromptAccepted {
                    request_id,
                    session_id,
                    result: Ok(()),
                }
            }
            Command::Abort { session_id } => {
                if self.busy.remove(&session_id) {
                    self.push_event(
                        &session_id,
                        CREATED + 80_000,
                        "session.execution.interrupted",
                        json!({ "reason": "user" }),
                    );
                }
                UiEvent::Aborted {
                    session_id,
                    result: Ok(()),
                }
            }
            Command::Inbox {
                session_id,
                inbox_id,
                request,
            } => UiEvent::InboxSettled {
                result: Ok(self.inbox_request(&session_id, &inbox_id, request)),
                session_id,
                inbox_id,
                request,
            },
            Command::ReplyPermission { request_id, .. } => UiEvent::PermissionReplied {
                result: Ok(self.settle(&request_id)),
                request_id,
            },
            Command::CancelForm { form_id, .. } => UiEvent::FormCancelled {
                result: Ok(self.settle(&form_id)),
                form_id,
            },
        }
    }

    fn bootstrap(&self) -> Bootstrap {
        let mut statuses: HashMap<_, _> = self
            .sessions
            .iter()
            .map(|session| {
                let status = if self.busy.contains(&session.id) {
                    RunStatus::Busy
                } else {
                    RunStatus::Idle
                };
                (session.id.clone(), status)
            })
            .collect();
        statuses.insert(CHILD_ID.to_owned(), RunStatus::Busy);
        Bootstrap {
            version: "preview".into(),
            sessions: self.sessions.clone(),
            sessions_complete: true,
            projects: vec![Project::from_info(&decode(json!({
                "id": "prj_preview",
                "canonical": DIRECTORY,
                "name": "opencode-gpui",
                "sandboxes": []
            })))],
            projects_complete: true,
            statuses,
            statuses_complete: true,
            pending: self.pending.clone(),
            pending_covered: HashSet::from([DIRECTORY.to_owned()]),
            shells: canned_shells(),
            retry_needed: false,
            warnings: Vec::new(),
        }
    }

    fn settle(&mut self, id: &str) -> Settled {
        let before = self.pending.len();
        self.pending.retain(|request| request.id() != id);
        if self.pending.len() == before {
            Settled::AlreadyResolved
        } else {
            Settled::Done
        }
    }

    /// History as the client presents it: the whole canned transcript is one
    /// chronological page, so there is never an older page.
    fn message_page(&self, session_id: &str, cursor: Option<&str>) -> MessagePage {
        if self.paged_history && session_id == ACTIVE_ID {
            let messages = self.messages.get(session_id).cloned().unwrap_or_default();
            let midpoint = messages.len() / 2;
            let (messages, next_cursor) = match cursor {
                None => (
                    messages[midpoint..].to_vec(),
                    Some("older-preview".to_owned()),
                ),
                Some("older-preview") => (messages[..midpoint].to_vec(), None),
                Some(_) => (Vec::new(), None),
            };
            return MessagePage {
                messages,
                next_cursor,
                queued: cursor.is_none().then_some(Vec::new()),
            };
        }
        if cursor.is_some() {
            return MessagePage {
                messages: Vec::new(),
                next_cursor: None,
                queued: None,
            };
        }
        let queued = self
            .inbox
            .get(session_id)
            .into_iter()
            .flatten()
            .map(|waiting| {
                decode(json!({
                    "id": waiting.id,
                    "sessionID": session_id,
                    "time": { "created": waiting.created },
                    "type": "user",
                    "payload": { "text": waiting.text },
                    "delivery": waiting.delivery
                }))
            })
            .collect();
        MessagePage {
            messages: self.messages.get(session_id).cloned().unwrap_or_default(),
            next_cursor: None,
            queued: Some(queued),
        }
    }

    fn create_session(&mut self, directory: String, title: Option<String>) -> Session {
        let id = format!("ses_new_{}", self.next_id);
        self.next_id += 1;
        let created = CREATED + self.next_id * 1_000;
        let mut session = session_info(&id, &directory, title.as_deref(), created, created);
        // New sessions follow the server default until a model is picked.
        session.model = None;
        self.messages.insert(id, Vec::new());
        self.sessions.insert(0, session.clone());
        session
    }

    fn rename_session(&mut self, session_id: &str, title: String) -> Result<Session, String> {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err("session title cannot be blank".into());
        }
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| format!("unknown session {session_id}"))?;
        session.title = title;
        session.time.updated = CREATED + 60_000;
        Ok(session.clone())
    }

    fn select_model(&mut self, session_id: &str, model: &protocol::ModelRef) -> Result<(), String> {
        if !catalog()
            .models
            .iter()
            .any(|option| option.provider_id == model.provider_id && option.model_id == model.id)
        {
            return Err(format!("Unknown model {}/{}", model.provider_id, model.id));
        }
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| format!("unknown session {session_id}"))?;
        session.model = Some(SessionModel::from_selection(
            &crate::model::ModelSelection::from_ref(model),
        ));
        Ok(())
    }

    /// Like the server: the prompt waits in the inbox (its `id` is the
    /// inbox ID and becomes the user message ID) and wakes an idle session;
    /// a running (canned) session keeps it waiting.
    fn prompt(
        &mut self,
        session_id: &str,
        message_id: String,
        text: String,
        delivery: Option<protocol::Delivery>,
    ) {
        self.next_id += 1;
        let delivery = delivery.unwrap_or(protocol::Delivery::Steer);
        let created = CREATED + self.next_id * 1_000;
        self.push_event(
            session_id,
            created,
            "session.inbox.enqueued",
            json!({ "inboxID": message_id,
                "item": { "type": "user", "payload": { "text": text }, "delivery": delivery } }),
        );
        self.inbox
            .entry(session_id.to_owned())
            .or_default()
            .push(Waiting {
                id: message_id,
                text,
                delivery,
                created,
            });
        if !self.busy.contains(session_id) {
            self.run(session_id);
        }
    }

    /// A tray request, like the client's API calls: switch (a switch to
    /// steer wakes an idle session for everything), cancel, or Resume (a
    /// queued item is steered; a steer is queued and steered again). Mirrors
    /// the server's 409 for an item that is gone or already in that mode.
    fn inbox_request(
        &mut self,
        session_id: &str,
        inbox_id: &str,
        request: InboxRequest,
    ) -> Settled {
        match request {
            InboxRequest::SetDelivery(delivery) => {
                self.set_delivery(session_id, inbox_id, delivery)
            }
            InboxRequest::Cancel => {
                let waiting = self.inbox.entry(session_id.to_owned()).or_default();
                if let Some(index) = waiting.iter().position(|item| item.id == inbox_id) {
                    waiting.remove(index);
                    self.push_event(
                        session_id,
                        CREATED + 85_000,
                        "session.inbox.cancelled",
                        json!({ "inboxID": inbox_id }),
                    );
                }
                Settled::Done
            }
            InboxRequest::Resume(protocol::Delivery::Queue) => {
                self.set_delivery(session_id, inbox_id, protocol::Delivery::Steer)
            }
            InboxRequest::Resume(_) => {
                match self.set_delivery(session_id, inbox_id, protocol::Delivery::Queue) {
                    Settled::Done => {
                        self.set_delivery(session_id, inbox_id, protocol::Delivery::Steer)
                    }
                    resolved => resolved,
                }
            }
        }
    }

    /// `PATCH .../inbox/{id}`: a conditional switch from the other mode.
    fn set_delivery(
        &mut self,
        session_id: &str,
        inbox_id: &str,
        delivery: protocol::Delivery,
    ) -> Settled {
        let busy = self.busy.contains(session_id);
        let waiting = self.inbox.entry(session_id.to_owned()).or_default();
        let Some(item) = waiting
            .iter_mut()
            .find(|item| item.id == inbox_id && item.delivery != delivery)
        else {
            return Settled::AlreadyResolved;
        };
        item.delivery = delivery;
        self.push_event(
            session_id,
            CREATED + 85_000,
            "session.inbox.delivery.changed",
            json!({ "inboxID": inbox_id, "delivery": delivery }),
        );
        if delivery == protocol::Delivery::Steer && !busy {
            self.run(session_id);
        }
        Settled::Done
    }

    /// Delivers waiting prompts like the server's runner after a wake (a
    /// prompt or a switch to steer): every steer at once, then one queued
    /// prompt per turn, each with a canned reply streamed as v2 events that
    /// match the stored entries.
    fn run(&mut self, session_id: &str) {
        let mut started = false;
        loop {
            let waiting = self.inbox.entry(session_id.to_owned()).or_default();
            let steers: Vec<Waiting> = waiting
                .iter()
                .filter(|item| item.delivery != protocol::Delivery::Queue)
                .cloned()
                .collect();
            let batch = if !steers.is_empty() {
                steers
            } else if !waiting.is_empty() {
                vec![waiting[0].clone()]
            } else {
                break;
            };
            waiting.retain(|item| !batch.iter().any(|sent| sent.id == item.id));
            self.next_id += 1;
            let delivered = CREATED + self.next_id * 1_000;
            if !started {
                started = true;
                self.push_event(
                    session_id,
                    delivered,
                    "session.execution.started",
                    json!({}),
                );
            }
            self.reply(session_id, &batch, delivered);
        }
        if started {
            self.next_id += 1;
            let finished = CREATED + self.next_id * 1_000;
            self.push_event(
                session_id,
                finished,
                "session.execution.succeeded",
                json!({}),
            );
        }
    }

    fn reply(&mut self, session_id: &str, batch: &[Waiting], delivered: u64) {
        let assistant_id = format!("msg_preview_reply_{}", self.next_id);
        let said: Vec<&str> = batch.iter().map(|item| item.text.as_str()).collect();
        let reply = format!("(preview) You said: {}", said.join(" / "));
        let tokens = json!({ "input": 1200, "output": 40, "reasoning": 0, "cache": { "read": 0, "write": 0 } });
        let messages = self.messages.entry(session_id.to_owned()).or_default();
        for item in batch {
            messages.push(entry(json!({
                "id": item.id,
                "type": "user",
                "time": { "created": delivered },
                "text": item.text
            })));
        }
        messages.push(entry(json!({
            "id": assistant_id,
            "type": "assistant",
            "time": { "created": delivered + 100, "completed": delivered + 200 },
            "agent": "build",
            "content": [{ "type": "text", "text": reply }],
            "finish": "stop",
            "tokens": tokens
        })));
        for item in batch {
            self.push_event(
                session_id,
                delivered,
                "session.inbox.delivered",
                json!({ "inboxID": item.id }),
            );
        }
        let events = [
            (
                100,
                "session.step.started",
                json!({ "assistantMessageID": assistant_id,
                "agent": "build", "started": delivered + 100 }),
            ),
            (
                100,
                "session.text.started",
                json!({ "assistantMessageID": assistant_id, "ordinal": 0 }),
            ),
            (
                150,
                "session.text.delta",
                json!({ "assistantMessageID": assistant_id, "ordinal": 0,
                "delta": "(preview) " }),
            ),
            (
                200,
                "session.text.ended",
                json!({ "assistantMessageID": assistant_id, "ordinal": 0,
                "text": reply }),
            ),
            (
                200,
                "session.step.ended",
                json!({ "assistantMessageID": assistant_id,
                "finish": "stop", "cost": 0, "tokens": tokens }),
            ),
        ];
        for (offset, kind, data) in events {
            self.push_event(session_id, delivered + offset, kind, data);
        }
    }

    fn push_event(&mut self, session_id: &str, created: u64, kind: &str, mut data: Value) {
        self.next_id += 1;
        data["sessionID"] = json!(session_id);
        self.server_events.push(json!({
            "id": format!("evt_preview_{:06}", self.next_id),
            "created": created,
            "type": kind,
            "location": { "directory": DIRECTORY },
            "data": data
        }));
    }

    /// Events of the last command, as the event stream would deliver them.
    pub fn take_server_events(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.server_events)
            .into_iter()
            .map(|payload| {
                UiEvent::ServerEvent(ServerEnvelope {
                    directory: Some(DIRECTORY.to_owned()),
                    payload,
                })
            })
            .collect()
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Running shells, as `GET /api/shell` lists them, started relative to now
/// so the elapsed times read naturally: two of the active session (one its
/// subagent started), the running tab's test run and the other tab's log
/// tail.
fn canned_shells() -> ShellSnapshot {
    let now = now_ms() as i64;
    let shell = |id: &str, command: &str, owner: &str, minutes: i64| {
        let info: protocol::ShellInfo = decode(json!({
            "id": id,
            "status": "running",
            "command": command,
            "cwd": DIRECTORY,
            "shell": "/bin/bash",
            "file": format!("/tmp/{id}.out"),
            "metadata": { "sessionID": owner },
            "time": { "started": now - minutes * 60_000 }
        }));
        ShellJob::running(&info, DIRECTORY).expect("a running shell")
    };
    ShellSnapshot {
        shells: vec![
            shell("sh_preview_dev", "pnpm dev --port 5173", ACTIVE_ID, 22),
            shell("sh_preview_grep", "rg -n 'api/v1' src", CHILD_ID, 2),
            shell("sh_preview_test", "cargo test api::", RUNNING_ID, 1),
            shell("sh_preview_tail", "tail -f tunnel.log", OTHER_ID, 9),
        ],
        queried: [DIRECTORY.to_owned()].into(),
        covered: HashSet::from([DIRECTORY.to_owned()]),
        warnings: Vec::new(),
    }
}

/// One permission for the background tab (so the active composer stays
/// usable until that tab is opened) and one form notice for the active one.
fn canned_pending() -> Vec<PendingRequest> {
    vec![
        PendingRequest::Permission {
            directory: DIRECTORY.into(),
            request: decode(json!({
                "id": "per_preview",
                "sessionID": OTHER_ID,
                "action": "shell",
                "resources": ["ssh -N -L 4096:127.0.0.1:4096 host"],
                "save": ["ssh *"],
                "metadata": { "description": "Open the tunnel" },
                "source": { "type": "tool", "messageID": "msg_other_assistant", "id": "call_preview_ssh" }
            })),
        },
        PendingRequest::Form(PendingForm {
            form: decode(json!({
                "id": "frm_preview",
                "sessionID": ACTIVE_ID,
                "title": "Choose a padding",
                "fields": [{ "key": "px", "type": "integer", "title": "Pixels" }]
            })),
            directory: Some(DIRECTORY.into()),
        }),
    ]
}

/// Canned `GET /api/model` and `/api/model/default`, built through the same
/// path as the real client.
fn catalog() -> ModelCatalog {
    let model = |id: &str, provider: &str, name: &str, variants: &[&str]| {
        json!({
            "id": id,
            "modelID": id,
            "providerID": provider,
            "name": name,
            "capabilities": { "tools": true, "input": ["text", "image", "pdf"], "output": ["text"] },
            "variants": variants.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            "status": "active",
            "enabled": true,
            "limit": { "context": 200000, "output": 32000 }
        })
    };
    let gpt = model("gpt-5.6", "openai", "GPT-5.6", &["high", "low", "medium"]);
    let list: protocol::ModelListResponse = decode(json!({
        "location": { "directory": DIRECTORY },
        "data": [
            gpt,
            model("claude-sonnet-4.6", "anthropic", "Claude Sonnet 4.6", &["medium", "high"])
        ]
    }));
    let default: protocol::ModelDefaultResponse = decode(json!({
        "location": { "directory": DIRECTORY },
        "data": gpt
    }));
    ModelCatalog::from_models(&list.data, default.data.as_ref())
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("canned preview data matches the v2 protocol")
}

/// A canned v2 `SessionInfo`, mapped the same way the real client maps it.
fn session_info(
    id: &str,
    directory: &str,
    title: Option<&str>,
    created: u64,
    updated: u64,
) -> Session {
    Session::from_info(&decode::<protocol::SessionInfo>(json!({
        "id": id,
        "projectID": "prj_preview",
        "agent": "build",
        "model": { "id": "gpt-5.6", "providerID": "openai", "variant": "medium" },
        "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        "time": { "created": created, "updated": updated },
        "title": title,
        "location": { "directory": directory }
    })))
}

fn active_session() -> Session {
    session_info(
        ACTIVE_ID,
        DIRECTORY,
        Some("Fix the attach clip padding"),
        CREATED,
        CREATED + 90_000,
    )
}

fn other_session() -> Session {
    session_info(
        OTHER_ID,
        DIRECTORY,
        Some("SSH tunnel notes"),
        CREATED - 86_400_000,
        CREATED - 3_600_000,
    )
}

/// The background subagent, created (and started) four minutes ago.
fn child_session() -> Session {
    let created = now_ms() - 4 * 60_000;
    Session::from_info(&decode::<protocol::SessionInfo>(json!({
        "id": CHILD_ID,
        "parentID": ACTIVE_ID,
        "projectID": "prj_preview",
        "agent": "general",
        "time": { "created": created, "updated": created },
        "title": "Audit v1 call sites",
        "location": { "directory": DIRECTORY }
    })))
}

fn entry(value: Value) -> protocol::SessionMessage {
    let message = protocol::SessionMessage::from_value(value);
    assert!(
        !matches!(message, protocol::SessionMessage::Unknown(_)),
        "canned preview entry matches the v2 protocol: {message:?}"
    );
    message
}

fn user_text(id: &str, created: u64, text: &str) -> protocol::SessionMessage {
    entry(json!({ "id": id, "type": "user", "time": { "created": created }, "text": text }))
}

fn idle(id: &str, created: u64) -> protocol::SessionMessage {
    entry(
        json!({ "id": id, "type": "idle", "time": { "created": created }, "outcome": "succeeded" }),
    )
}

fn active_messages() -> Vec<protocol::SessionMessage> {
    let mut messages = vec![
        user_text(
            "msg_user",
            CREATED,
            "The paperclip is crowding the attach button. Match send's padding.",
        ),
        entry(json!({
            "id": "msg_assistant",
            "type": "assistant",
            "time": { "created": CREATED + 30_000, "completed": CREATED + 50_000 },
            "agent": "build",
            "model": { "id": "gpt-5.6", "providerID": "openai" },
            "content": [
                {
                    "type": "reasoning",
                    "text": "The clip is a tall outline, so it reads larger than the paper plane at the same pixel size.",
                    "time": { "created": CREATED + 31_000, "completed": CREATED + 33_000 }
                },
                {
                    "type": "text",
                    "text": "# Padding\n\nDraw the clip at **22px**, same as send.\n\n- Inner wire stays visible\n- Composer actions stay `34×32`\n\n```rust\npaperclip_icon(COMPOSER_ICON_PX)\n```"
                },
                {
                    "type": "tool",
                    "id": "call_preview_shell",
                    "name": "shell",
                    "state": {
                        "status": "completed",
                        "input": { "command": "cargo test composer", "description": "Run composer tests" },
                        "content": [{ "type": "text", "text": "test result: ok. 12 passed" }]
                    },
                    "time": { "created": CREATED + 45_000, "ran": CREATED + 45_100, "completed": CREATED + 48_000 }
                }
            ],
            "finish": "stop",
            "tokens": { "input": 12400, "output": 800, "reasoning": 200, "cache": { "read": 0, "write": 0 } }
        })),
        idle("msg_idle", CREATED + 50_000),
        entry(json!({
            "id": "msg_user_attachment",
            "type": "user",
            "time": { "created": CREATED + 55_000 },
            "text": "Here is the clip rendered at 22px:",
            "files": [{
                "data": "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",
                "mime": "image/png",
                "name": "clip-22px.png",
                "source": { "type": "inline" }
            }]
        })),
    ];
    // The GTK 14d24ae fixture ends after the assistant tool call. The extra
    // image turn exercises GPUI's attachment renderer, but must be omitted
    // for a like-for-like main-window screenshot of the reference fixture.
    if std::env::var_os("OPENCODE_PREVIEW_MATCH_GTK").is_some() {
        messages.pop();
    }
    messages
}

fn other_messages() -> Vec<protocol::SessionMessage> {
    vec![
        user_text(
            "msg_other_user",
            CREATED - 86_400_000,
            "How do I reach the remote serve over SSH?",
        ),
        entry(json!({
            "id": "msg_other_assistant",
            "type": "assistant",
            "time": { "created": CREATED - 86_370_000 },
            "agent": "build",
            "content": [{
                "type": "text",
                "text": "Tunnel loopback: `ssh -N -L 4096:127.0.0.1:4096 host`, then connect to `http://127.0.0.1:4096`."
            }]
        })),
        entry(json!({
            "id": "msg_other_synthetic",
            "type": "synthetic",
            "time": { "created": CREATED - 86_340_000 },
            "text": "The background subagent finished: port 4096 is reachable through the tunnel.",
            "description": "Check the tunnel",
            "metadata": { "source": "subagent", "state": "completed" }
        })),
        entry(json!({
            "id": "msg_other_error",
            "type": "assistant",
            "time": { "created": CREATED - 86_300_000 },
            "agent": "build",
            "content": [],
            "finish": "error",
            "error": { "type": "provider.api", "message": "AI_APICallError: Not Found (404)", "status": 404 }
        })),
    ]
}

const RUNNING_CREATED: u64 = CREATED - 7_200_000;

fn running_messages() -> Vec<protocol::SessionMessage> {
    vec![
        user_text(
            "msg_running_user",
            RUNNING_CREATED,
            "Refactor the retry logic in the event worker into its own function.",
        ),
        entry(json!({
            "id": "msg_running_assistant",
            "type": "assistant",
            "time": { "created": RUNNING_CREATED + 20_000 },
            "agent": "build",
            "content": [{
                "type": "tool",
                "id": "call_running_shell",
                "name": "shell",
                "state": {
                    "status": "running",
                    "input": { "command": "cargo test api::", "description": "Run the API tests" }
                },
                "time": { "created": RUNNING_CREATED + 21_000, "ran": RUNNING_CREATED + 21_100 }
            }]
        })),
    ]
}

const PARKED_CREATED: u64 = CREATED - 10_800_000;

fn parked_messages() -> Vec<protocol::SessionMessage> {
    vec![
        user_text(
            "msg_parked_user",
            PARKED_CREATED,
            "Refactor the retry logic in the event worker into its own function.",
        ),
        entry(json!({
            "id": "msg_parked_assistant",
            "type": "assistant",
            "time": { "created": PARKED_CREATED + 20_000, "completed": PARKED_CREATED + 60_000 },
            "agent": "build",
            "content": [{
                "type": "tool",
                "id": "call_parked_shell",
                "name": "shell",
                "state": {
                    "status": "error",
                    "input": { "command": "cargo test api::", "description": "Run the API tests" },
                    "error": { "type": "aborted", "message": "Tool execution interrupted" }
                },
                "time": { "created": PARKED_CREATED + 21_000, "ran": PARKED_CREATED + 21_100, "completed": PARKED_CREATED + 60_000 }
            }],
            "finish": "error",
            "error": { "type": "aborted", "message": "Step interrupted" }
        })),
        entry(json!({ "id": "msg_parked_idle", "type": "idle",
            "time": { "created": PARKED_CREATED + 60_000 }, "outcome": "interrupted" })),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conversation;

    #[test]
    fn bootstrap_opens_a_representative_session() {
        let bootstrap = State::new().bootstrap();
        assert_eq!(bootstrap.version, "preview");
        assert_eq!(bootstrap.sessions[0].id, ACTIVE_ID);
        assert!(bootstrap.sessions_complete);
        let catalog = catalog();
        assert!(
            catalog
                .models
                .iter()
                .all(|model| model.supports_attachments)
        );
        assert_eq!(
            catalog.preferred.map(|model| model.model_id),
            Some("gpt-5.6".to_owned())
        );
        assert_eq!(catalog.models[1].variants, ["low", "medium", "high"]);
    }

    fn render(messages: &[protocol::SessionMessage]) -> Vec<Value> {
        let mut conversation = Conversation::default();
        conversation.replace_from_api(messages, None);
        conversation
            .transcript_rows()
            .iter()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect()
    }

    #[test]
    fn canned_messages_render_as_transcript_rows() {
        let rows = render(&active_messages());
        // user, reasoning, text, tool, then the user's image attachment turn
        // (its text and image share one row); the idle entry renders nothing.
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0]["role"], "YOU");
        assert_eq!(rows[1]["kind"], "reasoning");
        assert_eq!(rows[1]["time"], CREATED + 31_000);
        assert!(rows[2]["body"].as_str().unwrap().contains("22px"));
        assert_eq!(rows[3]["kind"], "tool");
        assert_eq!(rows[3]["body"], "shell · completed — cargo test composer");
        assert_eq!(rows[3]["time"], CREATED + 45_000);
        assert_eq!(rows[4]["role"], "YOU");
        assert!(
            rows[4]["body"]
                .as_str()
                .unwrap()
                .contains("rendered at 22px")
        );
        assert!(
            rows[4]["images"][0]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"),
            "the attachment exposes its image URL: {}",
            rows[4]
        );

        let rows = render(&other_messages());
        let kinds: Vec<_> = rows
            .iter()
            .map(|row| (row["role"].as_str().unwrap(), row["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("YOU", ""),
                ("AGENT", ""),
                ("AGENT", ""),
                ("AGENT", "error")
            ]
        );
        assert!(
            rows[2]["body"]
                .as_str()
                .unwrap()
                .starts_with("The background subagent")
        );
        assert_eq!(rows[3]["body"], "AI_APICallError: Not Found (404)");
    }

    #[test]
    fn paged_preview_reconstructs_the_full_transcript() {
        let mut state = State::new();
        state.enable_paged_history();
        let newest = state.message_page(ACTIVE_ID, None);
        assert_eq!(newest.next_cursor.as_deref(), Some("older-preview"));
        let older = state.message_page(ACTIVE_ID, Some("older-preview"));
        assert_eq!(older.next_cursor, None);
        let mut conversation = Conversation::default();
        conversation.replace_from_api(&newest.messages, newest.next_cursor);
        conversation.prepend_from_api(&older.messages, older.next_cursor);
        let full = render(&active_messages());
        let paged: Vec<Value> = conversation
            .transcript_rows()
            .iter()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect();
        assert_eq!(paged, full);
    }

    #[test]
    fn create_session_returns_a_new_tab() {
        let mut state = State::new();
        let event = state.handle(Command::CreateSession {
            request_id: 1,
            directory: DIRECTORY.into(),
            title: Some("Scratch".into()),
        });
        match event {
            UiEvent::SessionCreated {
                result: Ok(session),
                ..
            } => {
                assert_eq!(session.title, "Scratch");
                assert_eq!(session.directory, DIRECTORY);
            }
            other => panic!("unexpected {other:?}"),
        }
        let untitled = state.create_session(DIRECTORY.into(), None);
        assert_eq!(untitled.title, "Untitled session");
    }

    #[test]
    fn a_sent_prompt_becomes_a_user_entry_with_the_prompt_id() {
        let mut state = State::new();
        let message_id = protocol::new_message_id();
        let event = state.handle(Command::SendPrompt {
            request_id: 3,
            message_id: message_id.clone(),
            session_id: ACTIVE_ID.into(),
            text: "Ship it".into(),
            attachments: Vec::new(),
            paste_lifetime: Vec::new(),
            delivery: None,
        });
        assert!(matches!(
            event,
            UiEvent::PromptAccepted {
                request_id: 3,
                result: Ok(()),
                ..
            }
        ));
        let page = state.message_page(ACTIVE_ID, None);
        let user = &page.messages[page.messages.len() - 2];
        let protocol::SessionMessage::User(user) = user else {
            panic!("unexpected {user:?}");
        };
        assert_eq!(user.id, message_id);
        assert_eq!(user.text, "Ship it");
        let last = page.messages.last().unwrap().clone();
        let protocol::SessionMessage::Assistant(reply) = last else {
            panic!("unexpected {last:?}");
        };
        let mut reloaded = Conversation::default();
        reloaded.replace_from_api(&page.messages, None);
        assert!(reloaded.has_user_message(&message_id));

        // The canned v2 events stream the same transcript a reload shows.
        let mut live = Conversation::default();
        live.replace_from_api(&active_messages(), None);
        let events = state.take_server_events();
        assert_eq!(events.len(), 9);
        let mut statuses = Vec::new();
        for event in events {
            let UiEvent::ServerEvent(envelope) = event else {
                panic!("unexpected {event:?}");
            };
            live.apply_event(&envelope.payload);
            statuses.extend(crate::model::event_run_status(&envelope.payload));
        }
        assert!(live.transcript_rows() == reloaded.transcript_rows());
        assert!(live.messages.iter().any(|message| message.id == reply.id));
        assert_eq!(
            statuses.last().map(|(_, status)| status),
            Some(&RunStatus::Idle)
        );
        assert!(state.take_server_events().is_empty());
    }

    fn replay(state: &mut State, conversation: &mut Conversation) -> Vec<String> {
        let mut types = Vec::new();
        for event in state.take_server_events() {
            let UiEvent::ServerEvent(envelope) = event else {
                panic!("unexpected {event:?}");
            };
            conversation.apply_event(&envelope.payload);
            types.push(envelope.payload["type"].as_str().unwrap().to_owned());
        }
        types
    }

    fn tray(conversation: &Conversation) -> Vec<(String, protocol::Delivery)> {
        conversation
            .tray_items()
            .into_iter()
            .map(|item| (item.id, item.delivery))
            .collect()
    }

    #[test]
    fn the_running_session_keeps_prompts_waiting_until_stopped_and_sent() {
        let mut state = State::new();
        assert_eq!(state.bootstrap().statuses[RUNNING_ID], RunStatus::Busy);
        assert_eq!(state.bootstrap().statuses[PARKED_ID], RunStatus::Idle);
        let page = state.message_page(RUNNING_ID, None);
        let mut live = Conversation::default();
        live.replace_from_api(&page.messages, None);
        live.sync_queued(page.queued.as_deref().unwrap());
        assert_eq!(
            tray(&live),
            [
                ("msg_running_steer".to_owned(), protocol::Delivery::Steer),
                ("msg_running_queue".to_owned(), protocol::Delivery::Queue),
                ("msg_running_review".to_owned(), protocol::Delivery::Queue)
            ]
        );
        let rows = render(&page.messages);
        assert_eq!(
            rows.last().unwrap()["body"],
            "shell · running — cargo test api::"
        );

        // A prompt sent into the run waits too; so does a queued one.
        state.handle(Command::SendPrompt {
            request_id: 1,
            message_id: "msg_more".into(),
            session_id: RUNNING_ID.into(),
            text: "one more thing".into(),
            attachments: Vec::new(),
            paste_lifetime: Vec::new(),
            delivery: Some(protocol::Delivery::Queue),
        });
        assert_eq!(replay(&mut state, &mut live), ["session.inbox.enqueued"]);
        assert_eq!(live.tray_items().len(), 4);

        // Switch, and a switch to the mode it already has.
        let switch = |state: &mut State, id: &str, delivery| {
            state.handle(Command::Inbox {
                session_id: RUNNING_ID.into(),
                inbox_id: id.into(),
                request: InboxRequest::SetDelivery(delivery),
            })
        };
        assert!(matches!(
            switch(&mut state, "msg_running_steer", protocol::Delivery::Queue),
            UiEvent::InboxSettled {
                result: Ok(Settled::Done),
                ..
            }
        ));
        assert!(matches!(
            switch(&mut state, "msg_running_steer", protocol::Delivery::Queue),
            UiEvent::InboxSettled {
                result: Ok(Settled::AlreadyResolved),
                ..
            }
        ));
        assert_eq!(
            replay(&mut state, &mut live),
            ["session.inbox.delivery.changed"]
        );
        assert_eq!(live.tray_items()[0].delivery, protocol::Delivery::Queue);

        // Stop parks everything; switching one to steer delivers it, then the rest in turn.
        state.handle(Command::Abort {
            session_id: RUNNING_ID.into(),
        });
        assert_eq!(
            replay(&mut state, &mut live),
            ["session.execution.interrupted"]
        );
        assert_eq!(live.tray_items().len(), 4);
        switch(&mut state, "msg_more", protocol::Delivery::Steer);
        let types = replay(&mut state, &mut live);
        let delivered: Vec<_> = types
            .iter()
            .filter(|kind| *kind == "session.inbox.delivered")
            .collect();
        assert_eq!(delivered.len(), 4);
        assert_eq!(types.last().unwrap(), "session.execution.succeeded");
        assert!(live.tray_items().is_empty());
        let mut reloaded = Conversation::default();
        reloaded.replace_from_api(&state.message_page(RUNNING_ID, None).messages, None);
        assert_eq!(
            live.transcript_rows().len(),
            reloaded.transcript_rows().len()
        );
    }

    /// Resume as the tray sends it, and the inbox IDs delivered, in order.
    fn resume(state: &mut State, live: &mut Conversation, session_id: &str) -> Vec<String> {
        let rows = crate::tray::tray_rows(&live.tray_items(), None, None, &HashSet::new());
        let (inbox_id, request) = crate::tray::resume_request(&rows).unwrap();
        assert!(matches!(
            state.handle(Command::Inbox {
                session_id: session_id.into(),
                inbox_id,
                request,
            }),
            UiEvent::InboxSettled {
                result: Ok(Settled::Done),
                ..
            }
        ));
        let mut delivered = Vec::new();
        for event in state.take_server_events() {
            let UiEvent::ServerEvent(envelope) = event else {
                panic!("unexpected {event:?}");
            };
            live.apply_event(&envelope.payload);
            if envelope.payload["type"] == "session.inbox.delivered" {
                delivered.push(
                    envelope.payload["data"]["inboxID"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        delivered
    }

    #[test]
    fn resume_runs_every_parked_message_in_order() {
        // A parked steer and two queued messages: the steer is bounced.
        let mut state = State::new();
        let page = state.message_page(PARKED_ID, None);
        let mut live = Conversation::default();
        live.replace_from_api(&page.messages, None);
        live.sync_queued(page.queued.as_deref().unwrap());
        assert_eq!(live.tray_items().len(), 3);
        assert_eq!(
            resume(&mut state, &mut live, PARKED_ID),
            ["msg_parked_steer", "msg_parked_queue", "msg_parked_review"]
        );
        assert!(live.tray_items().is_empty());

        // Only queued messages: the first is steered and the rest follow.
        let page = state.message_page(RUNNING_ID, None);
        let mut live = Conversation::default();
        live.replace_from_api(&page.messages, None);
        live.sync_queued(page.queued.as_deref().unwrap());
        state.handle(Command::Abort {
            session_id: RUNNING_ID.into(),
        });
        state.handle(Command::Inbox {
            session_id: RUNNING_ID.into(),
            inbox_id: "msg_running_steer".into(),
            request: InboxRequest::Cancel,
        });
        replay(&mut state, &mut live);
        assert_eq!(
            tray(&live),
            [
                ("msg_running_queue".to_owned(), protocol::Delivery::Queue),
                ("msg_running_review".to_owned(), protocol::Delivery::Queue)
            ]
        );
        assert_eq!(
            resume(&mut state, &mut live, RUNNING_ID),
            ["msg_running_queue", "msg_running_review"]
        );
        assert!(live.tray_items().is_empty());
        assert_eq!(
            state.inbox_request(
                RUNNING_ID,
                "msg_running_queue",
                InboxRequest::Resume(protocol::Delivery::Queue)
            ),
            Settled::AlreadyResolved,
            "a delivered item is resolved"
        );
    }

    #[test]
    fn canned_requests_settle_once() {
        let mut state = State::new();
        let bootstrap = state.bootstrap();
        assert!(bootstrap.pending_covered.contains(DIRECTORY));
        assert_eq!(bootstrap.pending.len(), 2);
        let event = state.handle(Command::ReplyPermission {
            request_id: "per_preview".into(),
            session_id: OTHER_ID.into(),
            decision: protocol::PermissionDecision::Once,
        });
        assert!(matches!(
            event,
            UiEvent::PermissionReplied {
                result: Ok(Settled::Done),
                ..
            }
        ));
        let event = state.handle(Command::CancelForm {
            form_id: "frm_preview".into(),
            session_id: ACTIVE_ID.into(),
            directory: None,
        });
        assert!(matches!(
            event,
            UiEvent::FormCancelled {
                result: Ok(Settled::Done),
                ..
            }
        ));
        let again = state.handle(Command::CancelForm {
            form_id: "frm_preview".into(),
            session_id: ACTIVE_ID.into(),
            directory: None,
        });
        assert!(matches!(
            again,
            UiEvent::FormCancelled {
                result: Ok(Settled::AlreadyResolved),
                ..
            }
        ));
        assert!(state.bootstrap().pending.is_empty());
    }

    #[test]
    fn canned_jobs_match_the_mockup() {
        let mut state = State::new();
        let bootstrap = state.bootstrap();
        let running: HashSet<String> = bootstrap
            .statuses
            .iter()
            .filter(|(_, status)| status.is_busy())
            .map(|(id, _)| id.clone())
            .collect();
        let context = crate::jobs::Context {
            roots: &bootstrap.sessions,
            directories: &[],
        };
        let mut jobs = crate::jobs::Jobs::default();
        jobs.apply_snapshot(Some(&running), bootstrap.shells, &context);
        let wanted = jobs.take_wanted(&context);
        assert_eq!(wanted, [CHILD_ID]);
        let UiEvent::SessionInfoLoaded(results) = state.handle(Command::LoadSessionInfo {
            session_ids: wanted,
        }) else {
            panic!("no session info");
        };
        jobs.apply_session_info(results);
        let now = now_ms();
        let rows = |active: &str| -> Vec<(String, String)> {
            jobs.rows(Some(active))
                .into_iter()
                .map(|row| (row.title.clone(), row.subtitle(now)))
                .collect()
        };
        let row = |title: &str, subtitle: &str| (title.to_owned(), subtitle.to_owned());
        assert_eq!(
            rows(ACTIVE_ID),
            [
                row("pnpm dev --port 5173", "shell · 22m"),
                row("Audit v1 call sites", "subagent · 4m"),
                row("rg -n 'api/v1' src", "shell · Audit v1 call sites · 2m"),
            ]
        );
        assert_eq!(
            rows(RUNNING_ID),
            [row("cargo test api::", "shell · 1m")],
            "switching tabs shows another session's job"
        );
        assert_eq!(rows(OTHER_ID), [row("tail -f tunnel.log", "shell · 9m")]);
        assert!(rows(IDLE_ID).is_empty(), "and hides the section");
        let mut with_jobs: Vec<_> = jobs.sessions_with_jobs().into_iter().collect();
        with_jobs.sort();
        assert_eq!(
            with_jobs,
            [OTHER_ID, ACTIVE_ID, RUNNING_ID],
            "the parked and idle tabs show a dot"
        );
        let unread = server_state().unread;
        assert!(unread.contains(OTHER_ID) && unread.contains(PARKED_ID));
        assert_eq!(unread.len(), 2);
    }

    #[test]
    fn blank_rename_is_refused() {
        let mut state = State::new();
        assert!(state.rename_session(ACTIVE_ID, "  ".into()).is_err());
        assert_eq!(
            state
                .rename_session(ACTIVE_ID, " Renamed ".into())
                .unwrap()
                .title,
            "Renamed"
        );
    }
}
