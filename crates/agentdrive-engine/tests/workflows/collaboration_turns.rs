use super::*;

#[tokio::test]
async fn overlapping_turns_checkpoint_individually_and_release_only_after_remote_confirmation() {
    let remote = Arc::new(MemoryRemote::default());
    let h = new(Some(remote.clone())).await;
    let workspace = h.workspace().await;
    let (mount, fs) = h.branch_mount(workspace.id, "overlap", None).await;
    configure(&h, &mount).await;
    event(&h, mount.id, "one", ExecutionPhase::Started).await;
    event(&h, mount.id, "two", ExecutionPhase::Started).await;
    h.write_new(&fs, "first.txt", b"first turn").await;
    let one = event(&h, mount.id, "one", ExecutionPhase::Completed).await;
    assert_eq!(one.active_executions, 1);
    let first = h.local.branch(mount.branch).await.unwrap().unwrap();
    let revision = h.local.revision(first.formal_head).await.unwrap().unwrap();
    assert_eq!(revision.kind, RevisionKind::Commit);
    assert!(revision.message.unwrap().contains("one"));
    assert_eq!(first.state, BranchState::Writable);
    h.sync(workspace.id, SyncDirection::Push).await;
    let key = RefKey::Branch {
        workspace: workspace.id,
        branch: first.id,
    };
    let reference: BranchRef = decode(&remote.get(&key).await.unwrap().unwrap().bytes).unwrap();
    assert_eq!(reference.formal_head, first.formal_head);
    assert!(!reference.write_available);

    let file = h.write_new(&fs, "second.txt", b"second turn").await;
    let fd = h
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
    remote.set_available(false);
    event(&h, mount.id, "two", ExecutionPhase::Completed).await;
    let second = h.local.branch(mount.branch).await.unwrap().unwrap();
    assert_ne!(first.formal_head, second.formal_head);
    let revision = h.local.revision(second.formal_head).await.unwrap().unwrap();
    assert_eq!(revision.kind, RevisionKind::Commit);
    assert_eq!(revision.parents, vec![first.formal_head]);
    let count = h.local.revisions(workspace.id).await.unwrap().len();
    event(&h, mount.id, "two", ExecutionPhase::Completed).await;
    event(&h, mount.id, "two", ExecutionPhase::Started).await;
    assert_eq!(h.local.revisions(workspace.id).await.unwrap().len(), count);
    assert!(h.engine.collaborate_due().await.is_err());
    assert_eq!(
        h.local.branch(mount.branch).await.unwrap().unwrap().state,
        BranchState::Writable
    );
    assert_eq!(
        h.engine
            .services
            .files
            .write(&fs, fd.id, 0, Bytes::from_static(b"late"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::BranchBusy
    );
    assert_eq!(
        h.engine
            .services
            .files
            .read(&fs, fd.id, 0, 20)
            .await
            .unwrap()
            .as_ref(),
        b"second turn"
    );
    assert!(
        !h.engine
            .services
            .files
            .readdir(&fs, InodeId::ROOT)
            .await
            .unwrap()
            .is_empty()
    );
    let next = CollaborationEvent {
        mount: mount.id,
        source: "test-runtime".into(),
        thread_id: "thread".into(),
        turn_id: "three".into(),
        phase: ExecutionPhase::Started,
    };
    let error = h
        .engine
        .services
        .collaboration
        .execution_event(&actor(), next.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BranchBusy);
    remote.set_available(true);
    h.engine.collaborate_due().await.unwrap();
    let reference: BranchRef = decode(&remote.get(&key).await.unwrap().unwrap().bytes).unwrap();
    assert_eq!(reference.formal_head, second.formal_head);
    assert_eq!(reference.working_root, second.working_root);
    assert!(reference.write_available);
    assert_eq!(
        h.engine
            .services
            .files
            .write(&fs, fd.id, 0, Bytes::from_static(b"late"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::StaleBinding
    );
    let fresh = current_context(&h, mount.id).await;
    h.engine
        .services
        .files
        .can_unlink(&fresh, InodeId::ROOT, "second.txt", false, None)
        .await
        .unwrap();
    h.engine
        .services
        .files
        .unlink(&fresh, InodeId::ROOT, "second.txt", false)
        .await
        .unwrap();
    let engine = h.engine.clone();
    let closed = tokio::spawn(async move {
        engine.services.files.release(&fs, fd.id).await.unwrap();
        engine.services.files.release(&fs, fd.id).await.unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(closed.code, ErrorCode::StaleBinding);
    h.engine
        .services
        .collaboration
        .execution_event(&actor(), next)
        .await
        .unwrap();
    let fresh = current_context(&h, mount.id).await;
    h.write_new(&fresh, "fresh.txt", b"new turn").await;
}

#[tokio::test]
async fn restart_discards_an_unpublished_idle_release_from_an_older_daemon() {
    let remote = Arc::new(MemoryRemote::default());
    let h = new(Some(remote.clone())).await;
    let workspace = h.workspace().await;
    let (mount, _) = h.branch_mount(workspace.id, "migration", None).await;
    configure(&h, &mount).await;
    let record = h.local.collaboration(mount.branch).await.unwrap().unwrap();
    let branch = h.local.branch(mount.branch).await.unwrap().unwrap();
    let legacy = format!(
        r#"{{
        "workspace":"{}", "branch":"{}", "actor":"{}", "phase":"waiting_for_handles",
        "settings":{{"enabled":true,"release_on_event":true,"idle_timeout_secs":1}},
        "release":{{"operation":"{}", "authority_epoch":{}, "mutation_seq":{}, "reason":"idle"}},
        "executions":[], "transition":null, "recovery":null, "error":null
    }}"#,
        record.workspace,
        record.branch,
        record.actor.as_str(),
        OperationId::new(),
        branch.authority_epoch,
        branch.mutation_seq
    );
    h.local
        .commit(LocalCommit {
            collaborations: vec![decode(legacy.as_bytes()).unwrap()],
            ..Default::default()
        })
        .await
        .unwrap();
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = h;
    drop((local, engine, clock, driver));
    let (local, engine, clock, _) = open_collaborating(directory.path(), Some(remote)).await;
    clock.advance(1_000_000);
    engine.collaborate_due().await.unwrap();
    assert_eq!(
        local.branch(mount.branch).await.unwrap().unwrap().state,
        BranchState::Writable
    );
    assert!(
        local
            .collaboration(mount.branch)
            .await
            .unwrap()
            .unwrap()
            .release
            .is_none()
    );
}

#[tokio::test]
async fn only_a_writable_working_mount_can_submit_turns_and_the_worker_authors_its_commit() {
    let h = new(Some(Arc::new(MemoryRemote::default()))).await;
    let worker: Principal = "worker".to_owned().try_into().unwrap();
    let created = success(
        h.engine
            .services
            .workspace
            .create(
                context(),
                CreateWorkspace {
                    name: "shared".into(),
                    grants: vec![Grant {
                        principal: worker.clone(),
                        branch: None,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                    max_file_bytes: None,
                    max_working_bytes: None,
                    max_inodes: None,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Workspace { workspace }) = created.result else {
        panic!("workspace not created")
    };
    let (mount, fs) = h.branch_mount(workspace.id, "work", None).await;
    configure(&h, &mount).await;
    let historical = success(
        h.engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    workspace: workspace.id,
                    automatic: false,
                    branch: Some(mount.branch),
                    revision: Some(workspace.initial_revision),
                    path: h.directory.path().join("history"),
                    access: AccessMode::ReadOnly,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding: history }) = historical.result else {
        panic!("missing historical mount")
    };
    let mut request = CollaborationEvent {
        mount: history.id,
        source: "runtime".into(),
        thread_id: "chat".into(),
        turn_id: "article".into(),
        phase: ExecutionPhase::Started,
    };
    assert_eq!(
        h.engine
            .services
            .collaboration
            .execution_event(&worker, request.clone())
            .await
            .unwrap_err()
            .code,
        ErrorCode::ReadOnly
    );
    request.mount = mount.id;
    h.engine
        .services
        .collaboration
        .execution_event(&worker, request.clone())
        .await
        .unwrap();
    h.write_new(&fs, "article.txt", b"draft").await;
    request.phase = ExecutionPhase::Completed;
    h.engine
        .services
        .collaboration
        .execution_event(&worker, request)
        .await
        .unwrap();
    let saved = h.local.branch(mount.branch).await.unwrap().unwrap();
    assert_eq!(
        h.local
            .revision(saved.formal_head)
            .await
            .unwrap()
            .unwrap()
            .actor,
        worker
    );
    h.engine.collaborate_due().await.unwrap();
    assert_eq!(
        h.local.branch(mount.branch).await.unwrap().unwrap().state,
        BranchState::Available
    );
}

#[tokio::test]
async fn incomplete_registration_cannot_acknowledge_a_terminal_event() {
    let h = new(Some(Arc::new(MemoryRemote::default()))).await;
    let workspace = h.workspace().await;
    let (mount, _) = h
        .branch_mount(workspace.id, "interrupted-registration", None)
        .await;
    configure(&h, &mount).await;
    event(&h, mount.id, "resume", ExecutionPhase::Started).await;
    let mut registration = h
        .local
        .operations(workspace.id)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.name == "collaboration_execution")
        .unwrap();
    let mut record = h.local.collaboration(mount.branch).await.unwrap().unwrap();
    // Reproduce a restart after reserving the request ID and before registering the execution.
    registration.phase = OperationPhase::Reserved;
    registration.local_saved = false;
    registration.result = None;
    record.executions.clear();
    h.local
        .commit(LocalCommit {
            operations: vec![registration],
            collaborations: vec![record],
            ..Default::default()
        })
        .await
        .unwrap();
    let completed = CollaborationEvent {
        mount: mount.id,
        source: "test-runtime".into(),
        thread_id: "thread".into(),
        turn_id: "resume".into(),
        phase: ExecutionPhase::Completed,
    };
    assert_eq!(
        h.engine
            .services
            .collaboration
            .execution_event(&actor(), completed)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let registered = event(&h, mount.id, "resume", ExecutionPhase::Started).await;
    assert_eq!(registered.active_executions, 1);
    event(&h, mount.id, "resume", ExecutionPhase::Completed).await;
    h.engine.collaborate_due().await.unwrap();
    assert_eq!(
        h.local.branch(mount.branch).await.unwrap().unwrap().state,
        BranchState::Available
    );
}
