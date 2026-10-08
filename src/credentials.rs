use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use url::Url;

const KEYRING_SERVICE: &str = "ai.opencode.Gpui.cloudflare-access";
const PASSWORD_KEYRING_SERVICE: &str = "ai.opencode.Gpui.basic-auth";
const LEGACY_KEYRING_SERVICE: &str = "ai.opencode.Cosmic.cloudflare-access";
const LEGACY_PASSWORD_KEYRING_SERVICE: &str = "ai.opencode.Cosmic.basic-auth";
const STORED_VERSION: u8 = 1;

/// Where secrets live. The app uses [`SystemKeyring`]; tests inject an
/// in-memory store so they never touch a real keyring.
pub trait SecretStore {
    fn get(&self, service: &str, account: &str) -> keyring::Result<String>;
    fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()>;
    fn delete(&self, service: &str, account: &str) -> keyring::Result<()>;
}

/// The desktop's Secret Service provider (GNOME Keyring, KWallet, ...).
pub struct SystemKeyring;

impl SecretStore for SystemKeyring {
    fn get(&self, service: &str, account: &str) -> keyring::Result<String> {
        Entry::new(service, account)?.get_password()
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()> {
        Entry::new(service, account)?.set_password(secret)
    }

    fn delete(&self, service: &str, account: &str) -> keyring::Result<()> {
        Entry::new(service, account)?.delete_credential()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
pub struct CloudflareAccessCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl CloudflareAccessCredentials {
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let client_id = client_id.trim().to_owned();
        let client_secret = client_secret.trim().to_owned();
        if client_id.is_empty() || client_secret.is_empty() {
            bail!("Cloudflare Access client ID and secret are both required");
        }
        Ok(Self {
            client_id,
            client_secret,
        })
    }
}

impl fmt::Debug for CloudflareAccessCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudflareAccessCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct StoredCredentials {
    version: u8,
    credentials: CloudflareAccessCredentials,
}

pub fn load(server: &str) -> Result<Option<CloudflareAccessCredentials>> {
    load_from(&SystemKeyring, server)
}

fn load_from(
    store: &impl SecretStore,
    server: &str,
) -> Result<Option<CloudflareAccessCredentials>> {
    let account = server_account(server)?;
    let stored = match store.get(KEYRING_SERVICE, &account) {
        Ok(stored) => stored,
        Err(KeyringError::NoEntry) => match store.get(LEGACY_KEYRING_SERVICE, &account) {
            Ok(stored) => {
                // Copy, never move: an installed older client can still read its entry.
                // Validate before writing: a corrupt old entry must not shadow
                // a later repair under the new service name.
                decode(&stored)?;
                store
                    .set(KEYRING_SERVICE, &account, &stored)
                    .context("failed to migrate Cloudflare Access credentials")?;
                stored
            }
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(error) => {
                return Err(error).context("failed to read legacy Cloudflare Access credentials");
            }
        },
        Err(error) => return Err(error).context("failed to read Cloudflare Access credentials"),
    };
    decode(&stored).map(Some)
}

pub fn save(server: &str, credentials: &CloudflareAccessCredentials) -> Result<()> {
    let stored = serde_json::to_string(&StoredCredentials {
        version: STORED_VERSION,
        credentials: credentials.clone(),
    })
    .context("failed to encode Cloudflare Access credentials")?;
    SystemKeyring
        .set(KEYRING_SERVICE, &server_account(server)?, &stored)
        .context("failed to save Cloudflare Access credentials")
}

pub fn remove(server: &str) -> Result<()> {
    let account = server_account(server)?;
    for service in [KEYRING_SERVICE, LEGACY_KEYRING_SERVICE] {
        match SystemKeyring.delete(service, &account) {
            Ok(()) | Err(KeyringError::NoEntry) => {}
            Err(error) => {
                return Err(error).context("failed to remove Cloudflare Access credentials");
            }
        }
    }
    Ok(())
}

fn server_account(server: &str) -> Result<String> {
    let mut url = Url::parse(server.trim()).context("invalid OpenCode server URL")?;
    url.set_fragment(None);
    url.set_query(None);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn decode(stored: &str) -> Result<CloudflareAccessCredentials> {
    let stored: StoredCredentials =
        serde_json::from_str(stored).context("Cloudflare Access keyring entry is invalid")?;
    if stored.version != STORED_VERSION {
        bail!("Cloudflare Access keyring entry uses an unsupported version");
    }
    CloudflareAccessCredentials::new(
        stored.credentials.client_id,
        stored.credentials.client_secret,
    )
}

#[derive(Deserialize, Serialize)]
struct StoredPassword {
    version: u8,
    password: String,
}

/// The keyring account of an OpenCode Basic password: the username and the
/// server's mount root, as in `https://opencode@opencode.example.com/prefix`.
/// The root is the one the client talks to (scheme, lowercased host,
/// non-default port and mount prefix, without a trailing slash or `/api`), so
/// every spelling of one server shares an entry while another server, port,
/// mount or user never sees the password.
pub fn password_account(server: &str, username: &str) -> Result<String> {
    if username.is_empty() {
        bail!("OpenCode username is required");
    }
    let mut url = crate::api::mount_root(server)?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        bail!("OpenCode server URL must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("put OpenCode credentials in the username and password options, not the URL");
    }
    url.set_query(None);
    url.set_fragment(None);
    url.set_username(username)
        .map_err(|()| anyhow!("OpenCode username cannot be stored in the keyring"))?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Whether two connections share a password entry: the same server root and
/// the same username.
pub fn same_password_identity(
    left_server: &str,
    left_username: &str,
    right_server: &str,
    right_username: &str,
) -> bool {
    match (
        password_account(left_server, left_username),
        password_account(right_server, right_username),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => {
            left_username == right_username
                && left_server.trim().trim_end_matches('/')
                    == right_server.trim().trim_end_matches('/')
        }
    }
}

pub fn load_password(
    store: &impl SecretStore,
    server: &str,
    username: &str,
) -> Result<Option<String>> {
    let account = password_account(server, username)?;
    let stored = match store.get(PASSWORD_KEYRING_SERVICE, &account) {
        Ok(stored) => stored,
        Err(KeyringError::NoEntry) => match store.get(LEGACY_PASSWORD_KEYRING_SERVICE, &account) {
            Ok(stored) => {
                decode_password(&stored)?;
                store
                    .set(PASSWORD_KEYRING_SERVICE, &account, &stored)
                    .map_err(|error| {
                        anyhow!("could not migrate the stored OpenCode password: {error}")
                    })?;
                stored
            }
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(error) => return Err(anyhow!("could not read the legacy system keyring: {error}")),
        },
        Err(error) => return Err(anyhow!("could not read the system keyring: {error}")),
    };
    decode_password(&stored).map(Some)
}

fn decode_password(stored: &str) -> Result<String> {
    // Never chain the JSON error: it can quote the stored value.
    let stored: StoredPassword = serde_json::from_str(stored)
        .map_err(|_| anyhow!("the stored OpenCode password entry is invalid"))?;
    if stored.version != STORED_VERSION {
        bail!("the stored OpenCode password entry uses an unsupported version");
    }
    Ok(stored.password)
}

pub fn save_password(
    store: &impl SecretStore,
    server: &str,
    username: &str,
    password: &str,
) -> Result<()> {
    if password.is_empty() {
        bail!("an empty OpenCode password is not stored");
    }
    let account = password_account(server, username)?;
    let stored = serde_json::to_string(&StoredPassword {
        version: STORED_VERSION,
        password: password.to_owned(),
    })
    .map_err(|_| anyhow!("could not encode the OpenCode password"))?;
    store
        .set(PASSWORD_KEYRING_SERVICE, &account, &stored)
        .map_err(|error| anyhow!("could not write the system keyring: {error}"))
}

pub fn remove_password(store: &impl SecretStore, server: &str, username: &str) -> Result<()> {
    let account = password_account(server, username)?;
    for service in [PASSWORD_KEYRING_SERVICE, LEGACY_PASSWORD_KEYRING_SERVICE] {
        match store.delete(service, &account) {
            Ok(()) | Err(KeyringError::NoEntry) => {}
            Err(error) => return Err(anyhow!("could not update the system keyring: {error}")),
        }
    }
    Ok(())
}

/// The Basic password to start with and whether it came from the keyring.
#[derive(Default, PartialEq, Eq)]
pub struct PasswordLoad {
    pub password: Option<String>,
    pub stored: bool,
    pub warning: Option<String>,
}

impl fmt::Debug for PasswordLoad {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PasswordLoad")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("stored", &self.stored)
            .field("warning", &self.warning)
            .finish()
    }
}

/// Startup precedence: a `--password`/`OPENCODE_SERVER_PASSWORD` value, then
/// the keyring entry of this server and username (only read when
/// `load_stored`, i.e. a password was saved from Settings), then none.
/// `expect_entry` reports a missing entry, for the connection it was saved
/// for. Keyring errors never block connecting; they become a warning.
pub fn initial_password(
    store: &impl SecretStore,
    server: &str,
    username: &str,
    cli_password: Option<String>,
    load_stored: bool,
    expect_entry: bool,
) -> PasswordLoad {
    if let Some(password) = cli_password.filter(|password| !password.is_empty()) {
        return PasswordLoad {
            password: Some(password),
            ..PasswordLoad::default()
        };
    }
    if !load_stored {
        return PasswordLoad::default();
    }
    match load_password(store, server, username) {
        Ok(Some(password)) => PasswordLoad {
            password: Some(password),
            stored: true,
            warning: None,
        },
        Ok(None) => PasswordLoad {
            warning: expect_entry.then(|| {
                "The saved OpenCode password was not found in the system keyring; enter it in Settings"
                    .to_owned()
            }),
            ..PasswordLoad::default()
        },
        Err(error) => PasswordLoad {
            warning: Some(format!(
                "The saved OpenCode password is unavailable ({error}); enter it in Settings to keep it in memory"
            )),
            ..PasswordLoad::default()
        },
    }
}

/// What applying Settings does to the keyring entry of the new connection.
#[derive(PartialEq, Eq)]
pub enum KeyringChange {
    Keep,
    Save(String),
    Remove,
}

impl fmt::Debug for KeyringChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Keep => "Keep",
            Self::Save(_) => "Save(<redacted>)",
            Self::Remove => "Remove",
        })
    }
}

