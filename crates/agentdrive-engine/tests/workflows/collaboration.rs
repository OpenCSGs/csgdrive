use super::*;

async fn open_collaborating(
    directory: &std::path::Path,
    remote: Option<Arc<MemoryRemote>>,
) -> (
    Arc<LocalBackend>,
    Arc<Engine>,
    Arc<ManualClock>,
    Arc<MemoryMountDriver>,
) {
    open_with_config(
        directory,
        remote,
        LiveConfig::new(RuntimeConfig::default()).unwrap(),
    )
    .await
}

async fn new(remote: Option<Arc<MemoryRemote>>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let (local, engine, clock, driver) = open_collaborating(directory.path(), remote).await;
    Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    }
}

async fn configurable(remote: Arc<MemoryRemote>, enabled: bool) -> (Harness, Arc<ConfigStore>) {
    let directory = tempfile::tempdir().unwrap();
    let path = initialize(
        &directory.path().join("configuration"),
        "127.0.0.1:7421".parse().unwrap(),
    )
    .unwrap();
    let config = ConfigStore::open(&path).unwrap();
    set_enabled(&config, enabled);
    let (local, engine, clock, driver) =
        open_with_config(directory.path(), Some(remote), config.live()).await;
    (
        Harness {
            directory,
            local,
            engine,
            clock,
            driver,
        },
        config,
    )
}

fn set_enabled(config: &ConfigStore, enabled: bool) {
    let current = config.settings();
    let mut runtime = current.runtime;
    runtime.collaboration.enabled = enabled;
    config
        .update(SettingsUpdate {
            revision: current.revision,
            runtime,
        })
        .unwrap();
}

async fn configure(harness: &Harness, mount: &MountBinding, idle: Option<u64>) {
    harness
        .engine
        .services
        .collaboration
        .configure(
            &actor(),
            ConfigureCollaboration {
                mount: mount.id,
                expected_binding_generation: Some(mount.generation),
                enabled: true,
                idle_timeout_secs: idle,
                release_on_event: true,
            },
        )
        .await
        .unwrap();
    harness.engine.collaborate_due().await.unwrap();
}

async fn automatic_mount(
    harness: &Harness,
    workspace: WorkspaceId,
    branch: BranchId,
) -> (MountBinding, FsContext) {
    let operation = success(
        harness
            .engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: true,
                    workspace,
                    branch: Some(branch),
                    revision: None,
                    path: harness.directory.path().join("shared"),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding }) = operation.result else {
        panic!("missing mount");
    };
    let context = FsContext {
        mount: binding.id,
        generation: binding.generation,
        identity: identity(),
    };
    (binding, context)
}

async fn event(
    harness: &Harness,
    mount: MountId,
    turn: &str,
    phase: ExecutionPhase,
) -> CollaborationStatus {
    harness
        .engine
        .services
        .collaboration
        .execution_event(
            &actor(),
            CollaborationEvent {
                mount,
                source: "test-runtime".into(),
                thread_id: "thread".into(),
                turn_id: turn.into(),
                phase,
            },
        )
        .await
        .unwrap()
}

async fn current_context(harness: &Harness, mount: MountId) -> FsContext {
    let binding = harness.local.mount(mount).await.unwrap().unwrap();
    FsContext {
        mount,
        generation: binding.generation,
        identity: identity(),
    }
}

fn command_error(result: Result<OperationRecord>) -> Error {
    match result {
        Ok(operation) => operation.error.expect("command should be blocked"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn idle_release_waits_for_handles_and_new_mutations_restart_the_timer() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    first.clock.advance(1_001);
    first.engine.collaborate_due().await.unwrap();
    let waiting = first
        .engine
        .services
        .collaboration
        .status(&actor(), workspace.id)
        .await
        .unwrap();
    assert_eq!(waiting[0].phase, CollaborationPhase::WaitingForHandles);
    assert_eq!(waiting[0].open_handles, Some(1));
    first
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"updated"))
        .await
        .unwrap();
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Writable
    );
    first
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    first.clock.advance(999);
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Writable
    );
    remote.lose_next_branch_cas_response();
    first.clock.advance(2);
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Available
    );
    let remote_ref = remote
        .get(&RefKey::Branch {
            workspace: workspace.id,
            branch: binding.branch,
        })
        .await
        .unwrap()
        .unwrap();
    let remote_ref: BranchRef = decode(&remote_ref.bytes).unwrap();
    assert!(remote_ref.write_available && remote_ref.write_stopped);
    assert_eq!(remote_ref.owner, first.engine.runtime.location);
    assert!(
        first
            .engine
            .services
            .collaboration
            .status(&actor(), workspace.id)
            .await
            .unwrap()[0]
            .remote_confirmed
    );
}

