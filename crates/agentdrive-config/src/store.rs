use crate::{CacheConfig, CollaborationConfig, Config, RemoteConfig, StartupConfig};
use agentdrive_model::{Error, ErrorCode, Result, WorkspaceId};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub workspace_remotes: BTreeMap<WorkspaceId, Option<RemoteConfig>>,
    pub cache: CacheConfig,
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub collaboration: CollaborationConfig,
}

impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        self.cache.validate()?;
        for remote in self.workspace_remotes.values().flatten() {
            remote.validate()?;
        }
        for origin in &self.allowed_origins {
            let url = url::Url::parse(origin)
                .map_err(|_| Error::invalid("allowed_origins must contain HTTP(S) origins"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.origin().ascii_serialization() != *origin
            {
                return Err(Error::invalid(
                    "allowed_origins must use scheme://host[:port] without a path, credentials, wildcards or a default port",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub revision: String,
    pub values: RuntimeConfig,
    pub reload_error: Option<String>,
}

/// 每次操作取得一份快照，并在操作结束前保持该版本。
#[derive(Debug)]
pub struct LiveConfig(ArcSwap<Snapshot>);

impl LiveConfig {
    pub fn new(values: RuntimeConfig) -> Result<Arc<Self>> {
        values.validate()?;
        Ok(Arc::new(Self(ArcSwap::from_pointee(Snapshot {
            revision: revision(&serde_json::to_vec(&values)?),
            values,
            reload_error: None,
        }))))
    }

    pub fn load(&self) -> Arc<Snapshot> {
        self.0.load_full()
    }
}

#[derive(Debug, Serialize)]
pub struct Settings {
    pub config_path: PathBuf,
    pub revision: String,
    pub runtime: RuntimeConfig,
    pub startup: StartupConfig,
    pub reload_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub revision: String,
    pub runtime: RuntimeConfig,
}

/// 配置文件替换与资源切换共用一次操作，准备失败时保留当前配置。
pub trait ConfigApplier: Send + Sync + std::fmt::Debug {
    fn apply(
        &self,
        previous: &RuntimeConfig,
        next: &RuntimeConfig,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()>;
}

/// Persistent runtime settings on a server. The configuration file supplies startup settings.
pub trait RuntimeSettingsRepository: Send + Sync + std::fmt::Debug {
    fn load(&self) -> Result<Option<RuntimeConfig>>;
    fn save(&self, values: &RuntimeConfig) -> Result<()>;
}

/// 写入与文件加载使用同一个锁；业务操作通过 ArcSwap 读取配置。
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    startup: StartupConfig,
    live: Arc<LiveConfig>,
    writer: Mutex<()>,
    applier: OnceLock<Arc<dyn ConfigApplier>>,
    repository: OnceLock<Arc<dyn RuntimeSettingsRepository>>,
}

impl ConfigStore {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let path = path.canonicalize()?;
        let bytes = read(&path)?;
        let config = Config::decode(&bytes)?;
        let live = Arc::new(LiveConfig(ArcSwap::from_pointee(Snapshot {
            revision: revision(&bytes),
            values: config.runtime(),
            reload_error: None,
        })));
        Ok(Arc::new(Self {
            path,
            startup: config.startup,
            live,
            writer: Mutex::new(()),
            applier: OnceLock::new(),
            repository: OnceLock::new(),
        }))
    }

    pub fn use_repository(&self, repository: Arc<dyn RuntimeSettingsRepository>) -> Result<()> {
        let _writer = self.writer.lock();
        let values = match repository.load()? {
            Some(values) => values,
            None => {
                let initial = self.live.load().values.clone();
                if !initial.workspace_remotes.is_empty() {
                    return Err(Error::invalid(
                        "configure server workspace storage in the dashboard",
                    ));
                }
                repository.save(&initial)?;
                initial
            }
        };
        values.validate()?;
        let revision = revision(&serde_json::to_vec(&values)?);
        self.repository
            .set(repository)
            .map_err(|_| Error::invalid("settings repository already assigned"))?;
        self.live.0.store(Arc::new(Snapshot {
            values,
            revision,
            reload_error: None,
        }));
        Ok(())
    }

    pub fn set_applier(&self, applier: Arc<dyn ConfigApplier>) -> Result<()> {
        self.applier.set(applier).map_err(|_| {
            Error::new(
                ErrorCode::Conflict,
                "configuration applier is already installed",
            )
        })
    }

    fn apply(
        &self,
        previous: &RuntimeConfig,
        next: &RuntimeConfig,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        match self.applier.get() {
            Some(applier) => applier.apply(previous, next, persist),
            None => persist(),
        }
    }

    pub fn startup(&self) -> &StartupConfig {
        &self.startup
    }

    pub fn live(&self) -> Arc<LiveConfig> {
        self.live.clone()
    }

    pub fn settings(&self) -> Settings {
        let snapshot = self.live.load();
        let mut runtime = snapshot.values.clone();
        for remote in runtime.workspace_remotes.values_mut().flatten() {
            remote.redact();
        }
        Settings {
            config_path: self.path.clone(),
            revision: snapshot.revision.clone(),
            runtime,
            startup: self.startup.clone(),
            reload_error: snapshot.reload_error.clone(),
        }
    }

    pub fn reload(&self) -> Result<bool> {
        // The open server has exclusive ownership of the database. All settings changes use update().
        if self.repository.get().is_some() {
            return Ok(false);
        }
        let _writer = self.writer.lock();
        let result = (|| {
            let bytes = read(&self.path)?;
            let revision = revision(&bytes);
            let current = self.live.load();
            if revision == current.revision && current.reload_error.is_none() {
                return Ok(false);
            }
            let config = Config::decode(&bytes)?;
            if config.startup != self.startup {
                let old = serde_json::to_value(&self.startup)?;
                let new = serde_json::to_value(&config.startup)?;
                let changed: Vec<_> = old
                    .as_object()
                    .expect("startup object")
                    .keys()
                    .chain(new.as_object().expect("startup object").keys())
                    .filter(|key| old[*key] != new[*key])
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                return Err(Error::new(
                    ErrorCode::Conflict,
                    format!(
                        "restart required for {}; active configuration was retained",
                        changed.join(", ")
                    ),
                ));
            }
            self.apply(&current.values, &config.runtime(), &mut || {
                if self::revision(&read(&self.path)?) != revision {
                    return Err(Error::new(
                        ErrorCode::Conflict,
                        "configuration changed during reload",
                    ));
                }
                Ok(())
            })?;
            self.live.0.store(Arc::new(Snapshot {
                revision,
                values: config.runtime(),
                reload_error: None,
            }));
            Ok(true)
        })();
        if let Err(error) = &result {
            let current = self.live.load();
            if current.reload_error.as_deref() != Some(&error.to_string()) {
                let mut snapshot = (*current).clone();
                snapshot.reload_error = Some(error.to_string());
                self.live.0.store(Arc::new(snapshot));
            }
        }
        result
    }

    /// Bind a server-assigned workspace ID before its first publication job becomes visible.
    pub fn bind_new_workspace(&self, id: WorkspaceId, remote: Option<RemoteConfig>) -> Result<()> {
        let _writer = self.writer.lock();
        let mut settings = self.settings();
        if let Some(stored) = self.live.load().values.workspace_remotes.get(&id) {
            return if stored == &remote {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorCode::Conflict,
                    "workspace already has different storage settings",
                ))
            };
        }
        settings.runtime.workspace_remotes.insert(id, remote);
        self.update_locked(SettingsUpdate {
            revision: settings.revision,
            runtime: settings.runtime,
        })?;
        Ok(())
    }

    pub fn discard_new_binding(
        &self,
        id: WorkspaceId,
        expected: &Option<RemoteConfig>,
    ) -> Result<()> {
        let _writer = self.writer.lock();
        if self.live.load().values.workspace_remotes.get(&id) == Some(expected) {
            let mut settings = self.settings();
            settings.runtime.workspace_remotes.remove(&id);
            self.update_locked(SettingsUpdate {
                revision: settings.revision,
                runtime: settings.runtime,
            })?;
        }
        Ok(())
    }

    pub fn update(&self, update: SettingsUpdate) -> Result<Settings> {
        let _writer = self.writer.lock();
        self.update_locked(update)
    }

    fn update_locked(&self, mut update: SettingsUpdate) -> Result<Settings> {
        let current = self.live.load();
        if current.revision != update.revision
            || (self.repository.get().is_none() && revision(&read(&self.path)?) != update.revision)
        {
            return Err(Error::new(
                ErrorCode::Conflict,
                "configuration changed; reload settings before saving",
            ));
        }
        for (workspace, remote) in &mut update.runtime.workspace_remotes {
            if let Some(remote) = remote {
                let saved = current
                    .values
                    .workspace_remotes
                    .get(workspace)
                    .and_then(Option::as_ref);
                remote.restore_credentials(saved)?;
            }
        }
        update.runtime.validate()?;
        if let Some(repository) = self.repository.get() {
            let bytes = serde_json::to_vec(&update.runtime)?;
            if bytes.len() > MAX_CONFIG_BYTES {
                return Err(Error::invalid("configuration exceeds 1 MiB"));
            }
            self.apply(&current.values, &update.runtime, &mut || {
                repository.save(&update.runtime)
            })?;
            self.live.0.store(Arc::new(Snapshot {
                revision: revision(&bytes),
                values: update.runtime,
                reload_error: None,
            }));
            return Ok(self.settings());
        }
        let config = Config::with_runtime(self.startup.clone(), update.runtime);
        let bytes = serde_json::to_vec_pretty(&config)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(Error::invalid("configuration exceeds 1 MiB"));
        }
        let parent = self.path.parent().expect("absolute configuration path");
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        temporary
            .as_file()
            .set_permissions(fs::metadata(&self.path)?.permissions())?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        let mut temporary = Some(temporary);
        self.apply(&current.values, &config.runtime(), &mut || {
            if revision(&read(&self.path)?) != update.revision {
                return Err(Error::new(
                    ErrorCode::Conflict,
                    "configuration changed; reload settings before saving",
                ));
            }
            temporary
                .take()
                .ok_or_else(|| Error::integrity("configuration was already persisted"))?
                .persist(&self.path)
                .map_err(|error| Error::from(error.error))?;
            Ok(())
        })?;
        let reload_error = sync_directory(parent)
            .err()
            .map(|error| format!("configuration applied; directory sync failed: {error}"));
        let snapshot = Snapshot {
            revision: revision(&bytes),
            values: config.runtime(),
            reload_error,
        };
        self.live.0.store(Arc::new(snapshot));
        Ok(self.settings())
    }
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(Error::invalid("configuration exceeds 1 MiB"));
    }
    Ok(bytes)
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
