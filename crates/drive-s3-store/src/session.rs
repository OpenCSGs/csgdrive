use crate::{BranchId, Error, ErrorKind, Result, SessionStatus, WorkspaceId, WriteMode};
use async_trait::async_trait;
use std::fmt::Debug;

#[derive(Debug)]
pub enum SessionChange {
    Acquire,
    Release,
    Handoff(String),
}

/// While held, the repository must exclude any ownership change for this branch.
/// Dropping the guard after publication releases that reservation.
pub trait WriteReservation: Send {
    fn session(&self) -> &SessionStatus;
}

/// Persistence boundary for S3 mount aliases and branch ownership.
#[async_trait]
pub trait SessionRepository: Send + Sync + Debug {
    async fn bind(
        &self,
        bucket: &str,
        workspace: &str,
        branch: &str,
        workspace_id: WorkspaceId,
        branch_id: BranchId,
        mode: WriteMode,
    ) -> Result<SessionStatus>;
    async fn branch_mounts(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Vec<SessionStatus>>;
    async fn status(&self, bucket: &str) -> Result<SessionStatus>;
    async fn change(&self, bucket: &str, change: SessionChange) -> Result<SessionStatus>;
    async fn guard(&self, bucket: &str) -> Result<Box<dyn WriteReservation>>;
}

impl SessionStatus {
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