#[tokio::test]
async fn foreign_turn_registration_binds_once_to_the_first_local_grant() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (a, fs_a) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs_a, "base.txt", b"base").await;
    first.commit(a.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &a, None).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let (b, fs_b) = automatic_mount(&second, workspace.id, a.branch).await;
    second
        .engine
        .services
        .collaboration
        .configure(
            &actor(),
            ConfigureCollaboration {
                mount: b.id,
                expected_binding_generation: Some(b.generation),
                enabled: true,
                idle_timeout_secs: None,
                release_on_event: true,
            },
        )
        .await
        .unwrap();
    event(&second, b.id, "b-turn", ExecutionPhase::Started).await;
    let error = second
        .engine
        .services
        .files
        .create(&fs_b, InodeId::ROOT, "blocked.txt", FileKind::File, 0o644)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BranchBusy);
    event(&first, a.id, "a-turn", ExecutionPhase::Started).await;
    event(&first, a.id, "a-turn", ExecutionPhase::Completed).await;
    first.engine.collaborate_due().await.unwrap();
    second
        .write_new(&fs_b, "continued.txt", b"second location")
        .await;
    let started = second.local.collaboration(b.branch).await.unwrap().unwrap();
    assert!(!started.executions[0].pending_grant);
    event(&second, b.id, "b-turn", ExecutionPhase::Completed).await;
    second.engine.collaborate_due().await.unwrap();
    assert_eq!(
        second.local.branch(b.branch).await.unwrap().unwrap().state,
        BranchState::Available
    );
    event(&second, b.id, "b-turn", ExecutionPhase::Started).await;
    event(&second, b.id, "b-turn", ExecutionPhase::Completed).await;
    assert!(
        second
            .local
            .collaboration(b.branch)
            .await
            .unwrap()
            .unwrap()
            .executions
            .is_empty()
    );
    first.clock.advance(5_001);
    first.engine.collaborate_due().await.unwrap();
    let fs_a = current_context(&first, a.id).await;
    first
        .write_new(&fs_a, "back-at-first.txt", b"third turn")
        .await;
    let epoch = first
        .local
        .branch(a.branch)
        .await
        .unwrap()
        .unwrap()
        .authority_epoch;
    event(&second, b.id, "b-turn", ExecutionPhase::Completed).await;
    second.engine.collaborate_due().await.unwrap();
    let remote_ref = remote
        .get(&RefKey::Branch {
            workspace: workspace.id,
            branch: a.branch,
        })
        .await
        .unwrap()
        .unwrap();
    let remote_ref: BranchRef = decode(&remote_ref.bytes).unwrap();
    assert_eq!(remote_ref.authority_epoch, epoch);
    assert_eq!(remote_ref.owner, first.engine.runtime.location);
    assert!(!remote_ref.write_available);
}

#[tokio::test]
async fn simultaneous_first_writes_have_one_remote_authority_winner() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote.clone())).await;
    let third = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (a, _) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &a, Some(1)).await;
    first.clock.advance(1_001);
    first.engine.collaborate_due().await.unwrap();
    second.sync(workspace.id, SyncDirection::Pull).await;
    third.sync(workspace.id, SyncDirection::Pull).await;
    let (_, fs_b) = automatic_mount(&second, workspace.id, a.branch).await;
    let (_, fs_c) = automatic_mount(&third, workspace.id, a.branch).await;
    let (b, c) = tokio::join!(
        second
            .engine
            .services
            .files
            .create(&fs_b, InodeId::ROOT, "b.txt", FileKind::File, 0o644),
        third
            .engine
            .services
            .files
            .create(&fs_c, InodeId::ROOT, "c.txt", FileKind::File, 0o644),
    );
    assert_eq!(usize::from(b.is_ok()) + usize::from(c.is_ok()), 1);
    let error = b.err().or_else(|| c.err()).unwrap();
    assert_eq!(error.code, ErrorCode::BranchBusy);
}