#[derive(PartialEq, Eq)]
pub struct PasswordPlan {
    pub password: Option<String>,
    pub change: KeyringChange,
    /// Whether the keyring holds the password once `change` succeeds.
    pub stored: bool,
    pub warning: Option<String>,
}

impl fmt::Debug for PasswordPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PasswordPlan")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("change", &self.change)
            .field("stored", &self.stored)
            .field("warning", &self.warning)
            .finish()
    }
}

/// The connection a Settings form applies to.
pub struct PasswordTarget<'a> {
    pub server: &'a str,
    pub username: &'a str,
}

/// The password the Settings form connects with, and its keyring change.
///
/// A typed password is used and, with `remember`, saved for the new server and
/// username. A blank field keeps the current password when the server and user
/// are unchanged, and otherwise loads the new connection's own keyring entry,
/// so a password never follows a URL or username change. Unchecking
/// `remember` forgets the stored password of an unchanged connection. A
/// password that only came from the command line or environment is never
/// written by a blank field.
pub fn plan_password(
    store: &impl SecretStore,
    current: PasswordTarget<'_>,
    current_password: Option<&str>,
    current_stored: bool,
    next: PasswordTarget<'_>,
    entered: &str,
    remember: bool,
) -> PasswordPlan {
    let same_identity =
        same_password_identity(current.server, current.username, next.server, next.username);
    if !entered.is_empty() {
        return PasswordPlan {
            password: Some(entered.to_owned()),
            change: if remember {
                KeyringChange::Save(entered.to_owned())
            } else if same_identity && current_stored {
                KeyringChange::Remove
            } else {
                KeyringChange::Keep
            },
            stored: remember,
            warning: None,
        };
    }
    if same_identity {
        let remove = !remember && current_stored;
        return PasswordPlan {
            password: current_password.map(str::to_owned),
            change: if remove {
                KeyringChange::Remove
            } else {
                KeyringChange::Keep
            },
            stored: current_stored && !remove,
            warning: None,
        };
    }
    if !remember {
        return PasswordPlan {
            password: None,
            change: KeyringChange::Keep,
            stored: false,
            warning: None,
        };
    }
    match load_password(store, next.server, next.username) {
        Ok(password) => PasswordPlan {
            stored: password.is_some(),
            password,
            change: KeyringChange::Keep,
            warning: None,
        },
        Err(error) => PasswordPlan {
            password: None,
            change: KeyringChange::Keep,
            stored: false,
            warning: Some(format!(
                "The saved OpenCode password is unavailable ({error}); enter it in Settings to keep it in memory"
            )),
        },
    }
}

