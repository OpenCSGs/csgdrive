use crate::{CacheConfig, CollaborationConfig, RemoteConfig, RuntimeConfig};
use agentdrive_model::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,
    #[serde(flatten)]
    pub startup: StartupConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default_cache")]
    pub cache: CacheConfig,
    #[serde(default)]
    pub collaboration: CollaborationConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupConfig {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub mount_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directory_roots: Vec<PathBuf>,
    #[serde(
        default = "local_principal",
        skip_serializing_if = "is_default_principal"
    )]
    pub principal: Principal,
}
fn is_default_cache(cache: &CacheConfig) -> bool {
    cache == &CacheConfig::default()
}
impl Config {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| Error::invalid(error.to_string()))?;
        // 保留已有工作区使用的身份，旧令牌文件不再参与启动。
        if let Some(object) = value.as_object_mut() {
            if let Some(credentials) = object.remove("credentials")
                && !object.contains_key("principal")
                && let Some(principal) = credentials.get(0).and_then(|entry| entry.get("principal"))
            {
                object.insert("principal".into(), principal.clone());
            }
            object.remove("administrators");
        }
        // 已保存的配置继续使用容量设置，清理阈值由程序管理。
        if let Some(cache) = value
            .get_mut("cache")
            .and_then(|cache| cache.as_object_mut())
        {
            cache.remove("target_bytes");
            cache.remove("min_free_bytes");
        }
        let config: Self =
            serde_json::from_value(value).map_err(|error| Error::invalid(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }
    pub fn runtime(&self) -> RuntimeConfig {
        RuntimeConfig {
            remote: self.remote.clone(),
            cache: self.cache.clone(),
            allowed_origins: self.allowed_origins.clone(),
            collaboration: self.collaboration.clone(),
        }
    }
    pub(crate) fn with_runtime(startup: StartupConfig, runtime: RuntimeConfig) -> Self {
        Self {
            remote: runtime.remote,
            startup,
            cache: runtime.cache,
            allowed_origins: runtime.allowed_origins,
            collaboration: runtime.collaboration,
        }
    }
    fn validate(&self) -> Result<()> {
        if !self.startup.listen.ip().is_loopback() {
            return Err(Error::invalid(
                "MCP must listen on a loopback address; use an authenticated TLS proxy for remote access",
            ));
        }
        for path in std::iter::once(&self.startup.data_dir)
            .chain(self.startup.mount_roots.iter())
            .chain(self.startup.directory_roots.iter())
        {
            if !path.is_absolute() {
                return Err(Error::invalid(
                    "configured filesystem paths must be absolute",
                ));
            }
        }
        self.runtime().validate()
    }
}
fn create_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn initialize(directory: &Path, listen: SocketAddr) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let directory = directory.canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let config_path = directory.join("agentdrive.json");
    if config_path.exists() {
        return Err(Error::new(
            ErrorCode::AlreadyExists,
            "configuration already exists",
        ));
    }
    let mounts = directory.join("mounts");
    fs::create_dir_all(&mounts)?;
    let exchange = directory.join("exchange");
    fs::create_dir_all(&exchange)?;
    let config = Config {
        remote: None,
        startup: StartupConfig {
            data_dir: directory.join("state"),
            listen,
            mount_roots: vec![mounts],
            directory_roots: vec![exchange],
            principal: local_principal(),
        },
        allowed_origins: vec![],
        cache: CacheConfig::default(),
        collaboration: CollaborationConfig::default(),
    };
    create_file(&config_path, &serde_json::to_vec_pretty(&config)?)?;
    Ok(config_path)
}

fn local_principal() -> Principal {
    "owner"
        .to_owned()
        .try_into()
        .expect("valid local principal")
}
fn is_default_principal(value: &Principal) -> bool {
    value == &local_principal()
}
