use agentdrive_config::{LiveConfig, RuntimeConfig};
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_model::{BranchId, ObjectKind, Principal, WorkspaceId, WorkspacePath, object_ref};
use agentdrive_platform::{HostDirectories, NativeMountDriver, SystemClock, local_identity};
use agentdrive_ports::{
    CreateWorkspace, ForkBranch, ListBranchFiles, LocalObjectStore, ReadBranchFile, RequestContext,
};
use bytes::Bytes;
use drive_s3_store::{ErrorKind, Store, WriteMode};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::time::{Duration, timeout};

async fn open_store(data_dir: &std::path::Path) -> drive_s3_store::Result<Store> {
    let local = Arc::new(LocalBackend::open(data_dir)?);
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local,
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: NativeMountDriver::new(vec![])?,
            directories: HostDirectories::new(vec![])?,
        },
        EngineConfig {
            live: LiveConfig::new(RuntimeConfig::default())?,
            identity: local_identity(),
        },
    )
    .await?;
    Store::from_engine(
        engine,
        &data_dir.join("s3-sessions.sqlite3"),
        "owner".to_owned().try_into()?,
    )
    .await
}

fn request(id: &str) -> RequestContext {
    RequestContext {
        principal: "owner".to_owned().try_into().unwrap(),
        request_id: id.into(),
    }
}

async fn create_workspace(store: &Store, name: &str) -> (WorkspaceId, BranchId) {
    let result = store
        .engine()
        .services
        .workspace
        .create(
            request(&format!("create-{name}")),
            CreateWorkspace {
                name: name.into(),
                grants: vec![],
                max_file_bytes: None,
                max_working_bytes: None,
                max_inodes: None,
            },
        )
        .await
        .unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    (result.workspace.unwrap(), result.branch.unwrap())
}

async fn fork(store: &Store, workspace: WorkspaceId, source: BranchId, name: &str) -> BranchId {
    store
        .engine()
        .services
        .revisions
        .fork(
            request(&format!("fork-{workspace}-{name}")),
            ForkBranch {
                workspace,
                source_branch: Some(source),
                source: None,
                name: name.into(),
            },
        )
        .await
        .unwrap()
        .branch
        .unwrap()
}

#[tokio::test]
async fn binding_requires_an_existing_branch_in_the_requested_workspace() {
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let (workspace, branch) = create_workspace(&store, "one").await;
    let (other, _) = create_workspace(&store, "two").await;
    for (id, branch_id, expected) in [
        (WorkspaceId::new(), branch, ErrorKind::NoSuchKey),
        (workspace, BranchId::new(), ErrorKind::NoSuchKey),
        (other, branch, ErrorKind::AccessDenied),
    ] {
        assert_eq!(
            store
                .bind_ids("typo", id, branch_id, WriteMode::Serialized)
                .await
                .unwrap_err()
                .kind,
            expected
        );
        assert_eq!(
            store.status("typo").await.unwrap_err().kind,
            ErrorKind::NoSuchBucket
        );
    }
    let visitor = Store::from_engine(
        store.engine(),
        &root.path().join("s3-sessions.sqlite3"),
        "visitor".to_owned().try_into().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        visitor
            .bind_ids("visitor", workspace, branch, WriteMode::Serialized)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(
        visitor
            .branch_mounts(workspace, branch)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(
        store.check_bucket("visitor").await.unwrap_err().kind,
        ErrorKind::NoSuchBucket
    );
    let state = &store.engine().runtime.state;
    assert_eq!(state.workspaces().await.unwrap().len(), 2);
    assert_eq!(state.branches(workspace).await.unwrap().len(), 1);
    assert_eq!(state.branches(other).await.unwrap().len(), 1);
}

#[tokio::test]
async fn local_daemon_rejects_unsupported_session_files_without_changes() {
    let current_dir = tempdir().unwrap();
    let current = open_store(current_dir.path()).await.unwrap();
    let owner: Principal = "owner".to_owned().try_into().unwrap();
    let file = "s3-sessions.sqlite3";

    for version in [0, 1, 37] {
        let directory = tempdir().unwrap();
        let path = directory.path().join(file);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE drive_sessions(bucket TEXT PRIMARY KEY,workspace TEXT,branch TEXT);
            INSERT INTO drive_sessions VALUES('retired-alias','workspace','branch');",
        )
        .unwrap();
        db.pragma_update(None, "user_version", version).unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        for error in [
            open_store(directory.path()).await.unwrap_err(),
            Store::from_engine(current.engine(), &path, owner.clone())
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.kind, ErrorKind::Invalid);
            assert!(
                error.message.contains("unsupported S3 session database"),
                "{error}"
            );
            assert!(error.message.contains(&version.to_string()), "{error}");
        }
        // Includes the application row, catalog SQL and SQLite journal/version header bytes.
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!directory.path().join(format!("{file}-wal")).exists());
        let db = rusqlite::Connection::open(&path).unwrap();
        let journal_mode: String = db
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "delete");
        let bucket: String = db
            .query_row("SELECT bucket FROM drive_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(bucket, "retired-alias");
    }

    let already_empty = current_dir.path().join("empty.sqlite");
    std::fs::File::create(&already_empty).unwrap();
    Store::from_engine(current.engine(), &already_empty, owner)
        .await
        .unwrap();
    for path in [current_dir.path().join(file), already_empty] {
        let db = rusqlite::Connection::open(path).unwrap();
        let version: i32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }
}

