#![cfg(windows)]
use agentdrive_config::{LiveConfig, RuntimeConfig};
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_model::*;
use agentdrive_platform::{HostDirectories, NativeMountDriver, SystemClock, local_identity};
use agentdrive_ports::*;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

fn context() -> RequestContext {
    RequestContext {
        principal: "native-test".to_owned().try_into().unwrap(),
        request_id: OperationId::new().to_string(),
    }
}
fn success(record: OperationRecord) -> OperationRecord {
    assert!(record.error.is_none(), "{record:?}");
    record
}

async fn unmount_when_idle(engine: &Engine, binding: &MountBinding) {
    let request = context();
    let mut record = engine
        .services
        .mounts
        .unmount(request.clone(), binding.id, binding.generation)
        .await
        .unwrap();
    let operation = record.id;
    let release_context = context();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if record
                .error
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::MountBusy && error.retryable)
            {
                assert!(record.pending.is_some(), "{record:?}");
                // Resume the original pending action after native close callbacks complete.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let released = engine
                    .services
                    .mounts
                    .release(
                        release_context.clone(),
                        ReleaseMount {
                            operation,
                            expected_binding_generation: binding.generation,
                        },
                    )
                    .await
                    .unwrap();
                if !released
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == ErrorCode::MountBusy && error.retryable)
                {
                    success(released);
                }
                record = engine
                    .services
                    .operations
                    .get(&request.principal, operation)
                    .await
                    .unwrap();
                continue;
            }
            assert_eq!(success(record.clone()).phase, OperationPhase::Complete);
            assert_eq!(record.id, operation);
            return;
        }
    })
    .await;
    assert!(
        completed.is_ok(),
        "native mount did not become idle within five seconds: {record:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_mount_supports_file_io_durable_snapshots_and_read_only_history() {
    let root = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(root.path().join("state")).unwrap());
    let mounts = root.path().join("mounts");
    let driver = NativeMountDriver::new(vec![mounts.clone()]).unwrap();
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: driver.clone(),
            directories: HostDirectories::new(vec![]).unwrap(),
        },
        EngineConfig {
            live: LiveConfig::new(RuntimeConfig::default()).unwrap(),
            identity: local_identity(),
        },
    )
    .await
    .unwrap();
    let created = success(
        engine
            .services
            .workspace
            .create(
                context(),
                CreateWorkspace {
                    name: "native".into(),
                    grants: vec![],
                    max_file_bytes: None,
                    max_working_bytes: None,
                    max_inodes: None,
                },
            )
            .await
            .unwrap(),
    );
    let workspace = match created.result.unwrap() {
        CommandResult::Workspace { workspace } => workspace,
        _ => panic!("wrong result"),
    };
    let path = mounts.join("working");
    let normalized = driver.normalize_path(&path).await.unwrap();
    let opened = success(
        engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: false,
                    workspace: workspace.id,
                    branch: None,
                    revision: None,
                    path: path.clone(),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(opened.mount_ready, Some(true));
    assert_eq!(driver.normalize_path(&path).await.unwrap(), normalized);
    let binding = match opened.result.unwrap() {
        CommandResult::Mount { binding } => binding,
        _ => panic!("wrong result"),
    };
    assert!(driver.supports_automatic_collaboration());
    let operation_path = path.clone();
    let expected = tokio::task::spawn_blocking(move || {
        let path = operation_path;
        std::fs::create_dir(path.join("directory")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path.join("directory/Data"))
            .unwrap();
        let payload: Vec<u8> = (0..3 * 1024 * 1024 + 17)
            .map(|index| (index % 251) as u8)
            .collect();
        file.write_all(&payload).unwrap();
        file.seek(SeekFrom::Start(23)).unwrap();
        file.write_all(b"updated").unwrap();
        file.set_len(1_000_013).unwrap();
        file.sync_all().unwrap();
        std::fs::rename(path.join("directory/Data"), path.join("directory/Renamed")).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut expected = Vec::new();
        file.read_to_end(&mut expected).unwrap();
        assert_eq!(&expected[23..30], b"updated");
        assert_eq!(expected.len(), 1_000_013);
        drop(file);
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(path.join("directory/renamed"))
            .unwrap();
        append.write_all(b"appended").unwrap();
        append.sync_all().unwrap();
        drop(append);
        expected.extend_from_slice(b"appended");
        assert_eq!(
            std::fs::read(path.join("directory/Renamed")).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read_dir(path.join("directory")).unwrap().count(),
            1
        );
        std::fs::write(path.join("temporary"), b"remove").unwrap();
        std::fs::remove_file(path.join("temporary")).unwrap();
        for index in 0..130 {
            std::fs::write(path.join(format!("entry-{index:03}")), [index as u8]).unwrap();
        }
        expected
    })
    .await
    .unwrap();
    let branch = local.branch(binding.branch).await.unwrap().unwrap();
    let committed = success(
        engine
            .services
            .revisions
            .commit(
                context(),
                CommitRevision {
                    message: None,
                    guard: BranchGuard::from(&branch),
                    kind: CommitKind::Commit,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let revision = match committed.result.unwrap() {
        CommandResult::Revision { revision, .. } => revision,
        _ => panic!("wrong result"),
    };
    let history_path = mounts.join("history");
    let history = success(
        engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: false,
                    workspace: workspace.id,
                    branch: Some(binding.branch),
                    revision: Some(revision),
                    path: history_path.clone(),
                    access: AccessMode::ReadOnly,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(history.mount_ready, Some(true));
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            std::fs::read(history_path.join("directory/Renamed")).unwrap(),
            expected
        );
        assert_eq!(std::fs::read_dir(&history_path).unwrap().count(), 131);
        assert!(std::fs::write(history_path.join("forbidden"), b"readonly").is_err());
    })
    .await
    .unwrap();
    let fork = success(
        engine
            .services
            .revisions
            .fork(
                context(),
                ForkBranch {
                    workspace: workspace.id,
                    source: Some(revision),
                    source_branch: None,
                    name: "recovery-native".into(),
                },
            )
            .await
            .unwrap(),
    );
    let recovery = local.branch(fork.branch.unwrap()).await.unwrap().unwrap();
    let recovery_path = mounts.join("recovery");
    let recovery_mount = success(
        engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    automatic: false,
                    workspace: workspace.id,
                    branch: Some(recovery.id),
                    revision: None,
                    path: recovery_path.clone(),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount {
        binding: recovery_binding,
    }) = recovery_mount.result
    else {
        panic!("missing recovery mount");
    };
    tokio::task::spawn_blocking(move || {
        std::fs::write(
            recovery_path.join("directory/Renamed"),
            b"recovery branch contents",
        )
        .unwrap();
        std::fs::write(recovery_path.join("recovery-only.txt"), b"new branch").unwrap();
    })
    .await
    .unwrap();
    unmount_when_idle(&engine, &recovery_binding).await;
    let mut next = binding.clone();
    next.branch = recovery.id;
    next.generation += 1;
    next.branch_generation = Some(recovery.generation);
    let directory_path = path.clone();
    let directory = tokio::task::spawn_blocking(move || {
        let mut entries = std::fs::read_dir(directory_path).unwrap();
        entries.next().unwrap().unwrap();
        entries
    })
    .await
    .unwrap();
    let directory_handles = driver.open_handles(&binding).await.unwrap();
    eprintln!(
        "open directory: {directory_handles} handles, scope {:?}",
        driver.handle_tracking_scope()
    );
    if driver.handle_tracking_scope() == HandleTrackingScope::Native {
        assert!(directory_handles > 0);
    }
    let file_path = path.join("directory/Renamed");
    let file = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(file_path).unwrap();
        file.read_exact(&mut [0]).unwrap();
        file
    })
    .await
    .unwrap();
    let file_handles = driver.open_handles(&binding).await.unwrap();
    eprintln!(
        "open file: {file_handles} handles, scope {:?}",
        driver.handle_tracking_scope()
    );
    if driver.handle_tracking_scope() == HandleTrackingScope::Native {
        assert!(file_handles > 0);
    }
    local
        .commit(LocalCommit {
            mounts: vec![next.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    driver.rebind(&binding, &next).await.unwrap();
    driver.rebind(&binding, &next).await.unwrap();
    drop(directory);
    drop(file);
    assert_eq!(
        driver.status(&next).await.unwrap(),
        NativeMountState::Mounted
    );
    assert_eq!(
        engine
            .services
            .files
            .getattr(
                &FsContext {
                    mount: binding.id,
                    generation: binding.generation,
                    identity: local_identity(),
                },
                InodeId::ROOT,
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::StaleBinding
    );
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(path.join("directory/Renamed")).unwrap();
        assert_eq!(bytes, b"recovery branch contents");
        assert_eq!(
            std::fs::read(path.join("recovery-only.txt")).unwrap(),
            b"new branch"
        );
        std::fs::write(path.join("continued.txt"), b"after rebind").unwrap();
    })
    .await
    .unwrap();
    assert!(
        local
            .nodes(binding.branch)
            .await
            .unwrap()
            .iter()
            .all(|node| node.path.as_str() != "/continued.txt")
    );
    assert!(
        local
            .nodes(recovery.id)
            .await
            .unwrap()
            .iter()
            .any(|node| node.path.as_str() == "/continued.txt")
    );
    for mount in local.mounts().await.unwrap() {
        unmount_when_idle(&engine, &mount).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_directory_adoption_preserves_files_at_original_path() {
    let root = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(root.path().join("state")).unwrap());
    let mounts = root.path().join("mounts");
    std::fs::create_dir_all(&mounts).unwrap();
    let source = mounts.join("project");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("original.txt"), b"existing data").unwrap();
    let driver = NativeMountDriver::new(vec![mounts.clone()]).unwrap();
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: driver.clone(),
            directories: HostDirectories::new(vec![mounts]).unwrap(),
        },
        EngineConfig {
            live: LiveConfig::new(RuntimeConfig::default()).unwrap(),
            identity: local_identity(),
        },
    )
    .await
    .unwrap();
    let request_context = context();
    let request = AdoptDirectory {
        workspace: None,
        path: source.clone(),
        restart_agent: true,
    };
    let record = success(
        engine
            .services
            .workspace
            .adopt_directory(request_context.clone(), request.clone())
            .await
            .unwrap(),
    );
    let Some(CommandResult::DirectoryAdoption {
        binding: Some(binding),
        backup_path,
        restart_required,
        ..
    }) = record.result
    else {
        panic!("missing adopted mount")
    };
    assert!(restart_required);
    let path = source.clone();
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            std::fs::read(path.join("original.txt")).unwrap(),
            b"existing data"
        );
        let mut file = std::fs::File::create(path.join("new.txt")).unwrap();
        file.write_all(b"mounted data").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            std::fs::read(backup_path.join("original.txt")).unwrap(),
            b"existing data"
        );
        assert!(!backup_path.join("new.txt").exists());
    })
    .await
    .unwrap();
    let replay = success(
        engine
            .services
            .workspace
            .adopt_directory(request_context, request)
            .await
            .unwrap(),
    );
    assert_eq!(replay.id, record.id);
    unmount_when_idle(&engine, &binding).await;
}
