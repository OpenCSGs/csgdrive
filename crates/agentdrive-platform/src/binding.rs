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
}

impl NativeBinding {
    pub fn new(binding: MountBinding) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                binding,
                open_handles: 0,
            }),
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

    /// Reserve before entering the engine so rebind also observes incomplete opens.
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
        if state.open_handles != 0 {
            return Err(
                Error::new(ErrorCode::MountBusy, "native mount still has open handles").retryable(),
            );
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
        self.binding.state.lock().open_handles -= 1;
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
    fn every_native_handle_and_open_reservation_blocks_rebind() {
        let previous = binding();
        let state = NativeBinding::new(previous.clone());
        let caller = LocalIdentity {
            uid: 2000,
            gid: 2000,
        };
        let old_context = state.context(Some(caller));
        let file = state.reserve(previous.generation).unwrap();
        let directory = state.reserve(previous.generation).unwrap();
        let opening = state.reserve(previous.generation).unwrap();
        let mut next = previous.clone();
        next.branch = BranchId::new();
        next.generation += 1;
        assert_eq!(state.open_handles(), 3);
        assert_eq!(
            state.rebind(&previous, &next).unwrap_err().code,
            ErrorCode::MountBusy
        );
        drop(file);
        drop(directory);
        assert_eq!(state.open_handles(), 1);
        assert_eq!(
            state.rebind(&previous, &next).unwrap_err().code,
            ErrorCode::MountBusy
        );
        drop(opening);
        state.rebind(&previous, &next).unwrap();
        assert_eq!(state.snapshot(), next);
        assert_eq!(old_context.generation, previous.generation);
        assert_eq!(state.context(Some(caller)).generation, next.generation);
        assert_eq!(state.context(Some(caller)).identity, caller);
        assert_eq!(state.context(None).identity, next.identity);
        assert_eq!(
            state.reserve(previous.generation).unwrap_err().code,
            ErrorCode::StaleBinding
        );
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

    #[test]
    fn concurrent_open_and_rebind_cannot_both_use_the_previous_generation() {
        for _ in 0..32 {
            let previous = binding();
            let state = NativeBinding::new(previous.clone());
            let mut next = previous.clone();
            next.branch = BranchId::new();
            next.generation += 1;
            let start = Arc::new(std::sync::Barrier::new(2));
            let worker = {
                let state = state.clone();
                let start = start.clone();
                let previous = previous.clone();
                std::thread::spawn(move || {
                    start.wait();
                    state.rebind(&previous, &next)
                })
            };
            start.wait();
            let opened = state.reserve(previous.generation);
            let rebound = worker.join().unwrap();
            match (opened, rebound) {
                (Ok(_handle), Err(error)) => assert_eq!(error.code, ErrorCode::MountBusy),
                (Err(error), Ok(())) => assert_eq!(error.code, ErrorCode::StaleBinding),
                outcome => panic!("open and rebind must have one winner: {outcome:?}"),
            }
        }
    }
}
