use agentdrive_model::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MergeHistoryFilter {
    pub branch: Option<BranchId>,
    pub source_branch: Option<BranchId>,
    pub target_branch: Option<BranchId>,
    pub location: Option<LocationId>,
    pub status: Option<MergeHistoryStatus>,
    pub revision: Option<RevisionId>,
    pub from_ns: Option<i64>,
    pub to_ns: Option<i64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeHistoryStatus {
    Conflicts,
    Pending,
    WaitingForMount,
    Applied,
    Failed,
    RecoveryRequired,
    AlreadyIncluded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryEntry {
    pub id: String,
    pub merge_id: Option<MergeId>,
    pub source_branch: Option<BranchId>,
    pub source_name: Option<String>,
    pub source_revision: Option<RevisionId>,
    pub target_branch: Option<BranchId>,
    pub target_name: Option<String>,
    pub target_revision: Option<RevisionId>,
    pub base_revision: Option<RevisionId>,
    pub candidate_revision: Option<RevisionId>,
    pub applied_revision: Option<RevisionId>,
    pub principal: Principal,
    pub origin_location: Option<LocationId>,
    pub created_ns: i64,
    pub updated_ns: i64,
    pub status: MergeHistoryStatus,
    pub remote_confirmed: Option<bool>,
    pub mount_ready: Option<bool>,
    pub history_complete: bool,
    pub error: Option<Error>,
    pub conflict_count: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryPage {
    pub workspace: WorkspaceId,
    pub location: LocationId,
    pub observed_ns: i64,
    pub entries: Vec<MergeHistoryEntry>,
    pub total: usize,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MergeEventsQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryAction {
    pub id: String,
    pub operation: OperationId,
    pub request_id: String,
    pub name: String,
    pub principal: Principal,
    pub location: Option<LocationId>,
    pub created_ns: i64,
    pub phase: OperationPhase,
    pub local_saved: bool,
    pub remote_confirmed: bool,
    pub mount_ready: Option<bool>,
    pub error: Option<Error>,
    pub candidate_before: Option<RevisionId>,
    pub candidate_revision: Option<RevisionId>,
    pub applied_revision: Option<RevisionId>,
    pub conflict_count: Option<usize>,
    pub paths: Vec<MergeHistoryPath>,
    pub history_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryPath {
    pub path: WorkspacePath,
    pub choice: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryEvents {
    pub entries: Vec<MergeHistoryAction>,
    pub total: usize,
    pub next: Option<String>,
    pub revisions: Vec<Revision>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeHistoryDetail {
    pub entry: MergeHistoryEntry,
    pub revisions: Vec<Revision>,
    pub events: MergeHistoryEvents,
    pub location: LocationId,
    pub observed_ns: i64,
}

/// Storage pagination uses the immutable start time so active updates do not
/// move an existing merge between pages.
#[derive(Clone, Debug)]
pub struct MergeHistoryScan {
    pub workspace: WorkspaceId,
    pub filter: MergeHistoryFilter,
    pub readable_branches: Option<Vec<BranchId>>,
    pub before: Option<(i64, String)>,
    pub limit: usize,
}

#[derive(Clone, Debug)]
pub struct MergeEventsScan {
    pub workspace: WorkspaceId,
    pub record: String,
    pub after: Option<(i64, String)>,
    pub limit: usize,
}

#[async_trait]
pub trait MergeHistoryApi: Send + Sync + std::fmt::Debug {
    async fn list(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        filter: MergeHistoryFilter,
    ) -> Result<MergeHistoryPage>;
    async fn detail(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        record: &str,
    ) -> Result<MergeHistoryDetail>;
    async fn events(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        record: &str,
        query: MergeEventsQuery,
    ) -> Result<MergeHistoryEvents>;
}
