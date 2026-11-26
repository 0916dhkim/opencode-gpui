use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::model::ModelSelection;

static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedState {
    #[serde(default)]
    pub connection: ConnectionSettings,
    #[serde(default)]
    pub servers: HashMap<String, ServerState>,
    #[serde(default = "default_zoom_level")]
    pub zoom_level: f64,
}

fn default_zoom_level() -> f64 {
    1.0
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            connection: ConnectionSettings::default(),
            servers: HashMap::default(),
            zoom_level: default_zoom_level(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ConnectionSettings {
    pub server: String,
    pub username: String,
    #[serde(default)]
    pub cloudflare_access: bool,
    /// Whether the Basic password of this server and username was saved in
    /// the system keyring from Settings. The secret itself never lands here.
    #[serde(default)]
    pub basic_auth_in_keyring: bool,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            server: "http://127.0.0.1:4096".into(),
            username: "opencode".into(),
            cloudflare_access: false,
            basic_auth_in_keyring: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ServerState {
    #[serde(default)]
    pub tabs: Vec<PersistedTab>,
    #[serde(default)]
    pub active: Option<String>,
    /// v1 per-tab model overrides. Still decoded so older state files load,
    /// but no longer used or written: v2 saves the model on the session.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub selections: HashMap<String, ModelSelection>,
    #[serde(default)]
    pub unread: HashSet<String>,
    #[serde(default)]
    pub busy: HashSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedTab {
    pub id: String,
    pub directory: String,
    pub title: String,
}

impl PersistedState {
    pub fn load(path: &Path) -> Result<(Self, Option<String>)> {
        let contents = match fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        match serde_json::from_slice(&contents) {
            Ok(state) => Ok((state, None)),
            Err(error) => {
                let backup = path.with_extension(format!("json.corrupt.{}", std::process::id()));
                fs::rename(path, &backup).with_context(|| {
                    format!(
                        "state is invalid ({error}) and could not be moved to {}",
                        backup.display()
                    )
                })?;
                Ok((
                    Self::default(),
                    Some(format!(
                        "Invalid state was preserved at {}",
                        backup.display()
                    )),
                ))
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("state path does not have a parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", parent.display()))?;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".state.json.{}.{}.{}.tmp",
            std::process::id(),
            nonce,
            seq
        ));
        let contents = serde_json::to_vec_pretty(self).context("failed to serialize state")?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(&contents)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}

struct SaveJob {
    path: PathBuf,
    state: PersistedState,
    reply: Option<mpsc::SyncSender<Result<()>>>,
}

#[derive(Default)]
struct SaveQueue {
    jobs: VecDeque<SaveJob>,
    closed: bool,
}

/// Serialized off-UI state writer. Keeps `file.sync_all()` and atomic rename
/// off the GPUI thread, coalesces superseded async snapshots for the same path,
/// and serializes synchronous Settings saves behind any in-flight write.
pub struct StateSaver {
    shared: Arc<(Mutex<SaveQueue>, Condvar)>,
    errors: Sender<String>,
    worker: Option<JoinHandle<()>>,
}

impl StateSaver {
    pub fn start() -> (Self, Receiver<String>) {
        let shared = Arc::new((Mutex::new(SaveQueue::default()), Condvar::new()));
        let (errors, receiver) = async_channel::bounded(16);
        let worker_shared = Arc::clone(&shared);
        let worker_errors = errors.clone();
        let worker = thread::Builder::new()
            .name("opencode-state-save".into())
            .spawn(move || run_save_worker(worker_shared, worker_errors))
            .ok();
        (
            Self {
                shared,
                errors,
                worker,
            },
            receiver,
        )
    }

    pub fn enqueue(&self, path: PathBuf, state: PersistedState) {
        if self.worker.is_none() {
            if let Err(error) = state.save(&path) {
                let _ = self.errors.try_send(error.to_string());
            }
            return;
        }
        let (lock, ready) = &*self.shared;
        let mut queue = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        if queue.closed {
            return;
        }
        if let Some(existing) = queue
            .jobs
            .iter_mut()
            .rev()
            .find(|job| job.path == path)
            .filter(|job| job.reply.is_none())
        {
            existing.state = state;
        } else {
            queue.jobs.push_back(SaveJob {
                path,
                state,
                reply: None,
            });
        }
        ready.notify_one();
    }

    pub fn save_sync(&self, path: PathBuf, state: PersistedState) -> Result<()> {
        if self.worker.is_none() {
            return state.save(&path);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let (lock, ready) = &*self.shared;
            let mut queue = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            if queue.closed {
                drop(queue);
                return state.save(&path);
            }
            // Drop any superseded queued async writes for the same path; any
            // already-in-flight write finishes on the worker before this job runs.
            queue
                .jobs
                .retain(|job| !(job.path == path && job.reply.is_none()));
            queue.jobs.push_back(SaveJob {
                path,
                state,
                reply: Some(tx),
            });
            ready.notify_one();
        }
        rx.recv()
            .context("state saver worker exited before reply")?
    }
}

impl Drop for StateSaver {
    fn drop(&mut self) {
        {
            let (lock, ready) = &*self.shared;
            let mut queue = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            queue.closed = true;
            ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_save_worker(shared: Arc<(Mutex<SaveQueue>, Condvar)>, errors: Sender<String>) {
    let (lock, ready) = &*shared;
    loop {
        let job = {
            let mut queue = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            while queue.jobs.is_empty() && !queue.closed {
                queue = ready
                    .wait(queue)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
            match queue.jobs.pop_front() {
                Some(job) => job,
                None => return,
            }
        };
        let result = job.state.save(&job.path);
        if let Some(reply) = job.reply {
            let _ = reply.send(result);
        } else if let Err(error) = result {
            let _ = errors.try_send(error.to_string());
        }
    }
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("opencode-gpui")
        .join("state.json")
}

/// Reuse the last client's non-secret state once, without changing its files.
/// The old client can still run while the GPUI replacement is under review.
pub fn load_with_legacy(path: &Path) -> Result<(PersistedState, Option<String>)> {
    if path.exists() {
        return PersistedState::load(path);
    }
    let Some(config) = path.parent().and_then(Path::parent) else {
        return PersistedState::load(path);
    };
    // The installed client may be either GTK or COSMIC. Import the most
    // recently written *valid* state instead of preferring an obsolete one.
    let mut candidates = ["opencode-gtk", "opencode-cosmic"]
        .into_iter()
        .map(|name| config.join(name).join("state.json"))
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        std::cmp::Reverse(
            candidate
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH),
        )
    });
    let mut errors = Vec::new();
    for legacy in candidates {
        match fs::read(&legacy)
            .with_context(|| format!("failed to read {}", legacy.display()))
            .and_then(|bytes| {
                serde_json::from_slice::<PersistedState>(&bytes)
                    .with_context(|| format!("legacy state is invalid at {}", legacy.display()))
            }) {
            Ok(state) => {
                state.save(path)?;
                let skipped = if errors.is_empty() {
                    String::new()
                } else {
                    format!(" (skipped invalid/unreadable state: {})", errors.join(", "))
                };
                return Ok((
                    state,
                    Some(format!("Copied state from {}{skipped}", legacy.display())),
                ));
            }
            Err(_) => errors.push(legacy.display().to_string()),
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(
            "could not read a valid legacy state at {}",
            errors.join(", ")
        );
    }
    PersistedState::load(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_state_is_copied_once_without_touching_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("opencode-cosmic/state.json");
        let new = dir.path().join("opencode-gpui/state.json");
        let state = PersistedState {
            zoom_level: 1.25,
            ..PersistedState::default()
        };
        state.save(&old).unwrap();

        let (loaded, warning) = load_with_legacy(&new).unwrap();
        assert_eq!(loaded.zoom_level, 1.25);
        assert!(warning.unwrap().contains("opencode-cosmic"));
        assert!(old.exists());
        assert!(new.exists());
        let (_, warning) = load_with_legacy(&new).unwrap();
        assert!(warning.is_none());
    }

    #[test]
    fn newest_valid_client_state_wins_without_modifying_either_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let cosmic = dir.path().join("opencode-cosmic/state.json");
        let gtk = dir.path().join("opencode-gtk/state.json");
        let gpui = dir.path().join("opencode-gpui/state.json");
        PersistedState {
            zoom_level: 1.25,
            ..PersistedState::default()
        }
        .save(&cosmic)
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        PersistedState {
            zoom_level: 1.5,
            ..PersistedState::default()
        }
        .save(&gtk)
        .unwrap();
        let cosmic_bytes = fs::read(&cosmic).unwrap();
        let gtk_bytes = fs::read(&gtk).unwrap();
        let (loaded, warning) = load_with_legacy(&gpui).unwrap();
        assert_eq!(loaded.zoom_level, 1.5);
        assert!(warning.unwrap().contains("opencode-gtk"));
        assert_eq!(fs::read(&cosmic).unwrap(), cosmic_bytes);
        assert_eq!(fs::read(&gtk).unwrap(), gtk_bytes);
    }

    #[test]
    fn invalid_newer_client_state_falls_back_without_mutating_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let cosmic = dir.path().join("opencode-cosmic/state.json");
        let gtk = dir.path().join("opencode-gtk/state.json");
        let gpui = dir.path().join("opencode-gpui/state.json");
        PersistedState {
            zoom_level: 1.25,
            ..PersistedState::default()
        }
        .save(&cosmic)
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        fs::create_dir_all(gtk.parent().unwrap()).unwrap();
        fs::write(&gtk, b"invalid JSON").unwrap();
        let (loaded, warning) = load_with_legacy(&gpui).unwrap();
        assert_eq!(loaded.zoom_level, 1.25);
        assert!(warning.unwrap().contains("opencode-gtk"));
        assert_eq!(fs::read(&gtk).unwrap(), b"invalid JSON");
    }

    #[test]
    fn state_round_trips_without_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/state.json");
        let mut state = PersistedState {
            connection: ConnectionSettings {
                server: "https://opencode.example.com".into(),
                username: "danny".into(),
                cloudflare_access: true,
                basic_auth_in_keyring: true,
            },
            ..PersistedState::default()
        };
        state.servers.insert(
            "http://127.0.0.1:4096".into(),
            ServerState {
                tabs: vec![PersistedTab {
                    id: "ses_1".into(),
                    directory: "/repo".into(),
                    title: "Test".into(),
                }],
                active: Some("ses_1".into()),
                selections: HashMap::new(),
                unread: HashSet::from(["ses_1".into()]),
                busy: HashSet::from(["ses_1".into()]),
            },
        );

        state.save(&path).unwrap();
        let (loaded, warning) = PersistedState::load(&path).unwrap();

        assert!(warning.is_none());
        assert_eq!(loaded.servers["http://127.0.0.1:4096"].tabs[0].id, "ses_1");
        assert!(
            loaded.servers["http://127.0.0.1:4096"]
                .unread
                .contains("ses_1")
        );
        assert!(
            loaded.servers["http://127.0.0.1:4096"]
                .busy
                .contains("ses_1")
        );
        assert_eq!(loaded.connection, state.connection);
        assert_eq!(loaded.zoom_level, 1.0);
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("\"basic_auth_in_keyring\": true"));
        assert!(!contents.contains("password"));
        assert!(!contents.contains("secret"));
        assert!(!contents.contains("client.access"));
        assert!(!contents.contains("theme"));
    }

    #[test]
    fn older_state_gets_connection_defaults_and_ignores_legacy_theme() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, br#"{"theme":"light","servers":{}}"#).unwrap();

        let (loaded, warning) = PersistedState::load(&path).unwrap();

        assert!(warning.is_none());
        assert_eq!(loaded.connection, ConnectionSettings::default());
        assert_eq!(loaded.zoom_level, 1.0);
    }

    #[test]
    fn v1_model_selections_still_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(
            &path,
            br#"{"servers":{"https://opencode1.example.com":{
                "tabs":[{"id":"ses_1","directory":"/repo","title":"T"}],
                "active":"ses_1",
                "selections":{
                    "ses_1":{"provider_id":"openai","model_id":"gpt-5.6","variant":"high"},
                    "ses_2":{"provider_id":"openai","model_id":"gpt-5.6"}
                },
                "unread":[],"busy":[]}}}"#,
        )
        .unwrap();

        let (loaded, warning) = PersistedState::load(&path).unwrap();

        assert!(warning.is_none());
        let server = &loaded.servers["https://opencode1.example.com"];
        assert_eq!(server.tabs[0].id, "ses_1");
        assert_eq!(
            server.selections["ses_1"],
            ModelSelection {
                provider_id: "openai".into(),
                model_id: "gpt-5.6".into(),
                variant: Some("high".into()),
            }
        );
        assert_eq!(server.selections["ses_2"].variant, None);
        loaded.save(&path).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"selections\"")
        );

        let mut fresh = PersistedState::default();
        fresh.servers.insert(
            "https://opencode.example.com".into(),
            ServerState::default(),
        );
        fresh.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("selections"));
    }

    #[test]
    fn invalid_state_is_backed_up_before_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, b"not json").unwrap();

        let (loaded, warning) = PersistedState::load(&path).unwrap();

        assert!(loaded.servers.is_empty());
        assert!(warning.is_some());
        assert!(!path.exists());
        assert!(
            path.with_extension(format!("json.corrupt.{}", std::process::id()))
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        PersistedState::default().save(&path).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn state_saver_coalesces_async_writes_and_flushes_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let (saver, errors) = StateSaver::start();

        for step in 1..=25 {
            let state = PersistedState {
                zoom_level: 1.0 + step as f64 * 0.05,
                ..Default::default()
            };
            saver.enqueue(path.clone(), state);
        }
        drop(saver);

        assert!(errors.try_recv().is_err());
        let (loaded, warning) = PersistedState::load(&path).unwrap();
        assert!(warning.is_none());
        assert!((loaded.zoom_level - 2.25).abs() < 1e-9);
    }

    #[test]
    fn state_saver_serializes_sync_save_after_queued_async_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let (saver, errors) = StateSaver::start();

        let first = PersistedState {
            zoom_level: 1.1,
            ..Default::default()
        };
        saver.enqueue(path.clone(), first);

        let second = PersistedState {
            zoom_level: 1.4,
            ..Default::default()
        };
        saver.save_sync(path.clone(), second).unwrap();

        let (loaded, _) = PersistedState::load(&path).unwrap();
        assert!((loaded.zoom_level - 1.4).abs() < 1e-9);
        drop(saver);
        assert!(errors.try_recv().is_err());
    }

    #[test]
    fn state_saver_reports_async_write_errors() {
        let directory = tempfile::tempdir().unwrap();
        // Blocking parent creation with a regular file forces save() to fail.
        let blocker = directory.path().join("not-a-dir");
        fs::write(&blocker, b"x").unwrap();
        let bad_path = blocker.join("state.json");

        let (saver, errors) = StateSaver::start();
        saver.enqueue(bad_path, PersistedState::default());
        drop(saver);

        let error = errors.try_recv().expect("async error should be reported");
        assert!(
            error.contains("failed to create"),
            "unexpected error: {error}"
        );
    }
}
