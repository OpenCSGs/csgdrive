use super::*;

impl WorkspaceService {
    pub(crate) async fn refresh_catalog_due(&self) {
        let Ok(_catalog) = self.catalog_gate.try_lock() else {
            return;
        };
        let now = self.runtime.clock.monotonic_ms();
        if self
            .catalog_checked
            .lock()
            .is_some_and(|last| now.saturating_sub(last) < 2_000)
        {
            return;
        }
        *self.catalog_checked.lock() = Some(now);
        let actor = self.catalog_actor.lock().clone();
        if let Err(error) = self.refresh_catalog(actor.as_ref()).await {
            tracing::warn!(%error, "workspace catalog refresh failed");
        }
    }

    pub(super) async fn refresh_catalog(&self, actor: Option<&Principal>) -> Result<()> {
        let locals = self.runtime.state.workspaces().await?;
        for workspace in locals.iter().filter(|w| w.deleted) {
            self.release_deleted_mounts(workspace).await?;
        }
        let routes = self.revisions.trees.content.storage.load_full();
        let mut ids: std::collections::BTreeSet<_> = locals.iter().map(|w| w.id).collect();
        let mut failure = None;
        if actor.is_some() {
            ids.extend(routes.workspace_ids());
            if let Some(default) = &routes.default {
                match default.refs.list_workspaces().await {
                    Ok(discovered) => ids.extend(discovered),
                    Err(error) => failure = Some(error),
                }
            }
        }
        for workspace in locals.iter().filter(|w| w.deleted) {
            ids.remove(&workspace.id);
        }
        for id in ids {
            if let Some(storage) = routes.for_workspace(id)
                && let Err(error) = self.refresh_catalog_workspace(id, &storage, actor).await
            {
                failure.get_or_insert(error);
            }
        }
        *self.catalog_checked.lock() = Some(self.runtime.clock.monotonic_ms());
        failure.map_or(Ok(()), Err)
    }

    async fn refresh_catalog_workspace(
        &self,
        id: WorkspaceId,
        storage: &Arc<crate::RemoteStorage>,
        actor: Option<&Principal>,
    ) -> Result<()> {
        let Some(value) = storage.refs.get(&RefKey::Workspace(id)).await? else {
            return Ok(());
        };
        let control = crate::replica::decode_workspace_control(id, &value.bytes)?;
        let local = self.runtime.state.workspace(id).await?;
        if control.workspace.deleted {
            if let Some(local) = local {
                self.record_deletion(local.clone(), storage).await?;
                self.release_deleted_mounts(&local).await?;
            }
        } else if local.is_none()
            && actor.is_some_and(|actor| {
                control
                    .workspace
                    .authorize(actor, None, Permission::Read)
                    .is_ok()
            })
        {
            self.cache_discovered_workspace(control, storage).await?;
        }
        Ok(())
    }

    async fn cache_discovered_workspace(
        &self,
        control: WorkspaceControl,
        storage: &Arc<crate::RemoteStorage>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        self.check_catalog_storage(control.workspace.id, storage)?;
        let _objects = self.runtime.objects.read().await;
        let id = control.workspace.id;
        self.revisions.trees.content.storage.load().check_target(
            control.workspace.id,
            control.workspace.storage_target.as_deref(),
        )?;
        self.revisions
            .trees
            .content
            .metadata(id, &control.initial_revision.root)
            .await?;
        let result = self
            .runtime
            .state
            .commit(LocalCommit {
                new_workspaces: vec![control.workspace],
                revisions: vec![control.initial_revision],
                ..Default::default()
            })
            .await;
        if let Err(error) = result
            && (error.code != ErrorCode::Conflict
                || self.runtime.state.workspace(id).await?.is_none())
        {
            return Err(error);
        }
        Ok(())
    }

    fn check_catalog_storage(
        &self,
        workspace: WorkspaceId,
        expected: &Arc<crate::RemoteStorage>,
    ) -> Result<()> {
        if self
            .revisions
            .trees
            .content
            .storage
            .load()
            .for_workspace(workspace)
            .is_none_or(|current| !Arc::ptr_eq(&current, expected))
        {
            return Err(Error::new(
                ErrorCode::TargetMoved,
                "remote storage changed during discovery",
            )
            .retryable());
        }
        Ok(())
    }

    async fn record_deletion(
        &self,
        workspace: Workspace,
        storage: &Arc<crate::RemoteStorage>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        self.check_catalog_storage(workspace.id, storage)?;
        self.commit_deletion(workspace).await
    }

    async fn commit_deletion(&self, mut workspace: Workspace) -> Result<()> {
        let branches = self.runtime.state.branches(workspace.id).await?;
        let mut guards = Vec::new();
        for branch in &branches {
            guards.push(self.runtime.lock_branch(branch.id).await);
        }
        workspace.deleted = true;
        self.runtime
            .state
            .commit(LocalCommit {
                workspaces: vec![workspace],
                ..Default::default()
            })
            .await?;
        for branch in branches {
            self.runtime.dirty.lock().remove(&branch.id);
            self.runtime.collaboration_blocked.lock().remove(&branch.id);
        }
        Ok(())
    }

