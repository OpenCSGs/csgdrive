use super::*;
use agentdrive_config::{LiveConfig, RuntimeConfig};
use agentdrive_engine::{Adapters, Engine, EngineConfig, SandboxService};
use agentdrive_local::LocalBackend;
use agentdrive_test_support::{ManualClock, MemoryMountDriver, MemoryRemote, NoHostDirectories};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::Mutex;

#[path = "sandbox_collaboration_tests.rs"]
mod collaboration;

#[derive(Default)]
struct RuntimeStub {
    states: Mutex<BTreeMap<String, SandboxStatus>>,
    fail_stop: std::sync::atomic::AtomicBool,
    start_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    starting: tokio::sync::Notify,
}
#[async_trait]
impl SandboxRuntime for RuntimeStub {
    async fn mcp_config(&self, id: &str) -> Result<SandboxMcpConfig> {
        Ok(SandboxMcpConfig {
            mcp_servers: BTreeMap::from([(
                "ubuntu_desktop".into(),
                SandboxMcpServer {
                    command: "python3".into(),
                    args: vec![id.into()],
                },
            )]),
        })
    }

    async fn desktop_asset(&self, _: &str) -> Result<bytes::Bytes> {
        Err(Error::new(ErrorCode::NotFound, "missing"))
    }
    async fn connect(&self, _: &str, _: u32) -> Result<SandboxConnection> {
        Err(Error::new(ErrorCode::Unavailable, "no guest"))
    }
    async fn start(&self, request: SandboxLaunch) -> Result<SandboxStatus> {
        if let Some(gate) = self.start_gate.lock().await.take() {
            self.starting.notify_one();
            gate.notified().await;
        }
        let status = SandboxStatus {
            template_id: default_sandbox_template(),
            services: Vec::new(),
            id: request.id.clone(),
            workspace: request.binding.workspace,
            branch: request.binding.branch,
            state: SandboxState::Ready,
            desktop_url: None,
            message: None,
            startup_step: None,
        };
        assert!(
            self.states
                .lock()
                .await
                .insert(request.id, status.clone())
                .is_none_or(|previous| matches!(
                    previous.state,
                    SandboxState::Stopped | SandboxState::Creating
                )),
            "duplicate runtime launch"
        );
        Ok(status)
    }
    async fn stop(&self, id: &str) -> Result<()> {
        if self.fail_stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::new(
                ErrorCode::Unavailable,
                "guest helper unavailable",
            ));
        }
        self.states.lock().await.get_mut(id).unwrap().state = SandboxState::Stopped;
        Ok(())
    }
    async fn force_stop(&self, id: &str) -> Result<()> {
        self.states.lock().await.get_mut(id).unwrap().state = SandboxState::Stopped;
        Ok(())
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.states.lock().await.remove(id);
        Ok(())
    }
    async fn status(&self, id: &str) -> Result<SandboxStatus> {
        self.states
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "missing"))
    }
    async fn report_progress(&self, status: SandboxStatus) {
        self.states.lock().await.insert(status.id.clone(), status);
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    local: Arc<LocalBackend>,
    engine: Arc<Engine>,
    owner: Principal,
    workspace: WorkspaceId,
    branch: BranchId,
    driver: Arc<RuntimeStub>,
    sandbox: Arc<SandboxService>,
    mounts: Arc<MemoryMountDriver>,
}
async fn fixture() -> Fixture {
    fixture_with_remote(None).await
}
async fn fixture_with_remote(remote: Option<Arc<MemoryRemote>>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(directory.path()).unwrap());
    let mounts = Arc::new(MemoryMountDriver::default());
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: remote
                .clone()
                .map(|remote| remote as Arc<dyn RemoteObjectStore>),
            remote_refs: remote.map(|remote| remote as Arc<dyn RemoteRefStore>),
            clock: Arc::new(ManualClock::default()),
            mounts: mounts.clone(),
            directories: Arc::new(NoHostDirectories),
        },
        EngineConfig {
            live: LiveConfig::new(RuntimeConfig::default()).unwrap(),
            identity: LocalIdentity {
                uid: 1000,
                gid: 1000,
            },
        },
    )
    .await
    .unwrap();
    let owner: Principal = "owner".to_owned().try_into().unwrap();
    let server = crate::McpServer::new(engine.services.clone(), owner.clone());
    let created = server
        .dispatch(
            "workspace_create",
            json!({"request_id":OperationId::new(),"request":{"name":"sandbox"}}),
        )
        .await
        .unwrap();
    let workspace: WorkspaceId = serde_json::from_value(created["workspace"].clone()).unwrap();
    let branch: BranchId = serde_json::from_value(created["branch"].clone()).unwrap();
    let driver = Arc::new(RuntimeStub::default());
    let sandbox = SandboxService::new(
        engine.runtime.clone(),
        engine.services.clone(),
        driver.clone(),
        directory.path().join("mounts"),
    );
    Fixture {
        _directory: directory,
        local,
        engine,
        owner,
        workspace,
        branch,
        driver,
        sandbox,
        mounts,
    }
}

