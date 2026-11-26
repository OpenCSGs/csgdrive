use crate::{Branch, BranchId, Error, LocationId, OperationId, Principal, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationSettings {
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationPhase {
    Disabled,
    Following,
    #[serde(alias = "waiting_for_handles")]
    Writing,
    Saving,
    Recovering,
    Switching,
    Failed,
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
