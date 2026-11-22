use crate::{Engine, RemoteStorage, StorageRoutes};
use agentdrive_model::*;
use std::sync::Arc;

impl Engine {
    /// Install new workspace routes or refreshed credentials under the publication barrier.
    pub async fn configure_workspace_storage(
        &self,
        next: StorageRoutes,
        persist: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let _publication = self.publisher.publication.write().await;
        let _objects = self.runtime.objects.write().await;
        next.check_existing(self.runtime.state.as_ref()).await?;
        persist()?;
        self.revisions.trees.content.storage.store(Arc::new(next));
        Ok(())
    }

    /// Replace credentials for the same remote namespace, as verified by the configuration adapter.
    pub async fn refresh_storage_connection(
        &self,
        next: RemoteStorage,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let _publication = self.publisher.publication.write().await;
        let content = &self.revisions.trees.content;
        let mut routes = (*content.storage.load_full()).clone();
        if routes.default.is_none() {
            return Err(Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            ));
        }
        persist()?;
        routes.default = Some(Arc::new(next));
        content.storage.store(Arc::new(routes));
        Ok(())
    }
}
