use super::*;

struct Fixture {
    workspace: Workspace,
    source: Revision,
    target: Revision,
    candidate: MergeCandidate,
    revision: Revision,
    location: LocationId,
}

impl Fixture {
    async fn new(h: &Harness) -> Self {
        let workspace = h.workspace().await;
        let target = h
            .local
            .revision(workspace.initial_revision)
            .await
            .unwrap()
            .unwrap();
        let branch = h
            .local
            .branch(target.branch.unwrap())
            .await
            .unwrap()
            .unwrap();
        let source = Revision {
            id: RevisionId::new(),
            branch: Some(BranchId::new()),
            ..target.clone()
        };
        let revision = Revision {
            id: RevisionId::new(),
            branch: None,
            kind: RevisionKind::Candidate,
            ..target.clone()
        };
        let candidate = MergeCandidate {
            id: MergeId::new(),
            workspace: workspace.id,
            base: target.id,
            source: source.id,
            target: BranchGuard::from(&branch),
            revision: revision.id,
            conflicts: vec![],
            applied_revision: None,
        };
        h.local
            .commit(LocalCommit {
                revisions: vec![source.clone(), revision.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        Self {
            workspace,
            source,
            target,
            candidate,
            revision,
            location: LocationId::new(),
        }
    }

    fn operation(&self, name: &str, created_ns: i64) -> OperationRecord {
        let mut record = OperationRecord {
            id: OperationId::new(),
            request_id: OperationId::new().to_string(),
            principal: actor(),
            workspace: Some(self.workspace.id),
            branch: self.target.branch,
            name: name.into(),
            fingerprint: object_ref(ObjectKind::RecordIndex, b"merge-history-test").id,
            phase: OperationPhase::Reserved,
            local_saved: false,
            remote_confirmed: false,
            mount_ready: None,
            result: None,
            error: None,
            pending: None,
            created_ns,
            sync_progress: None,
            merge_history: Some(MergeOperationHistory {
                merge: Some(self.candidate.id),
                source: self.source.id,
                source_branch: self.source.branch,
                target: self.candidate.target,
                base: Some(self.target.id),
                location: self.location,
                candidate_before: None,
                resolutions: vec![],
                events: vec![],
            }),
        };
        record.record_merge_event(created_ns);
        record
    }

    fn prepared(&self, created_ns: i64) -> OperationRecord {
        let mut record = self.operation("merge_prepare", created_ns);
        record.phase = OperationPhase::Complete;
        record.local_saved = true;
        record.result = Some(CommandResult::Merge {
            candidate: self.candidate.clone(),
        });
        record.record_merge_event(created_ns + 1);
        record
    }
}

#[tokio::test]
async fn merge_history_reads_source_and_target_names_from_remote_observations() {
    let remote = Arc::new(MemoryRemote::default());
    let sender = Harness::new(Some(remote.clone())).await;
    let receiver = Harness::new(Some(remote)).await;
    let workspace = sender.workspace().await;
    let (source, source_fs) = sender
        .branch_mount(workspace.id, "remote-source", None)
        .await;
    let (target, target_fs) = sender
        .branch_mount(workspace.id, "remote-target", None)
        .await;
    sender
        .write_new(&source_fs, "article.md", b"source article")
        .await;
    sender
        .write_new(&target_fs, "article.md", b"target article")
        .await;
    let source_revision = sender.commit(source.branch).await;
    sender.commit(target.branch).await;
    let target_branch = sender.local.branch(target.branch).await.unwrap().unwrap();
    let prepared = success(
        sender
            .engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source: source_revision,
                    target: BranchGuard::from(&target_branch),
                    base: None,
                },
            )
            .await
            .unwrap(),
    );
    let CommandResult::Merge { candidate } = prepared.result.unwrap() else {
        panic!("expected merge candidate");
    };
    sender.engine.sync_pending().await.unwrap();
    receiver.sync(workspace.id, SyncDirection::Pull).await;
    assert!(
        receiver
            .local
            .branch(source.branch)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        receiver
            .local
            .branch(target.branch)
            .await
            .unwrap()
            .is_none()
    );
    let service = &receiver.engine.services.merge_history;
    let page = service
        .list(&actor(), workspace.id, MergeHistoryFilter::default())
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        page.entries[0].source_name.as_deref(),
        Some("remote-source")
    );
    assert_eq!(
        page.entries[0].target_name.as_deref(),
        Some("remote-target")
    );
    let detail = service
        .detail(&actor(), workspace.id, &candidate.id.to_string())
        .await
        .unwrap();
    assert_eq!(detail.entry.source_name.as_deref(), Some("remote-source"));
    assert_eq!(detail.entry.target_name.as_deref(), Some("remote-target"));
}

