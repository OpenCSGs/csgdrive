use super::*;

fn save_request(binding: &MountBinding, message: Option<String>) -> SaveMount {
    SaveMount {
        mount: binding.id,
        expected_binding_generation: binding.generation,
        durability: Durability::Local,
        wait_for_remote_ms: None,
        message,
    }
}

#[tokio::test]
async fn mount_save_preserves_messages_across_retries_unchanged_saves_and_sync() {
    let remote = Arc::new(MemoryRemote::default());
    let h = Harness::new(Some(remote.clone())).await;
    let workspace = h.workspace().await;
    let (binding, fs) = h.branch_mount(workspace.id, "article", None).await;
    h.write_new(&fs, "article.md", b"reviewed article").await;
    let message = "完善文章说明\n保留按项目组织资料的描述";
    let request = SaveMount {
        durability: Durability::Remote,
        ..save_request(&binding, Some(message.into()))
    };
    let identity = context();
    let saved = success(
        h.engine
            .services
            .mounts
            .save(identity.clone(), request.clone())
            .await
            .unwrap(),
    );
    assert!(saved.local_saved && saved.remote_confirmed);
    let CommandResult::Revision { revision, branch } = saved.result.clone().unwrap() else {
        panic!("revision result missing");
    };
    let committed = h.local.revision(revision).await.unwrap().unwrap();
    assert_eq!(committed.message.as_deref(), Some(message));
    assert_eq!(committed.kind, RevisionKind::Commit);
    assert_eq!(
        h.local.branch(branch).await.unwrap().unwrap().formal_head,
        revision
    );

    let replay = success(
        h.engine
            .services
            .mounts
            .save(identity.clone(), request.clone())
            .await
            .unwrap(),
    );
    assert_eq!(replay.id, saved.id);
    assert_eq!(replay.result, saved.result);
    let changed_message = SaveMount {
        message: Some("another description".into()),
        ..request
    };
    let conflict = h
        .engine
        .services
        .mounts
        .save(identity, changed_message.clone())
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::RequestIdConflict);

    let revision_count = h.local.revisions(workspace.id).await.unwrap().len();
    let unchanged = success(
        h.engine
            .services
            .mounts
            .save(context(), changed_message)
            .await
            .unwrap(),
    );
    assert_eq!(unchanged.result, saved.result);
    assert_eq!(
        h.local.revisions(workspace.id).await.unwrap().len(),
        revision_count
    );
    assert_eq!(
        h.local.revision(revision).await.unwrap().unwrap(),
        committed
    );

    let peer = Harness::new(Some(remote)).await;
    peer.sync(workspace.id, SyncDirection::Pull).await;
    let imported = peer.local.revision(revision).await.unwrap().unwrap();
    assert_eq!(imported.message.as_deref(), Some(message));
    assert_eq!(imported.root, committed.root);
}

#[tokio::test]
async fn mount_save_message_validation_preserves_changes_and_accepts_utf8_boundary() {
    let h = Harness::new(None).await;
    let workspace = h.workspace().await;
    let (binding, fs) = h.branch_mount(workspace.id, "limits", None).await;
    h.write_new(&fs, "draft.md", b"pending changes").await;
    let before = h.local.branch(binding.branch).await.unwrap().unwrap();
    let revision_count = h.local.revisions(workspace.id).await.unwrap().len();
    for message in ["bad\0message".into(), format!("{}ab", "中".repeat(1365))] {
        let rejected = h
            .engine
            .services
            .mounts
            .save(context(), save_request(&binding, Some(message)))
            .await
            .unwrap();
        assert_eq!(rejected.error.unwrap().code, ErrorCode::InvalidArgument);
        assert!(!rejected.local_saved);
        assert_eq!(
            h.local.branch(binding.branch).await.unwrap().unwrap(),
            before
        );
        assert_eq!(
            h.local.revisions(workspace.id).await.unwrap().len(),
            revision_count
        );
    }
    let message = format!("{}a", "中".repeat(1365));
    assert_eq!(message.len(), 4096);
    let saved = success(
        h.engine
            .services
            .mounts
            .save(context(), save_request(&binding, Some(message.clone())))
            .await
            .unwrap(),
    );
    let CommandResult::Revision { revision, .. } = saved.result.unwrap() else {
        panic!("revision result missing");
    };
    assert_eq!(
        h.local.revision(revision).await.unwrap().unwrap().message,
        Some(message)
    );
}
