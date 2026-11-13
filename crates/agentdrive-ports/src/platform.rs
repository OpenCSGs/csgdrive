use crate::FileSystem;
use agentdrive_model::*;
use async_trait::async_trait;
use std::{path::PathBuf, sync::Arc};

pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
    fn monotonic_ms(&self) -> u64;
}

#[derive(Clone, Debug)]
pub struct MountSpec {
    pub binding: MountBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeMountState {
    Mounted,
    Absent,
    Unknown,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HandleTrackingScope {
    Native,
    #[default]
    Protocol,
}

#[async_trait]
pub trait MountDriver: Send + Sync {
    /// Whether the compiled backend supports collaboration and native binding updates.
    fn supports_automatic_collaboration(&self) -> bool {
        false
    }
    fn handle_tracking_scope(&self) -> HandleTrackingScope {
        HandleTrackingScope::Protocol
    }
    async fn normalize_path(&self, path: &std::path::Path) -> Result<PathBuf>;
    async fn mount(&self, spec: MountSpec, file_system: Arc<dyn FileSystem>) -> Result<()>;
    /// Counts driver-visible file, directory and metadata handles, including incomplete opens.
    async fn open_handles(&self, _binding: &MountBinding) -> Result<usize> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "native handle accounting is not supported",
        ))
    }
    /// The caller prevents new engine requests until the updated binding is persisted.
    async fn rebind(&self, _previous: &MountBinding, _next: &MountBinding) -> Result<()> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "updating a native mount binding is not supported",
        ))
    }
    async fn unmount(&self, binding: &MountBinding) -> Result<()>;
    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState>;
    fn capability(&self) -> &'static str;
}

#[async_trait]
pub trait DirectoryAccess: Send + Sync {
    /// Preserve the original directory and leave an empty path for mounting. Retrying is safe.
    async fn stage_mount(
        &self,
        _source: &std::path::Path,
        _backup: &std::path::Path,
    ) -> Result<()> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "directory conversion is not supported",
        ))
    }
    async fn import_tree(
        &self,
        workspace: &Workspace,
        source: &std::path::Path,
        objects: Arc<dyn crate::LocalObjectStore>,
    ) -> Result<FileTree>;
    async fn export_tree(
        &self,
        workspace: WorkspaceId,
        tree: FileTree,
        destination: &std::path::Path,
        content: Arc<dyn crate::ContentResolver>,
    ) -> Result<()>;
}
