use agentdrive_config::{ConfigApplier, RemoteConfig, RuntimeConfig};
use agentdrive_engine::{Engine, RemoteStorage, StorageRoutes};
use agentdrive_model::{Result, WorkspaceId};
use agentdrive_opencsg::OpenCsgBackend;
use agentdrive_s3::S3Backend;
use std::sync::Arc;

fn remote_storage(config: &RemoteConfig) -> Result<Arc<RemoteStorage>> {
    Ok(Arc::new(match config {
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

/// Reuse existing clients so an edit only opens connections for newly selected settings.
pub fn storage_routes<'a>(
    next: &'a RuntimeConfig,
    previous: Option<(&'a RuntimeConfig, &StorageRoutes)>,
) -> Result<(StorageRoutes, Vec<Arc<RemoteStorage>>)> {
    let mut clients = Vec::new();
    if let Some((previous, routes)) = previous {
        for (id, config) in &previous.workspace_remotes {
            if let (Some(config), Some(Some(client))) = (config, routes.workspaces.get(id)) {
                clients.push((config, client.clone()));
            }
        }
    }
    let mut opened = Vec::new();
    let mut resolve = |config: Option<&'a RemoteConfig>| -> Result<Option<Arc<RemoteStorage>>> {
        let Some(config) = config.filter(|c| c.enabled()) else {
            return Ok(None);
        };
        if let Some((_, client)) = clients.iter().find(|(existing, _)| *existing == config) {
            return Ok(Some(client.clone()));
        }
        let client = remote_storage(config)?;
        clients.push((config, client.clone()));
        opened.push(client.clone());
        Ok(Some(client))
    };
    let overrides = next
        .workspace_remotes
        .iter()
        .map(|(id, config)| Ok((*id, resolve(config.as_ref())?)))
        .collect::<Result<_>>()?;
    Ok((StorageRoutes::new(None, overrides), opened))
}

#[derive(Debug)]
pub struct StorageSettings {
    pub engine: Arc<Engine>,
    pub runtime: tokio::runtime::Handle,
}

fn same_namespace(previous: &RemoteConfig, next: &RemoteConfig) -> bool {
    match (previous, next) {
        (RemoteConfig::S3(previous), RemoteConfig::S3(next)) => {
            let mut same = previous.clone();
            same.credentials = next.credentials.clone();
            same == *next
        }
        (RemoteConfig::OpenCsg(previous), RemoteConfig::OpenCsg(next)) => {
            let mut same = previous.clone();
            same.token = next.token.clone();
            same.xnet = next.xnet;
            same == *next
        }
        _ => false,
    }
}

fn selected(config: &RuntimeConfig, workspace: WorkspaceId) -> Option<&RemoteConfig> {
    config
        .workspace_remotes
        .get(&workspace)
        .and_then(Option::as_ref)
        .filter(|r| r.enabled())
}

impl ConfigApplier for StorageSettings {
    fn apply(
        &self,
        previous: &RuntimeConfig,
        next: &RuntimeConfig,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if previous.workspace_remotes == next.workspace_remotes {
            return persist();
        }
        self.runtime.block_on(async {
            let current = self.engine.storage_routes();
            let (routes, opened) = storage_routes(next, Some((previous, &current)))?;
            for remote in opened {
                remote.refs.check_connection().await?;
            }
            let moved = |workspace| match (selected(previous, workspace), selected(next, workspace))
            {
                (Some(a), Some(b)) => a != b && !same_namespace(a, b),
                (None, None) => false,
                _ => true,
            };
            self.engine
                .switch_storage_routes(routes, moved, persist)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdrive_config::{OpenCsgConfig, S3Config, S3Credentials};

    #[test]
    fn credentials_and_codes_transfer_mode_preserve_the_remote_namespace() {
        let previous = OpenCsgConfig {
            enabled: true,
            endpoint: "https://example.test".into(),
            repository: "team/project".into(),
            branch: "main".into(),
            prefix: "drive".into(),
            token: "old-test-token".into(),
            allow_http: false,
            xnet: false,
        };
        let mut next = previous.clone();
        next.token = "new-test-token".into();
        let previous = RemoteConfig::OpenCsg(previous);
        assert!(same_namespace(
            &previous,
            &RemoteConfig::OpenCsg(next.clone())
        ));
        next.xnet = true;
        assert!(same_namespace(
            &previous,
            &RemoteConfig::OpenCsg(next.clone())
        ));
        next.branch = "other".into();
        assert!(!same_namespace(&previous, &RemoteConfig::OpenCsg(next)));

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
        assert!(same_namespace(&previous, &RemoteConfig::S3(next.clone())));
        next.prefix = "other".into();
        assert!(!same_namespace(&previous, &RemoteConfig::S3(next)));
    }
}