async fn interrupted_mount(f: &Fixture, state: MountState) -> MountBinding {
    let prepared = f
        .engine
        .services
        .mounts
        .prepare(
            RequestContext {
                principal: f.owner.clone(),
                request_id: OperationId::new().to_string(),
            },
            PrepareMount {
                automatic: false,
                workspace: f.workspace,
                branch: Some(f.branch),
                revision: None,
                path: f
                    ._directory
                    .path()
                    .join("mounts")
                    .join(f.branch.to_string()),
                access: AccessMode::ReadWrite,
                durability: Durability::Local,
            },
        )
        .await
        .unwrap();
    assert!(prepared.error.is_none());
    let mut binding = f.local.mounts().await.unwrap().pop().unwrap();
    f.mounts.unmount(&binding).await.unwrap();
    binding.state = state;
    f.local
        .commit(LocalCommit {
            mounts: vec![binding.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    binding
}

#[tokio::test]
async fn sandbox_recovers_interrupted_and_suspended_mounts_at_the_original_path() {
    for state in [MountState::RecoveryRequired, MountState::Suspended] {
        let f = fixture().await;
        let binding = interrupted_mount(&f, state).await;
        let started = f
            .sandbox
            .create(&f.owner, f.workspace, f.branch)
            .await
            .unwrap();
        assert!(matches!(started.state, SandboxState::Ready));
        let mounts = f.local.mounts().await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].id, binding.id);
        assert_eq!(mounts[0].path, binding.path);
        assert_eq!(mounts[0].generation, binding.generation);
        assert_eq!(mounts[0].state, MountState::Ready);
        assert_eq!(
            f.mounts.status(&mounts[0]).await.unwrap(),
            NativeMountState::Mounted
        );
    }
}

#[tokio::test]
async fn sandbox_mount_recovery_reports_the_native_error_and_can_retry() {
    let f = fixture().await;
    let binding = interrupted_mount(&f, MountState::RecoveryRequired).await;
    f.mounts
        .fail_mount
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = f
        .sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap_err();
    assert!(error.message.contains("injected mount failure"));
    assert!(error.message.contains(binding.path.to_str().unwrap()));
    let status = f
        .sandbox
        .status(&f.owner, f.workspace, f.branch)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(status.state, SandboxState::Error));
    assert_eq!(status.startup_step.as_deref(), Some("prepare_branch"));
    assert_eq!(status.message.as_deref(), Some(error.message.as_str()));
    assert_eq!(
        f.local.mount(binding.id).await.unwrap().unwrap().state,
        MountState::RecoveryRequired
    );
    assert!(matches!(
        f.sandbox
            .create(&f.owner, f.workspace, f.branch)
            .await
            .unwrap()
            .state,
        SandboxState::Ready
    ));
}

