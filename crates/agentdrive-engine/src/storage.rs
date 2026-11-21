use crate::{ContentService, Engine, RemoteStorage, StorageRoutes, replica::decode_branch_ref};
use agentdrive_model::*;
use agentdrive_ports::*;
use std::{collections::BTreeMap, sync::Arc};

impl Engine {
    /// Replace credentials for the same remote namespace, as verified by the configuration adapter.
    pub async fn refresh_storage_connection(
        &self,
        next: RemoteStorage,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let _publication = self.publisher.publication.write().await;
        let content = &self.revisions.trees.content;
        let mut routes = (*content.storage.load_full()).clone();
        if routes.default.is_none() {
            return Err(Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            ));
        }
        persist()?;
        routes.default = Some(Arc::new(next));
        content.storage.store(Arc::new(routes));
        Ok(())
    }

    /// 准备全部历史内容后保存配置，并在持有保护锁期间更换远程适配器。
    pub async fn switch_storage(
        &self,
        next: Option<RemoteStorage>,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let previous = self.storage_routes();
        let routes = StorageRoutes::new(next.map(Arc::new), previous.workspaces.clone());
        self.apply_storage_routes(routes, |id| !previous.workspaces.contains_key(&id), persist)
            .await
    }

    /// Migrate retained objects and branch references for changed namespaces, then publish the complete route table.
    pub async fn switch_storage_routes(
        &self,
        next: StorageRoutes,
        affected: impl Fn(WorkspaceId) -> bool + Send + Sync,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        self.apply_storage_routes(next, affected, persist).await
    }

    async fn apply_storage_routes(
        &self,
        next: StorageRoutes,
        affected: impl Fn(WorkspaceId) -> bool + Send + Sync,
        persist: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let workspaces: Vec<_> = self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|w| !w.deleted && affected(w.id))
            .collect();
        for workspace in &workspaces {
            if self
                .runtime
                .state
                .collaborations(workspace.id)
                .await?
                .iter()
                .any(|record| {
                    (self.collaboration.automatic_enabled_for(workspace.id)
                        && record.settings.enabled)
                        || record.transition.is_some()
                        || record.recovery.is_some()
                        || record.release.is_some()
                })
            {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "disable automatic collaboration and complete pending handoffs before changing remote storage",
                ));
            }
        }
        let _publication = self.publisher.publication.write().await;
        let _objects = self.runtime.objects.write().await;
        let trees = &self.revisions.trees;
        let content = &trees.content;
        let mut retained = Vec::new();
        let mut sources = Vec::new();
        for workspace in workspaces {
            if self
                .runtime
                .state
                .operations(workspace.id)
                .await?
                .iter()
                .any(|record| matches!(record.pending, Some(PendingAction::Transfer { .. })))
            {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "complete the pending ownership transfer before changing storage",
                ));
            }
            let mut roots: Vec<_> = self
                .runtime
                .state
                .revisions(workspace.id)
                .await?
                .into_iter()
                .map(|revision| revision.root)
                .collect();
            roots.extend(
                self.runtime
                    .state
                    .branches(workspace.id)
                    .await?
                    .into_iter()
                    .map(|branch| branch.working_root),
            );
            let observed = self.runtime.state.remote_refs(workspace.id).await?;
            let jobs = self.runtime.state.sync_jobs(workspace.id).await?;
            let known_heads = observed.iter().chain(jobs.iter().map(|job| &job.reference));
            if let Some(source) = content.storage.load().for_workspace(workspace.id) {
                let heads = source_heads(&source, workspace.id).await?;
                let mut missing: std::collections::BTreeSet<_> = heads.keys().copied().collect();
                for reference in known_heads.clone() {
                    if heads.get(&reference.branch) == Some(reference) {
                        missing.remove(&reference.branch);
                    }
                }
                if !missing.is_empty() {
                    return Err(source_changed(workspace.id));
                }
                sources.push((workspace.id, source, heads));
            }
            let mut references = BTreeMap::<BranchId, BranchRef>::new();
            for reference in known_heads {
                roots.extend([
                    reference.working_root.clone(),
                    reference.record_index.clone(),
                ]);
                let selected = references
                    .entry(reference.branch)
                    .or_insert_with(|| reference.clone());
                if (reference.authority_epoch, reference.source_generation)
                    > (selected.authority_epoch, selected.source_generation)
                {
                    *selected = reference.clone();
                }
            }
            for pin in self.runtime.state.cache_pins(workspace.id).await? {
                roots.extend(pin.objects);
            }
            let objects = trees.closure(workspace.id, roots.clone()).await?;
            for object in &objects {
                // install 会检查下载内容的大小与摘要。
                let _reader = content.open(workspace.id, object).await?;
            }
            retained.push((workspace, roots, objects, references));
        }
        let destination = ContentService::with_routes(content.local.clone(), next.clone());
        let pin: OperationId = self.runtime.location.to_string().parse()?;
        for (workspace, roots, objects, references) in &retained {
            if let Some(next) = next.for_workspace(workspace.id) {
                let initial = trees
                    .revision(workspace.id, workspace.initial_revision)
                    .await?;
                for object in trees
                    .closure(workspace.id, vec![initial.root.clone()])
                    .await?
                {
                    next.objects
                        .upload(
                            workspace.id,
                            content.local.open(workspace.id, &object).await?,
                        )
                        .await?;
                }
                publish_workspace(&next, workspace.clone(), initial).await?;
                destination
                    .protect_remote(workspace.id, pin, roots.clone())
                    .await?;
                let result: Result<()> = async {
                    for object in objects {
                        next.objects
                            .upload(
                                workspace.id,
                                content.local.open(workspace.id, object).await?,
                            )
                            .await?;
                    }
                    for reference in references.values() {
                        publish_branch(&next, reference, self.runtime.location).await?;
                    }
                    Ok(())
                }
                .await;
                let release = destination.release_remote(workspace.id, pin).await;
                result?;
                release?;
            }
        }
        // Another location can publish while the destination is receiving objects.
        for (workspace, source, original) in sources {
            if source_heads(&source, workspace).await? != original {
                return Err(source_changed(workspace));
            }
        }
        persist()?;
        let next = Arc::new(next);
        content.storage.store(next.clone());
        for (workspace, _, objects, _) in retained {
            if next.for_workspace(workspace.id).is_some() {
                for object in objects {
                    if let Err(error) = content.local.confirm_remote(workspace.id, &object).await {
                        tracing::warn!(%error, "storage applied; local cache confirmation will require a later upload");
                    }
                }
            }
        }
        Ok(())
    }
}