/// Applies a plan's keyring change once the connection is accepted. Returns
/// whether the keyring now holds the password, and a warning on failure; a
/// failure never blocks the connection, which keeps the password in memory.
pub fn apply_password_change(
    store: &impl SecretStore,
    server: &str,
    username: &str,
    plan: &PasswordPlan,
) -> (bool, Option<String>) {
    match &plan.change {
        KeyringChange::Keep => (plan.stored, plan.warning.clone()),
        KeyringChange::Save(password) => match save_password(store, server, username, password) {
            Ok(()) => (true, plan.warning.clone()),
            Err(error) => (
                false,
                Some(format!(
                    "The OpenCode password was not saved ({error}); it stays in memory for this session"
                )),
            ),
        },
        KeyringChange::Remove => match remove_password(store, server, username) {
            Ok(()) => (false, plan.warning.clone()),
            Err(error) => (
                false,
                Some(format!(
                    "The stored OpenCode password could not be removed ({error})"
                )),
            ),
        },
    }
}

#[cfg(test)]
pub mod testing {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;

    /// An in-memory [`SecretStore`] that can simulate an unavailable keyring.
    #[derive(Default)]
    pub struct MemoryStore {
        pub entries: RefCell<HashMap<(String, String), String>>,
        pub failing: bool,
    }

