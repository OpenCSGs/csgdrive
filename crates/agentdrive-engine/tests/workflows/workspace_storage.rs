use super::*;

fn storage(remote: &Arc<MemoryRemote>) -> Arc<RemoteStorage> {
    Arc::new(RemoteStorage {
        objects: remote.clone(),
        refs: remote.clone(),
    })
}

#[tokio::test]
async fn workspace_routes_isolate_publication_discovery_and_cold_reads() {
    let a = Arc::new(MemoryRemote::default());
    let b = Arc::new(MemoryRemote::default());
    let writer = Harness::new(None).await;
    let first = writer.workspace().await;
    let second = writer.workspace().await;
    let offline = writer.workspace().await;
    let routes = StorageRoutes::new(
        Some(storage(&a)),
        [(second.id, Some(storage(&b))), (offline.id, None)].into(),
    );
    writer
        .engine
        .switch_storage_routes(routes.clone(), |_| true, &mut || Ok(()))
        .await
        .unwrap();

    let mut revisions = Vec::new();
    for (workspace, text) in [(&first, "first"), (&second, "second"), (&offline, "local")] {
        let (mount, fs) = writer.branch_mount(workspace.id, text, None).await;
        writer.write_new(&fs, "input", text.as_bytes()).await;
        revisions.push(writer.commit(mount.branch).await);
    }
    writer.engine.sync_pending().await.unwrap();
    assert_eq!(a.list_workspaces().await.unwrap(), vec![first.id]);
    assert_eq!(b.list_workspaces().await.unwrap(), vec![second.id]);
    assert!(!writer.local.sync_jobs(offline.id).await.unwrap().is_empty());

    // A previous default namespace can still contain workspace records after migration.
    let key = RefKey::Workspace(second.id);
    let mut obsolete: WorkspaceControl =
        decode(&b.get(&key).await.unwrap().unwrap().bytes).unwrap();
    obsolete.workspace.deleted = true;
    assert!(matches!(
        a.compare_exchange(RefUpdate {
            key,
            expected_version: None,
            bytes: encode(&obsolete).unwrap().into()
        })
        .await
        .unwrap(),
        CasOutcome::Applied { .. }
    ));

    let reader = Harness::new(None).await;
    reader
        .engine
        .switch_storage_routes(routes, |_| true, &mut || Ok(()))
        .await
        .unwrap();
    let discovered: std::collections::BTreeSet<_> = reader
        .engine
        .services
        .workspace
        .discover(&actor())
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(discovered, [first.id, second.id].into());
    for ((workspace, expected), revision) in [(&first, "first"), (&second, "second")]
        .into_iter()
        .zip(revisions)
    {
        reader.sync(workspace.id, SyncDirection::Pull).await;
        let bytes = reader
            .engine
            .services
            .revisions
            .read_file(&actor(), workspace.id, revision, path("/input"), 0, 64)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), expected.as_bytes());
    }

    let migrated = Arc::new(MemoryRemote::default());
    b.set_available(false);
    let mut next = (*writer.engine.storage_routes()).clone();
    next.workspaces.insert(first.id, Some(storage(&migrated)));
    writer
        .engine
        .switch_storage_routes(next, |id| id == first.id, &mut || Ok(()))
        .await
        .unwrap();
    assert_eq!(migrated.list_workspaces().await.unwrap(), vec![first.id]);
}

#[tokio::test]
async fn local_workspace_retains_confirmed_history_while_another_can_evict() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(Some(remote)).await;
    let keep = harness.workspace().await;
    let evict = harness.workspace().await;
    let mut objects = Vec::new();
    for (workspace, name) in [(&keep, "local"), (&evict, "remote")] {
        let (mount, fs) = harness.branch_mount(workspace.id, name, None).await;
        let file = harness.write_new(&fs, "old", b"previous version").await;
        harness.commit(mount.branch).await;
        objects.push(
            harness
                .local
                .node(mount.branch, file.id)
                .await
                .unwrap()
                .unwrap()
                .inode
                .content
                .unwrap(),
        );
        harness
            .engine
            .services
            .files
            .unlink(&fs, InodeId::ROOT, "old", false)
            .await
            .unwrap();
        harness.commit(mount.branch).await;
        harness.sync(workspace.id, SyncDirection::Push).await;
    }
    let mut routes = (*harness.engine.storage_routes()).clone();
    routes.workspaces.insert(keep.id, None);
    harness
        .engine
        .switch_storage_routes(routes, |id| id == keep.id, &mut || Ok(()))
        .await
        .unwrap();
    harness.engine.services.cache.collect().await.unwrap();
    assert!(harness.local.contains(keep.id, &objects[0]).await.unwrap());
    assert!(!harness.local.contains(evict.id, &objects[1]).await.unwrap());
}

