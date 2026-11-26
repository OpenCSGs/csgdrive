use agentdrive_model::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureCollaboration {
    pub mount: MountId,
    #[serde(default)]
    pub expected_binding_generation: Option<u64>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationStatus {
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub settings: CollaborationSettings,
    pub phase: CollaborationPhase,
    pub writer: Option<LocationId>,
    pub authority_epoch: u64,
    pub mounts: Vec<MountId>,
    pub local_saved: bool,
    pub remote_confirmed: bool,
    pub recovery_branch: Option<BranchId>,
    pub error: Option<Error>,
}

#[async_trait]
pub trait CollaborationApi: Send + Sync {
    async fn configure(
        &self,
        actor: &Principal,
        request: ConfigureCollaboration,
    ) -> Result<CollaborationStatus>;
    async fn status(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
    ) -> Result<Vec<CollaborationStatus>>;
}

#[async_trait]
pub trait WriteAccess: Send + Sync {
    fn automatic_enabled(&self) -> bool;
    fn automatic_enabled_for(&self, _workspace: WorkspaceId) -> bool {
        self.automatic_enabled()
    }
    async fn prepare_mount(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<()>;
    /// 在取得分支修改锁之前调用，文件服务取得锁后再次检查冻结状态。
    async fn prepare_write(&self, mount: MountId, expected_binding_generation: u64) -> Result<()>;
    /// Acquire authority for an explicit command, preserving its content preconditions.
    /// Call before lifecycle and branch locks; validate the returned guard when saving.
    async fn prepare_branch_write(
        &self,
        actor: &Principal,
        guard: BranchGuard,
        permission: Permission,
    ) -> Result<BranchGuard>;
}