#[tokio::test]
async fn lost_authority_preserves_changes_and_rebinds_after_handles_close() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote.clone(), true).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(60)).await;
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    first
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"changed"))
        .await
        .unwrap();
    first.clock.advance(6_000);
    first.engine.autosave_due().await.unwrap();
    let old_jobs = first.local.sync_jobs(workspace.id).await.unwrap();
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: binding.branch,
    };
    let before = remote.get(&key).await.unwrap().unwrap();
    let mut replacement: BranchRef = decode(&before.bytes).unwrap();
    replacement.owner = LocationId::new();
    replacement.authority_epoch += 1;
    replacement.source_generation += 1;
    assert!(matches!(
        remote
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: Some(before.version),
                bytes: encode(&replacement).unwrap().into()
            })
            .await
            .unwrap(),
        CasOutcome::Applied { .. }
    ));
    let failed = command_error(
        first
            .engine
            .services
            .replica
            .sync(
                context(),
                SyncRequest {
                    workspace: workspace.id,
                    branches: Some(vec![binding.branch]),
                    direction: SyncDirection::Push,
                },
            )
            .await,
    );
    assert_eq!(failed.code, ErrorCode::StaleAuthority);
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Stopped
    );
    assert_eq!(
        first
            .engine
            .services
            .collaboration
            .configure(
                &actor(),
                ConfigureCollaboration {
                    mount: binding.id,
                    expected_binding_generation: Some(binding.generation),
                    enabled: true,
                    idle_timeout_secs: Some(60),
                    release_on_event: true
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::BranchBusy
    );
    first.engine.collaborate_due().await.unwrap();
    let recovery = first
        .local
        .collaboration(binding.branch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovery.phase, CollaborationPhase::WaitingForHandles);
    let target = recovery.recovery.unwrap().branch.unwrap();
    assert_ne!(target, binding.branch);
    assert_eq!(
        first.local.mount(binding.id).await.unwrap().unwrap().branch,
        binding.branch
    );
    assert!(
        first
            .engine
            .services
            .files
            .write(&fs, handle.id, 0, Bytes::from_static(b"late"))
            .await
            .is_err()
    );
    set_enabled(&config, false);
    first
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    first.engine.collaborate_due().await.unwrap();
    let moved = first.local.mount(binding.id).await.unwrap().unwrap();
    assert_eq!(moved.branch, target);
    assert_eq!(moved.path, binding.path);
    assert!(moved.generation > binding.generation);
    first.engine.collaborate_due().await.unwrap();
    assert!(
        !first
            .local
            .collaboration(target)
            .await
            .unwrap()
            .unwrap()
            .settings
            .enabled
    );
    success(
        first
            .engine
            .services
            .mounts
            .save(
                context(),
                SaveMount {
                    mount: moved.id,
                    expected_binding_generation: moved.generation,
                    durability: Durability::Local,
                    wait_for_remote_ms: None,
                },
            )
            .await
            .unwrap(),
    );
    for job in old_jobs
        .iter()
        .filter(|job| job.reference.branch == binding.branch)
    {
        let operation = first.local.operation(job.operation).await.unwrap().unwrap();
        assert_eq!(operation.phase, OperationPhase::Failed);
        assert!(
            operation
                .error
                .unwrap()
                .message
                .contains(&target.to_string())
        );
    }
    let fs = current_context(&first, binding.id).await;
    let opened = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: false,
                append: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .engine
            .services
            .files
            .read(&fs, opened.id, 0, 7)
            .await
            .unwrap()
            .as_ref(),
        b"changed"
    );
    first
        .engine
        .services
        .files
        .release(&fs, opened.id)
        .await
        .unwrap();
    assert!(
        first
            .local
            .collaboration(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .is_none()
    );
    let unchanged: BranchRef = decode(&remote.get(&key).await.unwrap().unwrap().bytes).unwrap();
    assert_eq!(unchanged, replacement);
    let source_branch_count = first.local.branches(workspace.id).await.unwrap().len();
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = first;
    drop((local, engine, clock, driver));
    let (local, engine, _, _) = open_collaborating(directory.path(), Some(remote)).await;
    let reopened = local.mount(binding.id).await.unwrap().unwrap();
    assert_eq!(reopened.branch, target);
    let resumed = success(
        engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: true,
                    workspace: workspace.id,
                    branch: Some(target),
                    revision: None,
                    path: reopened.path.clone(),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding: resumed }) = resumed.result else {
        panic!("missing resumed mount");
    };
    let fs = FsContext {
        mount: resumed.id,
        generation: resumed.generation,
        identity: identity(),
    };
    engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "work.txt")
        .await
        .unwrap();
    assert_eq!(
        local.branches(workspace.id).await.unwrap().len(),
        source_branch_count
    );
}

#[tokio::test]
async fn stale_first_write_waits_for_the_latest_view_before_retrying() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let (a, fs_a) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs_a, "base.txt", b"base").await;
    first.commit(a.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &a, None).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let (b, fs_b) = automatic_mount(&second, workspace.id, a.branch).await;
    event(&first, a.id, "changed", ExecutionPhase::Started).await;
    first.write_new(&fs_a, "latest.txt", b"latest").await;
    event(&first, a.id, "changed", ExecutionPhase::Completed).await;
    first.engine.collaborate_due().await.unwrap();
    let error = second
        .engine
        .services
        .files
        .create(&fs_b, InodeId::ROOT, "stale.txt", FileKind::File, 0o644)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetMoved);
    assert!(
        second
            .local
            .node_at(b.branch, &path("/stale.txt"))
            .await
            .unwrap()
            .is_none()
    );
    second.clock.advance(5_001);
    second.engine.collaborate_due().await.unwrap();
    let fs = current_context(&second, b.id).await;
    assert!(fs.generation > fs_b.generation);
    second
        .engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "latest.txt")
        .await
        .unwrap();
    second.write_new(&fs, "continued.txt", b"continued").await;
    assert_eq!(
        second.local.mount(b.id).await.unwrap().unwrap().branch,
        a.branch
    );
}

#[tokio::test]
async fn confirmed_contents_follow_authority_changes_without_a_recovery_branch() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs, "confirmed.txt", b"saved").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, None).await;
    let count = first.local.branches(workspace.id).await.unwrap().len();
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: binding.branch,
    };
    let before = remote.get(&key).await.unwrap().unwrap();
    let mut changed: BranchRef = decode(&before.bytes).unwrap();
    changed.owner = LocationId::new();
    changed.authority_epoch += 1;
    changed.source_generation += 1;
    remote
        .compare_exchange(RefUpdate {
            key,
            expected_version: Some(before.version),
            bytes: encode(&changed).unwrap().into(),
        })
        .await
        .unwrap();
    first.clock.advance(5_001);
    first.engine.collaborate_due().await.unwrap();
    let record = first
        .local
        .collaboration(binding.branch)
        .await
        .unwrap()
        .unwrap();
    assert!(record.recovery.is_none());
    assert_eq!(record.phase, CollaborationPhase::Following);
    assert_eq!(
        first.local.branches(workspace.id).await.unwrap().len(),
        count
    );
    first.clock.advance(5_001);
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .owner,
        changed.owner
    );
    first
        .engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "confirmed.txt")
        .await
        .unwrap();
}

