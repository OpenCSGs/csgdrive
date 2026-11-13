use super::*;
use agentdrive_config::{LiveConfig, RuntimeConfig};
use agentdrive_engine::{Adapters, Engine, EngineConfig, SandboxService};
use agentdrive_local::LocalBackend;
use agentdrive_test_support::{ManualClock, MemoryMountDriver, NoHostDirectories};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::Mutex;

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
                .is_none_or(|previous| matches!(previous.state, SandboxState::Stopped)),
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
}
async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(directory.path()).unwrap());
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(ManualClock::default()),
            mounts: Arc::new(MemoryMountDriver::default()),
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
