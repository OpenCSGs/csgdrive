use crate::{
    BranchId, Error, ErrorKind, Result, RevisionId, SessionStatus, WorkspaceId, WriteMode,
};
use async_trait::async_trait;
use std::fmt::Debug;

/// Durable mount identity and current branch write ownership.
#[derive(Clone, Debug)]
pub struct MountSession {
    pub bucket: String,
    pub revision: Option<RevisionId>,
    pub read_only: bool,
    pub workspace_id: WorkspaceId,
    pub branch_id: BranchId,
    pub epoch: u64,
    pub holder: Option<String>,
    pub retired: bool,
    pub writable: bool,
    pub write_mode: WriteMode,
}

#[derive(Debug)]
pub enum SessionChange {
    Acquire,
    Release,
    Handoff(String),
}

/// While held, the repository must exclude any ownership change for this branch.
/// Dropping the guard after publication releases that reservation.
pub trait WriteReservation: Send {
    fn session(&self) -> &MountSession;
}

/// Persistence boundary for S3 mount aliases and branch ownership.
#[async_trait]
pub trait SessionRepository: Send + Sync + Debug {
    async fn bind(
        &self,
        bucket: &str,
        workspace_id: WorkspaceId,
        branch_id: BranchId,
        mode: WriteMode,
        revision: Option<RevisionId>,
        read_only: bool,
    ) -> Result<MountSession>;
    async fn branch_mounts(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Vec<MountSession>>;
    async fn status(&self, bucket: &str) -> Result<MountSession>;
    async fn change(&self, bucket: &str, change: SessionChange) -> Result<MountSession>;
    async fn guard(&self, bucket: &str) -> Result<Box<dyn WriteReservation>>;
}

impl MountSession {
    pub(super) fn with_names(self, workspace: String, branch: String) -> SessionStatus {
        SessionStatus {
            workspace,
            branch,
            bucket: self.bucket,
            revision: self.revision,
            workspace_id: self.workspace_id,
            branch_id: self.branch_id,
            epoch: self.epoch,
            holder: self.holder,
            retired: self.retired,
            writable: self.writable,
            write_mode: self.write_mode,
        }
    }

    pub(crate) fn require_writer(&self) -> Result<()> {
        if !self.writable {
            return Err(Error::new(
                ErrorKind::AccessDenied,
                "mount session does not own the branch write lock",
            ));
        }
        Ok(())
    }
}
