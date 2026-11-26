use crate::S3Config;
use agentdrive_model::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, de};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum RemoteConfig {
    S3(S3Config),
    Scion(ScionConfig),
    #[serde(rename = "opencsg")]
    OpenCsg(OpenCsgConfig),
}

impl<'de> Deserialize<'de> for RemoteConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let backend = value
            .as_object_mut()
            .ok_or_else(|| de::Error::custom("remote must be an object"))?
            .remove("backend")
            .unwrap_or_else(|| "s3".into());
        match backend.as_str() {
            Some("s3") => serde_json::from_value(value)
                .map(Self::S3)
                .map_err(de::Error::custom),
            Some("scion") => serde_json::from_value(value)
                .map(Self::Scion)
                .map_err(de::Error::custom),
            Some("opencsg") => serde_json::from_value(value)
                .map(Self::OpenCsg)
                .map_err(de::Error::custom),
            _ => Err(de::Error::custom(
                "remote backend must be s3, scion or opencsg",
            )),
        }
    }
}

impl RemoteConfig {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Scion(config) => config.enabled,
            Self::S3(config) => config.enabled,
            Self::OpenCsg(config) => config.enabled,
        }
    }

    /// Stable identity shared by machines using the same workspace storage.
    pub fn storage_target(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let fields = match self {
            Self::S3(s3) => serde_json::to_vec(&(
                "s3",
                s3.effective_endpoint()
                    .as_deref()
                    .map(|url| url.trim_end_matches('/')),
                &s3.bucket,
                &s3.region,
                s3.prefix.trim_matches('/'),
            ))?,
            Self::Scion(config) => serde_json::to_vec(&(
                "scion",
                config.endpoint.trim_end_matches('/'),
                &config.repository,
            ))?,
            Self::OpenCsg(codes) => serde_json::to_vec(&(
                "opencsg",
                codes.endpoint.trim_end_matches('/'),
                &codes.repository,
                &codes.branch,
                &codes.prefix,
            ))?,
        };
        Ok(format!("{:x}", Sha256::digest(fields)))
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Scion(config) => config.validate(),
            Self::S3(config) => config.validate(),
            Self::OpenCsg(config) => config.validate(),
        }
    }

    pub fn redact(&mut self) {
        match self {
            Self::Scion(config) => config.token.clear(),
            Self::S3(config) => config.redact(),
            Self::OpenCsg(config) => config.token.clear(),
        }
    }

    pub(crate) fn restore_credentials(&mut self, previous: Option<&Self>) -> Result<()> {
        match self {
            Self::S3(config) => {
                if let Some(credentials) = &mut config.credentials
                    && credentials.secret_access_key.is_empty()
                {
                    let saved = match previous {
                        Some(Self::S3(saved)) if saved.endpoint == config.endpoint => {
                            saved.credentials.as_ref()
                        }
                        _ => None,
                    }
                    .filter(|saved| saved.access_key_id == credentials.access_key_id)
                    .ok_or_else(|| {
                        Error::invalid("provide the secret key for the selected access key")
                    })?;
                    *credentials = saved.clone();
                }
            }
            Self::OpenCsg(config) if config.token.is_empty() => {
                let saved = match previous {
                    Some(Self::OpenCsg(saved)) => Some(saved),
                    _ => None,
                }
                .filter(|saved| {
                    saved.endpoint == config.endpoint && saved.repository == config.repository
                })
                .ok_or_else(|| {
                    Error::invalid("provide an OpenCSG token for this domain and repository")
                })?;
                config.token = saved.token.clone();
            }
            Self::Scion(config) if config.token.is_empty() => {
                if let Some(Self::Scion(saved)) = previous
                    && config.endpoint == saved.endpoint
                    && config.repository == saved.repository
                {
                    config.token = saved.token.clone();
                }
            }
            Self::OpenCsg(_) | Self::Scion(_) => {}
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCsgConfig {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub endpoint: String,
    pub repository: String,
    #[serde(default = "branch")]
    pub branch: String,
    #[serde(default = "prefix")]
    pub prefix: String,
    pub token: String,
    #[serde(default)]
    pub allow_http: bool,
    #[serde(default)]
    pub xnet: bool,
}

impl std::fmt::Debug for OpenCsgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCsgConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("repository", &self.repository)
            .field("branch", &self.branch)
            .field("prefix", &self.prefix)
            .field("xnet", &self.xnet)
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl OpenCsgConfig {
    pub fn validate(&self) -> Result<()> {
        validate_origin(&self.endpoint, self.allow_http)?;
        let parts: Vec<_> = self.repository.split('/').collect();
        if parts.len() != 2 || parts.iter().any(|part| !valid_component(part)) {
            return Err(Error::invalid("OpenCSG repository must use namespace/name"));
        }
        if !valid_component(&self.branch) {
            return Err(Error::invalid(
                "OpenCSG storage branch requires a simple branch name",
            ));
        }
        if self.prefix.is_empty() || self.prefix.split('/').any(|part| !valid_component(part)) {
            return Err(Error::invalid("invalid OpenCSG storage prefix"));
        }
        validate_token(&self.token)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScionConfig {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub endpoint: String,
    pub repository: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub allow_http: bool,
}

impl std::fmt::Debug for ScionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScionConfig")
            .field("endpoint", &self.endpoint)
            .field("repository", &self.repository)
            .finish_non_exhaustive()
    }
}

impl ScionConfig {
    pub fn validate(&self) -> Result<()> {
        validate_origin(&self.endpoint, self.allow_http)?;
        if self
            .repository
            .split('/')
            .any(|part| !valid_component(part))
        {
            return Err(Error::invalid("invalid Scion repository path"));
        }
        if !self.token.is_empty() {
            validate_token(&self.token)?;
        }
        Ok(())
    }
}

fn validate_origin(endpoint: &str, allow_http: bool) -> Result<()> {
    let url =
        url::Url::parse(endpoint).map_err(|_| Error::invalid("invalid repository endpoint"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "http" && !allow_http)
    {
        return Err(Error::invalid(
            "repository endpoint requires an HTTP(S) origin without credentials, a path, query or fragment; HTTP requires allow_http",
        ));
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() || token.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(Error::invalid(
            "repository token must contain printable ASCII without whitespace",
        ));
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && !value.ends_with(".lock")
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}
fn enabled() -> bool {
    true
}
fn branch() -> String {
    "main".into()
}
fn prefix() -> String {
    "agentdrive/v1".into()
}
