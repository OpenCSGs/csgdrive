use super::*;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct MountedChild {
    child: Child,
    path: PathBuf,
}
impl Drop for MountedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux FUSE and mount permission"]
async fn only_disconnected_matching_mounts_can_be_detached() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mounts/working");
    let mut process = MountedChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "disconnected::disconnected_mount_child",
            ])
            .env("AGENTDRIVE_DISCONNECTED_TEST_ROOT", root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
        path: path.clone(),
    };
    let binding_path = root.path().join("binding");
    for _ in 0..200 {
        if binding_path.exists() {
            break;
        }
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "mount helper exited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let binding: MountBinding = decode(&std::fs::read(binding_path).unwrap()).unwrap();
    let driver = NativeMountDriver::new(vec![root.path().join("mounts")]).unwrap();
    assert!(!driver.detach_disconnected(&binding).await.unwrap());
    assert!(path.read_dir().is_ok());
    process.child.kill().unwrap();
    process.child.wait().unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&path).unwrap_err().raw_os_error(),
        Some(libc::ENOTCONN)
    );
    let mut other = binding.clone();
    other.id = MountId::new();
    assert!(!driver.detach_disconnected(&other).await.unwrap());
    assert_eq!(
        driver.status(&binding).await.unwrap(),
        NativeMountState::Unknown
    );
    assert!(driver.detach_disconnected(&binding).await.unwrap());
    assert_eq!(
        driver.status(&binding).await.unwrap(),
        NativeMountState::Absent
    );
    assert!(path.read_dir().is_ok());
    assert!(!driver.detach_disconnected(&binding).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess fixture for the disconnected mount test"]
async fn disconnected_mount_child() {
    let Some(root) = std::env::var_os("AGENTDRIVE_DISCONNECTED_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let local = Arc::new(LocalBackend::open(root.join("state")).unwrap());
    let mounts = root.join("mounts");
    let driver = NativeMountDriver::new(vec![mounts.clone()]).unwrap();
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local,
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: driver,
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
                    name: "disconnected mount".into(),
                    grants: vec![],
                    max_file_bytes: None,
                    max_working_bytes: None,
                    max_inodes: None,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Workspace { workspace }) = created.result else {
        panic!("missing workspace");
    };
    let mounted = success(
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
                    path: mounts.join("working"),
                    access: AccessMode::ReadWrite,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let Some(CommandResult::Mount { binding }) = mounted.result else {
        panic!("missing binding");
    };
    std::fs::write(root.join("binding.tmp"), encode(&binding).unwrap()).unwrap();
    std::fs::rename(root.join("binding.tmp"), root.join("binding")).unwrap();
    std::future::pending::<()>().await;
}
