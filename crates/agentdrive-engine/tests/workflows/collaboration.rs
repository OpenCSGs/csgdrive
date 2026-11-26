use super::*;

#[path = "collaboration_commands.rs"]
mod commands;
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

async fn configure(harness: &Harness, mount: &MountBinding) {
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

// Seed an available authority record to exercise acquisition of existing shared branches.
async fn seed_available_branch(h: &Harness, mount: MountId) {
    let binding = h.local.mount(mount).await.unwrap().unwrap();
    h.commit(binding.branch).await;
    h.sync(binding.workspace, SyncDirection::Push).await;
    let mut branch = h.local.branch(binding.branch).await.unwrap().unwrap();
    let remote = h
        .engine
        .storage_routes()
        .for_workspace(binding.workspace)
        .unwrap();
    let key = RefKey::Branch {
        workspace: binding.workspace,
        branch: binding.branch,
    };
    let previous = remote.refs.get(&key).await.unwrap().unwrap();
    let mut reference: BranchRef = decode(&previous.bytes).unwrap();
    reference.write_stopped = true;
    reference.write_available = true;
    reference.authority_epoch += 1;
    reference.source_generation += 1;
    branch.state = BranchState::Available;
    branch.authority_epoch = reference.authority_epoch;
    branch.source_generation = reference.source_generation;
    let CasOutcome::Applied { version } = remote
        .refs
        .compare_exchange(RefUpdate {
            key,
            expected_version: Some(previous.version),
            bytes: encode(&reference).unwrap().into(),
        })
        .await
        .unwrap()
    else {
        panic!("could not seed an available branch")
    };
    h.local
        .commit(LocalCommit {
            branches: vec![branch],
            remote_refs: vec![ObservedBranchRef::new(reference, version, h.clock.now_ns())],
            ..Default::default()
        })
        .await
        .unwrap();
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

fn save_request(binding: &MountBinding) -> SaveMount {
    SaveMount {
        mount: binding.id,
        message: None,
        expected_binding_generation: binding.generation,
        durability: Durability::Remote,
        wait_for_remote_ms: None,
    }
}

#[tokio::test]
async fn mount_save_acquires_released_authority_and_keeps_successful_retries_read_only() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = new(Some(remote)).await;
    let workspace = harness.workspace().await;
    let (binding, fs) = harness.branch_mount(workspace.id, "saved-work", None).await;
    harness.write_new(&fs, "work.txt", b"autosaved work").await;
    harness.sync(workspace.id, SyncDirection::Push).await;
    configure(&harness, &binding).await;
    seed_available_branch(&harness, binding.id).await;
    harness.engine.collaborate_due().await.unwrap();
    let before = harness.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(before.state, BranchState::Available);
    assert!(!before.dirty);
    let context = context();
    let request = save_request(&harness.local.mount(binding.id).await.unwrap().unwrap());
    let saved = success(
        harness
            .engine
            .services
            .mounts
            .save(context.clone(), request.clone())
            .await
            .unwrap(),
    );
    assert!(saved.local_saved && saved.remote_confirmed);
    assert!(saved.recovery().is_none());
    let after = harness.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(after.state, BranchState::Writable);
    assert!(after.authority_epoch > before.authority_epoch);
    assert_eq!(after.working_root, before.working_root);
    assert!(!after.dirty);
    let revision = harness
        .local
        .revision(after.formal_head)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revision.kind, RevisionKind::Commit);
    assert_eq!(revision.root, before.working_root);
    seed_available_branch(&harness, binding.id).await;
    harness.engine.collaborate_due().await.unwrap();
    let released = harness.local.branch(binding.branch).await.unwrap().unwrap();
    assert_eq!(released.state, BranchState::Available);
    let repeated = success(
        harness
            .engine
            .services
            .mounts
            .save(context, request)
            .await
            .unwrap(),
    );
    assert_eq!(saved.id, repeated.id);
    assert_eq!(saved.result, repeated.result);
    assert_eq!(
        harness.local.branch(binding.branch).await.unwrap().unwrap(),
        released
    );
}

