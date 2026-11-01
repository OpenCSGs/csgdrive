use crate::{Branch, BranchId, Error, LocationId, MountId, OperationId, Principal, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollaborationSettings {
    pub enabled: bool,
    pub idle_timeout_secs: Option<u64>,
    pub release_on_event: bool,
}

impl Default for CollaborationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_timeout_secs: Some(60),
            release_on_event: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationPhase {
    Disabled,
    Following,
    Writing,
    WaitingForHandles,
    Saving,
    Recovering,
    Switching,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Started,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationExecution {
    pub actor: Principal,
    pub mount: MountId,
    pub binding_generation: u64,
    pub source: String,
    pub thread_id: String,
    pub turn_id: String,
    pub authority_epoch: u64,
    pub pending_grant: bool,
    pub phase: ExecutionPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Idle,
    ExecutionEnded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationRelease {
    pub operation: OperationId,
    pub authority_epoch: u64,
    pub mutation_seq: u64,
    pub reason: ReleaseReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationRecovery {
    pub operation: OperationId,
    pub branch: Option<BranchId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationTransition {
    pub operation: OperationId,
    pub previous: Branch,
}

/// 每个分支持久保存一份协调状态，文件内容复用现有存储。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationRecord {
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub actor: Principal,
    pub settings: CollaborationSettings,
    pub phase: CollaborationPhase,
    pub executions: Vec<CollaborationExecution>,
    pub release: Option<CollaborationRelease>,
    pub transition: Option<CollaborationTransition>,
    pub recovery: Option<CollaborationRecovery>,
    pub error: Option<Error>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuthorityGuard {
    pub owner: LocationId,
    pub authority_epoch: u64,
    pub available: bool,
}
