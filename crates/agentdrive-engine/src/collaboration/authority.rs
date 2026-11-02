use super::*;
use crate::{replica::decode_branch_ref, runtime::increment};

impl CollaborationService {
    pub(super) async fn remote_branch(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Option<(BranchRef, String)>> {
        let refs = self.revisions.trees.content.refs().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let Some(value) = refs.get(&RefKey::Branch { workspace, branch }).await? else {
            return Ok(None);
        };
        let reference = decode_branch_ref(workspace, branch, &value.bytes)?;
        self.runtime
            .state
            .commit(LocalCommit {
                remote_refs: vec![ObservedBranchRef::new(
                    reference.clone(),
                    value.version.clone(),
                    self.runtime.clock.now_ns(),
                )],
                ..Default::default()
            })
            .await?;
        Ok(Some((reference, value.version)))
    }

    pub(super) async fn install_view(&self, reference: BranchRef, version: String) -> Result<()> {
        if reference.deleted {
            return Err(Error::new(ErrorCode::NotFound, "remote branch was deleted"));
        }
        let branch_id = reference.branch;
        if self
            .runtime
            .state
            .sync_jobs(reference.workspace)
            .await?
            .iter()
            .any(|job| job.reference.branch == branch_id)
        {
            return Err(Error::new(
                ErrorCode::RecoveryRequired,
                "pending local publication must finish before updating this branch view",
            ));
        }
        if self.mounts.collaboration_open_handles(branch_id).await? != 0 {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "close open handles before updating this branch view",
            )
            .retryable());
        }
        let _objects = self.runtime.objects.read().await;
        self.publisher
            .import_history(ObservedBranchRef::new(
                reference.clone(),
                version,
                self.runtime.clock.now_ns(),
            ))
            .await?;
        let previous = self.runtime.state.branch(branch_id).await?;
        let tree = if previous
            .as_ref()
            .is_some_and(|old| old.working_root == reference.working_root)
        {
            None
        } else {
            Some(
                self.revisions
                    .trees
                    .tree(reference.workspace, &reference.working_root)
                    .await?,
            )
        };
        let (next_inode, dirty) = if let Some(old) = &previous
            && tree.is_none()
            && old.formal_head == reference.formal_head
        {
            (old.next_inode, old.dirty)
        } else {
            let formal = self
                .revisions
                .trees
                .revision(reference.workspace, reference.formal_head)
                .await?;
            let next_inode = match &tree {
                Some(tree) => increment(tree.values().map(|inode| inode.id.0).max().unwrap_or(1))?,
                None => {
                    previous
                        .as_ref()
                        .ok_or_else(|| Error::integrity("branch tree is missing"))?
                        .next_inode
                }
            };
            (next_inode, reference.working_root != formal.root)
        };
        let changed = previous.as_ref().is_none_or(|old| {
            old.working_root != reference.working_root || old.generation != reference.generation
        });
        let recovering_binding = self
            .runtime
            .collaboration_blocked
            .lock()
            .contains(&branch_id);
        let branch = Branch {
            id: branch_id,
            workspace: reference.workspace,
            name: previous
                .as_ref()
                .map(|branch| branch.name.clone())
                .or_else(|| {
                    reference
                        .metadata
                        .as_ref()
                        .map(|metadata| metadata.name.clone())
                })
                .unwrap_or_else(|| format!("shared-{branch_id}")),
            owner: reference.owner,
            authority_epoch: reference.authority_epoch,
            generation: reference.generation,
            mutation_seq: reference.durable_seq,
            saved_seq: reference.durable_seq,
            source_generation: reference.source_generation,
            formal_head: reference.formal_head,
            autosave_head: reference.autosave_head,
            working_root: reference.working_root.clone(),
            base_revision: reference
                .metadata
                .as_ref()
                .map_or(reference.formal_head, |metadata| metadata.base_revision),
            source_branch: reference
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.source_branch),
            next_inode,
            dirty,
            state: if reference.write_available {
                BranchState::Available
            } else if reference.owner == self.runtime.location && !reference.write_stopped {
                BranchState::Writable
            } else {
                BranchState::Stopped
            },
            protected: previous.as_ref().is_some_and(|branch| branch.protected),
        };
        drop(_objects);
        {
            let _gate = self.runtime.lock_branch(branch_id).await;
            let _commit_objects = self.runtime.objects.read().await;
            if self.mounts.collaboration_open_handles(branch_id).await? != 0 {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "branch acquired a handle while its update was being downloaded",
                )
                .retryable());
            }
            let current = self.runtime.state.branch(branch_id).await?;
            if current != previous {
                return Err(Error::new(
                    ErrorCode::TargetMoved,
                    "local branch changed before its view update",
                )
                .retryable());
            }
            if current
                .as_ref()
                .is_some_and(|branch| branch.mutation_seq != branch.saved_seq)
            {
                return Err(Error::new(
                    ErrorCode::RecoveryRequired,
                    "local changes must be preserved before replacing the branch view",
                ));
            }
            self.runtime.collaboration_blocked.lock().insert(branch_id);
            self.runtime
                .state
                .commit(LocalCommit {
                    guards: previous
                        .as_ref()
                        .map(BranchGuard::from)
                        .into_iter()
                        .collect(),
                    branches: vec![branch.clone()],
                    node_changes: tree
                        .map(|tree| NodeChange::Replace {
                            branch: branch_id,
                            nodes: tree
                                .into_iter()
                                .map(|(path, inode)| Node {
                                    path,
                                    inode,
                                    dirty: false,
                                })
                                .collect(),
                        })
                        .into_iter()
                        .collect(),
                    ..Default::default()
                })
                .await?;
        }
        if previous.is_some() && (changed || recovering_binding) {
            self.mounts
                .rebind_collaboration(branch_id, branch_id)
                .await?;
        }
        self.runtime.collaboration_blocked.lock().remove(&branch_id);
        Ok(())
    }

    pub(super) async fn follow(&self, record: &mut CollaborationRecord) -> Result<()> {
        if record.phase == CollaborationPhase::Switching {
            self.resume_view_switch(record).await?;
            if record.phase == CollaborationPhase::Switching {
                return Ok(());
            }
        }
        let Some((reference, version)) =
            self.remote_branch(record.workspace, record.branch).await?
        else {
            return Err(Error::new(
                ErrorCode::NotFound,
                "remote branch does not exist",
            ));
        };
        let local = self.runtime.branch(record.branch).await?;
        let pending = self
            .runtime
            .state
            .sync_jobs(record.workspace)
            .await?
            .iter()
            .any(|job| job.reference.branch == record.branch);
        if local.mutation_seq != local.saved_seq || pending {
            return self.begin_recovery(record).await;
        }
        if local.working_root == reference.working_root
            && local.generation == reference.generation
            && local.authority_epoch == reference.authority_epoch
            && local.source_generation == reference.source_generation
            && local.owner == reference.owner
        {
            if record.error.is_some() {
                record.error = None;
                self.save_record(record).await?;
            }
            return Ok(());
        }
        if self
            .mounts
            .collaboration_open_handles(record.branch)
            .await?
            != 0
        {
            return Ok(());
        }
        let changed = local.working_root != reference.working_root
            || local.generation != reference.generation;
        if changed {
            record.phase = CollaborationPhase::Switching;
            self.save_record(record).await?;
        }
        self.install_view(reference, version).await?;
        let local = self.runtime.branch(record.branch).await?;
        record.phase =
            if local.owner == self.runtime.location && local.state == BranchState::Writable {
                CollaborationPhase::Writing
            } else {
                CollaborationPhase::Following
            };
        record.error = None;
        self.save_record(record).await
    }

    pub(super) async fn acquire(&self, record: &mut CollaborationRecord) -> Result<()> {
        let local = self.runtime.branch(record.branch).await?;
        if local.protected {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "protected branch cannot acquire automatic write authority",
            ));
        }
        let Some((remote, version)) = self.remote_branch(record.workspace, record.branch).await?
        else {
            if local.owner == self.runtime.location && local.state == BranchState::Writable {
                self.publisher
                    .drain(local.workspace, Some(local.id))
                    .await?;
                record.phase = CollaborationPhase::Writing;
                self.save_record(record).await?;
                return Ok(());
            }
            return Err(Error::new(
                ErrorCode::NotFound,
                "remote branch does not exist",
            ));
        };
        if local.owner == self.runtime.location && local.state == BranchState::Writable {
            if remote.owner != local.owner
                || remote.authority_epoch != local.authority_epoch
                || remote.write_stopped
            {
                self.begin_recovery(record).await?;
                return Err(Error::new(
                    ErrorCode::RecoveryRequired,
                    "write authority changed; local changes are being preserved",
                ));
            }
            self.bind_executions(record, local.authority_epoch).await?;
            record.phase = CollaborationPhase::Writing;
            record.error = None;
            return self.save_record(record).await;
        }
        if local.working_root != remote.working_root || local.generation != remote.generation {
            // 原生请求可能已经预留句柄，目录更新交给后续后台任务处理。
            record.phase = CollaborationPhase::Following;
            self.save_record(record).await?;
            return Err(Error::new(
                ErrorCode::TargetMoved,
                "remote files changed; retry after the branch view updates and reread the files",
            )
            .retryable());
        }
        self.publisher
            .import_history(ObservedBranchRef::new(
                remote.clone(),
                version,
                self.runtime.clock.now_ns(),
            ))
            .await?;
        let formal = self
            .revisions
            .trees
            .revision(record.workspace, remote.formal_head)
            .await?;
        let mut next = local.clone();
        next.formal_head = remote.formal_head;
        next.autosave_head = remote.autosave_head;
        next.mutation_seq = remote.durable_seq;
        next.saved_seq = remote.durable_seq;
        next.dirty = remote.working_root != formal.root;
        next.owner = self.runtime.location;
        next.state = BranchState::Writable;
        if !remote.write_available {
            if remote.owner != self.runtime.location || remote.write_stopped {
                return Err(Error::new(
                    ErrorCode::BranchBusy,
                    format!("branch write authority is held by {}", remote.owner),
                )
                .retryable());
            }
            next.authority_epoch = remote.authority_epoch;
            next.source_generation = remote.source_generation;
            self.bind_executions(record, next.authority_epoch).await?;
            record.phase = CollaborationPhase::Writing;
            record.error = None;
            let _gate = self.runtime.lock_branch(record.branch).await;
            if self.runtime.branch(record.branch).await? != local {
                return Err(Error::new(
                    ErrorCode::TargetMoved,
                    "branch changed before adopting write authority",
                )
                .retryable());
            }
            self.runtime
                .state
                .commit(LocalCommit {
                    guards: vec![BranchGuard::from(&local)],
                    branches: vec![next],
                    collaborations: vec![record.clone()],
                    ..Default::default()
                })
                .await?;
            return Ok(());
        }
        next.authority_epoch = increment(remote.authority_epoch)?;
        next.source_generation = increment(remote.source_generation)?;
        if !self.automatic_enabled() {
            return Err(Error::invalid(
                "automatic collaboration is disabled in daemon settings",
            ));
        }
        self.queue_transition(
            record,
            local,
            next,
            AuthorityGuard {
                owner: remote.owner,
                authority_epoch: remote.authority_epoch,
                available: true,
            },
            OperationId::new(),
        )
        .await?;
        self.finish_transition(record).await
    }

    async fn queue_transition(
        &self,
        record: &mut CollaborationRecord,
        previous: Branch,
        next: Branch,
        expected: AuthorityGuard,
        request: OperationId,
    ) -> Result<()> {
        let start = self
            .operations
            .start(
                RequestContext {
                    principal: record.actor.clone(),
                    request_id: format!("collaboration-authority-{request}"),
                },
                "collaboration_authority",
                &(record.branch, expected),
                Some(record.workspace),
                Some(record.branch),
            )
            .await?;
        let mut operation = match start {
            OperationStart::New(context) => context.operation,
            OperationStart::Existing(operation) => operation,
        };
        if operation.result.is_none() {
            operation.result = Some(CommandResult::Branch {
                branch: next.clone(),
            });
            operation.local_saved = true;
            operation.phase = OperationPhase::LocalSaved;
        }
        let mut job = self.publisher.build_job(&next, None, &operation).await?;
        job.expected_authority = Some(expected);
        let _gate = self.runtime.lock_branch(record.branch).await;
        if self.runtime.branch(record.branch).await? != previous {
            return Err(Error::new(
                ErrorCode::TargetMoved,
                "branch changed before authority publication",
            )
            .retryable());
        }
        record.transition = Some(CollaborationTransition {
            operation: operation.id,
            previous: previous.clone(),
        });
        record.phase = CollaborationPhase::Saving;
        self.runtime
            .collaboration_blocked
            .lock()
            .insert(record.branch);
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![BranchGuard::from(&previous)],
                operations: vec![operation],
                sync_jobs: vec![job],
                collaborations: vec![record.clone()],
                ..Default::default()
            })
            .await
    }

    pub(super) async fn finish_transition(&self, record: &mut CollaborationRecord) -> Result<()> {
        let Some(transition) = record.transition.clone() else {
            return Ok(());
        };
        let id = transition.operation;
        let operation = self
            .runtime
            .state
            .operation(id)
            .await?
            .ok_or_else(|| Error::integrity("collaboration authority operation is missing"))?;
        if !operation.remote_confirmed
            && let Err(error) = self
                .publisher
                .drain(record.workspace, Some(record.branch))
                .await
        {
            if error.code == ErrorCode::StaleAuthority {
                self.operations
                    .update(
                        &[id],
                        LocalCommit {
                            completed_jobs: vec![id],
                            ..Default::default()
                        },
                        |operation| {
                            operation.phase = OperationPhase::Failed;
                            operation.error = Some(error.clone());
                        },
                    )
                    .await?;
                record.transition = None;
                if record.release.is_some() {
                    return self.begin_recovery(record).await;
                }
                self.runtime
                    .collaboration_blocked
                    .lock()
                    .remove(&record.branch);
                record.phase = CollaborationPhase::Following;
                record.error = Some(error);
                self.save_record(record).await?;
                return Err(Error::new(
                    ErrorCode::BranchBusy,
                    "another location acquired write authority",
                )
                .retryable());
            }
            return Err(error);
        }
        let operation = self
            .runtime
            .state
            .operation(id)
            .await?
            .ok_or_else(|| Error::integrity("collaboration authority operation is missing"))?;
        if !operation.remote_confirmed {
            return Err(Error::new(
                ErrorCode::RemoteUnknown,
                "write authority publication is not yet confirmed",
            )
            .retryable());
        }
        let Some(CommandResult::Branch { branch: next }) = operation.result else {
            return Err(Error::integrity(
                "collaboration authority operation has no branch result",
            ));
        };
        let _gate = self.runtime.lock_branch(record.branch).await;
        let before = self.runtime.branch(record.branch).await?;
        if before != transition.previous && before != next {
            drop(_gate);
            return self.begin_recovery(record).await;
        }
        if next.state == BranchState::Writable {
            self.bind_executions(record, next.authority_epoch).await?;
            record.phase = CollaborationPhase::Writing;
        } else {
            record.phase = CollaborationPhase::Following;
            record.release = None;
        }
        record.transition = None;
        record.error = None;
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![BranchGuard::from(&before)],
                branches: vec![next],
                collaborations: vec![record.clone()],
                ..Default::default()
            })
            .await?;
        self.runtime
            .collaboration_blocked
            .lock()
            .remove(&record.branch);
        Ok(())
    }

    async fn bind_executions(
        &self,
        record: &mut CollaborationRecord,
        authority_epoch: u64,
    ) -> Result<()> {
        for execution in &mut record.executions {
            if execution.pending_grant
                && let Some(binding) = self.runtime.state.mount(execution.mount).await?
                && binding.branch == record.branch
                && binding.state != MountState::Released
            {
                execution.authority_epoch = authority_epoch;
                execution.binding_generation = binding.generation;
                execution.pending_grant = false;
            }
        }
        Ok(())
    }

    pub(super) async fn release_authority(&self, record: &mut CollaborationRecord) -> Result<()> {
        let Some(release) = record.release.clone() else {
            return Ok(());
        };
        let saved = {
            let _gate = self.runtime.lock_branch(record.branch).await;
            if !self.automatic_enabled() && !self.transition_active(record) {
                return self.suspend(record).await;
            }
            let branch = self.runtime.branch(record.branch).await?;
            if branch.authority_epoch != release.authority_epoch
                || (release.reason == ReleaseReason::Idle
                    && branch.mutation_seq != release.mutation_seq)
            {
                record.release = None;
                record.phase = CollaborationPhase::Writing;
                return self.save_record(record).await;
            }
            if self.mounts.collaboration_open_handles(branch.id).await? != 0 {
                record.phase = CollaborationPhase::WaitingForHandles;
                return self.save_record(record).await;
            }
            let _objects = self.runtime.objects.read().await;
            let saved = self
                .revisions
                .preserve_working_locked(&branch, &record.actor)
                .await?;
            if !self.automatic_enabled() && !self.transition_active(record) {
                return self.suspend(record).await;
            }
            self.runtime.collaboration_blocked.lock().insert(branch.id);
            record.phase = CollaborationPhase::Saving;
            self.save_record(record).await?;
            saved
        };
        if let Err(error) = self.publisher.drain(saved.workspace, Some(saved.id)).await {
            if error.code == ErrorCode::StaleAuthority {
                return self.begin_recovery(record).await;
            }
            return Err(error);
        }
        let Some((remote, _)) = self.remote_branch(saved.workspace, saved.id).await? else {
            return Err(Error::new(
                ErrorCode::RemoteUnknown,
                "remote branch is missing during authority release",
            )
            .retryable());
        };
        if remote.owner != self.runtime.location
            || remote.authority_epoch != saved.authority_epoch
            || remote.write_available
        {
            return self.begin_recovery(record).await;
        }
        let previous = self.runtime.branch(saved.id).await?;
        let mut next = previous.clone();
        next.authority_epoch = increment(remote.authority_epoch)?;
        next.source_generation = increment(remote.source_generation)?;
        next.state = BranchState::Available;
        self.queue_transition(
            record,
            previous,
            next,
            AuthorityGuard {
                owner: remote.owner,
                authority_epoch: remote.authority_epoch,
                available: false,
            },
            release.operation,
        )
        .await?;
        self.finish_transition(record).await
    }

    pub(super) async fn resume_view_switch(&self, record: &mut CollaborationRecord) -> Result<()> {
        if self
            .mounts
            .collaboration_open_handles(record.branch)
            .await?
            != 0
        {
            return Ok(());
        }
        self.mounts
            .rebind_collaboration(record.branch, record.branch)
            .await?;
        self.runtime
            .collaboration_blocked
            .lock()
            .remove(&record.branch);
        record.phase = CollaborationPhase::Following;
        self.save_record(record).await
    }
}