#[tokio::test]
async fn merge_history_groups_candidates_and_keeps_application_and_confirmation_separate() {
    let h = Harness::new(None).await;
    let f = Fixture::new(&h).await;
    let prepared = f.prepared(10);
    let mut resolved = f.operation("merge_resolve", 20);
    resolved.merge_history.as_mut().unwrap().candidate_before = Some(f.candidate.revision);
    resolved
        .merge_history
        .as_mut()
        .unwrap()
        .resolutions
        .push(MergeResolutionSummary {
            path: path("/article.md"),
            choice: "replace".into(),
            revision: None,
            source_path: None,
        });
    let revised = Revision {
        id: RevisionId::new(),
        parents: vec![f.revision.id],
        ..f.revision.clone()
    };
    let candidate = MergeCandidate {
        revision: revised.id,
        ..f.candidate.clone()
    };
    resolved.phase = OperationPhase::Complete;
    resolved.local_saved = true;
    resolved.result = Some(CommandResult::Merge {
        candidate: candidate.clone(),
    });
    resolved.record_merge_event(21);
    h.local
        .commit(LocalCommit {
            operations: vec![prepared.clone()],
            replicated_operations: vec![resolved.clone()],
            revisions: vec![revised.clone()],
            merges: vec![f.candidate.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let service = &h.engine.services.merge_history;
    let page = service
        .list(&actor(), f.workspace.id, MergeHistoryFilter::default())
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    let entry = &page.entries[0];
    assert_eq!(entry.id, f.candidate.id.to_string());
    assert_eq!(entry.candidate_revision, Some(revised.id));
    assert_eq!(entry.status, MergeHistoryStatus::Pending);
    assert_eq!(entry.remote_confirmed, None);
    assert!(entry.history_complete);
    let first = service
        .events(
            &actor(),
            f.workspace.id,
            &entry.id,
            MergeEventsQuery {
                limit: Some(2),
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.total, 4);
    assert_eq!(first.entries.len(), 2);
    let second = service
        .events(
            &actor(),
            f.workspace.id,
            &entry.id,
            MergeEventsQuery {
                limit: Some(2),
                cursor: first.next,
            },
        )
        .await
        .unwrap();
    assert!(second.next.is_none());
    assert_eq!(second.entries[1].paths[0].choice, "replace");
    assert_eq!(second.entries[1].candidate_before, Some(f.revision.id));
    assert_eq!(second.revisions.len(), 2);
    let mut applied = f.operation("merge_apply", 30);
    applied.merge_history.as_mut().unwrap().candidate_before = Some(revised.id);
    let final_revision = Revision {
        id: RevisionId::new(),
        parents: vec![f.target.id, f.source.id],
        operation: applied.id,
        ..f.target.clone()
    };
    applied.phase = OperationPhase::Complete;
    applied.local_saved = true;
    applied.mount_ready = Some(true);
    applied.result = Some(CommandResult::Revision {
        revision: final_revision.id,
        branch: f.target.branch.unwrap(),
    });
    applied.record_merge_event(31);
    h.local
        .commit(LocalCommit {
            operations: vec![applied.clone()],
            revisions: vec![final_revision.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let detail = service
        .detail(&actor(), f.workspace.id, &entry.id)
        .await
        .unwrap();
    assert_eq!(detail.entry.status, MergeHistoryStatus::Applied);
    assert_eq!(detail.entry.applied_revision, Some(final_revision.id));
    assert_eq!(detail.entry.remote_confirmed, Some(false));
    assert_eq!(detail.entry.source_revision, Some(f.source.id));
    assert_eq!(detail.entry.target_revision, Some(f.target.id));
    assert_eq!(detail.revisions.len(), 4);
    applied.remote_confirmed = true;
    applied.record_merge_event(40);
    h.local
        .commit(LocalCommit {
            operations: vec![applied],
            ..Default::default()
        })
        .await
        .unwrap();
    let detail = service
        .detail(&actor(), f.workspace.id, &entry.id)
        .await
        .unwrap();
    assert_eq!(detail.entry.remote_confirmed, Some(true));
    assert_eq!(detail.events.entries.len(), 7);
    assert_eq!(detail.events.entries.last().unwrap().created_ns, 40);
    assert_eq!(
        service
            .list(
                &actor(),
                f.workspace.id,
                MergeHistoryFilter {
                    revision: Some(f.revision.id),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .total,
        1
    );
}

#[tokio::test]
async fn merge_history_pagination_is_bounded_and_bound_to_filters_and_actor() {
    let h = Harness::new(None).await;
    let f = Fixture::new(&h).await;
    let mut records = Vec::new();
    for index in 0..121 {
        let mut operation = f.operation("merge_prepare", 100 + index / 3);
        operation.merge_history.as_mut().unwrap().merge = None;
        operation.phase = OperationPhase::Failed;
        operation.error = Some(Error::new(ErrorCode::TargetMoved, "target moved"));
        operation.record_merge_event(200 + index);
        records.push(operation);
    }
    h.local
        .commit(LocalCommit {
            operations: records.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let service = &h.engine.services.merge_history;
    let filter = MergeHistoryFilter {
        limit: Some(17),
        status: Some(MergeHistoryStatus::Failed),
        ..Default::default()
    };
    let first = service
        .list(&actor(), f.workspace.id, filter.clone())
        .await
        .unwrap();
    let mut query = filter.clone();
    let mut ids = Vec::new();
    loop {
        let page = service
            .list(&actor(), f.workspace.id, query.clone())
            .await
            .unwrap();
        assert_eq!(page.total, 121);
        assert!(page.entries.len() <= 17);
        ids.extend(page.entries.into_iter().map(|entry| entry.id));
        let Some(cursor) = page.next else { break };
        query.cursor = Some(cursor);
    }
    records.sort_by(|a, b| {
        b.created_ns
            .cmp(&a.created_ns)
            .then(b.id.to_string().cmp(&a.id.to_string()))
    });
    assert_eq!(
        ids,
        records
            .iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>()
    );
    for invalid in [
        MergeHistoryFilter {
            status: Some(MergeHistoryStatus::Applied),
            cursor: first.next.clone(),
            ..filter.clone()
        },
        MergeHistoryFilter {
            limit: Some(0),
            ..filter.clone()
        },
        MergeHistoryFilter {
            limit: Some(201),
            ..filter.clone()
        },
        MergeHistoryFilter {
            from_ns: Some(2),
            to_ns: Some(1),
            ..filter.clone()
        },
        MergeHistoryFilter {
            cursor: Some("invalid".into()),
            ..filter.clone()
        },
    ] {
        assert_eq!(
            service
                .list(&actor(), f.workspace.id, invalid)
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(
        service
            .list(
                &actor(),
                f.workspace.id,
                MergeHistoryFilter {
                    from_ns: Some(100),
                    to_ns: Some(100),
                    branch: f.source.branch,
                    location: Some(f.location),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .total,
        3
    );
    let visitor: Principal = "visitor".to_owned().try_into().unwrap();
    let workspace = Workspace {
        grants: vec![Grant {
            principal: visitor.clone(),
            branch: None,
            permissions: vec![Permission::Read],
        }],
        ..f.workspace.clone()
    };
    h.local
        .commit(LocalCommit {
            workspaces: vec![workspace],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .list(
                &visitor,
                f.workspace.id,
                MergeHistoryFilter {
                    cursor: first.next,
                    ..filter
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let connection = rusqlite::Connection::open(h.directory.path().join("meta.db")).unwrap();
    let mut statement=connection.prepare("EXPLAIN QUERY PLAN SELECT id FROM operations WHERE workspace=?1 AND json_extract(CAST(data AS TEXT),'$.name') IN ('merge_prepare','merge_resolve','merge_apply')").unwrap();
    let plan = statement
        .query_map([f.workspace.id.to_string()], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
        .join(" ");
    assert!(plan.contains("USING INDEX"), "{plan}");
}

#[tokio::test]
async fn merge_history_permissions_require_both_branches_and_treat_unowned_sources_as_workspace_data()
 {
    let h = Harness::new(None).await;
    let f = Fixture::new(&h).await;
    let prepared = f.prepared(10);
    let reader: Principal = "reader".to_owned().try_into().unwrap();
    let mut workspace = f.workspace.clone();
    workspace.grants.push(Grant {
        principal: reader.clone(),
        branch: f.target.branch,
        permissions: vec![Permission::Read],
    });
    h.local
        .commit(LocalCommit {
            operations: vec![prepared],
            workspaces: vec![workspace.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let service = &h.engine.services.merge_history;
    assert_eq!(
        service
            .list(&reader, workspace.id, MergeHistoryFilter::default())
            .await
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        service
            .detail(&reader, workspace.id, &f.candidate.id.to_string())
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    workspace.grants.push(Grant {
        principal: reader.clone(),
        branch: f.source.branch,
        permissions: vec![Permission::Read],
    });
    h.local
        .commit(LocalCommit {
            workspaces: vec![workspace.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let detail = service
        .detail(&reader, workspace.id, &f.candidate.id.to_string())
        .await
        .unwrap();
    assert_eq!(
        service
            .list(&reader, workspace.id, MergeHistoryFilter::default())
            .await
            .unwrap()
            .total,
        1
    );
    assert!(
        detail
            .revisions
            .iter()
            .all(|revision| revision.branch.is_some())
    );
    assert!(detail.events.revisions.is_empty());
    let source = Revision {
        id: RevisionId::new(),
        branch: None,
        ..f.source.clone()
    };
    let mut unowned = f.prepared(20);
    unowned.merge_history.as_mut().unwrap().source = source.id;
    unowned.merge_history.as_mut().unwrap().source_branch = None;
    let id = MergeId::new();
    unowned.merge_history.as_mut().unwrap().merge = Some(id);
    unowned.result = Some(CommandResult::Merge {
        candidate: MergeCandidate {
            id,
            source: source.id,
            ..f.candidate.clone()
        },
    });
    h.local
        .commit(LocalCommit {
            revisions: vec![source],
            operations: vec![unowned],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .list(&reader, workspace.id, MergeHistoryFilter::default())
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        service
            .events(
                &reader,
                workspace.id,
                &id.to_string(),
                MergeEventsQuery::default()
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        service
            .list(&actor(), workspace.id, MergeHistoryFilter::default())
            .await
            .unwrap()
            .total,
        2
    );
}

#[tokio::test]
async fn merge_history_marks_legacy_data_and_distinguishes_incomplete_preparation() {
    let h = Harness::new(None).await;
    let f = Fixture::new(&h).await;
    let mut legacy = f.prepared(10);
    legacy.merge_history = None;
    h.local
        .commit(LocalCommit {
            operations: vec![legacy],
            ..Default::default()
        })
        .await
        .unwrap();
    let service = &h.engine.services.merge_history;
    let legacy = service
        .detail(&actor(), f.workspace.id, &f.candidate.id.to_string())
        .await
        .unwrap();
    assert!(!legacy.entry.history_complete);
    assert!(!legacy.events.entries[0].history_complete);
    assert_eq!(legacy.entry.origin_location, None);
    let mut pending = f.operation("merge_prepare", 20);
    pending.merge_history.as_mut().unwrap().merge = None;
    let mut included = f.operation("merge_prepare", 30);
    included.merge_history.as_mut().unwrap().merge = None;
    included.phase = OperationPhase::Complete;
    included.local_saved = true;
    included.result = Some(CommandResult::Revision {
        revision: f.target.id,
        branch: f.target.branch.unwrap(),
    });
    included.record_merge_event(31);
    h.local
        .commit(LocalCommit {
            operations: vec![pending.clone(), included.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let detail = service
        .detail(&actor(), f.workspace.id, &pending.id.to_string())
        .await
        .unwrap();
    assert_eq!(detail.entry.status, MergeHistoryStatus::Pending);
    assert_eq!(detail.entry.candidate_revision, None);
    assert_eq!(detail.events.entries[0].phase, OperationPhase::Reserved);
    let detail = service
        .detail(&actor(), f.workspace.id, &included.id.to_string())
        .await
        .unwrap();
    assert_eq!(detail.entry.status, MergeHistoryStatus::AlreadyIncluded);
    assert_eq!(detail.entry.applied_revision, None);
    assert_eq!(detail.entry.remote_confirmed, None);
}
