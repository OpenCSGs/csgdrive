use super::*;
use crate::runtime::increment;

impl MountService {
    pub(super) async fn automatic_mount_branch(&self, request: &PrepareMount) -> Result<BranchId> {
        if let Some(branch) = request.branch {
            return Ok(branch);
        }
        if let Some(mount) = self
            .runtime
            .state
            .mounts()
            .await?
            .into_iter()
            .find(|mount| {
                mount.workspace == request.workspace
                    && mount.path == request.path
                    && mount.state != MountState::Released
            })
        {
            return Ok(mount.branch);
        }
        if let Some(branch) = self
            .runtime
            .state
            .selected_branch(request.workspace)
            .await?
        {
            return Ok(branch);
        }
        let workspace = self
            .runtime
            .state
            .workspace(request.workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        self.revisions
            .trees
            .revision(workspace.id, workspace.initial_revision)
            .await?
            .branch
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::RecoveryRequired,
                    "workspace has no root branch; select an explicit branch",
                )
            })
    }

    pub(crate) fn check_collaboration_support(&self) -> Result<()> {
        if !self.driver.supports_automatic_collaboration() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "automatic collaboration requires native mount support in this build",
            ));
        }
        Ok(())
    }

    pub(crate) async fn collaboration_open_handles(&self, branch: BranchId) -> Result<usize> {
        if self.runtime.sandbox_pins.lock().contains(&branch) {
            return Ok(1);
        }
        let mut count = 0usize;
        for mount in self.runtime.state.mounts().await? {
            if mount.branch == branch && mount.state != MountState::Released {
                // Native file handles also appear in FileService; directory and metadata
                // handles only appear in the driver. Both sources must report zero.
                let native = match self.driver.open_handles(&mount).await {
                    Ok(count) => count,
                    Err(_) if self.driver.status(&mount).await? == NativeMountState::Absent => 0,
                    Err(error) => return Err(error),
                };
                count = count.saturating_add(native.max(self.files.open_handles(mount.id)));
            }
        }
        Ok(count)
    }

    pub(crate) async fn rebind_collaboration(
        &self,
        source: BranchId,
        target: BranchId,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let target = self.runtime.branch(target).await?;
        for operation in self.runtime.state.operations(target.workspace).await? {
            if operation.name == "collaboration_rebind"
                && matches!(&operation.pending, Some(PendingAction::MountChange { old: Some(old), new: Some(new) }) if old.branch == source && new.branch == target.id)
            {
                let completed = self.execute(operation).await?;
                if let Some(error) = completed.error {
                    return Err(error);
                }
            }
        }
        for old in self.runtime.state.mounts().await? {
            if old.branch != source
                || old.state == MountState::Released
                || old.revision.is_some()
                || old.access != AccessMode::ReadWrite
            {
                continue;
            }
            let request = (
                old.id,
                source,
                target.id,
                target.authority_epoch,
                target.source_generation,
            );
            let context = RequestContext {
                principal: old.principal.clone(),
                request_id: format!(
                    "collaboration-rebind:{}:{}:{}:{}",
                    old.id, target.id, target.authority_epoch, target.source_generation
                ),
            };
            let operation = self
                .operations
                .start(
                    context,
                    "collaboration_rebind",
                    &request,
                    Some(target.workspace),
                    Some(target.id),
                )
                .await?;
            let record = match operation {
                OperationStart::Existing(record)
                    if record.pending.is_none() && record.local_saved =>
                {
                    continue;
                }
                OperationStart::Existing(record) if record.pending.is_some() => record,
                OperationStart::Existing(record)
                | OperationStart::New(OperationContext { operation: record }) => {
                    let mut next = old.clone();
                    next.branch = target.id;
                    next.generation = increment(old.generation)?;
                    next.branch_generation = Some(target.generation);
                    next.state = MountState::Ready;
                    let mut record = record;
                    record.pending = Some(PendingAction::MountChange {
                        old: Some(old),
                        new: Some(next),
                    });
                    record.phase = OperationPhase::CommittedMountPending;
                    record.mount_ready = Some(false);
                    self.runtime
                        .state
                        .commit(LocalCommit {
                            operations: vec![record.clone()],
                            ..Default::default()
                        })
                        .await?;
                    record
                }
            };
            let completed = self.execute(record).await?;
            if let Some(error) = completed.error {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) async fn execute_rebind(
        &self,
        mut operation: OperationRecord,
        old: &MountBinding,
        next: &MountBinding,
    ) -> Result<OperationRecord> {
        let mut ids = vec![old.branch, next.branch];
        ids.sort_unstable();
        ids.dedup();
        let mut gates = Vec::new();
        for id in &ids {
            gates.push(self.runtime.lock_branch(*id).await);
        }
        let current = self.runtime.state.mount(old.id).await?.ok_or_else(|| {
            Error::new(
                ErrorCode::StaleBinding,
                "collaborative mount no longer exists",
            )
        })?;
        let recovered_binding = current.state == MountState::RecoveryRequired && {
            let mut before_restart = current.clone();
            before_restart.state = old.state;
            let matches_old = before_restart == *old;
            before_restart.state = next.state;
            matches_old || before_restart == *next
        };
        if current != *old && current != *next && !recovered_binding {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "collaborative mount was changed by another operation",
            ));
        }
        if self.collaboration_open_handles(old.branch).await? != 0
            || (old.branch != next.branch
                && self.collaboration_open_handles(next.branch).await? != 0)
        {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "close all file and directory handles before switching the working branch",
            )
            .retryable());
        }
        let target = self.runtime.branch(next.branch).await?;
        if target.generation != next.branch_generation.unwrap_or(next.generation) {
            return Err(Error::new(
                ErrorCode::TargetMoved,
                "target working view changed before mount switching",
            ));
        }
        let inserted: Vec<_> = {
            let mut blocked = self.runtime.collaboration_blocked.lock();
            ids.into_iter().filter(|id| blocked.insert(*id)).collect()
        };
        drop(gates);
        let result = async {
            let next_state = self.driver.status(next).await?;
            let old_state = self.driver.status(old).await?;
            match (next_state, old_state) {
                (NativeMountState::Mounted, _) => (),
                (_, NativeMountState::Mounted) if self.driver.requires_remount_for_rebind() => {
                    self.driver.unmount(old).await?;
                    self.install_new(next).await?;
                }
                (_, NativeMountState::Mounted) => self.driver.rebind(old, next).await?,
                (NativeMountState::Absent, NativeMountState::Absent) => {
                    self.install_new(next).await?;
                }
                _ => {
                    return Err(Error::new(
                        ErrorCode::MountBusy,
                        "native mount identity cannot be confirmed while resuming collaboration",
                    )
                    .retryable());
                }
            }
            operation.pending = None;
            operation.local_saved = true;
            operation.mount_ready = Some(true);
            operation.phase = OperationPhase::Complete;
            operation.error = None;
            operation.result = Some(CommandResult::Mount {
                binding: next.clone(),
            });
            let selections = if old.branch != next.branch
                && self.runtime.state.selected_branch(old.workspace).await? == Some(old.branch)
            {
                vec![WorkspaceSelection {
                    workspace: old.workspace,
                    branch: next.branch,
                }]
            } else {
                Vec::new()
            };
            self.runtime
                .state
                .commit(LocalCommit {
                    guards: vec![BranchGuard::from(&target)],
                    mounts: vec![next.clone()],
                    operations: vec![operation.clone()],
                    selections,
                    ..Default::default()
                })
                .await?;
            Ok(operation)
        }
        .await;
        for id in inserted {
            self.runtime.collaboration_blocked.lock().remove(&id);
        }
        result
    }
}
