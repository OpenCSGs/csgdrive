use crate::{ErrorCode, OperationPhase, OperationRecord, PendingAction};
use serde::Serialize;

/// A safe continuation of an existing operation, derived from its saved action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationRecovery {
    RetryRequest,
    ReleaseMount { expected_binding_generation: u64 },
}

impl OperationRecord {
    pub fn recovery(&self) -> Option<OperationRecovery> {
        if !matches!(
            self.phase,
            OperationPhase::WaitingForQuiesce
                | OperationPhase::RecoveryRequired
                | OperationPhase::CommittedMountPending
                | OperationPhase::Failed
        ) {
            return None;
        }
        match &self.pending {
            None if self.name == "mount_save"
                && self.phase == OperationPhase::Failed
                && !self.local_saved
                && self.result.is_none()
                && self.error.as_ref().is_some_and(|error| {
                    error.retryable || error.code == ErrorCode::StaleAuthority
                }) =>
            {
                Some(OperationRecovery::RetryRequest)
            }
            Some(PendingAction::BranchSwitch { .. }) if self.name == "branch_switch" => {
                Some(OperationRecovery::RetryRequest)
            }
            Some(PendingAction::MountChange {
                old: None,
                new: Some(_),
            }) if self.name == "mount_prepare" => Some(OperationRecovery::RetryRequest),
            Some(
                PendingAction::MountChange { old: Some(old), .. }
                | PendingAction::ApplyRevision {
                    mount: Some(old), ..
                },
            ) => Some(OperationRecovery::ReleaseMount {
                expected_binding_generation: old.generation,
            }),
            _ => None,
        }
    }
}