    impl MemoryStore {
        pub fn failing() -> Self {
            Self {
                failing: true,
                ..Self::default()
            }
        }

        fn fail() -> KeyringError {
            KeyringError::NoStorageAccess("secret service is locked".into())
        }
    }

    impl SecretStore for MemoryStore {
        fn get(&self, service: &str, account: &str) -> keyring::Result<String> {
            if self.failing {
                return Err(Self::fail());
            }
            self.entries
                .borrow()
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
                .ok_or(KeyringError::NoEntry)
        }

        fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()> {
            if self.failing {
                return Err(Self::fail());
            }
            self.entries
                .borrow_mut()
                .insert((service.to_owned(), account.to_owned()), secret.to_owned());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> keyring::Result<()> {
            if self.failing {
                return Err(Self::fail());
            }
            self.entries
                .borrow_mut()
                .remove(&(service.to_owned(), account.to_owned()))
                .map(|_| ())
                .ok_or(KeyringError::NoEntry)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::MemoryStore, *};

    const SERVER: &str = "https://opencode.example.com";

    fn target<'a>(server: &'a str, username: &'a str) -> PasswordTarget<'a> {
        PasswordTarget { server, username }
    }

    #[test]
    fn stored_credentials_round_trip_without_debugging_the_secret() {
        let credentials =
            CloudflareAccessCredentials::new("client.access".into(), "super-secret".into())
                .unwrap();
        let stored = serde_json::to_string(&StoredCredentials {
            version: STORED_VERSION,
            credentials: credentials.clone(),
        })
        .unwrap();

        assert_eq!(decode(&stored).unwrap(), credentials);
        assert!(!format!("{credentials:?}").contains("super-secret"));
    }

    #[test]
    fn cloudflare_credentials_migrate_without_deleting_the_cosmic_entry() {
        let store = MemoryStore::default();
        let account = server_account(SERVER).unwrap();
        let credentials =
            CloudflareAccessCredentials::new("client".into(), "secret".into()).unwrap();
        let encoded = serde_json::to_string(&StoredCredentials {
            version: STORED_VERSION,
            credentials: credentials.clone(),
        })
        .unwrap();
        store
            .set(LEGACY_KEYRING_SERVICE, &account, &encoded)
            .unwrap();
        assert_eq!(load_from(&store, SERVER).unwrap(), Some(credentials));
        assert_eq!(store.get(KEYRING_SERVICE, &account).unwrap(), encoded);
        assert_eq!(
            store.get(LEGACY_KEYRING_SERVICE, &account).unwrap(),
            encoded
        );
    }

    #[test]
    fn corrupt_legacy_cloudflare_entry_is_not_migrated() {
        let store = MemoryStore::default();
        let account = server_account(SERVER).unwrap();
        store
            .set(LEGACY_KEYRING_SERVICE, &account, "invalid")
            .unwrap();
        assert!(load_from(&store, SERVER).is_err());
        assert!(matches!(
            store.get(KEYRING_SERVICE, &account),
            Err(KeyringError::NoEntry)
        ));
    }

    #[test]
    fn server_account_is_canonical_and_secret_free() {
        assert_eq!(
            server_account("https://OpenCode.Example.com/").unwrap(),
            "https://opencode.example.com"
        );
    }

    #[test]
    fn password_account_normalizes_the_server_root_and_names_the_user() {
        let expected = "https://opencode@opencode.example.com";
        for server in [
            "https://opencode.example.com",
            "https://OpenCode.Example.com/",
            " https://opencode.example.com:443 ",
            "https://opencode.example.com/api",
            "https://opencode.example.com/api/",
        ] {
            assert_eq!(
                password_account(server, "opencode").unwrap(),
                expected,
                "{server}"
            );
        }
        assert_eq!(
            password_account("http://127.0.0.1:4096/", "opencode").unwrap(),
            "http://opencode@127.0.0.1:4096"
        );
        assert_eq!(
            password_account("https://host/prefix/api/", "danny").unwrap(),
            "https://danny@host/prefix"
        );
        assert_eq!(
            password_account("https://host", "a@b:c").unwrap(),
            "https://a%40b%3Ac@host"
        );
        let distinct = [
            password_account("https://host", "opencode").unwrap(),
            password_account("http://host", "opencode").unwrap(),
            password_account("https://host:8443", "opencode").unwrap(),
            password_account("https://other", "opencode").unwrap(),
            password_account("https://host/prefix", "opencode").unwrap(),
            password_account("https://host", "Opencode").unwrap(),
        ];
        for (index, account) in distinct.iter().enumerate() {
            assert!(!distinct[index + 1..].contains(account), "{account}");
        }
        assert!(password_account("https://host", "").is_err());
        assert!(password_account("not a url", "opencode").is_err());
        assert!(password_account("https://user:pw@host", "opencode").is_err());
    }

    #[test]
    fn passwords_save_update_load_and_clear_per_connection() {
        let store = MemoryStore::default();
        assert_eq!(load_password(&store, SERVER, "opencode").unwrap(), None);

        save_password(&store, SERVER, "opencode", "first").unwrap();
        assert_eq!(
            load_password(&store, "https://opencode.example.com/api/", "opencode").unwrap(),
            Some("first".into())
        );
        save_password(&store, SERVER, "opencode", "second").unwrap();
        assert_eq!(
            load_password(&store, SERVER, "opencode").unwrap(),
            Some("second".into())
        );
        assert_eq!(store.entries.borrow().len(), 1);
        let (key, value) = store
            .entries
            .borrow()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .next()
            .unwrap();
        assert_eq!(
            key,
            (
                PASSWORD_KEYRING_SERVICE.to_owned(),
                "https://opencode@opencode.example.com".to_owned()
            )
        );
        assert_eq!(value, r#"{"version":1,"password":"second"}"#);

        assert_eq!(
            load_password(&store, "https://other.example.com", "opencode").unwrap(),
            None
        );
        assert_eq!(load_password(&store, SERVER, "danny").unwrap(), None);
        assert!(save_password(&store, SERVER, "opencode", "").is_err());

        remove_password(&store, SERVER, "opencode").unwrap();
        assert_eq!(load_password(&store, SERVER, "opencode").unwrap(), None);
        remove_password(&store, SERVER, "opencode").unwrap();
    }

    #[test]
    fn legacy_cosmic_password_is_copied_then_removed_with_new_entry() {
        let store = MemoryStore::default();
        let account = password_account(SERVER, "opencode").unwrap();
        store
            .set(
                LEGACY_PASSWORD_KEYRING_SERVICE,
                &account,
                r#"{"version":1,"password":"existing-password"}"#,
            )
            .unwrap();
        assert_eq!(
            load_password(&store, SERVER, "opencode").unwrap(),
            Some("existing-password".into())
        );
        assert!(store.get(PASSWORD_KEYRING_SERVICE, &account).is_ok());
        assert!(store.get(LEGACY_PASSWORD_KEYRING_SERVICE, &account).is_ok());
        remove_password(&store, SERVER, "opencode").unwrap();
        assert_eq!(load_password(&store, SERVER, "opencode").unwrap(), None);
    }

    #[test]
    fn invalid_legacy_password_is_not_copied_to_the_new_service() {
        let store = MemoryStore::default();
        let account = password_account(SERVER, "opencode").unwrap();
        store
            .set(
                LEGACY_PASSWORD_KEYRING_SERVICE,
                &account,
                "broken legacy entry",
            )
            .unwrap();
        let error = load_password(&store, SERVER, "opencode").unwrap_err();
        assert!(!format!("{error:#}").contains("broken legacy entry"));
        assert!(matches!(
            store.get(PASSWORD_KEYRING_SERVICE, &account),
            Err(KeyringError::NoEntry)
        ));
    }

    #[test]
    fn invalid_password_entries_never_echo_the_stored_value() {
        let store = MemoryStore::default();
        let account = password_account(SERVER, "opencode").unwrap();
        for stored in [
            r#"{"version":1,"password":7,"hint":"hunter2"}"#,
            "hunter2",
            r#"{"version":2,"password":"hunter2"}"#,
        ] {
            store.entries.borrow_mut().insert(
                (PASSWORD_KEYRING_SERVICE.to_owned(), account.clone()),
                stored.to_owned(),
            );
            let error = load_password(&store, SERVER, "opencode").unwrap_err();
            assert!(!format!("{error:#}").contains("hunter2"), "{error:#}");
        }
    }

    #[test]
    fn startup_prefers_the_command_line_then_the_keyring() {
        let store = MemoryStore::default();
        save_password(&store, SERVER, "opencode", "stored").unwrap();

        let load = initial_password(&store, SERVER, "opencode", Some("cli".into()), true, true);
        assert_eq!(load.password.as_deref(), Some("cli"));
        assert!(!load.stored);
        assert_eq!(load.warning, None);

        let load = initial_password(&store, SERVER, "opencode", None, true, true);
        assert_eq!(load.password.as_deref(), Some("stored"));
        assert!(load.stored);
        assert_eq!(load.warning, None);

        // An empty environment variable counts as unset.
        let load = initial_password(&store, SERVER, "opencode", Some(String::new()), true, true);
        assert_eq!(load.password.as_deref(), Some("stored"));

        // Nothing was saved from Settings: the keyring is not read.
        assert_eq!(
            initial_password(&store, SERVER, "opencode", None, false, false),
            PasswordLoad::default()
        );
        assert_eq!(
            initial_password(
                &MemoryStore::failing(),
                SERVER,
                "opencode",
                None,
                false,
                false
            ),
            PasswordLoad::default()
        );

        // Another server or user never gets the stored password.
        let load = initial_password(
            &store,
            "https://other.example.com",
            "opencode",
            None,
            true,
            false,
        );
        assert_eq!(load, PasswordLoad::default());
        let load = initial_password(&store, SERVER, "danny", None, true, true);
        assert_eq!(load.password, None);
        assert!(load.warning.unwrap().contains("not found"));

        // A command-line password is never written to the keyring.
        let empty = MemoryStore::default();
        initial_password(&empty, SERVER, "opencode", Some("cli".into()), true, true);
        assert!(empty.entries.borrow().is_empty());
    }

    #[test]
    fn startup_falls_back_to_memory_when_the_keyring_fails() {
        let load = initial_password(
            &MemoryStore::failing(),
            SERVER,
            "opencode",
            None,
            true,
            true,
        );
        assert_eq!(load.password, None);
        assert!(!load.stored);
        let warning = load.warning.unwrap();
        assert!(warning.contains("unavailable"), "{warning}");
        assert!(warning.contains("locked"), "{warning}");

        let load = initial_password(
            &MemoryStore::failing(),
            SERVER,
            "opencode",
            Some("cli".into()),
            true,
            true,
        );
        assert_eq!(load.password.as_deref(), Some("cli"));
        assert_eq!(load.warning, None);
    }

    #[test]
    fn settings_save_update_keep_and_forget_the_password() {
        let store = MemoryStore::default();
        let apply = |plan: &PasswordPlan, server: &str, username: &str| {
            apply_password_change(&store, server, username, plan)
        };

        // Typing a password stores it.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            None,
            false,
            target(SERVER, "opencode"),
            "first",
            true,
        );
        assert_eq!(plan.change, KeyringChange::Save("first".into()));
        assert_eq!(apply(&plan, SERVER, "opencode"), (true, None));
        assert_eq!(
            load_password(&store, SERVER, "opencode").unwrap(),
            Some("first".into())
        );

        // A blank field keeps it without rewriting.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("first"),
            true,
            target("https://opencode.example.com/", "opencode"),
            "",
            true,
        );
        assert_eq!(plan.password.as_deref(), Some("first"));
        assert_eq!(plan.change, KeyringChange::Keep);
        assert!(plan.stored);

        // Typing a new one updates it.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("first"),
            true,
            target(SERVER, "opencode"),
            "second",
            true,
        );
        assert_eq!(apply(&plan, SERVER, "opencode"), (true, None));
        assert_eq!(
            load_password(&store, SERVER, "opencode").unwrap(),
            Some("second".into())
        );