#[tokio::test]
async fn mount_names_follow_core_records_and_retired_aliases_remain_fenced() {
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let (workspace_id, branch_id) = create_workspace(&store, "original workspace").await;
    for bucket in ["vm-one", "vm-two"] {
        store
            .bind_ids(bucket, workspace_id, branch_id, WriteMode::Exclusive)
            .await
            .unwrap();
    }
    store.acquire("vm-one").await.unwrap();
    let epoch = store.handoff("vm-one", "vm-two").await.unwrap().epoch;

    let db = rusqlite::Connection::open(root.path().join("s3-sessions.sqlite3")).unwrap();
    let columns = |table: &str| -> Vec<String> {
        let mut query = db
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .unwrap();
        query
            .query_map([table], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        columns("drive_sessions"),
        [
            "bucket",
            "branch_id",
            "retired",
            "successor",
            "revision",
            "read_only"
        ]
    );
    assert_eq!(
        columns("drive_branches"),
        ["branch_id", "workspace_id", "epoch", "serialized", "holder"]
    );
    let associations: i64 = db
        .query_row("SELECT count(*) FROM drive_branches", [], |row| row.get(0))
        .unwrap();
    assert_eq!(associations, 1);

    let engine = store.engine();
    let state = &engine.runtime.state;
    let mut workspace = state.workspace(workspace_id).await.unwrap().unwrap();
    let mut branch = state.branch(branch_id).await.unwrap().unwrap();
    workspace.name = "current workspace".into();
    branch.name = "current branch".into();
    state
        .commit(agentdrive_ports::LocalCommit {
            workspaces: vec![workspace],
            branches: vec![branch],
            ..Default::default()
        })
        .await
        .unwrap();
    let listed = store.branch_mounts(workspace_id, branch_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].retired);
    assert!(listed[1].writable);
    for session in listed {
        assert_eq!(session.workspace, "current workspace");
        assert_eq!(session.branch, "current branch");
        assert_eq!(session.epoch, epoch);
        assert_eq!(session.holder.as_deref(), Some("vm-two"));
    }
    drop(engine);
    drop(store);
    let store = open_store(root.path()).await.unwrap();
    let retired = store
        .bind_ids("vm-one", workspace_id, branch_id, WriteMode::Exclusive)
        .await
        .unwrap();
    assert!(retired.retired);
    assert_eq!(retired.branch, "current branch");
    assert_eq!(
        store.status("vm-one").await.unwrap().workspace,
        "current workspace"
    );
    assert_eq!(
        store.handoff("vm-one", "vm-two").await.unwrap().epoch,
        epoch
    );
    assert_eq!(
        store.check_writer("vm-one").await.unwrap_err().kind,
        ErrorKind::AccessDenied
    );
    store.check_writer("vm-two").await.unwrap();
}