#[tokio::test]
async fn concurrent_mount_saves_have_one_writer_and_the_loser_can_retry_the_same_request() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote.clone())).await;
    let third = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let (a, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.write_new(&fs, "work.txt", b"shared contents").await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &a).await;
    seed_available_branch(&first, a.id).await;
    first.engine.collaborate_due().await.unwrap();
    second.sync(workspace.id, SyncDirection::Pull).await;
    third.sync(workspace.id, SyncDirection::Pull).await;
    let (b, _) = automatic_mount(&second, workspace.id, a.branch).await;
    let (c, _) = automatic_mount(&third, workspace.id, a.branch).await;
    let b_context = context();
    let c_context = context();
    let (b_result, c_result) = tokio::join!(
        second
            .engine
            .services
            .mounts
            .save(b_context.clone(), save_request(&b)),
        third
            .engine
            .services
            .mounts
            .save(c_context.clone(), save_request(&c)),
    );
    let b_result = b_result.unwrap();
    let c_result = c_result.unwrap();
    assert_eq!(
        usize::from(b_result.local_saved) + usize::from(c_result.local_saved),
        1
    );
    let (winner, mount, loser, other, failed, retry) = if b_result.local_saved {
        (&second, &b, &third, &c, c_result, c_context)
    } else {
        (&third, &c, &second, &b, b_result, b_context)
    };
    assert_eq!(failed.error.as_ref().unwrap().code, ErrorCode::BranchBusy);
    assert_eq!(failed.recovery(), Some(OperationRecovery::RetryRequest));
    assert!(failed.result.is_none());
    configure(winner, mount).await;
    seed_available_branch(winner, mount.id).await;
    winner.engine.collaborate_due().await.unwrap();
    let saved = success(
        loser
            .engine
            .services
            .mounts
            .save(retry, save_request(other))
            .await
            .unwrap(),
    );
    assert_eq!(saved.id, failed.id);
    assert!(saved.local_saved && saved.remote_confirmed);
    assert!(saved.recovery().is_none());
}

#[tokio::test]
async fn mount_save_rejects_an_outdated_view_and_binding_before_committing() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let second = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let (a, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &a).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let (b, _) = automatic_mount(&second, workspace.id, a.branch).await;
    first.write_new(&fs, "latest.txt", b"latest").await;
    seed_available_branch(&first, a.id).await;
    first.engine.collaborate_due().await.unwrap();
    let request = save_request(&b);
    let retry = context();
    let failed = second
        .engine
        .services
        .mounts
        .save(retry.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(failed.error.as_ref().unwrap().code, ErrorCode::TargetMoved);
    assert_eq!(failed.recovery(), Some(OperationRecovery::RetryRequest));
    assert!(!failed.local_saved);
    second.clock.advance(2_001);
    second.engine.collaborate_due().await.unwrap();
    let current = second.local.mount(b.id).await.unwrap().unwrap();
    assert!(current.generation > b.generation);
    let stale = second
        .engine
        .services
        .mounts
        .save(retry, request)
        .await
        .unwrap();
    assert_eq!(stale.error.as_ref().unwrap().code, ErrorCode::StaleBinding);
    assert!(stale.recovery().is_none());
    let saved = success(
        second
            .engine
            .services
            .mounts
            .save(context(), save_request(&current))
            .await
            .unwrap(),
    );
    let Some(CommandResult::Revision { revision, .. }) = saved.result else {
        panic!("missing revision")
    };
    assert_eq!(
        second
            .engine
            .services
            .revisions
            .read_file(
                &actor(),
                workspace.id,
                revision,
                path("/latest.txt"),
                0,
                1024
            )
            .await
            .unwrap(),
        b"latest"[..]
    );
}

#[tokio::test]
async fn concurrent_mount_save_retries_reuse_the_saved_commit() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = new(Some(remote.clone())).await;
    let workspace = harness.workspace().await;
    let (binding, fs) = harness
        .branch_mount(workspace.id, "shared-work", None)
        .await;
    harness.write_new(&fs, "work.txt", b"saved once").await;
    harness.sync(workspace.id, SyncDirection::Push).await;
    configure(&harness, &binding).await;
    seed_available_branch(&harness, binding.id).await;
    harness.engine.collaborate_due().await.unwrap();
    let before = harness.local.revisions(workspace.id).await.unwrap().len();
    let (waiting, resume) = remote.pause_next_branch_cas();
    let request = save_request(&harness.local.mount(binding.id).await.unwrap().unwrap());
    let retry = context();
    let engine = harness.engine.clone();
    let running_request = request.clone();
    let running_context = retry.clone();
    let task = tokio::spawn(async move {
        engine
            .services
            .mounts
            .save(running_context, running_request)
            .await
    });
    waiting.await.unwrap();
    let duplicate = harness
        .engine
        .services
        .mounts
        .save(retry.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        duplicate.error.as_ref().unwrap().code,
        ErrorCode::BranchBusy
    );
    resume.send(()).unwrap();
    let saved = success(task.await.unwrap().unwrap());
    assert_eq!(saved.id, duplicate.id);
    assert_eq!(
        harness.local.revisions(workspace.id).await.unwrap().len(),
        before
    );
    let repeated = success(
        harness
            .engine
            .services
            .mounts
            .save(retry, request)
            .await
            .unwrap(),
    );
    assert_eq!(saved.result, repeated.result);
}

