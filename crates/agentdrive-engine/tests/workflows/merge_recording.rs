use super::*;

fn candidate(record: &OperationRecord) -> MergeCandidate {
    match record.result.as_ref().unwrap() {
        CommandResult::Merge { candidate } => candidate.clone(),
        result => panic!("{result:?}"),
    }
}

async fn assert_remote_record(receiver: &Harness, expected: &OperationRecord) {
    let actual = receiver
        .local
        .operation(expected.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.merge_history, expected.merge_history);
    assert_eq!(actual.phase, expected.phase);
    assert_eq!(actual.error, expected.error);
    assert_eq!(actual.result, expected.result);
    assert_eq!(actual.remote_confirmed, expected.remote_confirmed);
    assert_eq!(actual.mount_ready, expected.mount_ready);
}

#[tokio::test]
async fn merge_history_replicates_candidates_and_final_receipts_without_moving_target() {
    let remote = Arc::new(MemoryRemote::default());
    let a = Harness::new(Some(remote.clone())).await;
    let b = Harness::new(Some(remote.clone())).await;
    let workspace = a.workspace().await;
    let (source, source_fs) = a.branch_mount(workspace.id, "source", None).await;
    let (target, target_fs) = a.branch_mount(workspace.id, "target", None).await;
    a.write_new(&source_fs, "article.md", b"Price requires confirmation.")
        .await;
    a.write_new(&target_fs, "article.md", b"Price is 99.").await;
    let source_revision = a.commit(source.branch).await;
    let target_revision = a.commit(target.branch).await;
    a.sync(workspace.id, SyncDirection::Push).await;
    b.sync(workspace.id, SyncDirection::Pull).await;
    let before = a.local.branch(target.branch).await.unwrap().unwrap();
    let request = PrepareMerge {
        workspace: workspace.id,
        source: source_revision,
        target: BranchGuard::from(&before),
        base: None,
    };
    let request_context = context();
    let prepared = success(
        a.engine
            .services
            .merge
            .prepare(request_context.clone(), request.clone())
            .await
            .unwrap(),
    );
    let prepared_candidate = candidate(&prepared);
    assert_eq!(prepared_candidate.conflicts.len(), 1);
    let history = prepared.merge_history.as_ref().unwrap();
    assert_eq!(history.source_branch, Some(source.branch));
    assert_eq!(history.base, Some(workspace.initial_revision));
    assert_eq!(history.events.len(), 2);
    assert_eq!(history.events[0].phase, OperationPhase::Reserved);
    assert_eq!(
        history.events[1].candidate,
        Some(prepared_candidate.revision)
    );
    assert_eq!(
        prepared,
        a.engine
            .services
            .merge
            .prepare(request_context, request)
            .await
            .unwrap()
    );

    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    assert_remote_record(&b, &prepared).await;
    assert!(
        !b.local
            .operation(prepared.id)
            .await
            .unwrap()
            .unwrap()
            .remote_confirmed
    );
    let after = a.local.branch(target.branch).await.unwrap().unwrap();
    assert_eq!(BranchGuard::from(&after), BranchGuard::from(&before));
    assert_eq!(after.working_root, before.working_root);
    assert_eq!(after.saved_seq, before.saved_seq);
    assert_eq!(after.mutation_seq, before.mutation_seq);
    assert!(after.source_generation > before.source_generation);
    assert_eq!(
        b.engine
            .services
            .revisions
            .read_file(
                &actor(),
                workspace.id,
                prepared_candidate.revision,
                path("/article.md"),
                0,
                1024
            )
            .await
            .unwrap(),
        "Price is 99."
    );

    let resolved = success(
        a.engine
            .services
            .merge
            .resolve(
                context(),
                ResolveMerge {
                    merge: prepared_candidate.id,
                    candidate: prepared_candidate.revision,
                    resolutions: vec![Resolution::Replace {
                        path: path("/article.md"),
                        content: b"Price: pending business approval.".to_vec(),
                        mode: 0o644,
                    }],
                },
            )
            .await
            .unwrap(),
    );
    let resolved_candidate = candidate(&resolved);
    let history = resolved.merge_history.as_ref().unwrap();
    assert_eq!(history.candidate_before, Some(prepared_candidate.revision));
    assert_eq!(history.resolutions[0].choice, "replace");
    assert_eq!(history.resolutions[0].path, path("/article.md"));
    assert_eq!(history.events.last().unwrap().conflict_count, Some(0));
    remote.set_available(false);
    assert!(a.engine.sync_pending().await.is_err());
    assert_eq!(
        a.local.operation(resolved.id).await.unwrap().unwrap(),
        resolved
    );
    remote.set_available(true);
    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    assert_remote_record(&b, &resolved).await;
    assert_eq!(
        b.engine
            .services
            .revisions
            .read_file(
                &actor(),
                workspace.id,
                resolved_candidate.revision,
                path("/article.md"),
                0,
                1024
            )
            .await
            .unwrap(),
        "Price: pending business approval."
    );
    assert_eq!(
        a.local
            .branch(target.branch)
            .await
            .unwrap()
            .unwrap()
            .formal_head,
        target_revision
    );

    let pending = success(
        a.engine
            .services
            .merge
            .apply(
                context(),
                ApplyMerge {
                    merge: resolved_candidate.id,
                    candidate: resolved_candidate.revision,
                    target: resolved_candidate.target,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    assert_remote_record(&b, &pending).await;
    a.driver
        .fail_mount
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed_release = a
        .engine
        .services
        .mounts
        .release(
            context(),
            ReleaseMount {
                operation: pending.id,
                expected_binding_generation: target_fs.generation,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        failed_release.error.as_ref().unwrap().code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        failed_release.merge_history.as_ref().unwrap().merge,
        Some(resolved_candidate.id)
    );
    let needs_mount = a.local.operation(pending.id).await.unwrap().unwrap();
    assert_eq!(needs_mount.phase, OperationPhase::CommittedMountPending);
    assert_eq!(needs_mount.mount_ready, Some(false));
    assert!(
        needs_mount
            .merge_history
            .as_ref()
            .unwrap()
            .events
            .iter()
            .any(|event| {
                event
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == ErrorCode::Unavailable)
            })
    );
    a.engine.sync_pending().await.unwrap();
    // Confirmation is another event and is published by the following pass.
    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    let confirmed = a.local.operation(pending.id).await.unwrap().unwrap();
    assert_remote_record(&b, &confirmed).await;
    assert_remote_record(&b, &failed_release).await;
    assert_eq!(confirmed.mount_ready, Some(false));
    let restored = success(
        a.engine
            .services
            .mounts
            .release(
                context(),
                ReleaseMount {
                    operation: pending.id,
                    expected_binding_generation: target_fs.generation,
                },
            )
            .await
            .unwrap(),
    );
    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    let applied = a.local.operation(pending.id).await.unwrap().unwrap();
    assert_remote_record(&b, &restored).await;
    assert_eq!(applied.phase, OperationPhase::Complete);
    assert!(applied.local_saved && applied.remote_confirmed);
    assert_eq!(applied.mount_ready, Some(true));
    assert_remote_record(&b, &applied).await;
    let history = applied.merge_history.as_ref().unwrap();
    assert!(
        history
            .events
            .iter()
            .any(|event| event.phase == OperationPhase::WaitingForQuiesce)
    );
    assert!(
        history
            .events
            .iter()
            .any(|event| event.phase == OperationPhase::CommittedMountPending)
    );
    assert!(history.events.last().unwrap().remote_confirmed);
    assert_eq!(history.events.last().unwrap().mount_ready, Some(true));
    let result = history.events.last().unwrap().result_revision.unwrap();
    assert_eq!(
        b.local.revision(result).await.unwrap().unwrap().parents,
        vec![target_revision, source_revision]
    );
    assert_eq!(
        b.engine
            .services
            .revisions
            .read_file(&actor(), workspace.id, result, path("/article.md"), 0, 1024)
            .await
            .unwrap(),
        "Price: pending business approval."
    );
    assert!(
        a.local
            .pending_merge_history(workspace.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(a.local.sync_jobs(workspace.id).await.unwrap().is_empty());
    let stable = a.local.branches(workspace.id).await.unwrap();
    a.engine.sync_pending().await.unwrap();
    assert_eq!(a.local.branches(workspace.id).await.unwrap(), stable);
}

#[tokio::test]
async fn failed_merge_preparations_sync_without_becoming_successful() {
    let remote = Arc::new(MemoryRemote::default());
    let a = Harness::new(Some(remote.clone())).await;
    let b = Harness::new(Some(remote)).await;
    let workspace = a.workspace().await;
    a.sync(workspace.id, SyncDirection::Push).await;
    let branch = a.local.branches(workspace.id).await.unwrap().remove(0);
    let request = PrepareMerge {
        workspace: workspace.id,
        source: RevisionId::new(),
        target: BranchGuard::from(&branch),
        base: None,
    };
    let ctx = context();
    let failed = a
        .engine
        .services
        .merge
        .prepare(ctx.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(failed.phase, OperationPhase::Failed);
    assert!(!failed.local_saved);
    assert!(failed.merge_history.as_ref().unwrap().merge.is_none());
    a.engine.sync_pending().await.unwrap();
    b.sync(workspace.id, SyncDirection::Pull).await;
    assert_remote_record(&b, &failed).await;
    assert_eq!(
        a.engine.services.merge.prepare(ctx, request).await.unwrap(),
        failed
    );
    assert_eq!(
        a.local
            .branch(branch.id)
            .await
            .unwrap()
            .unwrap()
            .formal_head,
        branch.formal_head
    );
    assert!(
        a.local
            .pending_merge_history(workspace.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ownership_publication_confirms_the_merge_history_it_carries() {
    for lose_response in [false, true] {
        let remote = Arc::new(MemoryRemote::default());
        let a = Harness::new(Some(remote.clone())).await;
        let b = Harness::new(Some(remote.clone())).await;
        let workspace = a.workspace().await;
        a.sync(workspace.id, SyncDirection::Push).await;
        let branch = a.local.branches(workspace.id).await.unwrap().remove(0);
        let failed = a
            .engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source: RevisionId::new(),
                    target: BranchGuard::from(&branch),
                    base: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(failed.phase, OperationPhase::Failed);
        assert_eq!(
            a.local
                .pending_merge_history(workspace.id)
                .await
                .unwrap()
                .len(),
            1
        );
        if lose_response {
            remote.lose_next_branch_cas_response();
        }
        success(
            a.engine
                .services
                .ownership
                .transfer(
                    context(),
                    TransferOwnership {
                        guard: BranchGuard::from(&branch),
                        target: b.local.location().await.unwrap(),
                    },
                )
                .await
                .unwrap(),
        );
        assert!(
            a.local
                .pending_merge_history(workspace.id)
                .await
                .unwrap()
                .is_empty()
        );
        let stopped = a.local.branch(branch.id).await.unwrap().unwrap();
        assert_eq!(stopped.state, BranchState::Stopped);
        assert_eq!(stopped.formal_head, branch.formal_head);
        b.sync(workspace.id, SyncDirection::Pull).await;
        assert_remote_record(&b, &failed).await;
    }
}
