use super::*;
use crate::revisions::ForkSource;

impl CollaborationService {
    pub(super) async fn begin_recovery(&self, record: &mut CollaborationRecord) -> Result<()> {
        self.runtime
            .collaboration_blocked
            .lock()
            .insert(record.branch);
        self.runtime.publishing_turns.lock().remove(&record.branch);
        if record.recovery.is_none() {
            record.recovery = self
                .runtime
                .state
                .collaboration(record.branch)
                .await?
                .and_then(|stored| stored.recovery)
                .or_else(|| {
                    Some(CollaborationRecovery {
                        operation: OperationId::new(),
                        branch: None,
                    })
                });
        }
        record.transition = None;
        record.phase = CollaborationPhase::Recovering;
        record.error = Some(Error::new(
            ErrorCode::StaleAuthority,
            "write authority changed; local changes are being preserved on a recovery branch",
        ));
        self.save_record(record).await?;
        self.recover_branch(record).await
    }

    pub(super) async fn recover_branch(&self, record: &mut CollaborationRecord) -> Result<()> {
        self.runtime
            .collaboration_blocked
            .lock()
            .insert(record.branch);
        let recovery = record
            .recovery
            .clone()
            .ok_or_else(|| Error::integrity("recovery state is missing"))?;
        if recovery.branch.is_none() && self.follow_confirmed_view(record).await? {
            return Ok(());
        }
        let target = if let Some(branch) = recovery.branch {
            branch
        } else {
            let _gate = self.runtime.lock_branch(record.branch).await;
            let _objects = self.runtime.objects.read().await;
            let original = self.runtime.branch(record.branch).await?;
            let saved = self.revisions.capture_recovery_locked(&original).await?;
            let start = self
                .operations
                .start(
                    RequestContext {
                        principal: record.actor.clone(),
                        request_id: format!("collaboration-recovery-{}", recovery.operation),
                    },
                    "collaboration_recovery",
                    &(record.branch, recovery.operation),
                    Some(record.workspace),
                    Some(record.branch),
                )
                .await?;
            let operation = match start {
                OperationStart::Existing(operation) if operation.result.is_some() => operation,
                OperationStart::Existing(operation) => {
                    self.fork_recovery(OperationContext { operation }, &saved, recovery.operation)
                        .await?
                }
                OperationStart::New(context) => {
                    self.fork_recovery(context, &saved, recovery.operation)
                        .await?
                }
            };
            let Some(CommandResult::Branch { branch: fork }) = operation.result else {
                return Err(Error::integrity(
                    "recovery operation did not create a branch",
                ));
            };
            let target = fork.id;
            self.runtime.collaboration_blocked.lock().insert(target);
            record
                .recovery
                .as_mut()
                .expect("recovery was checked")
                .branch = Some(target);
            record.phase = CollaborationPhase::Recovering;
            let mut stopped = saved;
            stopped.state = BranchState::Stopped;
            let obsolete: Vec<_> = self
                .runtime
                .state
                .sync_jobs(record.workspace)
                .await?
                .into_iter()
                .filter(|job| job.reference.branch == record.branch)
                .map(|job| job.operation)
                .collect();
            self.operations.update(&obsolete, LocalCommit { branches: vec![stopped], collaborations: vec![record.clone()], completed_jobs: obsolete.clone(), ..Default::default() }, |operation| {
                operation.phase = OperationPhase::Failed;
                operation.pending = None;
                operation.error = Some(Error::new(ErrorCode::StaleAuthority, format!("write authority changed; local work is preserved on recovery branch {target}")));
            }).await?;
            target
        };
        self.runtime.collaboration_blocked.lock().insert(target);
        {
            let _gate = self.runtime.lock_branch(target).await;
            let _objects = self.runtime.objects.read().await;
            let branch = self.runtime.branch(target).await?;
            self.revisions
                .preserve_working_locked(&branch, &record.actor)
                .await?;
        }
        self.publisher.drain(record.workspace, Some(target)).await?;
        record.phase = CollaborationPhase::Switching;
        self.save_record(record).await?;
        self.mounts
            .rebind_collaboration(record.branch, target)
            .await?;
        let target_branch = self.runtime.branch(target).await?;
        let source = self.runtime.branch(record.branch).await?;
        let mut next = self.new_record(
            &target_branch,
            record.actor.clone(),
            record.settings.clone(),
        );
        next.phase = CollaborationPhase::Writing;
        for execution in &record.executions {
            if !execution.pending_grant
                && execution.authority_epoch == source.authority_epoch
                && let Some(binding) = self.runtime.state.mount(execution.mount).await?
                && binding.branch == target
                && binding.state == MountState::Ready
            {
                let mut execution = execution.clone();
                execution.binding_generation = binding.generation;
                execution.authority_epoch = target_branch.authority_epoch;
                next.executions.push(execution);
            }
        }
        // A turn that already ended before authority was lost still hands off its recovered
        // contents. Recovery without a terminal callback keeps the branch writable.
        if next.executions.is_empty()
            && let Some(prior) = record.release.take()
            && prior.reason == ReleaseReason::ExecutionEnded
        {
            next.phase = CollaborationPhase::Saving;
            next.release = Some(CollaborationRelease {
                operation: recovery.operation,
                authority_epoch: target_branch.authority_epoch,
                reason: ReleaseReason::ExecutionEnded,
                message: prior.message,
            });
        }
        record.executions.retain(|execution| {
            !next.executions.iter().any(|continued| {
                continued.actor == execution.actor
                    && continued.mount == execution.mount
                    && continued.source == execution.source
                    && continued.thread_id == execution.thread_id
                    && continued.turn_id == execution.turn_id
            })
        });
        record.settings.enabled = false;
        record.phase = CollaborationPhase::Disabled;
        record.recovery = None;
        record.transition = None;
        record.release = None;
        record.error = None;
        let release_target = next.release.is_some();
        self.runtime
            .state
            .commit(LocalCommit {
                collaborations: vec![record.clone(), next],
                ..Default::default()
            })
            .await?;
        if release_target {
            self.runtime.publishing_turns.lock().insert(target);
        }
        self.runtime.collaboration_blocked.lock().remove(&target);
        self.runtime
            .collaboration_blocked
            .lock()
            .remove(&record.branch);
        Ok(())
    }

