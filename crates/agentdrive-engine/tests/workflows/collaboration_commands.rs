use super::*;

async fn branch(h: &Harness, id: BranchId) -> Branch {
    h.local.branch(id).await.unwrap().unwrap()
}

async fn idle(h: &Harness, id: BranchId) {
    h.engine.collaborate_due().await.unwrap();
    h.clock.advance(1_001);
    h.engine.collaborate_due().await.unwrap();
    assert_eq!(branch(h, id).await.state, BranchState::Available);
}

async fn released_work(remote: Arc<MemoryRemote>) -> (Harness, Workspace, MountBinding, FsContext) {
    let h = new(Some(remote)).await;
    let workspace = h.workspace().await;
    let (mount, fs) = h.branch_mount(workspace.id, "work", None).await;
    h.write_new(&fs, "work.txt", b"preserved work").await;
    configure(&h, &mount, Some(1)).await;
    idle(&h, mount.branch).await;
    (h, workspace, mount, fs)
}

fn commit_request(branch: &Branch, kind: CommitKind) -> CommitRevision {
    CommitRevision {
        message: Some("Save work".into()),
        guard: BranchGuard::from(branch),
        kind,
        durability: Durability::Remote,
    }
}

#[tokio::test]
async fn commit_and_checkpoint_acquire_authority_and_completed_replays_do_not() {
    for kind in [CommitKind::Commit, CommitKind::Checkpoint] {
        let (h, _, mount, _) = released_work(Arc::new(MemoryRemote::default())).await;
        let before = branch(&h, mount.branch).await;
        let request = commit_request(&before, kind);
        let retry = context();
        let saved = success(
            h.engine
                .services
                .revisions
                .commit(retry.clone(), request.clone())
                .await
                .unwrap(),
        );
        assert!(saved.local_saved && saved.remote_confirmed);
        let after = branch(&h, mount.branch).await;
        assert_eq!(after.state, BranchState::Writable);
        assert!(after.authority_epoch > before.authority_epoch);
        assert_eq!(after.working_root, before.working_root);
        match kind {
            CommitKind::Commit => assert!(!after.dirty),
            CommitKind::Checkpoint => assert_eq!(after.formal_head, before.formal_head),
        }
        idle(&h, mount.branch).await;
        let released = branch(&h, mount.branch).await;
        let repeated = success(
            h.engine
                .services
                .revisions
                .commit(retry, request)
                .await
                .unwrap(),
        );
        assert_eq!(repeated.id, saved.id);
        assert_eq!(repeated.result, saved.result);
        assert_eq!(branch(&h, mount.branch).await, released);
    }
}

#[tokio::test]
async fn unmounted_branch_commands_can_acquire_and_release_authority() {
    let (h, _, mount, _) = released_work(Arc::new(MemoryRemote::default())).await;
    success(
        h.engine
            .services
            .mounts
            .unmount(context(), mount.id, mount.generation)
            .await
            .unwrap(),
    );
    let request = commit_request(&branch(&h, mount.branch).await, CommitKind::Commit);
    let saved = success(
        h.engine
            .services
            .revisions
            .commit(context(), request)
            .await
            .unwrap(),
    );
    assert!(saved.remote_confirmed);
    idle(&h, mount.branch).await;
}

#[tokio::test]
async fn command_contending_with_a_file_writer_retries_without_changing_its_request() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, workspace, mount, fs) = released_work(remote.clone()).await;
    let second = new(Some(remote)).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let (_, _) = automatic_mount(&second, workspace.id, mount.branch).await;
    let node = first
        .engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "work.txt")
        .await
        .unwrap();
    let handle = first
        .engine
        .services
        .files
        .open(
            &fs,
            node.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    let request = commit_request(&branch(&second, mount.branch).await, CommitKind::Commit);
    let retry = context();
    let failed = second
        .engine
        .services
        .revisions
        .commit(retry.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(failed.error.as_ref().unwrap().code, ErrorCode::BranchBusy);
    assert_eq!(failed.recovery(), Some(OperationRecovery::RetryRequest));
    assert!(!failed.local_saved);
    first
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    idle(&first, mount.branch).await;
    let saved = success(
        second
            .engine
            .services
            .revisions
            .commit(retry, request)
            .await
            .unwrap(),
    );
    assert_eq!(saved.id, failed.id);
    assert!(saved.local_saved && saved.remote_confirmed);
}

#[tokio::test]
async fn concurrent_commit_retries_create_one_revision() {
    let remote = Arc::new(MemoryRemote::default());
    let (h, workspace, mount, _) = released_work(remote.clone()).await;
    let before = h.local.revisions(workspace.id).await.unwrap().len();
    let request = commit_request(&branch(&h, mount.branch).await, CommitKind::Commit);
    let retry = context();
    let (waiting, resume) = remote.pause_next_branch_cas();
    let engine = h.engine.clone();
    let (running_context, running_request) = (retry.clone(), request.clone());
    let task = tokio::spawn(async move {
        engine
            .services
            .revisions
            .commit(running_context, running_request)
            .await
    });
    waiting.await.unwrap();
    let duplicate = h
        .engine
        .services
        .revisions
        .commit(retry.clone(), request.clone())
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
        h.local.revisions(workspace.id).await.unwrap().len(),
        before + 1
    );
    let repeated = success(
        h.engine
            .services
            .revisions
            .commit(retry, request)
            .await
            .unwrap(),
    );
    assert_eq!(repeated.result, saved.result);
}

