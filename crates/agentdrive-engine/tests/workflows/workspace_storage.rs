use super::*;

fn storage(remote: &Arc<MemoryRemote>) -> Arc<RemoteStorage> {
    Arc::new(RemoteStorage {
        objects: remote.clone(),
        refs: remote.clone(),
    })
}

struct TestStorage {
    engine: Arc<Engine>,
    target: String,
    remote: Arc<RemoteStorage>,
}

#[async_trait::async_trait]
impl WorkspaceInitializer for TestStorage {
    fn storage_target(&self) -> Option<&str> {
        Some(&self.target)
    }
    async fn prepare(&self, id: WorkspaceId) -> Result<()> {
        let mut routes = (*self.engine.storage_routes()).clone();
        routes.bind(id, self.target.clone(), self.remote.clone());
        self.engine
            .configure_workspace_storage(routes, || Ok(()))
            .await
    }
    async fn discard(&self, _: WorkspaceId) -> Result<()> {
        Ok(())
    }
}

async fn remote_workspace(
    harness: &Harness,
    target: &str,
    remote: &Arc<MemoryRemote>,
) -> Workspace {
    let request = CreateWorkspace {
        name: target.into(),
        grants: vec![],
        max_file_bytes: None,
        max_working_bytes: None,
        max_inodes: None,
    };
    let binding = TestStorage {
        engine: harness.engine.clone(),
        target: target.into(),
        remote: storage(remote),
    };
    let operation = success(
        harness
            .engine
            .services
            .workspace
            .create_configured(context(), request, Some(&binding))
            .await
            .unwrap(),
    );
    let Some(CommandResult::Workspace { workspace }) = operation.result else {
        panic!("expected workspace")
    };
    workspace
}

#[tokio::test]
async fn workspace_routes_isolate_publication_discovery_and_cold_reads() {
    let a = Arc::new(MemoryRemote::default());
    let b = Arc::new(MemoryRemote::default());
    let writer = Harness::new(None).await;
    let first = remote_workspace(&writer, "first-store", &a).await;
    let second = remote_workspace(&writer, "second-store", &b).await;
    let offline = writer.workspace().await;
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

    let reader = Harness::new(None).await;
    reader
        .engine
        .configure_workspace_storage((*writer.engine.storage_routes()).clone(), || Ok(()))
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

    let mut next = (*writer.engine.storage_routes()).clone();
    next.bind(first.id, "second-store".into(), storage(&b));
    let err = writer
        .engine
        .configure_workspace_storage(next, || panic!("must reject before persisting"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test]
async fn local_workspace_retains_confirmed_history_while_another_can_evict() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(None).await;
    let keep = harness.workspace().await;
    let evict = remote_workspace(&harness, "remote-store", &remote).await;
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
        if workspace.id == evict.id {
            harness.sync(workspace.id, SyncDirection::Push).await;
        }
    }
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
        fn storage_target(&self) -> Option<&str> {
            None
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
