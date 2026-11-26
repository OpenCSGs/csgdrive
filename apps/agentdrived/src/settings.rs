use agentdrive_config::{ConfigApplier, RemoteConfig, RuntimeConfig};
use agentdrive_config::{ConfigStore, RuntimeSettingsRepository};
use agentdrive_engine::{Engine, RemoteStorage, StorageRoutes};
use agentdrive_local::{DatabaseStorage, LocalBackend, ServerSettings};
use agentdrive_model::Result;

use agentdrive_opencsg::OpenCsgBackend;
use agentdrive_s3::S3Backend;
use std::sync::Arc;

#[derive(Debug)]
struct DatabaseSettings(LocalBackend);

impl RuntimeSettingsRepository for DatabaseSettings {
    fn save(&self, config: &RuntimeConfig) -> Result<()> {
        let mut settings = config.clone();
        let workspaces = std::mem::take(&mut settings.workspace_remotes)
            .into_iter()
            .map(|(id, remote)| Ok((id, serde_json::to_vec(&remote)?)))
            .collect::<Result<_>>()?;
        self.0.save_server_settings(ServerSettings {
            runtime: serde_json::to_vec(&settings)?,
            workspaces,
        })
    }

    fn load(&self) -> Result<Option<RuntimeConfig>> {
        self.0
            .server_settings()?
            .map(|saved| {
                let mut runtime: RuntimeConfig = serde_json::from_slice(&saved.runtime)?;
                runtime.workspace_remotes = saved
                    .workspaces
                    .into_iter()
                    .map(|(id, bytes)| Ok((id, serde_json::from_slice(&bytes)?)))
                    .collect::<Result<_>>()?;
                Ok(runtime)
            })
            .transpose()
    }
}

pub fn use_database_settings(settings: &ConfigStore, db: &LocalBackend) -> Result<()> {
    settings.use_repository(Arc::new(DatabaseSettings(db.clone())))
}

fn remote_storage(config: &RemoteConfig, db: &LocalBackend) -> Result<Arc<RemoteStorage>> {
    let files: Arc<dyn agentdrive_ports::RemoteObjectStore> = match config {
        RemoteConfig::S3(config) => S3Backend::file_store(config.clone(), Arc::new(db.clone()))?,
        RemoteConfig::OpenCsg(config) => {
            OpenCsgBackend::file_store(config.clone(), Arc::new(db.clone()))?
        }
    };
    let adapter = DatabaseStorage::new(db.clone(), config.storage_target()?, files)?;
    Ok(Arc::new(RemoteStorage {
        objects: adapter.clone(),
        refs: adapter,
    }))
}

/// Reuse existing clients so an edit only opens connections for newly selected settings.
pub fn storage_routes<'a>(
    next: &'a RuntimeConfig,
    previous: Option<(&'a RuntimeConfig, &StorageRoutes)>,
    db: &LocalBackend,
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
                let client = remote_storage(config, db)?;
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
    pub metadata: LocalBackend,
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
            let (routes, opened) =
                storage_routes(next, Some((previous, &current)), &self.metadata)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentdrive_config::{S3Config, initialize};
    use agentdrive_model::WorkspaceId;

    #[test]
    fn server_reopens_workspace_routes_from_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = initialize(dir.path(), "127.0.0.1:7421".parse().unwrap()).unwrap();
        let workspace = WorkspaceId::new();
        let remote = RemoteConfig::S3(S3Config {
            enabled: true,
            bucket: "test".into(),
            prefix: "files".into(),
            region: "us-east-1".into(),
            endpoint: None,
            allow_http: false,
            credentials: None,
        });
        let original = std::fs::read(&config_path).unwrap();
        {
            let settings = ConfigStore::open(&config_path).unwrap();
            let db = LocalBackend::open_server(&settings.startup().data_dir).unwrap();
            use_database_settings(&settings, &db).unwrap();
            settings
                .bind_new_workspace(workspace, Some(remote.clone()))
                .unwrap();
            let saved = db.server_settings().unwrap().unwrap();
            assert_eq!(saved.workspaces.len(), 1);
            assert!(saved.workspaces.contains_key(&workspace));
        }
        let settings = ConfigStore::open(&config_path).unwrap();
        let db = LocalBackend::open_server(&settings.startup().data_dir).unwrap();
        use_database_settings(&settings, &db).unwrap();
        assert_eq!(
            settings.live().load().values.workspace_remotes[&workspace],
            Some(remote)
        );
        assert_eq!(std::fs::read(config_path).unwrap(), original);
    }
}
