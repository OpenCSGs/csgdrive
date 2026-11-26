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
    let objects = h
        .directory
        .path()
        .join("objects")
        .join(workspace.id.to_string());
    let root = h.local.revision(historical).await.unwrap().unwrap().root;
    let tree: TreeRoot = decode(&std::fs::read(objects.join(root.id.as_str())).unwrap()).unwrap();
    let IndexPage::Branch { children } =
        decode::<IndexPage<Inode>>(&std::fs::read(objects.join(tree.inodes.id.as_str())).unwrap())
            .unwrap()
    else {
        panic!("fixture must span more than one inode page");
    };
    let hidden = [
        objects.join(tree.directories.id.as_str()),
        objects.join(children[0].object.id.as_str()),
    ];
    for file in &hidden {
        std::fs::rename(file, file.with_extension("unavailable")).unwrap();
    }
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
    for file in &hidden {
        std::fs::rename(file.with_extension("unavailable"), file).unwrap();
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
async fn previous_flat_database_upgrades_atomically_and_builds_one_shared_historical_view() {
    let h = Harness::new(None).await;
    let workspace = h.workspace().await;
    let source = fork(&h, workspace.id, workspace.initial_revision, "source").await;
    put(&h, &source, "/document", b"version 1".to_vec()).await;
    let historical = h.commit(source.id).await;
    put(&h, &source, "/document", b"latest version".to_vec()).await;
    h.commit(source.id).await;
    let mut flat = Vec::new();
    for branch in h.local.branches(workspace.id).await.unwrap() {
        for node in h.local.nodes(branch.id).await.unwrap() {
            flat.push((branch.id, node));
        }
    }
    let Harness {
        directory,
        local,
        engine,
        ..
    } = h;
    drop((engine, local));
    let mut db = Connection::open(directory.path().join("meta.db")).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute_batch("DROP TABLE branch_node_views; DROP TABLE saved_node_views; DROP TABLE node_versions; DROP TABLE node_layers;
        CREATE TABLE nodes(branch TEXT,path TEXT,parent TEXT,inode INTEGER,data BLOB,PRIMARY KEY(branch,path),UNIQUE(branch,inode));").unwrap();
    for (branch, node) in flat {
        tx.execute(
            "INSERT INTO nodes(branch,path,parent,inode,data) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![
                branch.to_string(),
                node.path.lookup_key(),
                node.path.parent().map(|p| p.lookup_key()),
                i64::try_from(node.inode.id.0).unwrap(),
                encode(&node).unwrap()
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let (local, engine, clock, driver) = open(directory.path(), None).await;
    let h = Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    };
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('node_usage','nodes')"
        ),
        0
    );
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