#[tokio::test]
async fn global_disable_finishes_an_interrupted_release_and_allows_credential_repair() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote.clone(), true).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    assert_eq!(
        first
            .engine
            .switch_storage(None, &mut || Ok(()))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    first.clock.advance(1_001);
    remote.set_available(false);
    assert_eq!(
        first.engine.collaborate_due().await.unwrap_err().code,
        ErrorCode::Unavailable
    );
    set_enabled(&config, false);
    let disabled = ConfigureCollaboration {
        mount: binding.id,
        expected_binding_generation: Some(binding.generation),
        enabled: false,
        idle_timeout_secs: Some(1),
        release_on_event: true,
    };
    assert_eq!(
        first
            .engine
            .services
            .collaboration
            .configure(&actor(), disabled.clone())
            .await
            .unwrap_err()
            .code,
        ErrorCode::BranchBusy
    );
    assert_eq!(
        first
            .engine
            .services
            .files
            .create(&fs, InodeId::ROOT, "blocked.txt", FileKind::File, 0o644)
            .await
            .unwrap_err()
            .code,
        ErrorCode::BranchBusy
    );
    remote.set_available(true);
    let snapshot = first.local.collaboration(binding.branch).await.unwrap();
    let failed = first
        .engine
        .refresh_storage_connection(
            RemoteStorage {
                objects: remote.clone(),
                refs: remote.clone(),
            },
            &mut || Err(Error::new(ErrorCode::Io, "injected configuration failure")),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::Io);
    assert_eq!(
        first.local.collaboration(binding.branch).await.unwrap(),
        snapshot
    );
    first
        .engine
        .refresh_storage_connection(
            RemoteStorage {
                objects: remote.clone(),
                refs: remote.clone(),
            },
            &mut || Ok(()),
        )
        .await
        .unwrap();
    first.engine.collaborate_due().await.unwrap();
    first
        .engine
        .services
        .collaboration
        .configure(&actor(), disabled)
        .await
        .unwrap();
    first
        .engine
        .services
        .files
        .getattr(&fs, InodeId::ROOT)
        .await
        .unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Available
    );
    first
        .engine
        .switch_storage(None, &mut || Ok(()))
        .await
        .unwrap();
}

#[tokio::test]
async fn merge_history_publication_preserves_a_pending_authority_transition() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, _) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    first.clock.advance(1_001);

    let (waiting, resume) = remote.pause_next_branch_cas();
    let engine = first.engine.clone();
    let task = tokio::spawn(async move { engine.collaborate_due().await });
    waiting.await.unwrap();
    let before = first.local.branch(binding.branch).await.unwrap().unwrap();
    let prepared = success(
        first
            .engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source: before.formal_head,
                    target: BranchGuard::from(&before),
                    base: None,
                },
            )
            .await
            .unwrap(),
    );
    remote.lose_next_branch_cas_response();
    remote.set_available(false);
    resume.send(()).unwrap();
    assert_eq!(
        task.await.unwrap().unwrap_err().code,
        ErrorCode::Unavailable
    );
    let pending = first
        .local
        .collaboration(binding.branch)
        .await
        .unwrap()
        .unwrap();
    assert!(pending.transition.is_some());

    remote.set_available(true);
    first.engine.sync_pending().await.unwrap();
    assert_eq!(
        first.local.branch(binding.branch).await.unwrap().unwrap(),
        before
    );
    assert_eq!(
        first.local.collaboration(binding.branch).await.unwrap(),
        Some(pending)
    );
    first.engine.collaborate_due().await.unwrap();
    let after = first.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(after.state, BranchState::Available);
    assert_eq!(after.working_root, before.working_root);
    assert_eq!(after.source_generation, before.source_generation + 1);
    assert!(
        first
            .local
            .sync_jobs(workspace.id)
            .await
            .unwrap()
            .is_empty()
    );

    let receiver = Harness::new(Some(remote)).await;
    receiver.sync(workspace.id, SyncDirection::Pull).await;
    assert_eq!(
        receiver
            .local
            .operation(prepared.id)
            .await
            .unwrap()
            .unwrap()
            .merge_history,
        prepared.merge_history
    );
}

