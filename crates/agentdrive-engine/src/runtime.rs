use agentdrive_model::*;
use agentdrive_ports::*;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock};

/// Shared ordering for all services operating on one local database.
pub struct Runtime {
    pub state: Arc<dyn LocalStateStore>,
    pub clock: Arc<dyn Clock>,
    pub location: LocationId,
    pub(crate) objects: Arc<RwLock<()>>,
    pub(crate) lifecycle: AsyncMutex<()>,
    pub(crate) sandbox_pins: Mutex<BTreeSet<BranchId>>,
    pub(crate) stopped: Mutex<BTreeSet<BranchId>>,
    pub(crate) collaboration_blocked: Mutex<BTreeSet<BranchId>>,
    pub(crate) publishing_turns: Mutex<BTreeSet<BranchId>>,
    branches: Mutex<BTreeMap<BranchId, Weak<AsyncMutex<()>>>>,
    pub(crate) dirty: Mutex<BTreeMap<BranchId, DirtyTime>>,
    pub(crate) sync_progress: Mutex<BTreeMap<OperationId, Arc<Mutex<SyncProgress>>>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DirtyTime {
    pub first: u64,
    pub sequence: u64,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    pub async fn new(state: Arc<dyn LocalStateStore>, clock: Arc<dyn Clock>) -> Result<Arc<Self>> {
        let location = state.location().await?;
        Ok(Arc::new(Self {
            state,
            clock,
            location,
            objects: Arc::default(),
            lifecycle: AsyncMutex::new(()),
            sandbox_pins: Mutex::new(BTreeSet::new()),
            stopped: Mutex::new(BTreeSet::new()),
            collaboration_blocked: Mutex::new(BTreeSet::new()),
            publishing_turns: Mutex::new(BTreeSet::new()),
            branches: Mutex::new(BTreeMap::new()),
            dirty: Mutex::new(BTreeMap::new()),
            sync_progress: Mutex::new(BTreeMap::new()),
        }))
    }

    pub async fn lock_branch(&self, branch: BranchId) -> OwnedMutexGuard<()> {
        self.branch_gate(branch).lock_owned().await
    }

    pub(crate) fn try_lock_branch(&self, branch: BranchId) -> Option<OwnedMutexGuard<()>> {
        self.branch_gate(branch).try_lock_owned().ok()
    }

    fn branch_gate(&self, branch: BranchId) -> Arc<AsyncMutex<()>> {
        let mut branches = self.branches.lock();
        if let Some(gate) = branches.get(&branch).and_then(Weak::upgrade) {
            gate
        } else {
            branches.retain(|_, gate| gate.strong_count() != 0);
            let gate = Arc::new(AsyncMutex::new(()));
            branches.insert(branch, Arc::downgrade(&gate));
            gate
        }
    }

    pub async fn branch(&self, id: BranchId) -> Result<Branch> {
        let mut branch = self
            .state
            .branch(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "branch does not exist"))?;
        if self.stopped.lock().contains(&id) {
            branch.state = BranchState::Stopped;
        }
        Ok(branch)
    }

    pub(crate) async fn check_capacity(
        &self,
        branch: &Branch,
        old_size: u64,
        new_size: u64,
        additional_nodes: u64,
    ) -> Result<()> {
        let workspace = self
            .state
            .workspace(branch.workspace)
            .await?
            .ok_or_else(|| Error::integrity("workspace is missing"))?;
        let usage = self.state.usage(branch.id).await?;
        if new_size > workspace.max_file_bytes
            || usage.inodes.saturating_add(additional_nodes) > workspace.max_inodes
        {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "workspace file or inode limit exceeded",
            ));
        }
        let next = usage
            .bytes
            .checked_sub(old_size)
            .and_then(|total| total.checked_add(new_size))
            .ok_or_else(|| Error::integrity("invalid working tree size"))?;
        if next > workspace.max_working_bytes {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "workspace size limit exceeded",
            ));
        }
        Ok(())
    }

    pub(crate) fn check_sandbox(&self, branch: BranchId) -> Result<()> {
        if self.sandbox_pins.lock().contains(&branch) {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "Stop the branch Sandbox before changing its binding",
            ));
        }
        Ok(())
    }

    pub(crate) fn check_mutation(&self, branch: BranchId) -> Result<()> {
        if self.collaboration_blocked.lock().contains(&branch)
            || self.publishing_turns.lock().contains(&branch)
        {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "branch is completing a collaboration transition",
            )
            .retryable());
        }
        Ok(())
    }

    pub async fn authorize(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: Option<BranchId>,
        permission: Permission,
    ) -> Result<Workspace> {
        let value = self
            .state
            .workspace(workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        if let Some(branch) = branch
            && self.branch(branch).await?.workspace != workspace
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "branch belongs to another workspace",
            ));
        }
        value.authorize(actor, branch, permission)?;
        Ok(value)
    }

    pub(crate) fn mark_dirty(&self, branch: &Branch) {
        let now = self.clock.monotonic_ms();
        let mut dirty = self.dirty.lock();
        let value = dirty.entry(branch.id).or_insert(DirtyTime {
            first: now,
            sequence: branch.mutation_seq,
        });
        value.sequence = branch.mutation_seq;
    }

    pub(crate) fn clear_dirty(&self, branch: BranchId, sequence: u64) {
        let mut dirty = self.dirty.lock();
        if dirty
            .get(&branch)
            .is_some_and(|value| value.sequence <= sequence)
        {
            dirty.remove(&branch);
        }
    }
}

pub(crate) fn check_guard(branch: &Branch, guard: &BranchGuard) -> Result<()> {
    if branch.id != guard.branch || branch.authority_epoch != guard.authority_epoch {
        return Err(Error::new(
            ErrorCode::StaleAuthority,
            "branch authority changed",
        ));
    }
    check_content_guard(branch, guard)
}

pub(crate) fn check_content_guard(branch: &Branch, guard: &BranchGuard) -> Result<()> {
    if branch.id != guard.branch
        || branch.generation != guard.generation
        || branch.formal_head != guard.expected_head
        || guard
            .mutation_seq
            .is_some_and(|sequence| sequence != branch.mutation_seq)
    {
        return Err(Error::new(ErrorCode::TargetMoved, "branch changed"));
    }
    Ok(())
}

pub(crate) fn increment(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(|| Error::new(ErrorCode::CapacityExceeded, "persistent counter exhausted"))
}
