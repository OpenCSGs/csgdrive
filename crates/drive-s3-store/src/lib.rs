//! S3 objects backed by an AgentDrive working tree and durable mount sessions.
mod leases;
mod objects;
mod session;
pub use session::{SessionChange, SessionRepository, WriteReservation};

use agentdrive_config::{LiveConfig, RuntimeConfig};
pub use agentdrive_engine::ObjectConditions;
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_model::{BranchId, BranchState, ErrorCode, Principal, WorkspaceId};
use agentdrive_platform::{HostDirectories, NativeMountDriver, SystemClock, local_identity};
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
    #[default]
    Exclusive,
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
}

#[derive(Debug, Clone)]
pub struct Store {
    engine: Arc<Engine>,
    principal: Principal,
    leases: Arc<dyn SessionRepository>,
}

impl Store {
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let local = Arc::new(LocalBackend::open_server(data_dir)?);
        let engine = Engine::open(
            Adapters {
                state: local.clone(),
                objects: local.clone(),
                working: local,
                remote_objects: None,
                remote_refs: None,
                clock: Arc::new(SystemClock::default()),
                mounts: NativeMountDriver::new(vec![])?,
                directories: HostDirectories::new(vec![])?,
            },
            EngineConfig {
                live: LiveConfig::new(RuntimeConfig::default())?,
                identity: local_identity(),
            },
        )
        .await?;
        Self::from_engine(
            engine,
            &data_dir.join("s3-sessions.sqlite3"),
            "owner".to_owned().try_into()?,
        )
        .await
    }

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
        if bucket.is_empty() || bucket.len() > 255 || bucket.chars().any(char::is_control) {
            return Err(Error::new(ErrorKind::Invalid, "invalid bucket name"));
        }
        let record = self
            .engine
            .runtime
            .authorize(
                &self.principal,
                workspace,
                Some(branch),
                agentdrive_model::Permission::Read,
            )
            .await?;
        let branch = self.engine.runtime.branch(branch).await?;
        if record.deleted || branch.state == BranchState::Deleted {
            return Err(Error::new(
                ErrorKind::NoSuchKey,
                "workspace or branch has been deleted",
            ));
        }
        self.leases
            .bind(
                bucket,
                &record.name,
                &branch.name,
                workspace,
                branch.id,
                mode,
            )
            .await
    }

    pub async fn branch_mounts(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Vec<SessionStatus>> {
        self.engine
            .runtime
            .authorize(
                &self.principal,
                workspace,
                Some(branch),
                agentdrive_model::Permission::Read,
            )
            .await?;
        self.leases.branch_mounts(workspace, branch).await
    }

    pub async fn status(&self, bucket: &str) -> Result<SessionStatus> {
        self.leases.status(bucket).await
    }
    pub async fn check_writer(&self, bucket: &str) -> Result<()> {
        self.status(bucket).await?.require_writer()
    }
    pub async fn acquire(&self, bucket: &str) -> Result<SessionStatus> {
        self.leases.change(bucket, SessionChange::Acquire).await
    }
    pub async fn release(&self, bucket: &str) -> Result<SessionStatus> {
        self.leases.change(bucket, SessionChange::Release).await
    }
    pub async fn handoff(&self, from: &str, to: &str) -> Result<SessionStatus> {
        self.leases
            .change(from, SessionChange::Handoff(to.into()))
            .await
    }
}