#[tokio::test]
async fn configured_creation_reuses_the_reserved_id_after_interruption() {
    struct Binding {
        prepared: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
        interrupt: bool,
    }
    #[async_trait::async_trait]
    impl WorkspaceInitializer for Binding {
        fn selection(&self) -> &str {
            "local"
        }
        async fn prepare(&self, id: WorkspaceId) -> Result<()> {
            self.prepared.send(id).unwrap();
            if self.interrupt {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn discard(&self, _: WorkspaceId) -> Result<()> {
            Ok(())
        }
    }

    let harness = Harness::new(None).await;
    let service = harness.engine.services.workspace.clone();
    let context = context();
    let request = CreateWorkspace {
        name: "recover a creation".into(),
        grants: vec![],
        max_file_bytes: None,
        max_working_bytes: None,
        max_inodes: None,
    };
    let (prepared, mut received) = tokio::sync::mpsc::unbounded_channel();
    let first = {
        let service = service.clone();
        let context = context.clone();
        let request = request.clone();
        let binding = Binding {
            prepared: prepared.clone(),
            interrupt: true,
        };
        tokio::spawn(async move {
            service
                .create_configured(context, request, Some(&binding))
                .await
        })
    };
    let reserved = received.recv().await.unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let retry = Binding {
        prepared,
        interrupt: false,
    };
    let saved = service
        .create_configured(context, request, Some(&retry))
        .await
        .unwrap();
    assert_eq!(received.recv().await, Some(reserved));
    assert!(saved.local_saved);
    assert_eq!(saved.workspace, Some(reserved));
    assert_eq!(service.list(&actor()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn migration_requires_the_current_source_head_of_every_branch() {
    let source = Arc::new(MemoryRemote::default());
    let target = Arc::new(MemoryRemote::default());
    let first = Harness::new(Some(source.clone())).await;
    let workspace = first.workspace().await;
    first.sync(workspace.id, SyncDirection::Push).await;
    let other = Harness::new(Some(source.clone())).await;
    other.sync(workspace.id, SyncDirection::Pull).await;
    let (mount, fs) = other.branch_mount(workspace.id, "review", None).await;
    other.write_new(&fs, "draft", b"review").await;
    other.commit(mount.branch).await;
    other.sync(workspace.id, SyncDirection::Push).await;
    let mut routes = (*first.engine.storage_routes()).clone();
    routes
        .workspaces
        .insert(workspace.id, Some(storage(&target)));
    for revision in 0..2 {
        let error = first
            .engine
            .switch_storage_routes(routes.clone(), |id| id == workspace.id, &mut || {
                panic!("migration published incomplete history")
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetMoved);
        assert!(target.list_workspaces().await.unwrap().is_empty());
        first.sync(workspace.id, SyncDirection::Pull).await;
        if revision == 0 {
            other.write_new(&fs, "final", b"another revision").await;
            other.commit(mount.branch).await;
            other.sync(workspace.id, SyncDirection::Push).await;
        }
    }
    let interrupted_target = Arc::new(MemoryRemote::default());
    let mut interrupted_routes = routes.clone();
    interrupted_routes
        .workspaces
        .insert(workspace.id, Some(storage(&interrupted_target)));
    let (publishing, resume) = interrupted_target.pause_next_branch_cas();
    let migrate = async {
        first
            .engine
            .switch_storage_routes(
                interrupted_routes.clone(),
                |id| id == workspace.id,
                &mut || panic!("source advanced before persistence"),
            )
            .await
    };
    let advance_source = async {
        publishing.await.unwrap();
        other
            .write_new(&fs, "late", b"published during migration")
            .await;
        other.commit(mount.branch).await;
        other.sync(workspace.id, SyncDirection::Push).await;
        resume.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(migrate, advance_source);
    assert_eq!(result.unwrap_err().code, ErrorCode::TargetMoved);
    first.sync(workspace.id, SyncDirection::Pull).await;
    let error = first
        .engine
        .switch_storage_routes(interrupted_routes, |id| id == workspace.id, &mut || {
            panic!("destination has a divergent foreign branch")
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetMoved);
    first
        .engine
        .switch_storage_routes(routes, |id| id == workspace.id, &mut || Ok(()))
        .await
        .unwrap();
    for branch in source.list_branches(workspace.id).await.unwrap() {
        let key = RefKey::Branch {
            workspace: workspace.id,
            branch,
        };
        assert_eq!(
            target.get(&key).await.unwrap().unwrap().bytes,
            source.get(&key).await.unwrap().unwrap().bytes
        );
    }
}
