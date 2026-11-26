//! S3 objects backed by an AgentDrive working tree and durable mount sessions.
mod leases;
mod objects;
mod session;
pub use session::{MountSession, SessionChange, SessionRepository, WriteReservation};

use agentdrive_engine::Engine;
pub use agentdrive_engine::ObjectConditions;
use agentdrive_model::{
    Branch, BranchId, BranchState, ErrorCode, Permission, Principal, RevisionId, Workspace,
    WorkspaceId,
};
pub use objects::{ObjectMeta, ObjectReader};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, sync::Arc};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NoSuchBucket,
    NoSuchKey,
    AccessDenied,
    Conflict,
    Invalid,
    PreconditionFailed,
    Internal,
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<agentdrive_model::Error> for Error {
    fn from(e: agentdrive_model::Error) -> Self {
        let kind = match e.code {
            ErrorCode::PreconditionFailed => ErrorKind::PreconditionFailed,
            ErrorCode::NotFound => ErrorKind::NoSuchKey,
            ErrorCode::PermissionDenied | ErrorCode::StaleAuthority | ErrorCode::ReadOnly => {
                ErrorKind::AccessDenied
            }
            ErrorCode::InvalidArgument | ErrorCode::CapacityExceeded => ErrorKind::Invalid,
            ErrorCode::Io | ErrorCode::Integrity | ErrorCode::Unavailable => ErrorKind::Internal,
            _ => ErrorKind::Conflict,
        };
        Self::new(kind, e.message)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    Exclusive,
    #[default]
    Serialized,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub bucket: String,
    pub workspace: String,
    pub branch: String,
    pub workspace_id: WorkspaceId,
    pub branch_id: BranchId,
    pub epoch: u64,
    pub holder: Option<String>,
    pub retired: bool,
    pub writable: bool,
    pub write_mode: WriteMode,
    pub revision: Option<RevisionId>,
}

#[derive(Debug, Clone)]
pub struct Store {
    engine: Arc<Engine>,
    principal: Principal,
    leases: Arc<dyn SessionRepository>,
}

impl Store {
    pub async fn from_engine(
        engine: Arc<Engine>,
        session_database: &Path,
        principal: Principal,
    ) -> Result<Self> {
        let leases = Arc::new(leases::Database::open(session_database).await?);
        Ok(Self::with_repository(engine, principal, leases))
    }

    pub fn with_repository(
        engine: Arc<Engine>,
        principal: Principal,
        leases: Arc<dyn SessionRepository>,
    ) -> Self {
        Self {
            engine,
            principal,
            leases,
        }
    }

    pub fn engine(&self) -> Arc<Engine> {
        self.engine.clone()
    }

    pub async fn bind_ids(
        &self,
        bucket: &str,
        workspace: WorkspaceId,
        branch: BranchId,
        mode: WriteMode,
    ) -> Result<SessionStatus> {
        self.bind_view(bucket, workspace, branch, mode, None, false)
            .await
    }

    pub async fn bind_view(
        &self,
        bucket: &str,
        workspace: WorkspaceId,
        branch: BranchId,
        mode: WriteMode,
        revision: Option<RevisionId>,
        read_only: bool,
    ) -> Result<SessionStatus> {
        if bucket.is_empty() || bucket.len() > 255 || bucket.chars().any(char::is_control) {
            return Err(Error::new(ErrorKind::Invalid, "invalid bucket name"));
        }
        let (workspace, branch) = self.entities(workspace, branch).await?;
        workspace.authorize(&self.principal, Some(branch.id), Permission::Read)?;
        if branch.state == BranchState::Deleted {
            return Err(Error::new(ErrorKind::NoSuchKey, "branch has been deleted"));
        }
        if let Some(id) = revision {
            self.engine
                .open_revision_file(
                    &self.principal,
                    workspace.id,
                    branch.id,
                    id,
                    &agentdrive_model::WorkspacePath::root(),
                )
                .await?;
        }
        let session = self
            .leases
            .bind(
                bucket,
                workspace.id,
                branch.id,
                mode,
                revision,
                read_only || revision.is_some(),
            )
            .await?;
        Ok(session.with_names(workspace.name, branch.name))
    }

    async fn entities(
        &self,
        workspace_id: WorkspaceId,
        branch_id: BranchId,
    ) -> Result<(Workspace, Branch)> {
        let workspace = self
            .engine
            .runtime
            .state
            .workspace(workspace_id)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::NoSuchKey, "workspace does not exist"))?;
        let branch = self.engine.runtime.branch(branch_id).await?;
        if branch.workspace != workspace_id {
            return Err(Error::new(
                ErrorKind::AccessDenied,
                "branch belongs to another workspace",
            ));
        }
        Ok((workspace, branch))
    }

    pub async fn branch_mounts(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Vec<SessionStatus>> {
        let (workspace, branch) = self.entities(workspace, branch).await?;
        workspace.authorize(&self.principal, Some(branch.id), Permission::Read)?;
        Ok(self
            .leases
            .branch_mounts(workspace.id, branch.id)
            .await?
            .into_iter()
            .map(|session| session.with_names(workspace.name.clone(), branch.name.clone()))
            .collect())
    }

    async fn describe(&self, session: MountSession) -> Result<SessionStatus> {
        let (workspace, branch) = self
            .entities(session.workspace_id, session.branch_id)
            .await?;
        Ok(session.with_names(workspace.name, branch.name))
    }

    pub async fn status(&self, bucket: &str) -> Result<SessionStatus> {
        self.describe(self.leases.status(bucket).await?).await
    }
    pub async fn check_bucket(&self, bucket: &str) -> Result<()> {
        self.leases.status(bucket).await.map(|_| ())
    }
    pub async fn check_writer(&self, bucket: &str) -> Result<()> {
        self.leases.status(bucket).await?.require_writer()
    }
    pub async fn acquire(&self, bucket: &str) -> Result<SessionStatus> {
        self.describe(self.leases.change(bucket, SessionChange::Acquire).await?)
            .await
    }
    pub async fn release(&self, bucket: &str) -> Result<SessionStatus> {
        self.describe(self.leases.change(bucket, SessionChange::Release).await?)
            .await
    }
    pub async fn handoff(&self, from: &str, to: &str) -> Result<SessionStatus> {
        self.describe(
            self.leases
                .change(from, SessionChange::Handoff(to.into()))
                .await?,
        )
        .await
    }
}