    async fn fork_recovery(
        &self,
        context: OperationContext,
        saved: &Branch,
        recovery: OperationId,
    ) -> Result<OperationRecord> {
        self.revisions
            .fork_locked(
                context,
                saved.workspace,
                ForkSource {
                    revision: saved.autosave_head.unwrap_or(saved.formal_head),
                    working_root: Some(saved.working_root.clone()),
                },
                format!("recovery/{recovery}"),
                Some(saved.id),
                false,
            )
            .await
    }

    async fn follow_confirmed_view(&self, record: &mut CollaborationRecord) -> Result<bool> {
        let branch = self.runtime.branch(record.branch).await?;
        if branch.mutation_seq != branch.saved_seq
            || self
                .runtime
                .state
                .sync_jobs(record.workspace)
                .await?
                .iter()
                .any(|job| job.reference.branch == record.branch)
        {
            return Ok(false);
        }
        let Some((reference, version)) =
            self.remote_branch(record.workspace, record.branch).await?
        else {
            return Ok(false);
        };
        if reference.working_root != branch.working_root
            || reference.formal_head != branch.formal_head
            || reference.autosave_head != branch.autosave_head
            || reference.durable_seq != branch.saved_seq
        {
            return Ok(false);
        }
        let _gate = self.runtime.lock_branch(record.branch).await;
        if self.runtime.branch(record.branch).await? != branch {
            return Ok(false);
        }
        let mut stopped = branch.clone();
        stopped.state = BranchState::Stopped;
        record.phase = CollaborationPhase::Following;
        record.transition = None;
        record.release = None;
        record.recovery = None;
        record.error = None;
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![BranchGuard::from(&branch)],
                branches: vec![stopped],
                collaborations: vec![record.clone()],
                remote_refs: vec![ObservedBranchRef::new(
                    reference,
                    version,
                    self.runtime.clock.now_ns(),
                )],
                ..Default::default()
            })
            .await?;
        self.runtime
            .collaboration_blocked
            .lock()
            .remove(&record.branch);
        Ok(true)
    }
}
