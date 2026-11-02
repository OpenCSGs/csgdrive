use agentdrive_config::{ConfigApplier, RemoteConfig, RuntimeConfig};
use agentdrive_engine::{Engine, RemoteStorage};
use agentdrive_model::Result;
use agentdrive_opencsg::OpenCsgBackend;
use agentdrive_s3::S3Backend;
use std::sync::Arc;

pub fn remote_storage(config: Option<&RemoteConfig>) -> Result<Option<RemoteStorage>> {
    let Some(config) = config.filter(|config| config.enabled()) else {
        return Ok(None);
    };
    Ok(Some(match config {
        RemoteConfig::S3(config) => {
            let backend = S3Backend::new(config.clone())?;
            RemoteStorage {
                objects: backend.clone(),
                refs: backend,
            }
        }
        RemoteConfig::OpenCsg(config) => {
            let backend = OpenCsgBackend::new(config.clone())?;
            RemoteStorage {
                objects: backend.clone(),
                refs: backend,
            }
        }
    }))
}

#[derive(Debug)]
pub struct StorageSettings {
    pub engine: Arc<Engine>,
    pub runtime: tokio::runtime::Handle,
}

fn credentials_only(previous: &RemoteConfig, next: &RemoteConfig) -> bool {
    match (previous, next) {
        (RemoteConfig::S3(previous), RemoteConfig::S3(next)) => {
            let mut same = previous.clone();
            same.credentials = next.credentials.clone();
            same == *next
        }
        (RemoteConfig::OpenCsg(previous), RemoteConfig::OpenCsg(next)) => {
            let mut same = previous.clone();
            same.token = next.token.clone();
            same == *next
        }
        _ => false,
    }
}

impl ConfigApplier for StorageSettings {
    fn apply(
        &self,
        previous: &RuntimeConfig,
        next: &RuntimeConfig,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let active = previous.remote.as_ref().filter(|remote| remote.enabled());
        let requested = next.remote.as_ref().filter(|remote| remote.enabled());
        if active == requested {
            return persist();
        }
        self.runtime.block_on(async {
            let remote = remote_storage(requested)?;
            if let Some(remote) = &remote {
                remote.refs.check_connection().await?;
            }
            if let (Some(previous), Some(next), Some(remote)) = (active, requested, &remote)
                && credentials_only(previous, next)
            {
                return self
                    .engine
                    .refresh_storage_connection(remote.clone(), persist)
                    .await;
            }
            self.engine.switch_storage(remote, persist).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdrive_config::{OpenCsgConfig, S3Config, S3Credentials};

    #[test]
    fn rotating_credentials_requires_the_same_remote_namespace() {
        let previous = OpenCsgConfig {
            enabled: true,
            endpoint: "https://example.test".into(),
            repository: "team/project".into(),
            branch: "main".into(),
            prefix: "drive".into(),
            token: "old-test-token".into(),
            allow_http: false,
        };
        let mut next = previous.clone();
        next.token = "new-test-token".into();
        let previous = RemoteConfig::OpenCsg(previous);
        assert!(credentials_only(
            &previous,
            &RemoteConfig::OpenCsg(next.clone())
        ));
        next.branch = "other".into();
        assert!(!credentials_only(&previous, &RemoteConfig::OpenCsg(next)));

        let previous = S3Config {
            enabled: true,
            bucket: "files".into(),
            prefix: "drive".into(),
            region: "us-east-1".into(),
            endpoint: Some("https://example.test".into()),
            allow_http: false,
            credentials: None,
        };
        let mut next = previous.clone();
        next.credentials = Some(S3Credentials {
            access_key_id: "test-access".into(),
            secret_access_key: "test-secret".into(),
            session_token: None,
        });
        let previous = RemoteConfig::S3(previous);
        assert!(credentials_only(&previous, &RemoteConfig::S3(next.clone())));
        next.prefix = "other".into();
        assert!(!credentials_only(&previous, &RemoteConfig::S3(next)));
    }
}
