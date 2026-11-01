use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Metadata retained alongside an operation for the workspace's merge history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeOperationHistory {
    pub merge: Option<MergeId>,
    pub source: RevisionId,
    pub source_branch: Option<BranchId>,
    pub target: BranchGuard,
    pub base: Option<RevisionId>,
    pub location: LocationId,
    pub candidate_before: Option<RevisionId>,
    pub resolutions: Vec<MergeResolutionSummary>,
    pub events: Vec<MergeOperationEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeResolutionSummary {
    pub path: WorkspacePath,
    pub choice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<RevisionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<WorkspacePath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeOperationEvent {
    pub sequence: u64,
    pub at_ns: i64,
    pub phase: OperationPhase,
    pub local_saved: bool,
    pub remote_confirmed: bool,
    pub mount_ready: Option<bool>,
    pub error: Option<Error>,
    pub candidate: Option<RevisionId>,
    pub result_revision: Option<RevisionId>,
    pub conflict_count: Option<usize>,
}

/// The exact event prefix included in a published history index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeHistoryVersion {
    pub operation: OperationId,
    pub sequence: u64,
}

impl MergeOperationHistory {
    pub fn referenced_revisions(&self) -> BTreeSet<RevisionId> {
        let mut ids = BTreeSet::from([self.source, self.target.expected_head]);
        ids.extend(self.base);
        ids.extend(self.candidate_before);
        for resolution in &self.resolutions {
            ids.extend(resolution.revision);
        }
        for event in &self.events {
            ids.extend(event.candidate);
            ids.extend(event.result_revision);
        }
        ids
    }
}

impl OperationRecord {
    /// Capture receipt transitions without recording polling or duplicate retries.
    pub fn record_merge_event(&mut self, at_ns: i64) {
        let Some(history) = self.merge_history.as_mut() else {
            return;
        };
        let mut event = MergeOperationEvent {
            sequence: history.events.len() as u64 + 1,
            at_ns,
            phase: self.phase,
            local_saved: self.local_saved,
            remote_confirmed: self.remote_confirmed,
            mount_ready: self.mount_ready,
            error: self.error.clone(),
            candidate: None,
            result_revision: None,
            conflict_count: None,
        };
        if let Some(CommandResult::Merge { candidate }) = &self.result {
            history.merge = Some(candidate.id);
            history.base = Some(candidate.base);
            event.candidate = Some(candidate.revision);
            event.result_revision = candidate.applied_revision;
            event.conflict_count = Some(candidate.conflicts.len());
        } else if let Some(CommandResult::Revision { revision, .. }) = self.result {
            event.result_revision = Some(revision);
        }
        if let Some(previous) = history.events.last() {
            let mut comparable = event.clone();
            comparable.sequence = previous.sequence;
            comparable.at_ns = previous.at_ns;
            if &comparable == previous {
                return;
            }
        }
        history.events.push(event);
    }
}
