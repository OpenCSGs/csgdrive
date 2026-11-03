use super::*;

impl CollaborationService {
    pub(super) fn transition_active(&self, record: &CollaborationRecord) -> bool {
        record.transition.is_some()
            || record.recovery.is_some()
            || record.phase == CollaborationPhase::Switching
            || (record.phase == CollaborationPhase::Saving && record.release.is_some())
            || self
                .runtime
                .collaboration_blocked
                .lock()
                .contains(&record.branch)
    }

    pub(super) async fn suspend(&self, record: &mut CollaborationRecord) -> Result<()> {
        if record.settings.enabled
            || record.phase != CollaborationPhase::Disabled
            || record.release.is_some()
        {
            record.settings.enabled = false;
            record.release = None;
            record.phase = CollaborationPhase::Disabled;
            record.error = None;
            self.activity.lock().remove(&record.branch);
            self.save_record(record).await?;
        }
        Ok(())
    }

    pub(super) async fn register_mounted_branches(&self) -> Result<()> {
        if !self.automatic_enabled() {
            return Ok(());
        }
        for mount in self.runtime.state.mounts().await? {
            if self
                .runtime
                .state
                .workspace(mount.workspace)
                .await?
                .is_none_or(|w| w.deleted)
            {
                continue;
            }
            if mount.state == MountState::Released
                || mount.access != AccessMode::ReadWrite
                || mount.revision.is_some()
            {
                continue;
            }
            if self
                .runtime
                .state
                .collaboration(mount.branch)
                .await?
                .is_some_and(|record| record.settings.enabled)
            {
                continue;
            }
            let branch = self.runtime.branch(mount.branch).await?;
            if branch.protected || branch.state == BranchState::Deleted {
                continue;
            }
            match self
                .prepare_mount(&mount.principal, mount.workspace, mount.branch)
                .await
            {
                Ok(()) => {}
                Err(_) if !self.automatic_enabled() => return Ok(()),
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::BranchBusy | ErrorCode::PermissionDenied
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
