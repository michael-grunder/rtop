use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::model::{Target, TargetProtocol};
use crate::target_addr::tcp_endpoint_identity;

const FILE_NAME: &str = "rtop-auth.toml";

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
struct Credentials {
    username: Option<String>,
    password: Option<String>,
}

impl Credentials {
    fn from_target(target: &Target) -> Self {
        Self {
            username: target.username.clone(),
            password: target.password.clone(),
        }
    }

    fn apply(&self, target: &mut Target) {
        target.username.clone_from(&self.username);
        target.password.clone_from(&self.password);
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedTarget {
    addr: String,
    protocol: TargetProtocol,
    #[serde(flatten)]
    credentials: Credentials,
}

#[derive(Default, Deserialize, Serialize)]
struct CredentialFile {
    #[serde(default)]
    targets: Vec<SavedTarget>,
}

/// An opt-in credential cache shared by polling and discovery connections.
/// Config-declared credentials replace the saved pair, even if an environment
/// variable did not resolve to a password.
pub struct CredentialStore {
    path: PathBuf,
    configured: HashMap<String, Credentials>,
    saved: Mutex<CredentialFile>,
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    pub fn load(path: PathBuf, configured: &[Target]) -> Result<Self> {
        let saved = read_file(&path)?;
        Ok(Self {
            path,
            configured: configured
                .iter()
                .map(|target| {
                    (
                        endpoint_key(target.protocol, &target.addr),
                        Credentials::from_target(target),
                    )
                })
                .collect(),
            saved: Mutex::new(saved),
        })
    }

    pub fn apply(&self, target: &mut Target) -> Result<()> {
        // Explicit credentials (including an interactive retry) take precedence.
        if target.username.is_some() || target.password.is_some() {
            return Ok(());
        }
        let key = endpoint_key(target.protocol, &target.addr);
        if let Some(credentials) = self.configured.get(&key) {
            credentials.apply(target);
        } else {
            let saved = self
                .saved
                .lock()
                .map_err(|_| anyhow!("credential cache lock poisoned"))?;
            if let Some(entry) = saved
                .targets
                .iter()
                .find(|entry| endpoint_key(entry.protocol, &entry.addr) == key)
            {
                entry.credentials.apply(target);
            }
        }
        Ok(())
    }

    /// Call only after Redis has accepted AUTH, never after merely connecting.
    pub(crate) fn remember_authenticated(&self, target: &Target) -> Result<()> {
        if target.password.is_none() {
            return Ok(());
        }
        let key = endpoint_key(target.protocol, &target.addr);
        let credentials = Credentials::from_target(target);
        {
            let saved = self
                .saved
                .lock()
                .map_err(|_| anyhow!("credential cache lock poisoned"))?;
            if saved.targets.iter().any(|entry| {
                endpoint_key(entry.protocol, &entry.addr) == key && entry.credentials == credentials
            }) {
                return Ok(());
            }
        }

        let parent = self
            .path
            .parent()
            .context("credential file has no parent directory")?;
        fs::create_dir_all(parent).context("failed to create credential directory")?;
        // Lock a stable sibling, since the data file is replaced atomically.
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(self.path.with_extension("lock"))
            .context("failed to open credential file lock")?;
        lock.lock().context("failed to lock credential file")?;
        // Merge the latest disk contents so simultaneous rtop processes don't
        // discard credentials saved for other servers.
        let mut latest = read_file(&self.path)?;
        latest
            .targets
            .retain(|entry| endpoint_key(entry.protocol, &entry.addr) != key);
        latest.targets.push(SavedTarget {
            addr: target.addr.clone(),
            protocol: target.protocol,
            credentials,
        });
        latest
            .targets
            .sort_by_key(|entry| endpoint_key(entry.protocol, &entry.addr));
        let content = toml::to_string_pretty(&latest).context("failed to encode credentials")?;
        // NamedTempFile creates a private (0600 on Unix) file in the same
        // directory, then persist replaces the old file without a partial write.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("failed to create temporary credential file")?;
        writeln!(
            temporary,
            "# Generated by rtop. Contains plaintext credentials; do not edit.\n{content}"
        )
        .context("failed to write credential file")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to sync credential file")?;
        temporary
            .persist(&self.path)
            .context("failed to replace credential file")?;
        // Keep the in-memory lock out of disk I/O: async credential lookups
        // should never wait for another process to release the file lock.
        *self
            .saved
            .lock()
            .map_err(|_| anyhow!("credential cache lock poisoned"))? = latest;
        Ok(())
    }
}

pub fn default_path() -> Result<PathBuf> {
    path_from_dirs(
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
    )
}

fn path_from_dirs(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|directory| directory.join(FILE_NAME))
        .context("remember_auth requires an absolute XDG_CONFIG_HOME or HOME")
}

fn read_file(path: &Path) -> Result<CredentialFile> {
    match fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content)
            // TOML diagnostics may quote passwords from the input. Do not attach them.
            .map_err(|_| {
                anyhow!(
                    "failed to parse generated credential file {}",
                    path.display()
                )
            }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(CredentialFile::default()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read credential file {}", path.display()))
        }
    }
}