#[tokio::test]
async fn sandbox_initial_mount_failure_reports_the_operation_error() {
    let f = fixture().await;
    f.mounts
        .fail_mount
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = f
        .sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("injected mount failure"));
    let status = f
        .sandbox
        .status(&f.owner, f.workspace, f.branch)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(status.state, SandboxState::Error));
    assert_eq!(status.message.as_deref(), Some(error.message.as_str()));
}

#[tokio::test]
async fn sandbox_mount_recovery_checks_binding_owner_and_generation() {
    for owner_changed in [true, false] {
        let f = fixture().await;
        let mut binding = interrupted_mount(&f, MountState::RecoveryRequired).await;
        if owner_changed {
            binding.principal = "other".to_string().try_into().unwrap();
        } else {
            binding.branch_generation = Some(100);
        }
        f.local
            .commit(LocalCommit {
                mounts: vec![binding.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let error = f
            .sandbox
            .create(&f.owner, f.workspace, f.branch)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if owner_changed {
                ErrorCode::PermissionDenied
            } else {
                ErrorCode::StaleBinding
            }
        );
        assert_eq!(
            f.mounts.status(&binding).await.unwrap(),
            NativeMountState::Absent
        );
        assert_eq!(f.local.mount(binding.id).await.unwrap().unwrap(), binding);
    }
}

#[tokio::test]
async fn sandbox_mount_recovery_preserves_pending_branch_operations() {
    let f = fixture().await;
    let binding = interrupted_mount(&f, MountState::RecoveryRequired).await;
    let mut pending = f
        .local
        .operations(f.workspace)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "mount_prepare")
        .unwrap();
    pending.phase = OperationPhase::RecoveryRequired;
    pending.pending = Some(PendingAction::Transfer {
        target: LocationId::new(),
        guard: BranchGuard::from(&f.local.branch(f.branch).await.unwrap().unwrap()),
    });
    f.local
        .commit(LocalCommit {
            operations: vec![pending.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let error = f
        .sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(error.message.contains("未完成操作"));
    assert_eq!(
        f.mounts.status(&binding).await.unwrap(),
        NativeMountState::Absent
    );
    assert_eq!(
        f.local.mount(binding.id).await.unwrap().unwrap().state,
        MountState::RecoveryRequired
    );
    assert_eq!(
        f.local
            .operations(f.workspace)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == pending.id)
            .unwrap()
            .pending,
        pending.pending
    );
}

#[tokio::test]
async fn sandbox_mount_recovery_rejects_an_unrecognized_native_binding() {
    let f = fixture().await;
    let binding = interrupted_mount(&f, MountState::RecoveryRequired).await;
    let mut other = binding.clone();
    other.generation += 1;
    f.mounts
        .mount(
            MountSpec {
                binding: other.clone(),
            },
            f.engine.services.files.clone(),
        )
        .await
        .unwrap();
    let error = f
        .sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::MountBusy);
    assert!(
        error
            .message
            .contains("native mount identity cannot be confirmed")
    );
    assert_eq!(
        f.mounts.status(&other).await.unwrap(),
        NativeMountState::Mounted
    );
    assert_eq!(
        f.local.mount(binding.id).await.unwrap().unwrap().state,
        MountState::RecoveryRequired
    );
}

#[tokio::test]
async fn sandbox_mount_recovery_checks_the_binding_referenced_by_pending_unmounts() {
    for current_binding in [false, true] {
        let f = fixture().await;
        let binding = interrupted_mount(&f, MountState::RecoveryRequired).await;
        let mut old = binding.clone();
        if !current_binding {
            old.id = MountId::new();
        }
        old.state = MountState::Ready;
        let mut pending = f
            .local
            .operations(f.workspace)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.name == "mount_prepare")
            .unwrap();
        pending.name = "mount_unmount".into();
        pending.id = OperationId::new();
        pending.request_id = OperationId::new().to_string();
        pending.phase = OperationPhase::WaitingForQuiesce;
        pending.pending = Some(PendingAction::MountChange {
            old: Some(old),
            new: None,
        });
        f.local
            .commit(LocalCommit {
                operations: vec![pending.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let result = f.sandbox.create(&f.owner, f.workspace, f.branch).await;
        if current_binding {
            assert_eq!(result.unwrap_err().code, ErrorCode::Busy);
            assert_eq!(
                f.mounts.status(&binding).await.unwrap(),
                NativeMountState::Absent
            );
        } else {
            assert!(matches!(result.unwrap().state, SandboxState::Ready));
            assert_eq!(
                f.local.mount(binding.id).await.unwrap().unwrap().state,
                MountState::Ready
            );
        }
        assert_eq!(
            f.local
                .operations(f.workspace)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.id == pending.id)
                .unwrap()
                .pending,
            pending.pending
        );
    }
}

#[tokio::test]
async fn sandbox_authorizes_branch_and_pins_binding_until_stop() {
    let Fixture {
        _directory,
        local,
        engine,
        owner,
        workspace,
        branch,
        sandbox,
        ..
    } = fixture().await;
    let stranger: Principal = "stranger".to_owned().try_into().unwrap();
    assert_eq!(
        sandbox
            .create(&stranger, workspace, branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        sandbox
            .mcp_config(&stranger, workspace, branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        sandbox
            .mcp_config(&owner, workspace, branch)
            .await
            .unwrap()
            .mcp_servers["ubuntu_desktop"]
            .args[0],
        branch.to_string()
    );
    let first = sandbox.create(&owner, workspace, branch).await.unwrap();
    assert!(
        sandbox
            .create_with_template(&owner, workspace, branch, Some("missing".into()))
            .await
            .is_err()
    );
    assert!(matches!(
        sandbox
            .status(&owner, workspace, branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        SandboxState::Ready
    ));
    assert_eq!(
        sandbox
            .set_proxy_config(&stranger, workspace, branch, SandboxProxyConfig::default())
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );

    let mut services = engine.services.clone();
    services.sandbox = Some(sandbox.clone());
    let catalog = sandboxes(State(services.clone()), Extension(owner.clone()))
        .await
        .map_err(|error| error.0)
        .unwrap()
        .0;
    assert_eq!(catalog["entries"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["entries"][0]["sandbox"]["id"], first.id);
    assert_eq!(catalog["entries"][0]["can_start"], true);
    let other_catalog = sandboxes(State(services.clone()), Extension(stranger.clone()))
        .await
        .map_err(|error| error.0)
        .unwrap()
        .0;
    assert!(other_catalog["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        sandbox.create(&owner, workspace, branch).await.unwrap().id,
        first.id
    );
    assert_eq!(
        sandbox
            .create(&owner, WorkspaceId::new(), branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let mount = local
        .mounts()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.branch == branch)
        .unwrap();
    let result = engine
        .services
        .mounts
        .unmount(
            RequestContext {
                principal: owner.clone(),
                request_id: OperationId::new().to_string(),
            },
            mount.id,
            mount.generation,
        )
        .await;
    match result {
        Err(error) => assert_eq!(error.code, ErrorCode::BranchBusy),
        Ok(record) => assert_eq!(record.error.unwrap().code, ErrorCode::BranchBusy),
    }
    assert_eq!(
        sandbox
            .delete(&stranger, workspace, branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        sandbox
            .delete(&owner, WorkspaceId::new(), branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        sandbox
            .delete(&owner, workspace, branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    sandbox.stop(&owner, workspace, branch).await.unwrap();
    let mismatch = sandbox_delete(
        State(services.clone()),
        Extension(owner.clone()),
        Path((workspace, branch)),
        Json(SandboxDeleteRequest {
            confirm_branch: BranchId::new(),
        }),
    )
    .await;
    assert!(mismatch.is_err());
    assert!(
        sandbox
            .status(&owner, workspace, branch)
            .await
            .unwrap()
            .is_some()
    );
    let deleted = sandbox_delete(
        State(services.clone()),
        Extension(owner.clone()),
        Path((workspace, branch)),
        Json(SandboxDeleteRequest {
            confirm_branch: branch,
        }),
    )
    .await
    .map_err(|error| error.0)
    .unwrap()
    .0;
    assert_eq!(deleted["deleted"], true);
    assert!(
        sandbox
            .status(&owner, workspace, branch)
            .await
            .unwrap()
            .is_none()
    );
    sandbox.delete(&owner, workspace, branch).await.unwrap();
    assert_eq!(
        local
            .mounts()
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.id == mount.id)
            .unwrap()
            .state,
        MountState::Ready
    );
    sandbox.create(&owner, workspace, branch).await.unwrap();
    sandbox.stop(&owner, workspace, branch).await.unwrap();
    let record = engine
        .services
        .mounts
        .unmount(
            RequestContext {
                principal: owner,
                request_id: OperationId::new().to_string(),
            },
            mount.id,
            mount.generation,
        )
        .await
        .unwrap();
    assert!(record.error.is_none(), "{record:?}");
}

async fn unmount(f: &Fixture) -> Result<OperationRecord> {
    let mount = f
        .local
        .mounts()
        .await?
        .into_iter()
        .find(|m| m.branch == f.branch)
        .unwrap();
    f.engine
        .services
        .mounts
        .unmount(
            RequestContext {
                principal: f.owner.clone(),
                request_id: OperationId::new().to_string(),
            },
            mount.id,
            mount.generation,
        )
        .await
}

#[tokio::test]
async fn sandbox_restarts_with_restored_native_mount() {
    let f = fixture().await;
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    let original = f.local.mounts().await.unwrap().remove(0);
    f.sandbox
        .stop(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    f.engine.suspend_native_mounts().await.unwrap();
    assert_eq!(
        f.local.mounts().await.unwrap()[0].state,
        MountState::Suspended
    );
    f.engine.flush_local().await.unwrap();
    f.engine.restore_native_mounts().await.unwrap();
    let restored = f.local.mounts().await.unwrap().remove(0);
    assert_eq!(restored.state, MountState::Ready);
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.path, original.path);
    assert_eq!(restored.branch, original.branch);
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    match unmount(&f).await {
        Err(error) => assert_eq!(error.code, ErrorCode::BranchBusy),
        Ok(record) => assert_eq!(record.error.unwrap().code, ErrorCode::BranchBusy),
    }
    f.sandbox
        .stop(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    let record = unmount(&f).await.unwrap();
    assert!(record.error.is_none(), "{record:?}");
}

#[tokio::test]
async fn sandbox_worker_exit_releases_branch_without_explicit_stop() {
    let f = fixture().await;
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    f.driver.stop(&f.branch.to_string()).await.unwrap();
    f.sandbox.reconcile_exited().await.unwrap();
    assert!(matches!(
        f.sandbox
            .status(&f.owner, f.workspace, f.branch)
            .await
            .unwrap()
            .unwrap()
            .state,
        SandboxState::Stopped
    ));
    let record = unmount(&f).await.unwrap();
    assert!(record.error.is_none(), "{record:?}");
}

#[tokio::test]
async fn sandbox_force_stop_checks_confirmation_and_authority_then_releases_branch() {
    let f = fixture().await;
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    f.driver
        .fail_stop
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        f.sandbox
            .stop(&f.owner, f.workspace, f.branch)
            .await
            .is_err()
    );
    let stranger: Principal = "stranger".to_owned().try_into().unwrap();
    assert_eq!(
        f.sandbox
            .force_stop(&stranger, f.workspace, f.branch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    let mut services = f.engine.services.clone();
    services.sandbox = Some(f.sandbox.clone());
    assert!(
        sandbox_force_stop(
            State(services.clone()),
            Extension(f.owner.clone()),
            Path((f.workspace, f.branch)),
            Json(SandboxDeleteRequest {
                confirm_branch: BranchId::new()
            })
        )
        .await
        .is_err()
    );
    assert!(matches!(
        f.driver.status(&f.branch.to_string()).await.unwrap().state,
        SandboxState::Ready
    ));
    let Json(status) = sandbox_force_stop(
        State(services),
        Extension(f.owner.clone()),
        Path((f.workspace, f.branch)),
        Json(SandboxDeleteRequest {
            confirm_branch: f.branch,
        }),
    )
    .await
    .map_err(|e| e.0)
    .unwrap();
    assert!(matches!(status.state, SandboxState::Stopped));
    let record = unmount(&f).await.unwrap();
    assert!(record.error.is_none(), "{record:?}");
}

#[tokio::test]
async fn sandbox_reconciliation_does_not_release_a_restarting_branch() {
    let f = fixture().await;
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    f.driver.stop(&f.branch.to_string()).await.unwrap();
    f.sandbox.reconcile_exited().await.unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    *f.driver.start_gate.lock().await = Some(gate.clone());
    let service = f.sandbox.clone();
    let owner = f.owner.clone();
    let workspace = f.workspace;
    let branch = f.branch;
    let starting = tokio::spawn(async move { service.create(&owner, workspace, branch).await });
    f.driver.starting.notified().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        f.sandbox.reconcile_exited(),
    )
    .await
    .unwrap()
    .unwrap();
    gate.notify_one();
    starting.await.unwrap().unwrap();
    let error = match unmount(&f).await {
        Ok(record) => record.error.unwrap(),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::BranchBusy);
    f.sandbox
        .stop(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
}

#[tokio::test]
async fn desktop_switch_preserves_running_guest_branch_and_mount() {
    let f = fixture().await;
    let server = crate::McpServer::new(f.engine.services.clone(), f.owner.clone());
    let desktop = f._directory.path().join("desktop");
    let prepared = server
        .dispatch(
            "mount_prepare",
            json!({
                "request_id":"mount-desktop",
                "request":{"workspace":f.workspace, "branch":f.branch, "path":desktop}
            }),
        )
        .await
        .unwrap();
    assert!(
        prepared.get("error").is_none_or(|error| error.is_null()),
        "{prepared}"
    );
    let pinned = f._directory.path().join("another");
    let additional = server
        .dispatch(
            "mount_prepare",
            json!({
                "request_id":"mount-additional",
                "request":{"workspace":f.workspace, "branch":f.branch, "path":pinned}
            }),
        )
        .await
        .unwrap();
    assert_eq!(additional["mount_ready"], true, "{additional}");
    f.sandbox
        .create(&f.owner, f.workspace, f.branch)
        .await
        .unwrap();
    let fork = server
        .dispatch(
            "branch_create",
            json!({
                "request_id":"fork","request":{"workspace":f.workspace, "name":"work"}
            }),
        )
        .await
        .unwrap();
    let branch: BranchId = serde_json::from_value(fork["branch"].clone()).unwrap();
    let switched = server
        .dispatch(
            "branch_switch",
            json!({
                "request_id":"switch","request":{"workspace":f.workspace, "branch":branch}
            }),
        )
        .await
        .unwrap();
    assert_eq!(switched["mount_ready"], true, "{switched}");
    let mounts = f.local.mounts().await.unwrap();
    assert!(
        mounts
            .iter()
            .any(|m| m.path == pinned && m.branch == f.branch && m.state == MountState::Ready)
    );

    assert!(
        mounts
            .iter()
            .any(|m| m.path == desktop && m.branch == branch && m.state == MountState::Ready)
    );
    assert!(mounts.iter().any(|m| m.path.ends_with(f.branch.to_string())
        && m.branch == f.branch
        && m.state == MountState::Ready));
}
