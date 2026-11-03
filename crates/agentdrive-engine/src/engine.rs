use crate::*;
use agentdrive_config::LiveConfig;
use agentdrive_model::*;
use agentdrive_ports::*;
use std::sync::Arc;

pub struct Adapters {
    pub state: Arc<dyn LocalStateStore>,
    pub objects: Arc<dyn LocalObjectStore>,
    pub working: Arc<dyn WorkingTreeStore>,
    pub remote_objects: Option<Arc<dyn RemoteObjectStore>>,
    pub remote_refs: Option<Arc<dyn RemoteRefStore>>,
    pub clock: Arc<dyn Clock>,
    pub mounts: Arc<dyn MountDriver>,
    pub directories: Arc<dyn DirectoryAccess>,
}

impl std::fmt::Debug for Adapters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapters")
            .field("mounts", &self.mounts.capability())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub live: Arc<LiveConfig>,
    pub identity: LocalIdentity,
}

#[derive(Debug)]
pub struct Engine {
    workspaces: Arc<WorkspaceService>,
    pub services: Services,
    pub runtime: Arc<Runtime>,
    pub(crate) revisions: Arc<RevisionService>,
    pub(crate) publisher: Arc<ReplicaService>,
    pub(crate) collaboration: Arc<CollaborationService>,
}

impl Engine {
    pub async fn open(adapters: Adapters, config: EngineConfig) -> Result<Arc<Self>> {
        if adapters.remote_objects.is_some() != adapters.remote_refs.is_some() {
            return Err(Error::invalid(
                "remote object and reference stores must be configured together",
            ));
        }
        let runtime = Runtime::new(adapters.state, adapters.clock).await?;
        let operations = OperationService::new(runtime.clone());
        let content = ContentService::new(
            adapters.objects.clone(),
            adapters.remote_objects,
            adapters.remote_refs,
        );
        let trees = TreeService::new(runtime.clone(), content.clone());
        let snapshots = SnapshotService::new(
            runtime.clone(),
            trees.clone(),
            trees.clone(),
            adapters.objects.clone(),
            adapters.working.clone(),
        );
        snapshots.recover().await?;
        let publisher = ReplicaService::new(
            runtime.clone(),
            trees.clone(),
            content.clone(),
            operations.clone(),
        );
        let revisions = RevisionService::new(
            runtime.clone(),
            trees.clone(),
            operations.clone(),
            publisher.clone(),
            snapshots,
        );
        let branch_files = crate::branch_files::BranchFileService::new(
            runtime.clone(),
            content.clone(),
            adapters.working.clone(),
        );
        let files = FileService::new(
            runtime.clone(),
            trees.clone(),
            content.clone(),
            adapters.objects.clone(),
            adapters.working,
            revisions.clone(),
        );
        let mounts = MountService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            files.clone(),
            adapters.mounts.clone(),
            config.identity,
        );
        let queries =
            crate::tree_queries::IndexedTreeQuery::new(content.clone(), runtime.state.clone());
        let versions = VersionService::new(revisions.clone(), mounts.clone(), queries);
        let cache = CacheService::new(
            runtime.clone(),
            trees.clone(),
            trees,
            content,
            adapters.objects,
            operations.clone(),
            config.live.clone(),
        );
        let merge = MergeService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            mounts.clone(),
        );
        let workspace = WorkspaceService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            adapters.directories,
            mounts.clone(),
            adapters.mounts,
            config.live.clone(),
        );
        let ownership = OwnershipService::new(
            runtime.clone(),
            revisions.clone(),
            publisher.clone(),
            operations.clone(),
        );
        let collaboration = CollaborationService::new(
            runtime.clone(),
            revisions.clone(),
            publisher.clone(),
            operations.clone(),
            mounts.clone(),
            config.live.clone(),
        );
        let write_access: Arc<dyn WriteAccess> = collaboration.clone();
        files.set_write_access(Arc::downgrade(&write_access));
        mounts.set_write_access(Arc::downgrade(&write_access));
        versions.set_write_access(Arc::downgrade(&write_access));
        collaboration.initialize().await?;
        let services = Services {
            collaboration: collaboration.clone(),
            workspace: workspace.clone(),
            revisions: versions,
            replica: publisher.clone(),
            cache,
            ownership,
            merge,
            merge_history: crate::merge_history::MergeHistoryService::new(runtime.clone()),
            mounts,
            operations,
            files,
            branch_files,
        };
        Ok(Arc::new(Self {
            workspaces: workspace,
            services,
            runtime,
            revisions,
            publisher,
            collaboration,
        }))
    }

    pub async fn collaborate_due(&self) -> Result<()> {
        self.collaboration.tick().await
    }

    /// Run independently of uploads so network delays cannot delay local autosaves.
    pub async fn autosave_due(&self) -> Result<usize> {
        let now = self.runtime.clock.monotonic_ms();
        let due: Vec<_> = self
            .runtime
            .dirty
            .lock()
            .iter()
            .filter(|(_, time)| {
                now.saturating_sub(time.last) >= 5_000 || now.saturating_sub(time.first) >= 30_000
            })
            .map(|(id, _)| *id)
            .collect();
        let mut saved = 0;
        for id in due {
            let branch = self.runtime.branch(id).await?;
            if branch.owner == self.runtime.location
                && branch.state == BranchState::Writable
                && !branch.protected
            {
                let workspace = self
                    .runtime
                    .state
                    .workspace(branch.workspace)
                    .await?
                    .ok_or_else(|| Error::integrity("workspace is missing"))?;
                if workspace.deleted {
                    continue;
                }
                self.revisions.autosave(id, workspace.owner).await?;
                saved += 1;
            }
        }
        Ok(saved)
    }

    pub async fn sync_pending(&self) -> Result<()> {
        if self.revisions.trees.content.remote().is_none() {
            return Ok(());
        }
        let mut first_error = None;
        for workspace in self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|w| !w.deleted)
        {
            if (!self.runtime.state.sync_jobs(workspace.id).await?.is_empty()
                || !self
                    .runtime
                    .state
                    .pending_merge_history(workspace.id)
                    .await?
                    .is_empty())
                && let Err(error) = self.publisher.drain(workspace.id, None).await
            {
                if error.code == ErrorCode::StaleAuthority
                    && let Err(recovery) = self.collaboration.publication_failed(workspace.id).await
                {
                    tracing::warn!(workspace = %workspace.id, %recovery, "collaboration recovery will be retried");
                }
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// 更新远端工作区目录，并处理 Dashboard 已请求的分支历史刷新。
    pub async fn refresh_requested_history(&self) {
        tokio::join!(
            self.workspaces.refresh_catalog_due(),
            self.publisher.refresh_requested_history()
        );
    }

    pub async fn flush_local(&self) -> Result<()> {
        for workspace in self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|w| !w.deleted)
        {
            for branch in self.runtime.state.branches(workspace.id).await? {
                if branch.owner == self.runtime.location
                    && branch.state == BranchState::Writable
                    && branch.mutation_seq != branch.saved_seq
                {
                    self.revisions
                        .flush(branch.id, workspace.owner.clone(), Durability::Local)
                        .await?;
                }
            }
        }
        Ok(())
    }
}