#[tokio::test]
async fn mount_save_can_finish_authority_recovery_without_holding_the_mount_lifecycle() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = new(Some(remote.clone())).await;
    let workspace = harness.workspace().await;
    let (binding, fs) = harness
        .branch_mount(workspace.id, "recover-work", None)
        .await;
    harness.sync(workspace.id, SyncDirection::Push).await;
    configure(&harness, &binding).await;
    harness
        .write_new(&fs, "local.txt", b"preserve this work")
        .await;
    harness.clock.advance(6_000);
    harness.engine.autosave_due().await.unwrap();
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
        harness
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
    let saved = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        harness
            .engine
            .services
            .mounts
            .save(context(), save_request(&binding)),
    )
    .await
    .expect("save must allow recovery to rebind")
    .unwrap();
    assert_eq!(
        saved.error.as_ref().unwrap().code,
        ErrorCode::RecoveryRequired
    );
    assert!(!saved.local_saved);
    let recovered = harness.local.mount(binding.id).await.unwrap().unwrap();
    assert_ne!(recovered.branch, binding.branch);
    assert!(recovered.generation > binding.generation);
    let result = success(
        harness
            .engine
            .services
            .mounts
            .save(context(), save_request(&recovered))
            .await
            .unwrap(),
    );
    let Some(CommandResult::Revision { revision, branch }) = result.result else {
        panic!("missing recovered revision")
    };
    assert_eq!(branch, recovered.branch);
    assert_eq!(
        harness
            .engine
            .services
            .revisions
            .read_file(
                &actor(),
                workspace.id,
                revision,
                path("/local.txt"),
                0,
                1024
            )
            .await
            .unwrap(),
        b"preserve this work"[..]
    );
    let published: BranchRef = decode(&remote.get(&key).await.unwrap().unwrap().bytes).unwrap();
    assert_eq!(published, replacement);
}

#[tokio::test]
async fn closing_files_preserves_write_authority() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding).await;
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
    assert_eq!(waiting[0].phase, CollaborationPhase::Writing);
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
    configure(&first, &a).await;
    seed_available_branch(&first, a.id).await;
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
async fn lost_authority_preserves_changes_and_fences_open_handles() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote.clone(), true).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    let file = first.write_new(&fs, "work.txt", b"initial").await;
    first.commit(binding.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding).await;
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
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::BranchBusy
    );
    first.engine.collaborate_due().await.unwrap();
    let target = first.local.mount(binding.id).await.unwrap().unwrap().branch;
    assert_ne!(target, binding.branch);
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
                    message: None,
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
    configure(&first, &a).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let (b, fs_b) = automatic_mount(&second, workspace.id, a.branch).await;
    first.write_new(&fs_a, "latest.txt", b"latest").await;
    seed_available_branch(&first, a.id).await;
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
    second.clock.advance(2_001);
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
    configure(&first, &binding).await;
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
    first.clock.advance(2_001);
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
    first.clock.advance(2_001);
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
async fn unsupported_mount_build_keeps_the_owned_branch_manual() {
    let remote = Arc::new(MemoryRemote::default());
    let first = new(Some(remote)).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding).await;
    first
        .driver
        .automatic_collaboration_disabled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = ConfigureCollaboration {
        mount: binding.id,
        expected_binding_generation: Some(binding.generation),
        enabled: true,
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
async fn failed_view_update_resumes_the_original_binding() {
    let remote = Arc::new(MemoryRemote::default());
    let writer = new(Some(remote.clone())).await;
    let reader = new(Some(remote)).await;
    let workspace = writer.workspace().await;
    let (source, fs) = writer.branch_mount(workspace.id, "work", None).await;
    writer.sync(workspace.id, SyncDirection::Push).await;
    configure(&writer, &source).await;
    reader.sync(workspace.id, SyncDirection::Pull).await;
    let (binding, _) = automatic_mount(&reader, workspace.id, source.branch).await;
    writer.write_new(&fs, "latest.txt", b"latest").await;
    seed_available_branch(&writer, source.id).await;
    writer.engine.collaborate_due().await.unwrap();

    reader
        .driver
        .fail_rebind
        .store(true, std::sync::atomic::Ordering::SeqCst);
    reader.clock.advance(6_000);
    assert!(reader.engine.collaborate_due().await.is_err());
    reader.engine.collaborate_due().await.unwrap();
    let refreshed = reader.local.mount(binding.id).await.unwrap().unwrap();
    assert_eq!(refreshed.path, binding.path);
    assert!(refreshed.generation > binding.generation);
    assert_eq!(
        reader.driver.status(&refreshed).await.unwrap(),
        NativeMountState::Mounted
    );
    reader
        .engine
        .services
        .files
        .lookup(
            &current_context(&reader, binding.id).await,
            InodeId::ROOT,
            "latest.txt",
        )
        .await
        .unwrap();
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
async fn global_switch_manages_existing_mounts_and_preserves_manual_authority() {
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
    configure(&first, &binding).await;
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
        CollaborationPhase::Writing
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
    assert!(!disabled.settings.enabled);
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
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    set_enabled(&config, true);
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
}

#[tokio::test]
async fn disabled_collaboration_keeps_new_manual_publication_failures_manual() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, config) = configurable(remote.clone(), true).await;
    let workspace = first.workspace().await;
    let (binding, fs) = first.branch_mount(workspace.id, "shared-work", None).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    configure(&first, &binding).await;
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