#[tokio::test]
async fn stale_command_content_is_rejected_before_and_after_following_updates() {
    let remote = Arc::new(MemoryRemote::default());
    let (first, workspace, mount, fs) = released_work(remote.clone()).await;
    let second = new(Some(remote)).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    automatic_mount(&second, workspace.id, mount.branch).await;
    let request = commit_request(&branch(&second, mount.branch).await, CommitKind::Commit);
    first.write_new(&fs, "latest.txt", b"keep latest").await;
    idle(&first, mount.branch).await;
    let retry = context();
    let error = command_error(
        second
            .engine
            .services
            .revisions
            .commit(retry.clone(), request.clone())
            .await,
    );
    assert_eq!(error.code, ErrorCode::TargetMoved);
    second.clock.advance(5_001);
    second.engine.collaborate_due().await.unwrap();
    let updated = branch(&second, mount.branch).await;
    let error = command_error(
        second
            .engine
            .services
            .revisions
            .commit(retry, request)
            .await,
    );
    assert_eq!(error.code, ErrorCode::TargetMoved);
    assert_eq!(branch(&second, mount.branch).await, updated);
    let saved = success(
        second
            .engine
            .services
            .revisions
            .commit(context(), commit_request(&updated, CommitKind::Commit))
            .await
            .unwrap(),
    );
    assert!(saved.remote_confirmed);
}

