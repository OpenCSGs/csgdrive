use super::*;

#[cfg(unix)]
#[path = "metadata.rs"]
mod metadata;
use agentdrive_test_support::{ManualClock, MemoryRemote};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

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

    async fn event(&self, turn_id: &str, phase: ExecutionPhase) {
        self.engine
            .services
            .collaboration
            .execution_event(
                &context().principal,
                CollaborationEvent {
                    mount: self.binding.id,
                    source: "native-test".into(),
                    thread_id: "thread".into(),
                    turn_id: turn_id.into(),
                    phase,
                },
            )
            .await
            .unwrap();
    }

    async fn current(&self) -> MountBinding {
        self.local.mount(self.binding.id).await.unwrap().unwrap()
    }

    #[cfg(unix)]
    async fn wait_for_handles(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.driver.open_handles(&self.binding).await.unwrap() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("native handle count did not reach {expected}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires permission to mount a native filesystem"]
async fn turn_callback_releases_an_open_file_and_refreshes_the_existing_mount() {
    let writer = Harness::open().await;
    let path = writer.binding.path.join("article.txt");
    writer.event("first", ExecutionPhase::Started).await;
    let file_path = path.clone();
    let mut file = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&file_path)
            .unwrap();
        let root = file_path.parent().unwrap();
        std::fs::write(root.join("original-only.txt"), b"old").unwrap();
        assert!(!root.join("received-only.txt").exists());
        assert!(
            std::fs::read_dir(root)
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "original-only.txt")
        );
        file.write_all(b"first").unwrap();
        #[cfg(windows)]
        file.sync_all().unwrap();
        file
    })
    .await
    .unwrap();
    writer.clock.advance(600_000);
    writer.engine.collaborate_due().await.unwrap();
    assert!(!writer.reference().await.write_available);
    #[cfg(unix)]
    let device = std::fs::metadata(&writer.binding.path).unwrap().dev();

    writer.event("first", ExecutionPhase::Completed).await;
    writer.engine.collaborate_due().await.unwrap();
    assert!(writer.reference().await.write_available);
    assert!(writer.current().await.generation > writer.binding.generation);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&writer.binding.path).unwrap().dev(),
        device
    );
    let old_file = tokio::task::spawn_blocking(move || {
        // NFSv3 can surface rejected writes at either WRITE or fsync.
        file.write_all(b"stale")
            .and_then(|()| file.sync_all())
            .unwrap_err();
        file
    })
    .await
    .unwrap();
    assert!(writer.reference().await.write_available);

    let other = Harness::using(
        writer.remote.clone(),
        Some((writer.binding.workspace, writer.binding.branch)),
    )
    .await;
    other.event("second", ExecutionPhase::Started).await;
    let active = other.binding.path.join("article.txt");
    tokio::task::spawn_blocking(move || {
        assert_eq!(std::fs::read(&active).unwrap(), b"first");
        let root = active.parent().unwrap();
        std::fs::remove_file(root.join("original-only.txt")).unwrap();
        std::fs::write(root.join("received-only.txt"), b"new").unwrap();
        std::fs::write(active, b"other").unwrap();
    })
    .await
    .unwrap();
    other.event("second", ExecutionPhase::Completed).await;
    other.engine.collaborate_due().await.unwrap();
    writer.clock.advance(2_001);
    writer.engine.collaborate_due().await.unwrap();
    assert!(writer.reference().await.write_available);
    assert_eq!(
        writer
            .local
            .branch(writer.binding.branch)
            .await
            .unwrap()
            .unwrap()
            .formal_head,
        other.reference().await.formal_head
    );
    tokio::task::spawn_blocking(move || {
        let root = path.parent().unwrap();
        for _ in 0..12 {
            assert_eq!(std::fs::read(&path).unwrap(), b"other");
            let names: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert!(names.iter().any(|name| name == "received-only.txt"));
            assert!(!names.iter().any(|name| name == "original-only.txt"));
            assert!(!root.join("original-only.txt").exists());
            assert_eq!(
                std::fs::read(root.join("received-only.txt")).unwrap(),
                b"new"
            );
        }
    })
    .await
    .unwrap();
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&writer.binding.path).unwrap().dev(),
        device
    );
    writer.event("third", ExecutionPhase::Started).await;
    let path = writer.binding.path.join("article.txt");
    tokio::task::spawn_blocking(move || {
        // An open descriptor from the first turn must not keep the current inode busy.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
        drop(old_file);
    })
    .await
    .unwrap();
    writer.event("third", ExecutionPhase::Completed).await;
    writer.engine.collaborate_due().await.unwrap();
    for harness in [&writer, &other] {
        unmount_when_idle(&harness.engine, &harness.current().await).await;
    }
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires permission to mount a native filesystem"]
async fn native_create_waits_for_slow_remote_write_authority() {
    let harness = Harness::open().await;
    harness.event("publish", ExecutionPhase::Started).await;
    std::fs::write(harness.binding.path.join("original"), b"first").unwrap();
    harness.event("publish", ExecutionPhase::Completed).await;
    harness.engine.collaborate_due().await.unwrap();
    assert!(harness.reference().await.write_available);

    harness.event("continue", ExecutionPhase::Started).await;
    let (observed, resume) = harness.remote.pause_next_branch_cas();
    let path = harness.binding.path.join("slow-create");
    let writer = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut file = std::fs::File::create_new(&path)?;
        file.write_all(b"complete")?;
        file.sync_all()?;
        assert_eq!(std::fs::read(path)?, b"complete");
        Ok(())
    });
    tokio::time::timeout(Duration::from_secs(10), observed)
        .await
        .expect("CREATE did not request write authority")
        .unwrap();
    // Hold the authority response longer than Darwin's short soft-mount retry window.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let returned_early = writer.is_finished();
    resume.send(()).unwrap();
    let result = writer.await.unwrap();
    unmount_when_idle(&harness.engine, &harness.current().await).await;
    assert!(
        !returned_early,
        "CREATE returned before write authority was confirmed: {result:?}"
    );
    result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires permission to mount a native filesystem"]
async fn daemon_shutdown_waits_for_busy_volume_and_restores_original_binding() {
    let harness = Harness::open().await;
    let path = harness.binding.path.join("shutdown.txt");
    let mut open_file = std::fs::File::create(&path).unwrap();
    open_file.write_all(b"still mounted").unwrap();
    open_file.sync_all().unwrap();
    let occupied = harness.engine.suspend_native_mounts().await.unwrap_err();
    assert_eq!(occupied.code, ErrorCode::MountBusy);
    assert_eq!(harness.current().await.state, MountState::Ready);
    open_file.write_all(b" during shutdown").unwrap();
    drop(open_file);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match harness.engine.suspend_native_mounts().await {
                Err(error) if error.code == ErrorCode::MountBusy => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                result => return result,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        harness.driver.status(&harness.binding).await.unwrap(),
        NativeMountState::Absent
    );
    assert_eq!(harness.current().await.state, MountState::Suspended);
    harness.engine.restore_native_mounts().await.unwrap();
    assert_eq!(harness.current().await, harness.binding);
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"still mounted during shutdown"
    );
    unmount_when_idle(&harness.engine, &harness.binding).await;
}
