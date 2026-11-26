use crate::{
    BranchId, Error, ErrorKind, Result, RevisionId, SessionStatus, WorkspaceId, WriteMode,
};

/// Durable mount identity and current branch write ownership.
#[derive(Clone, Debug)]
pub(super) struct MountSession {
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
pub(super) enum SessionChange {
    Acquire,
    Release,
    Handoff(String),
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
