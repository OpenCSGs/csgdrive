use crate::OpenCsgBackend;
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use bytes::Bytes;

#[async_trait]
impl RemoteObjectStore for OpenCsgBackend {
    async fn check_file_storage(&self) -> Result<()> {
        self.check_connection().await
    }

    async fn read_verified_range(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Option<Bytes>> {
        self.git_range(workspace, object, offset, size).await
    }

    async fn upload(&self, workspace: WorkspaceId, source: ObjectStream) -> Result<()> {
        self.upload_file_object(workspace, source).await
    }

    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream> {
        self.open_file_object(workspace, object).await
    }

    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool> {
        Ok(self.file_placement(workspace, object).await?.is_some())
    }
}