#[tokio::test]
async fn authority_publication_blocks_external_mutations_until_the_cas_completes() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs, "saved.txt", b"saved").await;
    let target_revision = first.commit(binding.branch).await;
    let (source, source_fs) = first
        .branch_mount(workspace.id, "merge-source", Some(target_revision))
        .await;
    first.write_new(&source_fs, "merged.txt", b"merged").await;
    let source_revision = first.commit(source.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    let guard = BranchGuard::from(&first.local.branch(binding.branch).await.unwrap().unwrap());
    let merged = success(
        first
            .engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source: source_revision,
                    target: guard,
                    base: None,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Merge { candidate }) = merged.result else {
        panic!("merge candidate missing");
    };
    first.clock.advance(1_001);
    let (waiting, resume) = remote.pause_next_branch_cas();
    let engine = first.engine.clone();
    let task = tokio::spawn(async move { engine.collaborate_due().await });
    waiting.await.unwrap();
    let before = first.local.branch(binding.branch).await.unwrap().unwrap();
    let requests = [
        first
            .engine
            .services
            .revisions
            .commit(
                context(),
                CommitRevision {
                    guard,
                    kind: CommitKind::Commit,
                    message: Some("concurrent".into()),
                    durability: Durability::Local,
                },
            )
            .await,
        first
            .engine
            .services
            .revisions
            .restore(
                context(),
                RestoreBranch {
                    guard,
                    revision: workspace.initial_revision,
                    durability: Durability::Local,
                },
            )
            .await,
        first
            .engine
            .services
            .revisions
            .fork(
                context(),
                ForkBranch {
                    workspace: workspace.id,
                    source: None,
                    source_branch: Some(binding.branch),
                    name: "concurrent-fork".into(),
                },
            )
            .await,
        first
            .engine
            .services
            .merge
            .apply(
                context(),
                ApplyMerge {
                    merge: candidate.id,
                    candidate: candidate.revision,
                    target: guard,
                    durability: Durability::Local,
                },
            )
            .await,
        first
            .engine
            .services
            .ownership
            .transfer(
                context(),
                TransferOwnership {
                    guard,
                    target: LocationId::new(),
                },
            )
            .await,
        first
            .engine
            .services
            .mounts
            .save(
                context(),
                SaveMount {
                    mount: binding.id,
                    expected_binding_generation: binding.generation,
                    durability: Durability::Local,
                    wait_for_remote_ms: None,
                },
            )
            .await,
    ];
    for request in requests {
        assert_eq!(command_error(request).code, ErrorCode::BranchBusy);
    }
    assert_eq!(
        first.local.branch(binding.branch).await.unwrap().unwrap(),
        before
    );
    resume.send(()).unwrap();
    task.await.unwrap().unwrap();
    let after = first.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(after.formal_head, before.formal_head);
    assert_eq!(after.state, BranchState::Available);
}

#[tokio::test]
async fn completion_waits_for_final_handle_writes_without_an_idle_timer() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, None).await;
    event(&first, binding.id, "finishing", ExecutionPhase::Started).await;
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    event(&first, binding.id, "finishing", ExecutionPhase::Completed).await;
    first.engine.collaborate_due().await.unwrap();
    first
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"tail end"))
        .await
        .unwrap();
    first
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    first.engine.collaborate_due().await.unwrap();
    let branch = first.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(branch.state, BranchState::Available);
    assert_eq!(branch.saved_seq, branch.mutation_seq);
    assert!(
        first
            .engine
            .services
            .collaboration
            .status(&actor(), workspace.id)
            .await
            .unwrap()[0]
            .remote_confirmed
    );
}

#[tokio::test]
async fn unsupported_mount_build_keeps_the_owned_branch_manual() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    first
        .driver
        .automatic_collaboration_disabled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = ConfigureCollaboration {
        mount: binding.id,
        expected_binding_generation: Some(binding.generation),
        enabled: true,
        idle_timeout_secs: Some(1),
        release_on_event: true,
    };
    assert_eq!(
        first
            .engine
            .services
            .collaboration
            .configure(&actor(), request.clone())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unsupported
    );
    first
        .engine
        .services
        .files
        .create(
            &fs,
            InodeId::ROOT,
            "manual-write.txt",
            FileKind::File,
            0o644,
        )
        .await
        .unwrap();
    first.clock.advance(1_001);
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Writable
    );
    first
        .engine
        .services
        .collaboration
        .configure(
            &actor(),
            ConfigureCollaboration {
                enabled: false,
                ..request
            },
        )
        .await
        .unwrap_err();
    first.write_new(&fs, "manual.txt", b"manual").await;
}

#[tokio::test]
async fn recovery_restarts_once_and_moves_completion_to_the_recovery_grant() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, None).await;
    event(
        &first,
        binding.id,
        "continued-after-recovery",
        ExecutionPhase::Started,
    )
    .await;
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    first
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"changed"))
        .await
        .unwrap();
    first.clock.advance(6_000);
    first.engine.autosave_due().await.unwrap();
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: binding.branch,
    };
    let before = remote.get(&key).await.unwrap().unwrap();
    let mut replacement: BranchRef = decode(&before.bytes).unwrap();
    replacement.owner = LocationId::new();
    replacement.authority_epoch += 1;
    replacement.source_generation += 1;
    remote
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(before.version),
            bytes: encode(&replacement).unwrap().into(),
        })
        .await
        .unwrap();
    let _ = first.engine.sync_pending().await;
    let record = first
        .local
        .collaboration(binding.branch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.phase, CollaborationPhase::WaitingForHandles);
    let target = record.recovery.unwrap().branch.unwrap();
    let count = first.local.branches(workspace.id).await.unwrap().len();
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = first;
    drop((local, engine, clock, driver));
    let (local, engine, clock, driver) =
        open_collaborating(directory.path(), Some(remote.clone())).await;
    let resumed = Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    };
    resumed.engine.collaborate_due().await.unwrap();
    assert_eq!(
        resumed
            .local
            .mount(binding.id)
            .await
            .unwrap()
            .unwrap()
            .branch,
        target
    );
    let target_record = resumed.local.collaboration(target).await.unwrap().unwrap();
    assert_eq!(target_record.executions.len(), 1);
    assert_eq!(
        target_record.executions[0].authority_epoch,
        resumed
            .local
            .branch(target)
            .await
            .unwrap()
            .unwrap()
            .authority_epoch
    );
    event(
        &resumed,
        binding.id,
        "continued-after-recovery",
        ExecutionPhase::Completed,
    )
    .await;
    resumed.engine.collaborate_due().await.unwrap();
    assert_eq!(
        resumed.local.branch(target).await.unwrap().unwrap().state,
        BranchState::Available
    );
    event(
        &resumed,
        binding.id,
        "continued-after-recovery",
        ExecutionPhase::Completed,
    )
    .await;
    resumed.engine.collaborate_due().await.unwrap();
    assert_eq!(
        resumed.local.branches(workspace.id).await.unwrap().len(),
        count
    );
    let original: BranchRef = decode(&remote.get(&key).await.unwrap().unwrap().bytes).unwrap();
    assert_eq!(original, replacement);
}

