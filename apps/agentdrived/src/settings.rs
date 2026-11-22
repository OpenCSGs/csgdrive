use agentdrive_config::{ConfigApplier, RemoteConfig, RuntimeConfig};
use agentdrive_engine::{Engine, RemoteStorage, StorageRoutes};
use agentdrive_model::Result;
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
            if let (Some(config), Some(client)) = (config, routes.for_workspace(*id)) {
                clients.push((config, client));
            }
        }
    }
    let mut opened = Vec::new();
    let mut routes = StorageRoutes::default();
    for (id, config) in &next.workspace_remotes {
        let Some(config) = config.as_ref().filter(|remote| remote.enabled()) else {
            continue;
        };
        let client =
            if let Some((_, client)) = clients.iter().find(|(existing, _)| *existing == config) {
                client.clone()
            } else {
                let client = remote_storage(config)?;
                clients.push((config, client.clone()));
                opened.push(client.clone());
                client
            };
        routes.bind(*id, config.storage_target()?, client);
    }
    Ok((routes, opened))
}

#[derive(Debug)]
pub struct StorageSettings {
    pub engine: Arc<Engine>,
    pub runtime: tokio::runtime::Handle,
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
            routes
                .check_existing(self.engine.runtime.state.as_ref())
                .await?;
            for remote in opened {
                remote.refs.check_connection().await?;
            }
            self.engine
                .configure_workspace_storage(routes, persist)
                .await
        })
    }
}
