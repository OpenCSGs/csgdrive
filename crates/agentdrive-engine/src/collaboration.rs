mod authority;
mod events;
mod lifecycle;
mod recovery;
mod write_access;

use crate::{MountService, OperationService, ReplicaService, RevisionService, Runtime};
use agentdrive_config::LiveConfig;
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub struct CollaborationService {
    runtime: Arc<Runtime>,
    revisions: Arc<RevisionService>,
    publisher: Arc<ReplicaService>,
    operations: Arc<OperationService>,
    mounts: Arc<MountService>,
    config: Arc<LiveConfig>,
    busy: Arc<Mutex<BTreeSet<BranchId>>>,
    activity: Mutex<BTreeMap<BranchId, Activity>>,
}

#[derive(Clone, Copy)]
struct Activity {
    sequence: u64,
    last_write: u64,
    last_refresh: u64,
}

struct CoordinatorGuard {
    busy: Arc<Mutex<BTreeSet<BranchId>>>,
    branch: BranchId,
}
impl Drop for CoordinatorGuard {
    fn drop(&mut self) {
        self.busy.lock().remove(&self.branch);
    }
}

impl std::fmt::Debug for CollaborationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollaborationService")
            .finish_non_exhaustive()
    }
}

impl CollaborationService {
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        revisions: Arc<RevisionService>,
        publisher: Arc<ReplicaService>,
        operations: Arc<OperationService>,
        mounts: Arc<MountService>,
        config: Arc<LiveConfig>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            revisions,
            publisher,
            operations,
            mounts,
            config,
            busy: Arc::new(Mutex::new(BTreeSet::new())),
            activity: Mutex::new(BTreeMap::new()),
        })
    }

    fn coordinate(&self, branch: BranchId) -> Result<CoordinatorGuard> {
        if !self.busy.lock().insert(branch) {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "branch collaboration is already progressing",
            )
            .retryable());
        }
        Ok(CoordinatorGuard {
            busy: self.busy.clone(),
            branch,
        })
    }

    fn new_record(
        &self,
        branch: &Branch,
        actor: Principal,
        settings: CollaborationSettings,
    ) -> CollaborationRecord {
        CollaborationRecord {
            workspace: branch.workspace,
            branch: branch.id,
            actor,
            phase: if settings.enabled {
                CollaborationPhase::Following
            } else {
                CollaborationPhase::Disabled
            },
            settings,
            executions: Vec::new(),
            release: None,
            transition: None,
            recovery: None,
            error: None,
        }
    }

    async fn save_record(&self, record: &CollaborationRecord) -> Result<()> {
        self.runtime
            .state
            .commit(LocalCommit {
                collaborations: vec![record.clone()],
                ..Default::default()
            })
            .await
    }

    async fn target(
        &self,
        actor: &Principal,
        mount: MountId,
        permission: Permission,
    ) -> Result<MountBinding> {
        let binding = self
            .runtime
            .state
            .mount(mount)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "mount does not exist"))?;
        if binding.state == MountState::Released {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "mount has been released",
            ));
        }
        self.runtime
            .authorize(actor, binding.workspace, Some(binding.branch), permission)
            .await?;
        Ok(binding)
    }

    async fn branch_mounts(&self, branch: BranchId) -> Result<Vec<MountBinding>> {
        Ok(self
            .runtime
            .state
            .mounts()
            .await?
            .into_iter()
            .filter(|mount| mount.branch == branch && mount.state != MountState::Released)
            .collect())
    }

    async fn describe(&self, record: &CollaborationRecord) -> Result<CollaborationStatus> {
        let branch = self.runtime.branch(record.branch).await?;
        let mounts = self.branch_mounts(branch.id).await?;
        let pending = self
            .runtime
            .state
            .sync_jobs(branch.workspace)
            .await?
            .iter()
            .any(|job| job.reference.branch == branch.id);
        let observed = self
            .runtime
            .state
            .remote_refs(branch.workspace)
            .await?
            .into_iter()
            .find(|reference| reference.branch == branch.id);
        let synchronization = BranchSynchronization::new(
            &branch,
            observed.as_ref(),
            self.revisions.trees.content.refs().is_some(),
        );
        let remote_confirmed = !pending
            && record.recovery.is_none()
            && record.transition.is_none()
            && synchronization.remote_confirmed;
        let (open_handles, handle_error) =
            match self.mounts.collaboration_open_handles(branch.id).await {
                Ok(count) => (Some(count), None),
                Err(error) => (None, Some(error)),
            };
        let mut settings = record.settings.clone();
        settings.enabled = settings.enabled && self.automatic_enabled();
        let phase = if !settings.enabled && !self.transition_active(record) {
            CollaborationPhase::Disabled
        } else {
            record.phase
        };
        Ok(CollaborationStatus {
            workspace: branch.workspace,
            branch: branch.id,
            settings,
            phase,
            writer: if record.recovery.is_some()
                || record.transition.is_some()
                || branch.state == BranchState::Available
            {
                None
            } else if branch.state == BranchState::Writable {
                Some(branch.owner)
            } else {
                observed
                    .as_ref()
                    .filter(|reference| {
                        !reference.write_stopped
                            && !reference.write_available
                            && reference.authority_epoch >= branch.authority_epoch
                    })
                    .map(|reference| reference.owner)
            },
            authority_epoch: branch.authority_epoch,
            mounts: mounts.into_iter().map(|mount| mount.id).collect(),
            open_handles,
            active_executions: record.executions.len(),
            local_saved: synchronization.local_saved,
            remote_confirmed,
            recovery_branch: record
                .recovery
                .as_ref()
                .and_then(|recovery| recovery.branch),
            error: record.error.clone().or(handle_error).or_else(|| {
                (record.settings.enabled && self.config.load().values.collaboration.enabled)
                    .then(|| self.mounts.check_collaboration_support().err())
                    .flatten()
            }),
        })
    }

    /// 在服务开始接收文件请求之前恢复持久记录中的冻结状态。
    pub async fn initialize(&self) -> Result<()> {
        for workspace in self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|w| !w.deleted)
        {
            let failed = self.failed_authorities(workspace.id).await?;
            for record in self.runtime.state.collaborations(workspace.id).await? {
                if self.transition_active(&record)
                    || (record.settings.enabled && failed.contains(&record.branch))
                {
                    self.runtime
                        .collaboration_blocked
                        .lock()
                        .insert(record.branch);
                    if let Some(target) = record
                        .recovery
                        .as_ref()
                        .and_then(|recovery| recovery.branch)
                    {
                        self.runtime.collaboration_blocked.lock().insert(target);
                    }
                }
            }
        }
        self.register_mounted_branches().await
    }

    async fn failed_authorities(&self, workspace: WorkspaceId) -> Result<BTreeSet<BranchId>> {
        Ok(self
            .runtime
            .state
            .sync_jobs(workspace)
            .await?
            .into_iter()
            .filter(|job| {
                job.error
                    .as_ref()
                    .is_some_and(|error| error.code == ErrorCode::StaleAuthority)
            })
            .map(|job| job.reference.branch)
            .collect())
    }

    pub async fn publication_failed(&self, workspace: WorkspaceId) -> Result<()> {
        let failed = self.failed_authorities(workspace).await?;
        for candidate in self.runtime.state.collaborations(workspace).await? {
            if !failed.contains(&candidate.branch) {
                continue;
            }
            let Ok(_coordinate) = self.coordinate(candidate.branch) else {
                continue;
            };
            let Some(mut record) = self.runtime.state.collaboration(candidate.branch).await? else {
                continue;
            };
            if !record.settings.enabled
                || !self
                    .failed_authorities(workspace)
                    .await?
                    .contains(&record.branch)
            {
                continue;
            }
            self.runtime
                .collaboration_blocked
                .lock()
                .insert(record.branch);
            if record.transition.is_some() {
                self.finish_transition(&mut record).await?;
            } else {
                self.begin_recovery(&mut record).await?;
            }
        }
        Ok(())
    }

    pub async fn tick(&self) -> Result<()> {
        self.register_mounted_branches().await?;
        let mut first_error = None;
        for workspace in self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|w| !w.deleted)
        {
            for candidate in self.runtime.state.collaborations(workspace.id).await? {
                let Ok(_coordinate) = self.coordinate(candidate.branch) else {
                    continue;
                };
                let Some(mut record) = self.runtime.state.collaboration(candidate.branch).await?
                else {
                    continue;
                };
                if !record.settings.enabled {
                    continue;
                }
                if let Err(error) = self.advance(&mut record).await {
                    record.error = Some(error.clone());
                    self.save_record(&record).await?;
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn advance(&self, record: &mut CollaborationRecord) -> Result<()> {
        if self.transition_active(record) {
            self.mounts.check_collaboration_support()?;
        }
        if record.recovery.is_some() {
            return self.recover_branch(record).await;
        }
        if record.transition.is_some() {
            return self.finish_transition(record).await;
        }
        let branch = self.runtime.branch(record.branch).await?;
        if branch.state == BranchState::Stopped
            && self
                .failed_authorities(record.workspace)
                .await?
                .contains(&record.branch)
        {
            return self.begin_recovery(record).await;
        }
        if !self.automatic_enabled() {
            if record.phase == CollaborationPhase::Switching {
                return self.resume_view_switch(record).await;
            }
            if self.transition_active(record) && record.release.is_some() {
                return self.release_authority(record).await;
            }
            return self.suspend(record).await;
        }
        let now = self.runtime.clock.monotonic_ms();
        let activity = {
            let mut values = self.activity.lock();
            let value = values.entry(branch.id).or_insert(Activity {
                sequence: branch.mutation_seq,
                last_write: now,
                last_refresh: 0,
            });
            if value.sequence != branch.mutation_seq {
                value.sequence = branch.mutation_seq;
                value.last_write = now;
            }
            *value
        };
        if branch.owner == self.runtime.location && branch.state == BranchState::Writable {
            if let Some(release) = &record.release
                && (release.authority_epoch != branch.authority_epoch
                    || (release.reason == ReleaseReason::Idle
                        && release.mutation_seq != branch.mutation_seq))
            {
                record.release = None;
                record.phase = CollaborationPhase::Writing;
                self.save_record(record).await?;
            }
            if let Some(release) = &mut record.release
                && release.reason == ReleaseReason::ExecutionEnded
            {
                release.mutation_seq = branch.mutation_seq;
            }
            if record.release.is_none()
                && record.settings.idle_timeout_secs.is_some_and(|seconds| {
                    now.saturating_sub(activity.last_write) >= seconds.saturating_mul(1000)
                })
            {
                record.release = Some(CollaborationRelease {
                    operation: OperationId::new(),
                    authority_epoch: branch.authority_epoch,
                    mutation_seq: branch.mutation_seq,
                    reason: ReleaseReason::Idle,
                });
                self.save_record(record).await?;
            }
            if record.release.is_some() {
                return self.release_authority(record).await;
            }
            if now.saturating_sub(activity.last_refresh) >= 5_000
                || record.phase != CollaborationPhase::Writing
            {
                self.activity
                    .lock()
                    .entry(branch.id)
                    .and_modify(|value| value.last_refresh = now);
                self.check_writer(record, &branch).await?;
            }
        } else if now.saturating_sub(activity.last_refresh) >= 5_000
            || record.phase != CollaborationPhase::Following
        {
            self.activity
                .lock()
                .entry(branch.id)
                .and_modify(|value| value.last_refresh = now);
            self.follow(record).await?;
        }
        Ok(())
    }

    async fn check_writer(&self, record: &mut CollaborationRecord, branch: &Branch) -> Result<()> {
        let Some((reference, _)) = self.remote_branch(branch.workspace, branch.id).await? else {
            self.publisher
                .drain(branch.workspace, Some(branch.id))
                .await?;
            record.phase = CollaborationPhase::Writing;
            record.error = None;
            return self.save_record(record).await;
        };
        if reference.owner != self.runtime.location
            || reference.authority_epoch != branch.authority_epoch
            || reference.write_available
            || reference.write_stopped
        {
            return self.begin_recovery(record).await;
        }
        if record.phase != CollaborationPhase::Writing || record.error.is_some() {
            record.phase = CollaborationPhase::Writing;
            record.error = None;
            self.save_record(record).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl CollaborationApi for CollaborationService {
    async fn configure(
        &self,
        actor: &Principal,
        request: ConfigureCollaboration,
    ) -> Result<CollaborationStatus> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let global_enabled = self.config.load().values.collaboration.enabled;
        if request.enabled != global_enabled {
            return Err(Error::invalid(
                "automatic collaboration is controlled by daemon settings; update collaboration.enabled there",
            ));
        }
        let binding = self
            .target(actor, request.mount, Permission::Manage)
            .await?;
        if request
            .expected_binding_generation
            .is_some_and(|generation| generation != binding.generation)
        {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "mount changed before collaboration settings were applied",
            ));
        }
        if binding.access != AccessMode::ReadWrite || binding.revision.is_some() {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "automatic collaboration requires a writable branch mount",
            ));
        }
        if request.enabled {
            self.mounts.check_collaboration_support()?;
        }
        if request
            .idle_timeout_secs
            .is_some_and(|value| value == 0 || value > 86_400)
            || (request.idle_timeout_secs.is_none() && !request.release_on_event)
        {
            return Err(Error::invalid(
                "select an event release or an idle timeout between 1 and 86400 seconds",
            ));
        }
        if request.enabled && self.revisions.trees.content.refs().is_none() {
            return Err(Error::new(
                ErrorCode::RemoteRequired,
                "automatic collaboration requires remote storage",
            ));
        }
        let _coordinate = self.coordinate(binding.branch)?;
        let _gate = self.runtime.lock_branch(binding.branch).await;
        let branch = self.runtime.branch(binding.branch).await?;
        if branch.protected {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "protected branches cannot use automatic collaboration",
            ));
        }
        let settings = CollaborationSettings {
            enabled: global_enabled,
            idle_timeout_secs: request.idle_timeout_secs,
            release_on_event: request.release_on_event,
        };
        let mut record = self
            .runtime
            .state
            .collaboration(branch.id)
            .await?
            .unwrap_or_else(|| self.new_record(&branch, actor.clone(), settings.clone()));
        if record.transition.is_some()
            || record.recovery.is_some()
            || self
                .failed_authorities(branch.workspace)
                .await?
                .contains(&branch.id)
            || self
                .runtime
                .collaboration_blocked
                .lock()
                .contains(&branch.id)
        {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "finish the pending collaboration operation before changing settings",
            )
            .retryable());
        }
        record.settings = settings;
        record.actor = actor.clone();
        record.error = None;
        record.release = None;
        record.phase = if global_enabled {
            CollaborationPhase::Following
        } else {
            CollaborationPhase::Disabled
        };
        self.activity.lock().remove(&branch.id);
        self.save_record(&record).await?;
        self.describe(&record).await
    }

    async fn status(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
    ) -> Result<Vec<CollaborationStatus>> {
        self.runtime
            .authorize(actor, workspace, None, Permission::Read)
            .await?;
        let mut records: BTreeMap<_, _> = self
            .runtime
            .state
            .collaborations(workspace)
            .await?
            .into_iter()
            .map(|record| (record.branch, record))
            .collect();
        for mount in self.runtime.state.mounts().await? {
            if mount.workspace == workspace
                && mount.state != MountState::Released
                && !records.contains_key(&mount.branch)
            {
                let branch = self.runtime.branch(mount.branch).await?;
                records.insert(
                    branch.id,
                    self.new_record(&branch, actor.clone(), CollaborationSettings::default()),
                );
            }
        }
        let mut result = Vec::with_capacity(records.len());
        for record in records.into_values() {
            result.push(self.describe(&record).await?);
        }
        Ok(result)
    }

    async fn execution_event(
        &self,
        actor: &Principal,
        request: CollaborationEvent,
    ) -> Result<CollaborationStatus> {
        self.accept_event(actor, request).await
    }
}