#[tokio::test]
async fn remount_failure_resumes_the_same_binding_after_restart_or_native_completion() {
    for restart in [false, true] {
        let remote = Arc::new(MemoryRemote::default());
        let first = new(Some(remote.clone())).await;
        let workspace = first.workspace().await;
        let (source, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
        first.write_new(&fs, "base.txt", b"base").await;
        first.commit(source.branch).await;
        first.sync(workspace.id, SyncDirection::Push).await;
        configure(&first, &source, None).await;
        event(&first, source.id, "base", ExecutionPhase::Started).await;
        event(&first, source.id, "base", ExecutionPhase::Completed).await;
        first.engine.collaborate_due().await.unwrap();

        let second = new(Some(remote.clone())).await;
        let (binding, _) = automatic_mount(&second, workspace.id, source.branch).await;
        first.write_new(&fs, "latest.txt", b"latest").await;
        first.commit(source.branch).await;
        first.sync(workspace.id, SyncDirection::Push).await;
        event(&first, source.id, "update", ExecutionPhase::Started).await;
        event(&first, source.id, "update", ExecutionPhase::Completed).await;
        first.engine.collaborate_due().await.unwrap();
        second
            .driver
            .remount_on_rebind
            .store(true, std::sync::atomic::Ordering::SeqCst);
        second
            .driver
            .fail_mount
            .store(true, std::sync::atomic::Ordering::SeqCst);
        second.clock.advance(6_000);
        assert_eq!(
            second.engine.collaborate_due().await.unwrap_err().code,
            ErrorCode::Unavailable
        );
        let failed = second.local.mount(binding.id).await.unwrap().unwrap();
        assert_eq!(failed.id, binding.id);
        assert_eq!(failed.path, binding.path);
        assert_eq!(failed.generation, binding.generation + 1);
        assert_eq!(failed.state, MountState::RecoveryRequired);
        let unavailable = current_context(&second, binding.id).await;
        assert_eq!(
            second
                .engine
                .services
                .files
                .getattr(&unavailable, InodeId::ROOT)
                .await
                .unwrap_err()
                .code,
            ErrorCode::StaleBinding
        );
        assert_eq!(
            second.driver.status(&failed).await.unwrap(),
            NativeMountState::Absent
        );
        let pending: Vec<_> = second
            .local
            .operations(workspace.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|operation| operation.name == "collaboration_rebind")
            .collect();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].pending.is_some());
        let mut expected = failed.clone();
        expected.state = MountState::Ready;

        let second = if restart {
            let Harness {
                directory,
                local,
                engine,
                clock,
                driver,
            } = second;
            drop((local, engine, clock, driver));
            let (local, engine, clock, driver) =
                open_collaborating(directory.path(), Some(remote.clone())).await;
            Harness {
                directory,
                local,
                engine,
                clock,
                driver,
            }
        } else {
            // Simulate native installation completing before the final operation commit.
            second
                .driver
                .mount(
                    MountSpec {
                        binding: expected.clone(),
                    },
                    second.engine.services.files.clone(),
                )
                .await
                .unwrap();
            *second.driver.unmount_error.lock() = Some(Error::new(
                ErrorCode::Unavailable,
                "the completed native mount must not be unmounted again",
            ));
            second
        };
        second
            .driver
            .remount_on_rebind
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if restart {
            check_remount_startup_queries(&second, &binding).await;
        } else {
            second.engine.collaborate_due().await.unwrap();
        }
        assert_eq!(
            second.local.mount(binding.id).await.unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            second.driver.status(&expected).await.unwrap(),
            NativeMountState::Mounted
        );
        if !restart {
            assert!(second.driver.unmount_error.lock().is_some());
        }
        let operations: Vec<_> = second
            .local
            .operations(workspace.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|operation| operation.name == "collaboration_rebind")
            .collect();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].id, pending[0].id);
        assert!(operations[0].pending.is_none());
        assert_eq!(operations[0].phase, OperationPhase::Complete);
        let current = current_context(&second, binding.id).await;
        let inode = second
            .engine
            .services
            .files
            .lookup(&current, InodeId::ROOT, "latest.txt")
            .await
            .unwrap();
        let handle = second
            .engine
            .services
            .files
            .open(
                &current,
                inode.id,
                OpenMode {
                    read: true,
                    write: false,
                    append: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            second
                .engine
                .services
                .files
                .read(&current, handle.id, 0, 1024)
                .await
                .unwrap(),
            Bytes::from_static(b"latest")
        );
        second
            .engine
            .services
            .files
            .release(&current, handle.id)
            .await
            .unwrap();
    }
}

