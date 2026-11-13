use super::*;

#[tokio::test]
async fn running_sandbox_allows_turn_handoff_and_defers_recovery_branch_switch() {
    let remote = Arc::new(MemoryRemote::default());
    let f = fixture_with_remote(Some(remote.clone())).await;
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    let mount = f.local.mounts().await.unwrap().remove(0);
    f.engine
        .services
        .collaboration
        .configure(
            &f.owner,
            ConfigureCollaboration {
                mount: mount.id,
                expected_binding_generation: Some(mount.generation),
                enabled: true,
            },
        )
        .await
        .unwrap();
    f.engine.collaborate_due().await.unwrap();
    for (turn, phase) in [
        ("first", ExecutionPhase::Started),
        ("first", ExecutionPhase::Completed),
        ("second", ExecutionPhase::Started),
    ] {
        f.engine
            .services
            .collaboration
            .execution_event(
                &f.owner,
                CollaborationEvent {
                    mount: mount.id,
                    source: "sandbox-test".into(),
                    thread_id: "thread".into(),
                    turn_id: turn.into(),
                    phase,
                },
            )
            .await
            .unwrap();
        f.engine.collaborate_due().await.unwrap();
        if phase == ExecutionPhase::Completed {
            assert_eq!(
                f.local.branch(f.branch).await.unwrap().unwrap().state,
                BranchState::Available
            );
        }
    }
    let current = f.local.mount(mount.id).await.unwrap().unwrap();
    assert_eq!(current.branch, f.branch);
    assert!(current.generation > mount.generation);
    f.engine
        .services
        .files
        .create(
            &FsContext {
                mount: current.id,
                generation: current.generation,
                identity: LocalIdentity {
                    uid: 1000,
                    gid: 1000,
                },
            },
            InodeId::ROOT,
            "local-work.txt",
            FileKind::File,
            0o644,
        )
        .await
        .unwrap();

    let branch = f.local.branch(f.branch).await.unwrap().unwrap();
    let saved = f
        .engine
        .services
        .revisions
        .commit(
            RequestContext {
                principal: f.owner.clone(),
                request_id: OperationId::new().to_string(),
            },
            CommitRevision {
                message: None,
                guard: BranchGuard::from(&branch),
                kind: CommitKind::Commit,
                durability: Durability::Local,
            },
        )
        .await
        .unwrap();
    assert!(saved.error.is_none(), "{saved:?}");
    let key = RefKey::Branch {
        workspace: f.workspace,
        branch: f.branch,
    };
    let previous = remote.get(&key).await.unwrap().unwrap();
    let mut replacement: BranchRef = decode(&previous.bytes).unwrap();
    replacement.owner = LocationId::new();
    replacement.authority_epoch += 1;
    replacement.source_generation += 1;
    assert!(matches!(
        remote
            .compare_exchange(RefUpdate {
                key,
                expected_version: Some(previous.version),
                bytes: encode(&replacement).unwrap().into(),
            })
            .await
            .unwrap(),
        CasOutcome::Applied { .. }
    ));
    let sync = f
        .engine
        .services
        .replica
        .sync(
            RequestContext {
                principal: f.owner.clone(),
                request_id: OperationId::new().to_string(),
            },
            SyncRequest {
                workspace: f.workspace,
                branches: Some(vec![f.branch]),
                direction: SyncDirection::Push,
            },
        )
        .await;
    match sync {
        Err(error) => assert_eq!(error.code, ErrorCode::StaleAuthority),
        Ok(record) => assert_eq!(record.error.unwrap().code, ErrorCode::StaleAuthority),
    }
    assert_eq!(
        f.engine.collaborate_due().await.unwrap_err().code,
        ErrorCode::BranchBusy
    );
    assert_eq!(
        f.local.mount(mount.id).await.unwrap().unwrap().branch,
        f.branch
    );
    assert!(matches!(
        f.driver.status(&f.branch.to_string()).await.unwrap().state,
        SandboxState::Ready
    ));
    // Stopping releases the binding even when saving reports authority recovery.
    let stopped = f.sandbox.stop(&f.owner, f.workspace, f.branch).await;
    if let Err(error) = stopped {
        assert_eq!(error.code, ErrorCode::RecoveryRequired);
    }
    f.engine.collaborate_due().await.unwrap();
    let recovered = f.local.mount(mount.id).await.unwrap().unwrap();
    assert_ne!(recovered.branch, f.branch);
    assert_eq!(recovered.path, mount.path);
    let entries = f
        .engine
        .services
        .files
        .readdir(
            &FsContext {
                mount: recovered.id,
                generation: recovered.generation,
                identity: LocalIdentity {
                    uid: 1000,
                    gid: 1000,
                },
            },
            InodeId::ROOT,
        )
        .await
        .unwrap();
    assert!(entries.iter().any(|entry| entry.name == "local-work.txt"));
    assert!(matches!(
        f.driver.status(&f.branch.to_string()).await.unwrap().state,
        SandboxState::Stopped
    ));
}
