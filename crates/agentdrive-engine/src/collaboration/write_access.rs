use super::*;

impl CollaborationService {
    async fn register_branch(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        permission: Permission,
    ) -> Result<()> {
        if !self.config.load().values.collaboration.enabled {
            return Err(Error::invalid(
                "automatic collaboration is disabled in daemon settings",
            ));
        }
        self.mounts.check_collaboration_support()?;
        let workspace_record = self
            .runtime
            .state
            .workspace(workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        workspace_record.authorize(actor, Some(branch), permission)?;
        let _coordinate = self.coordinate(branch)?;
        if self.revisions.trees.content.refs().is_none() {
            return Err(Error::new(
                ErrorCode::RemoteRequired,
                "automatic collaboration requires remote storage",
            ));
        }
        if self.runtime.state.branch(branch).await?.is_none() {
            let (reference, version) = self
                .remote_branch(workspace, branch)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "remote branch does not exist"))?;
            self.install_view(reference, version).await?;
        }
        let _lifecycle = self.runtime.lifecycle.lock().await;
        if self.revisions.trees.content.refs().is_none() {
            return Err(Error::new(
                ErrorCode::RemoteRequired,
                "automatic collaboration requires remote storage",
            ));
        }
        if !self.config.load().values.collaboration.enabled {
            return Err(Error::invalid(
                "automatic collaboration is disabled in daemon settings",
            ));
        }
        let branch = self.runtime.branch(branch).await?;
        if branch.workspace != workspace {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "branch belongs to another workspace",
            ));
        }
        if branch.protected {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "protected branches cannot use automatic collaboration",
            ));
        }
        if let Some(mut record) = self.runtime.state.collaboration(branch.id).await? {
            if !record.settings.enabled {
                record.settings.enabled = true;
                if !self.transition_active(&record) {
                    record.phase = CollaborationPhase::Following;
                }
                self.save_record(&record).await?;
            }
        } else {
            self.save_record(&self.new_record(
                &branch,
                actor.clone(),
                CollaborationSettings {
                    enabled: true,
                    ..Default::default()
                },
            ))
            .await?;
        }
        Ok(())
    }

    async fn prepare_branch_access(
        &self,
        actor: &Principal,
        branch_id: BranchId,
        permission: Permission,
        expected: Option<BranchGuard>,
    ) -> Result<()> {
        if self.automatic_enabled()
            && self
                .runtime
                .state
                .collaboration(branch_id)
                .await?
                .is_none_or(|record| !record.settings.enabled)
        {
            let branch = self.runtime.branch(branch_id).await?;
            self.register_branch(actor, branch.workspace, branch_id, permission)
                .await?;
        }
        let Some(record) = self.runtime.state.collaboration(branch_id).await? else {
            return Ok(());
        };
        if !record.settings.enabled {
            return Ok(());
        }
        if !self.automatic_enabled() {
            let _coordinate = self.coordinate(branch_id)?;
            let Some(mut record) = self.runtime.state.collaboration(branch_id).await? else {
                return Ok(());
            };
            if self.transition_active(&record) {
                return Err(Error::new(
                    ErrorCode::BranchBusy,
                    "branch collaboration is completing a transition",
                )
                .retryable());
            }
            return self.suspend(&mut record).await;
        }
        let branch = self.runtime.branch(branch_id).await?;
        if matches!(
            record.phase,
            CollaborationPhase::Writing | CollaborationPhase::WaitingForHandles
        ) && record.transition.is_none()
            && record.recovery.is_none()
            && branch.owner == self.runtime.location
            && branch.state == BranchState::Writable
        {
            return Ok(());
        }
        let _coordinate = self.coordinate(branch_id)?;
        let Some(mut record) = self.runtime.state.collaboration(branch_id).await? else {
            return Ok(());
        };
        if !record.settings.enabled {
            return Ok(());
        }
        if !self.automatic_enabled() && !self.transition_active(&record) {
            return self.suspend(&mut record).await;
        }
        if self
            .failed_authorities(record.workspace)
            .await?
            .contains(&record.branch)
            && record.transition.is_none()
            && record.recovery.is_none()
        {
            self.begin_recovery(&mut record).await?;
            return Err(Error::new(
                ErrorCode::RecoveryRequired,
                "working changes are being moved to a recovery branch",
            )
            .retryable());
        }
        if record.recovery.is_some() {
            return Err(Error::new(
                ErrorCode::RecoveryRequired,
                "working changes are being moved to a recovery branch",
            )
            .retryable());
        }
        if record.transition.is_some() {
            self.finish_transition(&mut record).await?;
        }
        if record.release.is_some() {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "branch is waiting to release write authority",
            )
            .retryable());
        }
        match self.acquire(&mut record, expected).await {
            Ok(()) => {
                self.activity
                    .lock()
                    .entry(branch_id)
                    .and_modify(|activity| {
                        activity.last_write = self.runtime.clock.monotonic_ms();
                    });
                Ok(())
            }
            Err(error) => {
                record.error = Some(error.clone());
                self.save_record(&record).await?;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl WriteAccess for CollaborationService {
    fn automatic_enabled(&self) -> bool {
        self.config.load().values.collaboration.enabled
            && self.revisions.trees.content.refs().is_some()
            && self.mounts.check_collaboration_support().is_ok()
    }

    async fn prepare_mount(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<()> {
        self.register_branch(actor, workspace, branch, Permission::Write)
            .await
    }

    async fn prepare_write(&self, mount: MountId, expected_binding_generation: u64) -> Result<()> {
        let binding = self
            .runtime
            .state
            .mount(mount)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "mount does not exist"))?;
        if binding.state != MountState::Ready || binding.generation != expected_binding_generation {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "mount is no longer ready at the requested generation",
            ));
        }
        if binding.access != AccessMode::ReadWrite || binding.revision.is_some() {
            return Err(Error::new(ErrorCode::ReadOnly, "mount is read-only"));
        }
        self.prepare_branch_access(&binding.principal, binding.branch, Permission::Write, None)
            .await
    }

    async fn prepare_branch_write(
        &self,
        actor: &Principal,
        guard: BranchGuard,
        permission: Permission,
    ) -> Result<BranchGuard> {
        let before = self.runtime.branch(guard.branch).await?;
        self.runtime
            .authorize(actor, before.workspace, Some(before.id), permission)
            .await?;
        if !self.automatic_enabled() || before.protected {
            crate::runtime::check_guard(&before, &guard)?;
            return Ok(guard);
        }
        // The lease may change while a command waits or retries; its content must not.
        crate::runtime::check_content_guard(&before, &guard)?;
        if guard.authority_epoch > before.authority_epoch {
            return Err(Error::new(
                ErrorCode::StaleAuthority,
                "branch authority is newer than the local view",
            ));
        }
        self.prepare_branch_access(actor, before.id, permission, Some(guard))
            .await?;
        let after = self.runtime.branch(before.id).await?;
        crate::runtime::check_content_guard(&after, &guard)?;
        Ok(BranchGuard {
            authority_epoch: after.authority_epoch,
            ..guard
        })
    }
}