async fn check_remount_startup_queries(harness: &Harness, previous: &MountBinding) {
    let (waiting, resume) = harness.driver.pause_next_mount();
    let engine = harness.engine.clone();
    let mounting = tokio::spawn(async move { engine.collaborate_due().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let current = current_context(harness, previous.id).await;
    let files = &harness.engine.services.files;
    assert_eq!(
        files.getattr(&current, InodeId::ROOT).await.unwrap().id,
        InodeId::ROOT
    );
    files.statfs(&current).await.unwrap();
    let mut old = current.clone();
    old.generation = previous.generation;
    assert_eq!(
        files.getattr(&old, InodeId::ROOT).await.unwrap_err().code,
        ErrorCode::StaleBinding
    );
    assert_eq!(
        files.statfs(&old).await.unwrap_err().code,
        ErrorCode::StaleBinding
    );
    let mut stranger = current.clone();
    stranger.identity.uid += 1;
    assert_eq!(
        files
            .getattr(&stranger, InodeId::ROOT)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        files.statfs(&stranger).await.unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    let inode = harness
        .local
        .node_at(previous.branch, &path("/latest.txt"))
        .await
        .unwrap()
        .unwrap()
        .inode
        .id;
    let blocked = [
        files.getattr(&current, inode).await.map(|_| ()),
        files
            .lookup(&current, InodeId::ROOT, "latest.txt")
            .await
            .map(|_| ()),
        files.readdir(&current, InodeId::ROOT).await.map(|_| ()),
        files
            .open(
                &current,
                inode,
                OpenMode {
                    read: true,
                    write: false,
                    append: false,
                },
            )
            .await
            .map(|_| ()),
        files
            .open(
                &current,
                inode,
                OpenMode {
                    read: false,
                    write: true,
                    append: false,
                },
            )
            .await
            .map(|_| ()),
        files.fsync(&current).await.map(|_| ()),
        files
            .create(
                &current,
                InodeId::ROOT,
                "during-mount.txt",
                FileKind::File,
                0o644,
            )
            .await
            .map(|_| ()),
        files
            .setattr(
                &current,
                InodeId::ROOT,
                AttributeUpdate {
                    mode: Some(0o700),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ()),
    ];
    for result in blocked {
        assert_eq!(result.unwrap_err().code, ErrorCode::BranchBusy);
    }
    resume.send(()).unwrap();
    mounting.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_racing_publication_failure_preserves_recovery_across_restart() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs, "base.txt", b"base").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, None).await;
    first.write_new(&fs, "pending.txt", b"pending").await;
    first.clock.advance(6_000);
    first.engine.autosave_due().await.unwrap();

    // 登记读取协调记录之后，操作时间戳暂停；发布失败能够在此期间完成。
    let (waiting, resume) = first.clock.pause_next_now_ns();
    let service = first.engine.services.collaboration.clone();
    let runtime = tokio::runtime::Handle::current();
    let registration = tokio::task::spawn_blocking(move || {
        runtime.block_on(service.execution_event(
            &actor(),
            CollaborationEvent {
                mount: binding.id,
                source: "test-runtime".into(),
                thread_id: "thread".into(),
                turn_id: "racing-turn".into(),
                phase: ExecutionPhase::Started,
            },
        ))
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: binding.branch,
    };
    let observed = remote.get(&key).await.unwrap().unwrap();
    let mut reference: BranchRef = decode(&observed.bytes).unwrap();
    reference.owner = LocationId::new();
    reference.authority_epoch += 1;
    reference.source_generation += 1;
    remote
        .compare_exchange(RefUpdate {
            key,
            expected_version: Some(observed.version),
            bytes: encode(&reference).unwrap().into(),
        })
        .await
        .unwrap();
    let failed = command_error(
        first
            .engine
            .services
            .replica
            .sync(
                context(),
                SyncRequest {
                    workspace: workspace.id,
                    branches: Some(vec![binding.branch]),
                    direction: SyncDirection::Push,
                },
            )
            .await,
    );
    assert_eq!(failed.code, ErrorCode::StaleAuthority);
    resume.send(()).unwrap();
    registration.await.unwrap().unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Stopped
    );

    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = first;
    drop((local, engine, clock, driver));
    let (local, engine, clock, driver) = open_collaborating(directory.path(), Some(remote)).await;
    let resumed = Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    };
    let error = resumed
        .engine
        .services
        .collaboration
        .configure(
            &actor(),
            ConfigureCollaboration {
                mount: binding.id,
                expected_binding_generation: Some(binding.generation),
                enabled: true,
                idle_timeout_secs: None,
                release_on_event: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BranchBusy);
    resumed.engine.collaborate_due().await.unwrap();
    let target = resumed
        .local
        .mount(binding.id)
        .await
        .unwrap()
        .unwrap()
        .branch;
    assert_ne!(target, binding.branch);
    assert!(
        resumed
            .local
            .node_at(target, &path("/pending.txt"))
            .await
            .unwrap()
            .is_some()
    );
    let continued = resumed.local.collaboration(target).await.unwrap().unwrap();
    assert_eq!(continued.executions.len(), 1);
    event(
        &resumed,
        binding.id,
        "racing-turn",
        ExecutionPhase::Completed,
    )
    .await;
    resumed.engine.collaborate_due().await.unwrap();
    assert_eq!(
        resumed.local.branch(target).await.unwrap().unwrap().state,
        BranchState::Available
    );
}

#[tokio::test]
async fn default_mounts_share_the_root_branch_and_retry_the_original_request() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let root = first
        .local
        .revision(workspace.initial_revision)
        .await
        .unwrap()
        .unwrap()
        .branch
        .unwrap();
    let request = default_mount(workspace.id, first.directory.path().join("default"));
    let request_context = context();
    let mounted = success(
        first
            .engine
            .services
            .mounts
            .prepare(request_context.clone(), request.clone())
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding: a }) = mounted.result else {
        panic!("missing mount");
    };
    assert_eq!(a.branch, root);
    first.sync(workspace.id, SyncDirection::Push).await;
    let retry = success(
        first
            .engine
            .services
            .mounts
            .prepare(request_context, request)
            .await
            .unwrap(),
    );
    assert_eq!(retry.id, mounted.id);
    second.sync(workspace.id, SyncDirection::Pull).await;
    let mounted = success(
        second
            .engine
            .services
            .mounts
            .prepare(
                context(),
                default_mount(workspace.id, second.directory.path().join("default")),
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding: b }) = mounted.result else {
        panic!("missing mount");
    };
    assert_eq!(b.branch, root);
    assert_eq!(first.local.branches(workspace.id).await.unwrap().len(), 1);
    assert_eq!(second.local.branches(workspace.id).await.unwrap().len(), 1);
    assert_eq!(
        second.local.selected_branch(workspace.id).await.unwrap(),
        Some(root)
    );
    assert!(
        second
            .local
            .collaboration(root)
            .await
            .unwrap()
            .unwrap()
            .settings
            .enabled
    );
}

