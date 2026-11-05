use agentdrive_model::*;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug)]
struct State {
    binding: MountBinding,
    open_handles: usize,
}

#[derive(Debug)]
pub(crate) struct NativeBinding {
    state: Mutex<State>,
    #[cfg(target_os = "macos")]
    idle: tokio::sync::Notify,
}

impl NativeBinding {
    pub fn new(binding: MountBinding) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                binding,
                open_handles: 0,
            }),
            #[cfg(target_os = "macos")]
            idle: tokio::sync::Notify::new(),
        })
    }

    pub fn snapshot(&self) -> MountBinding {
        self.state.lock().binding.clone()
    }

    pub fn context(&self, identity: Option<LocalIdentity>) -> FsContext {
        let state = self.state.lock();
        FsContext {
            mount: state.binding.id,
            generation: state.binding.generation,
            identity: identity.unwrap_or(state.binding.identity),
        }
    }

    pub fn open_handles(&self) -> usize {
        self.state.lock().open_handles
    }

    #[cfg(target_os = "macos")]
    pub async fn wait_until_idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.open_handles() == 0 {
                return;
            }
            idle.await;
        }
    }

    /// Retain through the native reply for explicit unmount and diagnostics.
    pub fn reserve(self: &Arc<Self>, generation: u64) -> Result<NativeHandle> {
        let mut state = self.state.lock();
        if state.binding.generation != generation {
            return Err(Error::new(ErrorCode::StaleBinding, "mount binding changed"));
        }
        state.open_handles = state.open_handles.checked_add(1).ok_or_else(|| {
            Error::new(ErrorCode::CapacityExceeded, "native handle count overflow")
        })?;
        Ok(NativeHandle {
            binding: self.clone(),
        })
    }

    pub fn rebind(&self, previous: &MountBinding, next: &MountBinding) -> Result<()> {
        let mut state = self.state.lock();
        let current = &state.binding;
        if current == next {
            return Ok(());
        }
        if current.id != previous.id || current.generation != previous.generation {
            return Err(Error::new(ErrorCode::StaleBinding, "mount binding changed"));
        }
        if current.id != next.id
            || current.workspace != next.workspace
            || current.path != next.path
            || current.principal != next.principal
            || current.identity != next.identity
            || current.access != next.access
            || current.durability != next.durability
            || next.state != MountState::Ready
            || next.generation <= current.generation
        {
            return Err(Error::invalid(
                "native rebind must preserve mount access and location",
            ));
        }
        state.binding = next.clone();
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NativeHandle {
    binding: Arc<NativeBinding>,
}

impl Drop for NativeHandle {
    fn drop(&mut self) {
        let mut state = self.binding.state.lock();
        state.open_handles -= 1;
        #[cfg(target_os = "macos")]
        if state.open_handles == 0 {
            self.binding.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> MountBinding {
        MountBinding {
            id: MountId::new(),
            workspace: WorkspaceId::new(),
            branch: BranchId::new(),
            revision: None,
            generation: 1,
            branch_generation: None,
            path: "/mount/project".into(),
            principal: "owner".to_owned().try_into().unwrap(),
            identity: LocalIdentity {
                uid: 1000,
                gid: 1000,
            },
            durability: Durability::Local,
            access: AccessMode::ReadWrite,
            state: MountState::Ready,
        }
    }

    #[test]
    fn live_rebind_preserves_old_context_and_rejects_old_generation_reservations() {
        let previous = binding();
        let state = NativeBinding::new(previous.clone());
        let old_context = state.context(None);
        let old = state.reserve(previous.generation).unwrap();
        let mut next = previous.clone();
        next.generation += 1;
        state.rebind(&previous, &next).unwrap();
        assert_eq!(state.context(None).generation, next.generation);
        assert_eq!(old_context.generation, previous.generation);
        assert_eq!(
            state.reserve(old_context.generation).unwrap_err().code,
            ErrorCode::StaleBinding
        );
        drop(old);
        assert_eq!(state.open_handles(), 0);
    }

    #[test]
    fn retrying_completed_rebind_preserves_new_handles() {
        let previous = binding();
        let state = NativeBinding::new(previous.clone());
        let mut next = previous.clone();
        next.branch = BranchId::new();
        next.generation += 1;
        state.rebind(&previous, &next).unwrap();
        let _opened = state.reserve(next.generation).unwrap();
        state.rebind(&previous, &next).unwrap();
        assert_eq!(state.open_handles(), 1);
        let mut later = next.clone();
        later.generation += 1;
        assert_eq!(
            state.rebind(&previous, &later).unwrap_err().code,
            ErrorCode::StaleBinding
        );
    }

    #[test]
    fn rebind_preserves_native_read_only_mode_and_mount_identity() {
        let mut previous = binding();
        previous.access = AccessMode::ReadOnly;
        let state = NativeBinding::new(previous.clone());
        let mut next = previous.clone();
        next.generation += 1;
        next.access = AccessMode::ReadWrite;
        assert_eq!(
            state.rebind(&previous, &next).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        next.access = AccessMode::ReadOnly;
        next.path = "/mount/other".into();
        assert_eq!(
            state.rebind(&previous, &next).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(state.snapshot(), previous);
    }
}