        // Unchecking Remember forgets it but stays connected with it.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("second"),
            true,
            target(SERVER, "opencode"),
            "",
            false,
        );
        assert_eq!(plan.password.as_deref(), Some("second"));
        assert_eq!(plan.change, KeyringChange::Remove);
        assert_eq!(apply(&plan, SERVER, "opencode"), (false, None));
        assert_eq!(load_password(&store, SERVER, "opencode").unwrap(), None);

        // A typed password with Remember unchecked is memory-only.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            None,
            false,
            target(SERVER, "opencode"),
            "typed",
            false,
        );
        assert_eq!(plan.password.as_deref(), Some("typed"));
        assert_eq!(plan.change, KeyringChange::Keep);
        assert!(!plan.stored);
        assert!(store.entries.borrow().is_empty());
    }

    #[test]
    fn a_command_line_password_is_not_saved_by_a_blank_settings_form() {
        let store = MemoryStore::default();
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("from-env"),
            false,
            target(SERVER, "opencode"),
            "",
            true,
        );
        assert_eq!(plan.password.as_deref(), Some("from-env"));
        assert_eq!(plan.change, KeyringChange::Keep);
        assert_eq!(
            apply_password_change(&store, SERVER, "opencode", &plan),
            (false, None)
        );
        assert!(store.entries.borrow().is_empty());
    }

    #[test]
    fn settings_never_reuse_a_password_across_servers_or_users() {
        let store = MemoryStore::default();
        save_password(&store, SERVER, "opencode", "server-a").unwrap();
        save_password(&store, "https://b.example.com", "opencode", "server-b").unwrap();

        // Another server without an entry: the current password does not follow.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("server-a"),
            true,
            target("https://c.example.com", "opencode"),
            "",
            true,
        );
        assert_eq!(plan.password, None);
        assert!(!plan.stored);
        assert_eq!(plan.change, KeyringChange::Keep);

        // Another user on the same server: same.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("server-a"),
            true,
            target(SERVER, "danny"),
            "",
            true,
        );
        assert_eq!(plan.password, None);

        // A server with its own entry uses that one.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("server-a"),
            true,
            target("https://b.example.com/api", "opencode"),
            "",
            true,
        );
        assert_eq!(plan.password.as_deref(), Some("server-b"));
        assert!(plan.stored);

        // Unchecking Remember while switching neither loads nor deletes.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("server-a"),
            true,
            target("https://b.example.com", "opencode"),
            "",
            false,
        );
        assert_eq!(plan.password, None);
        assert_eq!(plan.change, KeyringChange::Keep);

        // Saving for the new server leaves the old entry alone.
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("server-a"),
            true,
            target("https://c.example.com", "opencode"),
            "server-c",
            true,
        );
        apply_password_change(&store, "https://c.example.com", "opencode", &plan);
        assert_eq!(
            load_password(&store, SERVER, "opencode").unwrap(),
            Some("server-a".into())
        );
        assert_eq!(
            load_password(&store, "https://c.example.com", "opencode").unwrap(),
            Some("server-c".into())
        );
    }

    #[test]
    fn settings_fall_back_to_memory_when_the_keyring_fails() {
        let store = MemoryStore::failing();
        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            None,
            false,
            target(SERVER, "opencode"),
            "typed",
            true,
        );
        assert_eq!(plan.password.as_deref(), Some("typed"));
        let (stored, warning) = apply_password_change(&store, SERVER, "opencode", &plan);
        assert!(!stored);
        let warning = warning.unwrap();
        assert!(warning.contains("stays in memory"), "{warning}");
        assert!(!warning.contains("typed"), "{warning}");

        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            None,
            false,
            target("https://b.example.com", "opencode"),
            "",
            true,
        );
        assert_eq!(plan.password, None);
        assert!(plan.warning.unwrap().contains("unavailable"));

        let plan = plan_password(
            &store,
            target(SERVER, "opencode"),
            Some("typed"),
            true,
            target(SERVER, "opencode"),
            "",
            false,
        );
        let (stored, warning) = apply_password_change(&store, SERVER, "opencode", &plan);
        assert!(!stored);
        assert!(warning.unwrap().contains("could not be removed"));
    }

    #[test]
    fn password_plans_never_debug_the_secret() {
        let plan = PasswordPlan {
            password: Some("hunter2".into()),
            change: KeyringChange::Save("hunter2".into()),
            stored: true,
            warning: None,
        };
        let load = PasswordLoad {
            password: Some("hunter2".into()),
            stored: true,
            warning: None,
        };
        assert!(!format!("{plan:?}{load:?}").contains("hunter2"));
    }
}