#[tokio::test]
async fn global_switch_manages_existing_mounts_and_cancels_unfrozen_releases() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote, false).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "manual-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    assert!(
        first
            .local
            .collaboration(binding.branch)
            .await
            .unwrap()
            .is_none()
    );
    set_enabled(&config, true);
    first.engine.collaborate_due().await.unwrap();
    configure(&first, &binding, Some(1)).await;
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    first.clock.advance(1_001);
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .collaboration(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .phase,
        CollaborationPhase::WaitingForHandles
    );
    let epoch = first
        .local
        .branch(binding.branch)
        .await
        .unwrap()
        .unwrap()
        .authority_epoch;
    set_enabled(&config, false);
    first
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"manual!"))
        .await
        .unwrap();
    let disabled = first
        .local
        .collaboration(binding.branch)
        .await
        .unwrap()
        .unwrap();
    assert!(!disabled.settings.enabled && disabled.release.is_none());
    first
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    first.clock.advance(120_000);
    first.engine.collaborate_due().await.unwrap();
    let manual = first.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(manual.state, BranchState::Writable);
    assert_eq!(manual.authority_epoch, epoch);
    let status = first
        .engine
        .services
        .collaboration
        .status(&actor(), workspace.id)
        .await
        .unwrap();
    assert!(
        status
            .iter()
            .filter(|entry| entry.branch == binding.branch)
            .all(|entry| !entry.settings.enabled && entry.phase == CollaborationPhase::Disabled)
    );
    assert_eq!(
        first
            .engine
            .services
            .collaboration
            .configure(
                &actor(),
                ConfigureCollaboration {
                    mount: binding.id,
                    expected_binding_generation: Some(binding.generation),
                    enabled: true,
                    idle_timeout_secs: Some(1),
                    release_on_event: true
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    set_enabled(&config, true);
    first.engine.collaborate_due().await.unwrap();
    event(&first, binding.id, "resumed", ExecutionPhase::Started).await;
    event(&first, binding.id, "resumed", ExecutionPhase::Completed).await;
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Available
    );
}

#[tokio::test]
async fn disabled_collaboration_keeps_new_manual_publication_failures_manual() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote.clone(), true).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding, Some(1)).await;
    set_enabled(&config, false);
    first.write_new(&fs, "manual.txt", b"manual").await;
    first.clock.advance(6_000);
    first.engine.autosave_due().await.unwrap();
    assert!(
        !first
            .local
            .collaboration(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .settings
            .enabled
    );
    let count = first.local.branches(workspace.id).await.unwrap().len();
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: binding.branch,
    };
    let observed = remote.get(&key).await.unwrap().unwrap();
    let mut reference: BranchRef = decode(&observed.bytes).unwrap();
    reference.owner = LocationId::new();
    reference.authority_epoch += 1;
    reference.source_generation += 1;
    remote
        .compare_exchange(RefUpdate {
            key,
            expected_version: Some(observed.version),
            bytes: encode(&reference).unwrap().into(),
        })
        .await
        .unwrap();
    assert_eq!(
        first.engine.sync_pending().await.unwrap_err().code,
        ErrorCode::StaleAuthority
    );
    first.engine.collaborate_due().await.unwrap();
    assert_eq!(
        first.local.branches(workspace.id).await.unwrap().len(),
        count
    );
    assert!(
        first
            .local
            .collaboration(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .is_none()
    );
    assert_eq!(
        first
            .local
            .branch(binding.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        BranchState::Stopped
    );
}
