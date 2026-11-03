use super::*;

#[tokio::test]
async fn observed_deletion_waits_for_handles_and_retires_the_mount_after_close() {
    let remote = Arc::new(MemoryRemote::default());
    let first = Harness::new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    first.sync(workspace.id, SyncDirection::Push).await;
    let second = Harness::new(Some(remote.clone())).await;
    second
        .engine
        .services
        .workspace
        .discover(&actor())
        .await
        .unwrap();
    let (mount, fs) = second.branch_mount(workspace.id, "work", None).await;
    let node = second.write_new(&fs, "work.txt", b"retained").await;
    second.commit(mount.branch).await;
    second.sync(workspace.id, SyncDirection::Push).await;
    let handle = second
        .engine
        .services
        .files
        .open(
            &fs,
            node.id,
            OpenMode {
                read: true,
                write: false,
                append: false,
            },
        )
        .await
        .unwrap();
    success(
        first
            .engine
            .services
            .workspace
            .delete(context(), workspace.id)
            .await
            .unwrap(),
    );
    second
        .engine
        .services
        .workspace
        .discover(&actor())
        .await
        .unwrap();
    assert!(
        second
            .engine
            .services
            .workspace
            .list(&actor())
            .await
            .unwrap()
            .is_empty()
    );
    assert_ne!(
        second.local.mount(mount.id).await.unwrap().unwrap().state,
        MountState::Released
    );
    assert_eq!(
        second
            .engine
            .services
            .files
            .lookup(&fs, InodeId::ROOT, "work.txt")
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    second
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    second.clock.advance(15_001);
    second.engine.refresh_requested_history().await;
    assert_eq!(
        second.local.mount(mount.id).await.unwrap().unwrap().state,
        MountState::Released
    );
    assert!(
        second
            .local
            .revisions(workspace.id)
            .await
            .unwrap()
            .iter()
            .any(|r| r.branch == Some(mount.branch))
    );
}

#[tokio::test]
async fn discovery_loads_remote_metadata_and_global_deletion_prevents_reimport() {
    let remote = Arc::new(MemoryRemote::default());
    let first = Harness::new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    first.sync(workspace.id, SyncDirection::Push).await;
    let second = Harness::new(Some(remote.clone())).await;
    let listed = second
        .engine
        .services
        .workspace
        .discover(&actor())
        .await
        .unwrap();
    assert_eq!(listed, vec![workspace.clone()]);
    let status = second
        .engine
        .services
        .workspace
        .status(&actor(), workspace.id)
        .await
        .unwrap();
    assert!(!status.remote_branches.is_empty());
    assert!(status.mounts.is_empty());
    remote.lose_next_cas_response();
    let retry = context();
    let deleted = success(
        first
            .engine
            .services
            .workspace
            .delete(retry.clone(), workspace.id)
            .await
            .unwrap(),
    );
    let replay = success(
        first
            .engine
            .services
            .workspace
            .delete(retry, workspace.id)
            .await
            .unwrap(),
    );
    assert_eq!(deleted.id, replay.id);
    assert!(
        first
            .engine
            .services
            .workspace
            .list(&actor())
            .await
            .unwrap()
            .is_empty()
    );
    second.clock.advance(15_001);
    second.engine.refresh_requested_history().await;
    assert!(
        second
            .engine
            .services
            .workspace
            .list(&actor())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        second
            .local
            .workspace(workspace.id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    let third = Harness::new(Some(remote)).await;
    assert!(
        third
            .engine
            .services
            .workspace
            .discover(&actor())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        third
            .engine
            .services
            .replica
            .sync(
                context(),
                SyncRequest {
                    workspace: workspace.id,
                    direction: SyncDirection::Pull,
                    branches: None,
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        second
            .engine
            .services
            .replica
            .sync(
                context(),
                SyncRequest {
                    workspace: workspace.id,
                    direction: SyncDirection::Push,
                    branches: None,
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn deletion_rejects_mounts_and_reuses_the_request_after_unmounting() {
    let h = Harness::new(Some(Arc::new(MemoryRemote::default()))).await;
    let workspace = h.workspace().await;
    let (mount, _) = h.branch_mount(workspace.id, "work", None).await;
    h.sync(workspace.id, SyncDirection::Push).await;
    let retry = context();
    let failed = h
        .engine
        .services
        .workspace
        .delete(retry.clone(), workspace.id)
        .await
        .unwrap();
    assert_eq!(failed.error.as_ref().unwrap().code, ErrorCode::MountBusy);
    assert!(
        !h.local
            .workspace(workspace.id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    success(
        h.engine
            .services
            .mounts
            .unmount(context(), mount.id, mount.generation)
            .await
            .unwrap(),
    );
    let deleted = success(
        h.engine
            .services
            .workspace
            .delete(retry, workspace.id)
            .await
            .unwrap(),
    );
    assert_eq!(deleted.id, failed.id);
}

#[tokio::test]
async fn deletion_waits_for_publication_and_rejects_unauthorized_callers() {
    let remote = Arc::new(MemoryRemote::default());
    let h = Harness::new(Some(remote.clone())).await;
    let workspace = h.workspace().await;
    let retry = context();
    let pending = h
        .engine
        .services
        .workspace
        .delete(retry.clone(), workspace.id)
        .await
        .unwrap();
    assert_eq!(pending.error.unwrap().code, ErrorCode::Busy);
    h.sync(workspace.id, SyncDirection::Push).await;
    let denied = RequestContext {
        principal: "outsider".to_owned().try_into().unwrap(),
        ..context()
    };
    assert_eq!(
        h.engine
            .services
            .workspace
            .delete(denied, workspace.id)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(
        h.engine
            .services
            .workspace
            .discover(&"outsider".to_owned().try_into().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    let key = RefKey::Workspace(workspace.id);
    let value = remote.get(&key).await.unwrap().unwrap();
    let mut control: WorkspaceControl = decode(&value.bytes).unwrap();
    let active = OperationId::new();
    control.active_operations.insert(active, vec![]);
    remote
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(value.version),
            bytes: encode(&control).unwrap().into(),
        })
        .await
        .unwrap();
    let busy = h
        .engine
        .services
        .workspace
        .delete(retry.clone(), workspace.id)
        .await
        .unwrap();
    assert_eq!(busy.error.unwrap().code, ErrorCode::Busy);
    let value = remote.get(&key).await.unwrap().unwrap();
    control.active_operations.clear();
    remote
        .compare_exchange(RefUpdate {
            key,
            expected_version: Some(value.version),
            bytes: encode(&control).unwrap().into(),
        })
        .await
        .unwrap();
    success(
        h.engine
            .services
            .workspace
            .delete(retry, workspace.id)
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn local_workspace_deletion_is_persistent_and_cannot_be_reverted_by_stale_metadata() {
    let h = Harness::new(None).await;
    let workspace = h.workspace().await;
    // A local-only creation has no remote publication to complete.
    success(
        h.engine
            .services
            .workspace
            .delete(context(), workspace.id)
            .await
            .unwrap(),
    );
    assert_eq!(
        h.local
            .commit(LocalCommit {
                workspaces: vec![workspace.clone()],
                ..Default::default()
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = h;
    drop(engine);
    drop(local);
    drop(clock);
    drop(driver);
    let (_, engine, _, _) = open(directory.path(), None).await;
    assert!(
        engine
            .services
            .workspace
            .list(&actor())
            .await
            .unwrap()
            .is_empty()
    );
}