fn source_changed(workspace: WorkspaceId) -> Error {
    Error::new(
        ErrorCode::TargetMoved,
        format!("source history changed for workspace {workspace}. Pause writes on all locations, pull all branches, and retry the storage change"),
    ).retryable()
}

async fn source_heads(
    source: &RemoteStorage,
    workspace: WorkspaceId,
) -> Result<BTreeMap<BranchId, BranchRef>> {
    if let Some(control) = source.refs.get(&RefKey::Workspace(workspace)).await?
        && crate::replica::decode_workspace_control(workspace, &control.bytes)?
            .workspace
            .deleted
    {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("source workspace {workspace} was deleted. Refresh workspace discovery"),
        ));
    }
    let mut heads = BTreeMap::new();
    for branch in source.refs.list_branches(workspace).await? {
        let value = source
            .refs
            .get(&RefKey::Branch { workspace, branch })
            .await?
            .ok_or_else(|| source_changed(workspace))?;
        heads.insert(branch, decode_branch_ref(workspace, branch, &value.bytes)?);
    }
    Ok(heads)
}

async fn publish_workspace(
    storage: &RemoteStorage,
    workspace: Workspace,
    initial_revision: Revision,
) -> Result<()> {
    let key = RefKey::Workspace(workspace.id);
    for _ in 0..8 {
        if let Some(existing) = storage.refs.get(&key).await? {
            if existing.bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::integrity("workspace control exceeds format limit"));
            }
            let control: WorkspaceControl = decode(&existing.bytes)?;
            if control.workspace != workspace || control.initial_revision != initial_revision {
                return Err(Error::integrity(
                    "destination workspace identity or policy differs",
                ));
            }
            if control.workspace.deleted {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "workspace has been deleted",
                ));
            }
            if control.phase != GcPhase::Open {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "destination reclamation is active",
                ));
            }
            return Ok(());
        }
        let control = WorkspaceControl {
            workspace: workspace.clone(),
            initial_revision: initial_revision.clone(),
            gc_epoch: 0,
            phase: GcPhase::Open,
            active_operations: BTreeMap::new(),
            history_pins: BTreeMap::new(),
        };
        storage
            .refs
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: None,
                bytes: encode(&control)?.into(),
            })
            .await?;
    }
    Err(Error::new(
        ErrorCode::RemoteUnknown,
        "destination workspace publication remains unconfirmed",
    )
    .retryable())
}

async fn publish_branch(
    storage: &RemoteStorage,
    reference: &BranchRef,
    location: LocationId,
) -> Result<()> {
    let key = RefKey::Branch {
        workspace: reference.workspace,
        branch: reference.branch,
    };
    for _ in 0..8 {
        let current = storage.refs.get(&key).await?;
        if let Some(current) = &current {
            let remote = decode_branch_ref(reference.workspace, reference.branch, &current.bytes)?;
            if remote == *reference {
                return Ok(());
            }
            if reference.owner != location {
                return Err(Error::new(
                    ErrorCode::TargetMoved,
                    "destination contains a different head for a branch owned by another location. Reconcile it with its owner or select a fresh storage namespace",
                ));
            }
            if remote.owner != reference.owner
                || remote.authority_epoch != reference.authority_epoch
            {
                return Err(Error::new(
                    ErrorCode::StaleAuthority,
                    "destination branch ownership differs; synchronize before changing storage",
                ));
            }
            if remote.source_generation >= reference.source_generation {
                return Err(Error::new(
                    ErrorCode::TargetMoved,
                    "destination branch is ahead; synchronize before changing storage",
                ));
            }
        }
        storage
            .refs
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: current.map(|value| value.version),
                bytes: encode(reference)?.into(),
            })
            .await?;
    }
    Err(Error::new(
        ErrorCode::RemoteUnknown,
        "destination branch publication remains unconfirmed",
    )
    .retryable())
}
