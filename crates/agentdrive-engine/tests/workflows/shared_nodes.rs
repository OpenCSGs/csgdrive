use super::*;
use rusqlite::Connection;

async fn fork(h: &Harness, workspace: WorkspaceId, revision: RevisionId, name: &str) -> Branch {
    let operation = success(
        h.engine
            .services
            .revisions
            .fork(
                context(),
                ForkBranch {
                    workspace,
                    source: Some(revision),
                    source_branch: None,
                    name: name.into(),
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Branch { branch }) = operation.result else {
        panic!("missing branch");
    };
    branch
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

async fn put(h: &Harness, branch: &Branch, name: &str, content: Vec<u8>) -> Inode {
    h.engine
        .update_branch_object(
            &actor(),
            branch.workspace,
            branch.id,
            path(name),
            BranchObjectChange::Put(Bytes::from(content)),
        )
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn historical_forks_share_index_pages_and_isolate_each_generation() {
    let h = Harness::new(None).await;
    let workspace = h.workspace().await;
    let (mount, fs) = h.branch_mount(workspace.id, "original", None).await;
    let mut highest = InodeId::ROOT;
    for i in 0..=INDEX_PAGE_ENTRIES {
        highest = h
            .engine
            .services
            .files
            .create(
                &fs,
                InodeId::ROOT,
                &format!("f{i:03}"),
                FileKind::File,
                0o644,
            )
            .await
            .unwrap()
            .id;
    }
    let historical = h.commit(mount.branch).await;
    h.write_new(&fs, "later", b"parent only").await;
    h.commit(mount.branch).await;
    h.local.compact_node_views().await.unwrap();
    let db = Connection::open(h.directory.path().join("meta.db")).unwrap();
    let rows = count(&db, "SELECT count(*) FROM node_versions");

    // A full-tree read would need the directory index and the first inode page.
    let metadata = |id: &ObjectId| -> Vec<u8> {
        db.query_row(
            "SELECT data FROM metadata_objects WHERE workspace_id=?1 AND hash=?2",
            rusqlite::params![workspace.id.to_string(), id.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    let root = h.local.revision(historical).await.unwrap().unwrap().root;
    let tree: TreeRoot = decode(&metadata(&root.id)).unwrap();
    let IndexPage::Branch { children } =
        decode::<IndexPage<Inode>>(&metadata(&tree.inodes.id)).unwrap()
    else {
        panic!("fixture must span more than one inode page");
    };
    let hidden = [tree.directories.id, children[0].object.id.clone()].map(|id| {
        let bytes = metadata(&id);
        db.execute(
            "UPDATE metadata_objects SET data=X'00' WHERE workspace_id=?1 AND hash=?2",
            rusqlite::params![workspace.id.to_string(), id.as_str()],
        )
        .unwrap();
        (id, bytes)
    });
    let child = fork(&h, workspace.id, historical, "history-child").await;
    let sibling = fork(&h, workspace.id, historical, "history-sibling").await;
    assert_eq!(child.next_inode, highest.0 + 1);
    assert!(
        h.local
            .node_at(child.id, &path("/later"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        h.local
            .children(child.id, &WorkspacePath::root())
            .await
            .unwrap()
            .len(),
        INDEX_PAGE_ENTRIES + 1
    );
    assert_eq!(count(&db, "SELECT count(*) FROM node_versions"), rows);
    for (id, bytes) in hidden {
        db.execute(
            "UPDATE metadata_objects SET data=?1 WHERE workspace_id=?2 AND hash=?3",
            rusqlite::params![bytes, workspace.id.to_string(), id.as_str()],
        )
        .unwrap();
    }

    let changed = put(&h, &child, "/f000", b"first child".to_vec()).await;
    let created = put(&h, &child, "/child-only", b"new".to_vec()).await;
    assert_eq!(created.id.0, highest.0 + 1);
    h.engine
        .update_branch_object(
            &actor(),
            workspace.id,
            child.id,
            path("/f001"),
            BranchObjectChange::Delete(FileKind::File, Default::default()),
        )
        .await
        .unwrap();
    let child_revision = h.commit(child.id).await;
    let before_grandchild = count(&db, "SELECT count(*) FROM node_versions");
    let grandchild = fork(&h, workspace.id, child_revision, "grandchild").await;
    assert_eq!(
        count(&db, "SELECT count(*) FROM node_versions"),
        before_grandchild
    );
    assert_eq!(
        h.local
            .node(grandchild.id, changed.id)
            .await
            .unwrap()
            .unwrap()
            .inode
            .size,
        11
    );
    assert!(
        h.local
            .node_at(grandchild.id, &path("/f001"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.local
            .node_at(sibling.id, &path("/f001"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        h.local
            .node_at(mount.branch, &path("/child-only"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        h.local
            .node_at(sibling.id, &path("/f000"))
            .await
            .unwrap()
            .unwrap()
            .inode
            .size,
        0
    );
    h.local.compact_node_views().await.unwrap();
    let before_rewrites = count(&db, "SELECT count(*) FROM node_versions");
    for size in 1..=32 {
        put(&h, &child, "/f000", vec![42; size]).await;
    }
    assert!(count(&db, "SELECT count(*) FROM node_versions") >= before_rewrites + 32);
    h.local.compact_node_views().await.unwrap();
    let after_compaction = count(&db, "SELECT count(*) FROM node_versions");
    assert!(
        after_compaction <= before_rewrites + 2,
        "{before_rewrites} -> {after_compaction}"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM metadata WHERE key='node_compaction_pending'"
        ),
        0
    );
    assert_eq!(
        h.local
            .node(grandchild.id, changed.id)
            .await
            .unwrap()
            .unwrap()
            .inode
            .size,
        11
    );
    assert_eq!(
        h.local
            .node(child.id, changed.id)
            .await
            .unwrap()
            .unwrap()
            .inode
            .size,
        32
    );

    let Harness {
        directory,
        engine,
        local,
        ..
    } = h;
    drop((engine, local));
    let (local, _engine, ..) = open(directory.path(), None).await;
    assert_eq!(
        local
            .node_at(grandchild.id, &path("/child-only"))
            .await
            .unwrap()
            .unwrap()
            .inode
            .id,
        created.id
    );
    assert!(
        local
            .node_at(grandchild.id, &path("/f001"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(local.usage(grandchild.id).await.unwrap().bytes, 14);
    assert_eq!(
        count(&db, "SELECT count(*) FROM node_versions"),
        after_compaction
    );
}

#[tokio::test]
async fn forks_materialize_one_shared_view_for_a_revision_without_a_saved_index() {
    let h = Harness::new(None).await;
    let workspace = h.workspace().await;
    let source = fork(&h, workspace.id, workspace.initial_revision, "source").await;
    put(&h, &source, "/document", b"version 1".to_vec()).await;
    let historical = h.commit(source.id).await;
    put(&h, &source, "/document", b"latest version".to_vec()).await;
    h.commit(source.id).await;
    let db = Connection::open(h.directory.path().join("meta.db")).unwrap();
    let revision = h.local.revision(historical).await.unwrap().unwrap();
    // Imported revisions can arrive before a saved directory index exists locally.
    db.execute(
        "DELETE FROM saved_node_views WHERE workspace_id=?1 AND hash=?2",
        rusqlite::params![workspace.id.to_string(), revision.root.id.as_str()],
    )
    .unwrap();
    assert_eq!(h.local.usage(source.id).await.unwrap().bytes, 14);
    let before = count(&db, "SELECT count(*) FROM node_versions");
    let a = fork(&h, workspace.id, historical, "restore-a").await;
    let b = fork(&h, workspace.id, historical, "restore-b").await;
    assert_eq!(count(&db, "SELECT count(*) FROM node_versions"), before);
    assert_eq!(h.local.usage(a.id).await.unwrap().bytes, 9);
    assert_eq!(count(&db, "SELECT count(*) FROM node_versions"), before + 2);
    assert_eq!(
        h.local
            .node_at(b.id, &path("/document"))
            .await
            .unwrap()
            .unwrap()
            .inode
            .size,
        9
    );
    assert_eq!(count(&db, "SELECT count(*) FROM node_versions"), before + 2);
}