#[tokio::test]
async fn restore_reacquires_after_authority_is_released_during_mount_handoff() {
    let remote = Arc::new(MemoryRemote::default());
    let (h, workspace, mount, _) = released_work(remote.clone()).await;
    let before = branch(&h, mount.branch).await;
    let request = RestoreBranch {
        guard: BranchGuard::from(&before),
        revision: workspace.initial_revision,
        durability: Durability::Remote,
    };
    let retry = context();
    let pending = success(
        h.engine
            .services
            .revisions
            .restore(retry.clone(), request.clone())
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    idle(&h, mount.branch).await;
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = h;
    drop(engine);
    drop(driver);
    drop(local);
    drop(clock);
    let (local, engine, clock, driver) = open_collaborating(directory.path(), Some(remote)).await;
    let h = Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    };
    success(
        h.engine
            .services
            .mounts
            .release(
                context(),
                ReleaseMount {
                    operation: pending.id,
                    expected_binding_generation: mount.generation,
                },
            )
            .await
            .unwrap(),
    );
    let saved = success(
        h.engine
            .services
            .operations
            .get(&actor(), pending.id)
            .await
            .unwrap(),
    );
    assert!(saved.local_saved && saved.remote_confirmed && saved.mount_ready == Some(true));
    let current = h.local.mount(mount.id).await.unwrap().unwrap();
    let fs = current_context(&h, mount.id).await;
    assert!(current.generation > mount.generation);
    assert_eq!(
        h.engine
            .services
            .files
            .lookup(&fs, InodeId::ROOT, "work.txt")
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let repeated = success(
        h.engine
            .services
            .revisions
            .restore(retry, request)
            .await
            .unwrap(),
    );
    assert_eq!(repeated.result, saved.result);
    let old = h
        .local
        .revision(before.autosave_head.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.root, before.working_root);
}

async fn prepared_merge() -> (Harness, MountBinding, FsContext, RevisionId, MergeCandidate) {
    let h = new(Some(Arc::new(MemoryRemote::default()))).await;
    let workspace = h.workspace().await;
    let (target, fs) = h.branch_mount(workspace.id, "target", None).await;
    h.write_new(&fs, "target.txt", b"target").await;
    let head = h.commit(target.branch).await;
    let (_, source_fs) = h.branch_mount(workspace.id, "source", None).await;
    h.write_new(&source_fs, "source.txt", b"source").await;
    let source_mount = h.local.mount(source_fs.mount).await.unwrap().unwrap();
    let source = h.commit(source_mount.branch).await;
    h.sync(workspace.id, SyncDirection::Push).await;
    configure(&h, &target, Some(1)).await;
    idle(&h, target.branch).await;
    let before = branch(&h, target.branch).await;
    let prepared = success(
        h.engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source,
                    target: BranchGuard::from(&before),
                    base: None,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(branch(&h, target.branch).await, before);
    let Some(CommandResult::Merge { candidate }) = prepared.result else {
        panic!("missing candidate")
    };
    assert!(candidate.conflicts.is_empty());
    (h, target, fs, head, candidate)
}

#[tokio::test]
async fn merge_preview_leaves_authority_available_and_apply_reacquires_after_waiting() {
    let (h, mount, fs, head, candidate) = prepared_merge().await;
    let retry = context();
    let request = ApplyMerge {
        merge: candidate.id,
        candidate: candidate.revision,
        target: candidate.target,
        durability: Durability::Remote,
    };
    let pending = success(
        h.engine
            .services
            .merge
            .apply(retry.clone(), request.clone())
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    idle(&h, mount.branch).await;
    success(
        h.engine
            .services
            .mounts
            .release(
                context(),
                ReleaseMount {
                    operation: pending.id,
                    expected_binding_generation: fs.generation,
                },
            )
            .await
            .unwrap(),
    );
    let saved = success(
        h.engine
            .services
            .operations
            .get(&actor(), pending.id)
            .await
            .unwrap(),
    );
    assert!(saved.local_saved && saved.remote_confirmed);
    let Some(CommandResult::Revision { revision, .. }) = saved.result else {
        panic!("missing merged revision")
    };
    assert_eq!(
        h.local.revision(revision).await.unwrap().unwrap().parents,
        vec![head, candidate.source]
    );
    let fs = current_context(&h, mount.id).await;
    for name in ["source.txt", "target.txt"] {
        h.engine
            .services
            .files
            .lookup(&fs, InodeId::ROOT, name)
            .await
            .unwrap();
    }
    let repeated = success(h.engine.services.merge.apply(retry, request).await.unwrap());
    assert_eq!(repeated.id, saved.id);
    assert_eq!(repeated.result, saved.result);
}

#[tokio::test]
async fn merge_apply_preserves_changes_made_after_preview() {
    let (h, mount, fs, _, candidate) = prepared_merge().await;
    h.write_new(&fs, "newer.txt", b"keep newer").await;
    let before = branch(&h, mount.branch).await;
    let failed = command_error(
        h.engine
            .services
            .merge
            .apply(
                context(),
                ApplyMerge {
                    merge: candidate.id,
                    candidate: candidate.revision,
                    target: candidate.target,
                    durability: Durability::Remote,
                },
            )
            .await,
    );
    assert_eq!(failed.code, ErrorCode::TargetMoved);
    assert_eq!(branch(&h, mount.branch).await, before);
    h.engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "newer.txt")
        .await
        .unwrap();
}

#[tokio::test]
async fn clean_follower_fsync_checks_durability_without_claiming_authority() {
    let remote = Arc::new(MemoryRemote::default());
    let (h, workspace, mount, fs) = released_work(remote.clone()).await;
    let before = branch(&h, mount.branch).await;
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: mount.branch,
    };
    let reference = remote.get(&key).await.unwrap().unwrap();
    let local = h.engine.services.files.fsync(&fs).await.unwrap();
    assert!(local.local_saved && !local.remote_confirmed && local.operation.is_none());
    success(
        h.engine
            .services
            .mounts
            .unmount(context(), mount.id, mount.generation)
            .await
            .unwrap(),
    );
    let prepared = success(
        h.engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: true,
                    workspace: workspace.id,
                    branch: Some(mount.branch),
                    revision: None,
                    path: h.directory.path().join("remote-durability"),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Remote,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding }) = prepared.result else {
        panic!("missing mount")
    };
    let fs = current_context(&h, binding.id).await;
    let confirmed = h.engine.services.files.fsync(&fs).await.unwrap();
    assert!(confirmed.local_saved && confirmed.remote_confirmed && confirmed.operation.is_none());
    assert_eq!(branch(&h, mount.branch).await, before);
    assert_eq!(
        remote.get(&key).await.unwrap().unwrap().version,
        reference.version
    );
    let mut observed = h
        .local
        .remote_refs(workspace.id)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.branch == mount.branch)
        .unwrap();
    observed.durable_seq += 1;
    observed.source_generation += 1;
    h.local
        .commit(LocalCommit {
            remote_refs: vec![ObservedBranchRef::new(observed, "unconfirmed".into(), 0)],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        h.engine.services.files.fsync(&fs).await.unwrap_err().code,
        ErrorCode::RemoteUnknown
    );
    assert_eq!(branch(&h, mount.branch).await, before);
}

