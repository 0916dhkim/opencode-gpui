use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::ModelSelection;

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
        let temporary = parent.join(format!(".state.json.{}.{}.tmp", std::process::id(), nonce));
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
}