fn endpoint_key(protocol: TargetProtocol, addr: &str) -> String {
    match protocol {
        TargetProtocol::Tcp => format!(
            "tcp:{}",
            tcp_endpoint_identity(addr).unwrap_or_else(|| addr.to_owned())
        ),
        TargetProtocol::Unix => format!("unix:{addr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(addr: &str, protocol: TargetProtocol, password: Option<&str>) -> Target {
        Target {
            alias: None,
            addr: addr.to_owned(),
            protocol,
            username: None,
            password: password.map(str::to_owned),
            tags: Vec::new(),
            process_id: None,
        }
    }

    #[test]
    fn private_file_round_trips_multiple_servers_and_replaces_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join(FILE_NAME);
        let store = CredentialStore::load(path.clone(), &[]).unwrap();
        assert!(!path.exists());
        let mut tcp = target("localhost:6380", TargetProtocol::Tcp, Some("old-secret"));
        let unix = target("/tmp/redis.sock", TargetProtocol::Unix, Some("unix-secret"));
        store.remember_authenticated(&tcp).unwrap();
        store.remember_authenticated(&unix).unwrap();
        tcp.addr = "127.0.0.1:6380".to_owned();
        tcp.username = Some("alice".to_owned());
        tcp.password = Some("new-secret\n\"\\".to_owned());
        store.remember_authenticated(&tcp).unwrap();
        assert_eq!(read_file(&path).unwrap().targets.len(), 2);
        let reopened = CredentialStore::load(path.clone(), &[]).unwrap();
        for original in [&tcp, &unix] {
            let mut empty = target(&original.addr, original.protocol, None);
            reopened.apply(&mut empty).unwrap();
            assert_eq!(empty.username, original.username);
            assert_eq!(empty.password, original.password);
        }
        assert!(!format!("{reopened:?}").contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn saved_credentials_are_scoped_to_endpoint_and_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::load(dir.path().join(FILE_NAME), &[]).unwrap();
        store
            .remember_authenticated(&target(
                "localhost:6380",
                TargetProtocol::Tcp,
                Some("secret"),
            ))
            .unwrap();
        for (addr, protocol, expected) in [
            ("127.0.0.1:6380", TargetProtocol::Tcp, Some("secret")),
            ("localhost:6381", TargetProtocol::Tcp, None),
            ("remote:6380", TargetProtocol::Tcp, None),
            ("localhost:6380", TargetProtocol::Unix, None),
        ] {
            let mut candidate = target(addr, protocol, None);
            store.apply(&mut candidate).unwrap();
            assert_eq!(candidate.password.as_deref(), expected);
        }
    }

    #[test]
    fn main_config_replaces_entire_saved_pair_including_missing_environment_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut saved = target(
            "localhost:6380",
            TargetProtocol::Tcp,
            Some("saved-password"),
        );
        saved.username = Some("saved-user".to_owned());
        CredentialStore::load(path.clone(), &[])
            .unwrap()
            .remember_authenticated(&saved)
            .unwrap();
        for (username, password) in [
            (None, Some("configured-password")),
            (Some("configured-user"), None),
            (None, None),
        ] {
            let mut configured = target("127.0.0.1:6380", TargetProtocol::Tcp, password);
            configured.username = username.map(str::to_owned);
            let store = CredentialStore::load(path.clone(), &[configured]).unwrap();
            let mut candidate = target("localhost:6380", TargetProtocol::Tcp, None);
            store.apply(&mut candidate).unwrap();
            assert_eq!(candidate.username.as_deref(), username);
            assert_eq!(candidate.password.as_deref(), password);
            candidate.username = None;
            candidate.password = Some("interactive-retry".to_owned());
            store.apply(&mut candidate).unwrap();
            assert_eq!(candidate.username, None);
            assert_eq!(candidate.password.as_deref(), Some("interactive-retry"));
        }
    }

    #[test]
    fn independent_stores_merge_updates_without_losing_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let first = CredentialStore::load(path.clone(), &[]).unwrap();
        let second = CredentialStore::load(path.clone(), &[]).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                first
                    .remember_authenticated(&target(
                        "localhost:6380",
                        TargetProtocol::Tcp,
                        Some("first"),
                    ))
                    .unwrap();
            });
            scope.spawn(|| {
                second
                    .remember_authenticated(&target(
                        "localhost:6381",
                        TargetProtocol::Tcp,
                        Some("second"),
                    ))
                    .unwrap();
            });
        });
        assert_eq!(read_file(&path).unwrap().targets.len(), 2);
    }

    #[test]
    fn malformed_file_is_not_overwritten_or_quoted_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let store = CredentialStore::load(path.clone(), &[]).unwrap();
        let malformed = "password = very-secret-not-valid-toml";
        fs::write(&path, malformed).unwrap();
        let error = store
            .remember_authenticated(&target("localhost:6380", TargetProtocol::Tcp, Some("new")))
            .unwrap_err();
        assert!(!format!("{error:#}").contains("very-secret"));
        assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        assert!(CredentialStore::load(path, &[]).is_err());
    }

    #[test]
    fn unauthenticated_targets_do_not_create_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join(FILE_NAME);
        let store = CredentialStore::load(path.clone(), &[]).unwrap();
        store
            .remember_authenticated(&target("localhost:6380", TargetProtocol::Tcp, None))
            .unwrap();
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn generated_path_prefers_absolute_xdg_and_falls_back_to_home() {
        assert_eq!(
            path_from_dirs(Some("/xdg".into()), Some("/home/user".into())).unwrap(),
            PathBuf::from("/xdg/rtop-auth.toml")
        );
        for xdg in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                path_from_dirs(xdg, Some("/home/user".into())).unwrap(),
                PathBuf::from("/home/user/.config/rtop-auth.toml")
            );
        }
        assert!(path_from_dirs(None, None).is_err());
    }
}