#[tokio::test]
async fn command_authority_respects_configuration_permissions_and_branch_protection() {
    let remote = Arc::new(MemoryRemote::default());
    let (h, config) = configurable(remote, true).await;
    let workspace = h.workspace().await;
    let (mount, fs) = h.branch_mount(workspace.id, "work", None).await;
    h.write_new(&fs, "work.txt", b"preserved work").await;
    configure(&h, &mount, Some(1)).await;
    idle(&h, mount.branch).await;
    let before = branch(&h, mount.branch).await;
    let request = commit_request(&before, CommitKind::Commit);
    let denied = command_error(
        h.engine
            .services
            .revisions
            .commit(
                RequestContext {
                    principal: "reader".to_owned().try_into().unwrap(),
                    ..context()
                },
                request.clone(),
            )
            .await,
    );
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    assert_eq!(branch(&h, mount.branch).await, before);

    let mut future = request.clone();
    future.guard.authority_epoch += 100;
    assert_eq!(
        command_error(h.engine.services.revisions.commit(context(), future).await).code,
        ErrorCode::StaleAuthority
    );
    assert_eq!(branch(&h, mount.branch).await, before);

    set_enabled(&config, false);
    let retry = context();
    let failed = h
        .engine
        .services
        .revisions
        .commit(retry.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        failed.error.as_ref().unwrap().code,
        ErrorCode::StaleAuthority
    );
    assert_eq!(branch(&h, mount.branch).await, before);
    set_enabled(&config, true);
    let saved = success(
        h.engine
            .services
            .revisions
            .commit(retry, request)
            .await
            .unwrap(),
    );
    assert_eq!(saved.id, failed.id);
    idle(&h, mount.branch).await;

    let mut protected = branch(&h, mount.branch).await;
    protected.protected = true;
    h.local
        .commit(LocalCommit {
            branches: vec![protected.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let rejected = h
        .engine
        .services
        .revisions
        .commit(context(), commit_request(&protected, CommitKind::Commit))
        .await
        .unwrap();
    assert!(!rejected.local_saved && rejected.error.is_some());
    assert_eq!(branch(&h, mount.branch).await, protected);
}

#[tokio::test]
async fn a_new_remote_head_with_identical_files_rejects_a_stale_command_without_claiming_authority()
{
    let remote = Arc::new(MemoryRemote::default());
    let (first, workspace, mount, _) = released_work(remote.clone()).await;
    let second = new(Some(remote.clone())).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    automatic_mount(&second, workspace.id, mount.branch).await;
    let before = branch(&second, mount.branch).await;
    let request = commit_request(&before, CommitKind::Commit);
    first.commit(mount.branch).await;
    idle(&first, mount.branch).await;
    let latest = branch(&first, mount.branch).await;
    assert_eq!(latest.working_root, before.working_root);
    assert_ne!(latest.formal_head, before.formal_head);
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: mount.branch,
    };
    let published = remote.get(&key).await.unwrap().unwrap();
    let failed = command_error(
        second
            .engine
            .services
            .revisions
            .commit(context(), request)
            .await,
    );
    assert_eq!(failed.code, ErrorCode::TargetMoved);
    assert_eq!(branch(&second, mount.branch).await, before);
    assert_eq!(
        remote.get(&key).await.unwrap().unwrap().version,
        published.version
    );
}
