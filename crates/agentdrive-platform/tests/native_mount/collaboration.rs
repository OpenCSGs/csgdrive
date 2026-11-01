use super::*;

#[cfg(unix)]
#[path = "metadata.rs"]
mod metadata;
use agentdrive_test_support::{ManualClock, MemoryRemote};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::time::Duration;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

struct Harness {
    local: Arc<LocalBackend>,
    remote: Arc<MemoryRemote>,
    clock: Arc<ManualClock>,
    driver: Arc<NativeMountDriver>,
    engine: Arc<Engine>,
    binding: MountBinding,
    _root: tempfile::TempDir,
}

impl Harness {
    async fn open() -> Self {
        Self::using(Arc::new(MemoryRemote::default()), None).await
    }

    async fn using(remote: Arc<MemoryRemote>, source: Option<(WorkspaceId, BranchId)>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalBackend::open(root.path().join("state")).unwrap());
        let clock = Arc::new(ManualClock::default());
        let mounts = root.path().join("mounts");
        let driver = NativeMountDriver::new(vec![mounts.clone()]).unwrap();
        assert!(driver.supports_automatic_collaboration());
        let engine = Engine::open(
            Adapters {
                state: local.clone(),
                objects: local.clone(),
                working: local.clone(),
                remote_objects: Some(remote.clone()),
                remote_refs: Some(remote.clone()),
                clock: clock.clone(),
                mounts: driver.clone(),
                directories: HostDirectories::new(vec![]).unwrap(),
            },
            EngineConfig {
                live: LiveConfig::new(RuntimeConfig::default()).unwrap(),
                identity: local_identity(),
            },
        )
        .await
        .unwrap();
        let (workspace, branch) = if let Some(source) = source {
            source
        } else {
            let created = success(
                engine
                    .services
                    .workspace
                    .create(
                        context(),
                        CreateWorkspace {
                            name: "native-collaboration".into(),
                            grants: vec![],
                            max_file_bytes: None,
                            max_working_bytes: None,
                            max_inodes: None,
                        },
                    )
                    .await
                    .unwrap(),
            );
            let Some(CommandResult::Workspace { workspace }) = created.result else {
                panic!("missing workspace");
            };
            let fork = success(
                engine
                    .services
                    .revisions
                    .fork(
                        context(),
                        ForkBranch {
                            workspace: workspace.id,
                            source: Some(workspace.initial_revision),
                            source_branch: None,
                            name: "shared-work".into(),
                        },
                    )
                    .await
                    .unwrap(),
            );
            engine.sync_pending().await.unwrap();
            (workspace.id, fork.branch.unwrap())
        };
        let prepared = success(
            engine
                .services
                .mounts
                .prepare(
                    context(),
                    PrepareMount {
                        automatic: false,
                        workspace,
                        branch: Some(branch),
                        revision: None,
                        path: mounts.join("working"),
                        access: AccessMode::ReadWrite,
                        durability: Durability::Local,
                    },
                )
                .await
                .unwrap(),
        );
        let Some(CommandResult::Mount { binding }) = prepared.result else {
            panic!("missing native mount");
        };
        engine
            .services
            .collaboration
            .configure(
                &context().principal,
                ConfigureCollaboration {
                    mount: binding.id,
                    expected_binding_generation: Some(binding.generation),
                    enabled: true,
                    idle_timeout_secs: Some(1),
                    release_on_event: false,
                },
            )
            .await
            .unwrap();
        engine.collaborate_due().await.unwrap();
        Self {
            _root: root,
            local,
            remote,
            clock,
            driver,
            engine,
            binding,
        }
    }

    async fn reference(&self) -> BranchRef {
        let reference = self
            .remote
            .get(&RefKey::Branch {
                workspace: self.binding.workspace,
                branch: self.binding.branch,
            })
            .await
            .unwrap()
            .unwrap();
        decode(&reference.bytes).unwrap()
    }

    async fn assert_handle_prevents_release(&self) {
        // Record this mutation before advancing the deterministic inactivity clock.
        self.engine.collaborate_due().await.unwrap();
        self.clock.advance(5_001);
        self.engine.collaborate_due().await.unwrap();
        let status = self
            .engine
            .services
            .collaboration
            .status(&context().principal, self.binding.workspace)
            .await
            .unwrap()
            .into_iter()
            .find(|status| status.branch == self.binding.branch)
            .unwrap();
        assert_eq!(
            status.phase,
            CollaborationPhase::WaitingForHandles,
            "{status:?}"
        );
        assert!(
            status.open_handles.is_some_and(|count| count > 0),
            "{status:?}"
        );
        assert!(self.driver.open_handles(&self.binding).await.unwrap() > 0);
        assert_eq!(
            self.local
                .branch(self.binding.branch)
                .await
                .unwrap()
                .unwrap()
                .state,
            BranchState::Writable
        );
        assert!(!self.reference().await.write_available);
        self.clock.advance(5_001);
        self.engine.collaborate_due().await.unwrap();
        assert!(!self.reference().await.write_available);
    }

    async fn wait_for_handles(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.driver.open_handles(&self.binding).await.unwrap() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("native handle count did not reach {expected}"));
    }

    async fn close_and_release(&self, handle: std::fs::File, expected: &[u8]) -> u64 {
        tokio::task::spawn_blocking(move || drop(handle))
            .await
            .unwrap();
        self.wait_for_handles(0).await;
        self.engine.autosave_due().await.unwrap();
        self.engine.collaborate_due().await.unwrap();
        if self
            .local
            .branch(self.binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state
            != BranchState::Available
        {
            // Close may update file metadata, which starts another inactivity interval.
            self.clock.advance(5_001);
            self.engine.autosave_due().await.unwrap();
            self.engine.collaborate_due().await.unwrap();
        }
        let branch = self
            .local
            .branch(self.binding.branch)
            .await
            .unwrap()
            .unwrap();
        let reference = self.reference().await;
        assert_eq!(branch.state, BranchState::Available);
        assert!(reference.write_available && reference.write_stopped);
        assert_eq!(reference.working_root, branch.working_root);
        assert_eq!(reference.durable_seq, branch.mutation_seq);
        assert_eq!(branch.saved_seq, branch.mutation_seq);
        assert!(reference.autosave_head.is_some());
        let status = self
            .engine
            .services
            .collaboration
            .status(&context().principal, self.binding.workspace)
            .await
            .unwrap()
            .into_iter()
            .find(|status| status.branch == branch.id)
            .unwrap();
        assert!(status.local_saved && status.remote_confirmed, "{status:?}");
        assert_eq!(status.open_handles, Some(0));
        let file = self
            .local
            .node_at(branch.id, &"/work.txt".to_owned().try_into().unwrap())
            .await
            .unwrap()
            .unwrap();
        let mut content = self
            .remote
            .download(branch.workspace, file.inode.content.as_ref().unwrap())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut content.reader, &mut bytes)
            .await
            .unwrap();
        assert_eq!(bytes, expected);
        reference.authority_epoch
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_automatic_collaboration_follows_driver_handle_tracking() {
    let harness = Harness::open().await;
    let path = harness.binding.path.clone();
    let file = tokio::task::spawn_blocking(move || {
        std::fs::create_dir(path.join("directory")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path.join("work.txt"))
            .unwrap();
        file.write_all(b"file handle work").unwrap();
        file.sync_all().unwrap();
        file
    })
    .await
    .unwrap();
    harness.assert_handle_prevents_release().await;
    let first_epoch = harness.close_and_release(file, b"file handle work").await;

    let path = harness.binding.path.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path.join("work.txt"))
            .unwrap();
        file.write_all(b"directory handle work").unwrap();
        drop(file);
    })
    .await
    .unwrap();
    harness.wait_for_handles(0).await;
    let path = harness.binding.path.join("directory");
    let directory = tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        options.open(path).unwrap()
    })
    .await
    .unwrap();
    // The directory handle has no corresponding FileService data handle.
    let observed = harness.driver.open_handles(&harness.binding).await.unwrap();
    let scope = harness.driver.handle_tracking_scope();
    eprintln!("directory handles observed: {observed}, scope {scope:?}");
    if scope == HandleTrackingScope::Native {
        assert_eq!(observed, 1);
    }
    assert!(harness.reference().await.authority_epoch > first_epoch);
    if observed != 0 {
        harness.assert_handle_prevents_release().await;
    } else {
        harness.engine.collaborate_due().await.unwrap();
        harness.clock.advance(5_001);
        harness.engine.autosave_due().await.unwrap();
        harness.engine.collaborate_due().await.unwrap();
        assert!(harness.reference().await.write_available);
    }
    let second_epoch = harness
        .close_and_release(directory, b"directory handle work")
        .await;
    assert!(second_epoch > first_epoch);
    unmount_when_idle(&harness.engine, &harness.binding).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_follower_can_reopen_after_rejected_concurrent_append() {
    let first = Harness::open().await;
    let path = first.binding.path.join("work.txt");
    let file = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"first\n").unwrap();
        file.sync_all().unwrap();
        file
    })
    .await
    .unwrap();
    first.assert_handle_prevents_release().await;
    first.close_and_release(file, b"first\n").await;
    let second = Harness::using(
        first.remote.clone(),
        Some((first.binding.workspace, first.binding.branch)),
    )
    .await;
    for path in [first.binding.path.clone(), second.binding.path.clone()] {
        tokio::task::spawn_blocking(move || {
            assert_eq!(std::fs::read(path.join("work.txt")).unwrap(), b"first\n");
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    let original_device = std::fs::metadata(&first.binding.path).unwrap().dev();
    let path = second.binding.path.join("work.txt");
    let file = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap();
        file.write_all(b"second\n").unwrap();
        file.sync_all().unwrap();
        file
    })
    .await
    .unwrap();
    second.assert_handle_prevents_release().await;
    second.engine.autosave_due().await.unwrap();
    second.engine.sync_pending().await.unwrap();
    let path = first.binding.path.join("work.txt");
    tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect_err("the other location still owns write authority");
    })
    .await
    .unwrap();
    second.close_and_release(file, b"first\nsecond\n").await;
    first.wait_for_handles(0).await;
    first.engine.collaborate_due().await.unwrap();
    let current = first.local.mount(first.binding.id).await.unwrap().unwrap();
    assert!(current.generation > first.binding.generation);
    assert_eq!(current.id, first.binding.id);
    assert_eq!(current.path, first.binding.path);
    assert_eq!(current.access, first.binding.access);
    #[cfg(unix)]
    {
        let current_device = std::fs::metadata(&current.path).unwrap().dev();
        if first.driver.requires_remount_for_rebind() {
            assert_ne!(current_device, original_device);
        } else {
            assert_eq!(current_device, original_device);
        }
    }
    let path = current.path.clone();
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            std::fs::read(path.join("work.txt")).unwrap(),
            b"first\nsecond\n"
        );
        assert!(
            std::fs::read_dir(&path)
                .unwrap()
                .any(|entry| { entry.unwrap().file_name() == "work.txt" })
        );
    })
    .await
    .unwrap();
    for harness in [&first, &second] {
        let binding = harness
            .local
            .mount(harness.binding.id)
            .await
            .unwrap()
            .unwrap();
        unmount_when_idle(&harness.engine, &binding).await;
    }
}