    async fn release_deleted_mounts(&self, workspace: &Workspace) -> Result<()> {
        for mount in self
            .runtime
            .state
            .mounts()
            .await?
            .into_iter()
            .filter(|m| m.workspace == workspace.id && m.state != MountState::Released)
        {
            let record = self
                .mounts
                .unmount(
                    RequestContext {
                        principal: workspace.owner.clone(),
                        request_id: format!(
                            "workspace-delete-unmount-{}-{}",
                            mount.id, mount.generation
                        ),
                    },
                    mount.id,
                    mount.generation,
                )
                .await?;
            if record.pending.is_some() {
                let result = self
                    .mounts
                    .release(
                        RequestContext {
                            principal: workspace.owner.clone(),
                            request_id: format!(
                                "workspace-delete-release-{}-{}",
                                mount.id, mount.generation
                            ),
                        },
                        ReleaseMount {
                            operation: record.id,
                            expected_binding_generation: mount.generation,
                        },
                    )
                    .await?;
                if let Some(error) = result.error {
                    tracing::warn!(workspace = %workspace.id, mount = %mount.id, %error, "deleted workspace mount is waiting for release");
                }
            }
        }
        Ok(())
    }

    pub(super) async fn delete_workspace(
        &self,
        context: RequestContext,
        id: WorkspaceId,
    ) -> Result<OperationRecord> {
        let workspace = self
            .runtime
            .authorize(&context.principal, id, None, Permission::Manage)
            .await?;
        let operation = match self
            .operations
            .start(context, "workspace_delete", &id, Some(id), None)
            .await?
        {
            OperationStart::Existing(record) if record.local_saved => return Ok(record),
            OperationStart::Existing(record) => OperationContext { operation: record },
            OperationStart::New(context) => context,
        };
        let result = async {
            // Serialize with mount creation and storage changes. No file mutation lock is held over remote I/O.
            let _lifecycle = self.runtime.lifecycle.lock().await;
            if self
                .runtime
                .state
                .mounts()
                .await?
                .iter()
                .any(|m| m.workspace == id && m.state != MountState::Released)
            {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "请先在挂载点管理中卸载此工作区的本机目录，再删除工作区",
                ));
            }
            if (self.revisions.trees.content.refs(id).is_some()
                && !self.runtime.state.sync_jobs(id).await?.is_empty())
                || self
                    .runtime
                    .state
                    .branches(id)
                    .await?
                    .iter()
                    .any(|b| b.mutation_seq != b.saved_seq)
                || self
                    .runtime
                    .state
                    .operations(id)
                    .await?
                    .iter()
                    .any(|o| o.pending.is_some())
            {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "工作区仍有未保存修改、同步任务或待完成操作，请完成后重试",
                )
                .retryable());
            }
            self.delete_remote(&workspace, &operation.operation.principal)
                .await?;
            self.commit_deletion(workspace.clone()).await?;
            let mut completed = operation.clone();
            completed.operation.remote_confirmed = self.revisions.trees.content.refs(id).is_some();
            self.operations
                .finish(completed, CommandResult::Updated, LocalCommit::default())
                .await
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn delete_remote(&self, workspace: &Workspace, actor: &Principal) -> Result<()> {
        let Some(refs) = self.revisions.trees.content.refs(workspace.id) else {
            return Ok(());
        };
        let key = RefKey::Workspace(workspace.id);
        for _ in 0..16 {
            let value = refs.get(&key).await?;
            let mut control = if let Some(value) = &value {
                crate::replica::decode_workspace_control(workspace.id, &value.bytes)?
            } else {
                WorkspaceControl {
                    workspace: workspace.clone(),
                    initial_revision: self
                        .runtime
                        .state
                        .revision(workspace.initial_revision)
                        .await?
                        .ok_or_else(|| Error::integrity("initial revision is missing"))?,
                    gc_epoch: 0,
                    phase: GcPhase::Open,
                    active_operations: Default::default(),
                    history_pins: Default::default(),
                }
            };
            control
                .workspace
                .authorize(actor, None, Permission::Manage)?;
            if control.workspace.deleted {
                return Ok(());
            }
            if !control.active_operations.is_empty()
                || !control.history_pins.is_empty()
                || control.phase != GcPhase::Open
            {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "远端仍有正在发布或保留的操作，请完成后重试删除",
                )
                .retryable());
            }
            control.workspace.deleted = true;
            // The publication barrier also prevents earlier clients from publishing into this workspace.
            control.phase = GcPhase::Closing;
            match refs
                .compare_exchange(RefUpdate {
                    key: key.clone(),
                    expected_version: value.map(|v| v.version),
                    bytes: encode(&control)?.into(),
                })
                .await?
            {
                CasOutcome::Applied { .. } => return Ok(()),
                CasOutcome::Unknown | CasOutcome::Conflict => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "workspace deletion remains unconfirmed",
        )
        .retryable())
    }
}