#[tokio::test]
async fn two_sessions_share_agentdrive_tree_across_handoff_and_restart() {
    let root = tempdir().unwrap();
    let s = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&s, "workspace").await;
    let a = s
        .bind_ids("demo-a", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    let b = s
        .bind_ids("demo-b", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    assert_eq!(a.branch_id, b.branch_id);
    assert_eq!(s.acquire("demo-a").await.unwrap().epoch, 1);
    assert_eq!(
        s.acquire("demo-b").await.unwrap_err().kind,
        ErrorKind::Conflict
    );
    let made = s
        .put("demo-a", "d/sub/file", Bytes::from_static(b"first"))
        .await
        .unwrap();
    assert_eq!(s.get("demo-b", "d/sub/file").await.unwrap(), "first");
    assert_eq!(
        s.head("demo-b", "d/sub/file").await.unwrap().etag,
        made.etag
    );
    assert_eq!(
        s.list("demo-b", "d/")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect::<Vec<_>>(),
        ["d/", "d/sub/", "d/sub/file"]
    );
    s.put("demo-a", "empty/", Bytes::new()).await.unwrap();
    s.put("demo-a", "empty/", Bytes::new()).await.unwrap();
    assert!(s.get("demo-b", "empty/").await.unwrap().is_empty());
    s.delete("demo-a", "empty/").await.unwrap();
    assert_eq!(
        s.delete("demo-a", "d/").await.unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        s.put("demo-a", "D/new", Bytes::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        s.put("demo-a", "../outside", Bytes::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    let api = s.engine().services.branch_files.clone();
    let owner: Principal = "owner".to_owned().try_into().unwrap();
    let page = api
        .list(
            &owner,
            ListBranchFiles {
                workspace: a.workspace_id,
                branch: a.branch_id,
                path: WorkspacePath::try_from("/d/sub".to_owned()).unwrap(),
                after: None,
                version: None,
            },
        )
        .await
        .unwrap();
    let content = api
        .read(
            &owner,
            ReadBranchFile {
                workspace: a.workspace_id,
                branch: a.branch_id,
                path: page.entries[0].path.clone(),
                version: page.version,
                offset: 0,
                size: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(content, "first");
    let b = s.handoff("demo-a", "demo-b").await.unwrap();
    assert!(b.writable);
    assert_eq!(b.epoch, 2);
    assert_eq!(s.handoff("demo-a", "demo-b").await.unwrap().epoch, 2);
    assert_eq!(
        s.delete("demo-a", "d/sub/file").await.unwrap_err().kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(
        s.put("demo-a", "late", Bytes::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::AccessDenied
    );
    s.put("demo-b", "d/sub/file", Bytes::from_static(b"second"))
        .await
        .unwrap();
    drop(api);
    drop(s);
    let s = open_store(root.path()).await.unwrap();
    assert_eq!(s.get("demo-a", "d/sub/file").await.unwrap(), "second");
    assert!(s.status("demo-b").await.unwrap().writable);
    assert!(
        s.bind_ids("demo-a", ws, main, WriteMode::Exclusive)
            .await
            .unwrap()
            .retired
    );
    assert_eq!(
        s.acquire("demo-a").await.unwrap_err().kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(
        s.handoff("demo-b", "demo-a").await.unwrap_err().kind,
        ErrorKind::Conflict
    );
    s.release("demo-b").await.unwrap();
    assert!(s.status("demo-b").await.unwrap().retired);
    assert_eq!(
        s.acquire("demo-b").await.unwrap_err().kind,
        ErrorKind::AccessDenied
    );
    s.bind_ids("demo-c", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    assert_eq!(s.acquire("demo-c").await.unwrap().epoch, 4);
}

#[tokio::test]
async fn different_branches_can_have_independent_writers() {
    let root = tempdir().unwrap();
    let s = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&s, "workspace").await;
    let another = fork(&s, ws, main, "another").await;
    s.bind_ids("main-session", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    let named = s
        .bind_ids("named-session", ws, another, WriteMode::Exclusive)
        .await
        .unwrap();
    assert_eq!(named.branch, "another");
    s.acquire("main-session").await.unwrap();
    s.acquire("named-session").await.unwrap();
    s.put("main-session", "k", Bytes::from_static(b"main value"))
        .await
        .unwrap();
    s.put("named-session", "k", Bytes::from_static(b"other value"))
        .await
        .unwrap();
    assert_eq!(s.get("main-session", "k").await.unwrap(), "main value");
    assert_eq!(s.get("named-session", "k").await.unwrap(), "other value");
    assert_eq!(
        s.handoff("main-session", "named-session")
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        s.bind_ids("main-session", ws, another, WriteMode::Exclusive)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        s.status("unknown").await.unwrap_err().kind,
        ErrorKind::NoSuchBucket
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_does_not_release_fence_while_publication_is_in_flight() {
    let root = tempdir().unwrap();
    let s = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&s, "workspace").await;
    let (other, independent) = create_workspace(&s, "independent").await;
    let a = s
        .bind_ids("a", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    s.bind_ids("b", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    s.acquire("a").await.unwrap();
    s.bind_ids("other", other, independent, WriteMode::Serialized)
        .await
        .unwrap();
    let branch_gate = s.engine().runtime.lock_branch(a.branch_id).await;
    let writer = s.clone();
    let request = tokio::spawn(async move { writer.put("a", "k", Bytes::from_static(b"A")).await });
    let lock_path = root
        .path()
        .join(format!("s3-sessions.sqlite3.{}.lock", a.branch_id));
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                && matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // A separate workspace can publish even while the first request waits for its engine gate.
    timeout(
        Duration::from_secs(2),
        s.put("other", "k", Bytes::from_static(b"independent")),
    )
    .await
    .unwrap()
    .unwrap();
    request.abort();
    let _ = request.await;
    let transfer = s.clone();
    let handoff = tokio::spawn(async move { transfer.handoff("a", "b").await });
    drop(branch_gate);
    assert!(handoff.await.unwrap().unwrap().writable);
    assert_eq!(s.get("b", "k").await.unwrap(), "A");
    s.put("b", "k", Bytes::from_static(b"B")).await.unwrap();
    assert_eq!(
        s.put("a", "k", Bytes::from_static(b"late A"))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(s.get("a", "k").await.unwrap(), "B");
}

#[tokio::test]
async fn bound_workspace_capacity_is_checked_before_storing_s3_objects() {
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let created = store
        .engine()
        .services
        .workspace
        .create(
            RequestContext {
                principal: "owner".to_owned().try_into().unwrap(),
                request_id: "create-limited-workspace".into(),
            },
            CreateWorkspace {
                name: "limited".into(),
                grants: vec![],
                max_file_bytes: Some(4),
                max_working_bytes: Some(6),
                max_inodes: Some(5),
            },
        )
        .await
        .unwrap();
    let workspace = created.workspace.unwrap();
    let session = store
        .bind_ids(
            "bucket",
            workspace,
            created.branch.unwrap(),
            WriteMode::Exclusive,
        )
        .await
        .unwrap();
    assert_eq!(session.workspace_id, workspace);
    store.acquire("bucket").await.unwrap();

    store
        .put("bucket", "first", Bytes::from_static(b"1234"))
        .await
        .unwrap();
    // Five bytes exceed the per-file limit, even when replacing a four-byte file.
    assert_eq!(
        store
            .put("bucket", "first", Bytes::from_static(b"large"))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert_eq!(store.get("bucket", "first").await.unwrap(), "1234");

    // These three bytes would bring the working tree to seven bytes.
    assert_eq!(
        store
            .put("bucket", "implicit/second", Bytes::from_static(b"sum"))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert!(store.list("bucket", "implicit").await.unwrap().is_empty());
    store
        .put("bucket", "second", Bytes::from_static(b"ab"))
        .await
        .unwrap();
    // Two inodes remain, but the new path needs three.
    for key in ["x/y/z/", "x/y/z"] {
        assert_eq!(
            store
                .put("bucket", key, Bytes::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
        assert!(store.list("bucket", "x").await.unwrap().is_empty());
    }
    store.put("bucket", "x/y/", Bytes::new()).await.unwrap();
    store
        .put("bucket", "first", Bytes::from_static(b"123"))
        .await
        .unwrap();
    store
        .put("bucket", "second", Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(store.get("bucket", "second").await.unwrap(), "abc");

    drop(store);
    let local = LocalBackend::open(root.path()).unwrap();
    for refused in [b"large".as_slice(), b"sum", b""] {
        assert!(
            !local
                .contains(workspace, &object_ref(ObjectKind::File, refused))
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn serialized_sessions_publish_each_request_and_keep_policy_after_restart() {
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&store, "documents").await;
    let work = fork(&store, ws, main, "work").await;
    for bucket in ["vm-a", "vm-b"] {
        let session = store
            .bind_ids(bucket, ws, work, WriteMode::Serialized)
            .await
            .unwrap();
        assert!(session.writable);
        assert!(session.holder.is_none());
    }
    assert_eq!(
        store
            .bind_ids("vm-a", ws, work, WriteMode::Exclusive)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        store
            .bind_ids("vm-c", ws, work, WriteMode::Exclusive)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        store.status("vm-c").await.unwrap_err().kind,
        ErrorKind::NoSuchBucket
    );
    assert_eq!(
        store.acquire("vm-a").await.unwrap_err().kind,
        ErrorKind::Conflict
    );

    let (a, b) = tokio::join!(
        store.put("vm-a", "a", Bytes::from_static(b"a")),
        store.put("vm-b", "b", Bytes::from_static(b"b"))
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(store.get("vm-a", "b").await.unwrap(), "b");
    assert_eq!(store.get("vm-b", "a").await.unwrap(), "a");

    store
        .put("vm-a", "shared", Bytes::from_static(b"first"))
        .await
        .unwrap();
    assert!(
        store
            .put("vm-a", "shared/child", Bytes::new())
            .await
            .is_err()
    );
    store
        .put("vm-b", "shared", Bytes::from_static(b"second"))
        .await
        .unwrap();
    drop(store);
    let store = open_store(root.path()).await.unwrap();
    let session = store.status("vm-b").await.unwrap();
    assert_eq!(session.write_mode, WriteMode::Serialized);
    let mounts = store
        .branch_mounts(session.workspace_id, session.branch_id)
        .await
        .unwrap();
    assert_eq!(
        mounts.iter().map(|s| s.bucket.as_str()).collect::<Vec<_>>(),
        ["vm-a", "vm-b"]
    );
    assert_eq!(store.get("vm-a", "shared").await.unwrap(), "second");
    store.delete("vm-a", "shared").await.unwrap();
    assert_eq!(
        store.head("vm-b", "shared").await.unwrap_err().kind,
        ErrorKind::NoSuchKey
    );
}

#[tokio::test]
async fn range_reader_keeps_its_content_across_sibling_and_same_key_writes() {
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&store, "team").await;
    for bucket in ["east", "west"] {
        store
            .bind_ids(bucket, ws, main, WriteMode::Serialized)
            .await
            .unwrap();
    }
    let original = Bytes::from(
        (0..1024 * 1024)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    store.put("east", "large", original.clone()).await.unwrap();
    let reader = store.reader("west", "large").await.unwrap();
    assert_eq!(
        reader.chunk(0, original.len() as u64).await.unwrap().len(),
        256 * 1024
    );
    store
        .put("east", "notes", Bytes::from_static(b"other file"))
        .await
        .unwrap();
    store
        .put("east", "large", Bytes::from_static(b"replacement"))
        .await
        .unwrap();
    assert_eq!(
        reader.bytes(65537..69633).await.unwrap(),
        original.slice(65537..69633)
    );
    assert_eq!(
        reader.bytes(262144..1048576).await.unwrap(),
        original.slice(262144..)
    );
    assert_eq!(store.get("west", "large").await.unwrap(), "replacement");
}

#[tokio::test]
async fn updating_one_key_does_not_read_an_unrelated_directory_index_page() {
    use agentdrive_model::{DirectoryEntry, INDEX_PAGE_ENTRIES, IndexPage, TreeRoot, decode};
    let root = tempdir().unwrap();
    let store = open_store(root.path()).await.unwrap();
    let (ws, main) = create_workspace(&store, "tree").await;
    let binding = store
        .bind_ids("east", ws, main, WriteMode::Exclusive)
        .await
        .unwrap();
    store.acquire("east").await.unwrap();
    for index in 0..=INDEX_PAGE_ENTRIES {
        store
            .put("east", &format!("a{index:04}"), Bytes::new())
            .await
            .unwrap();
    }
    let db = rusqlite::Connection::open(root.path().join("meta.db")).unwrap();
    let read = |hash: &str| -> Vec<u8> {
        db.query_row(
            "SELECT data FROM metadata_objects WHERE workspace_id=?1 AND hash=?2",
            rusqlite::params![binding.workspace_id.to_string(), hash],
            |row| row.get(0),
        )
        .unwrap()
    };
    let branch = store
        .engine()
        .runtime
        .branch(binding.branch_id)
        .await
        .unwrap();
    let tree: TreeRoot = decode(&read(branch.working_root.id.as_str())).unwrap();
    let page: IndexPage<DirectoryEntry> = decode(&read(tree.directories.id.as_str())).unwrap();
    let IndexPage::Branch { children } = page else {
        panic!("expected multiple directory pages")
    };
    assert!(children.len() >= 2);
    // Make the last leaf unreadable in this fixture. An edit of the first leaf must not open it.
    let last = children.last().unwrap().object.id.as_str();
    let saved = read(last);
    let update = |value: &[u8]| {
        db.execute(
            "UPDATE metadata_objects SET data=?1 WHERE workspace_id=?2 AND hash=?3",
            rusqlite::params![value, binding.workspace_id.to_string(), last],
        )
        .unwrap()
    };
    assert_eq!(update(b"unavailable test page"), 1);
    let result = store
        .put("east", "A-before-first", Bytes::from_static(b"new"))
        .await;
    update(&saved);
    result.unwrap();
    assert_eq!(store.get("east", "A-before-first").await.unwrap(), "new");
    assert_eq!(
        store.list("east", "a").await.unwrap().len(),
        INDEX_PAGE_ENTRIES + 1
    );
}

#[tokio::test]
async fn revision_alias_keeps_its_saved_root_and_rejects_writes_after_restart() {
    let directory = tempdir().unwrap();
    let store = open_store(directory.path()).await.unwrap();
    let (workspace, branch) = create_workspace(&store, "snapshots").await;
    let revision = store
        .engine()
        .runtime
        .branch(branch)
        .await
        .unwrap()
        .formal_head;
    store
        .bind_ids("current", workspace, branch, WriteMode::Serialized)
        .await
        .unwrap();
    store
        .bind_view(
            "initial",
            workspace,
            branch,
            WriteMode::Serialized,
            Some(revision),
            true,
        )
        .await
        .unwrap();
    store
        .put("current", "new.txt", Bytes::from_static(b"new"))
        .await
        .unwrap();
    drop(store);
    let store = open_store(directory.path()).await.unwrap();
    assert_eq!(
        store.status("initial").await.unwrap().revision,
        Some(revision)
    );
    assert!(store.list("initial", "").await.unwrap().is_empty());
    assert_eq!(
        store.head("initial", "new.txt").await.unwrap_err().kind,
        ErrorKind::NoSuchKey
    );
    assert_eq!(
        store
            .put("initial", "new.txt", Bytes::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::AccessDenied
    );
    assert_eq!(store.get("current", "new.txt").await.unwrap(), &b"new"[..]);
}
