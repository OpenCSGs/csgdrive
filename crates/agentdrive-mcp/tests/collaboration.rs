use agentdrive_config::{ConfigStore, initialize};
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_mcp::{GatewayConfig, router};
use agentdrive_model::*;
use agentdrive_ports::*;
use agentdrive_test_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use tokio_util::sync::CancellationToken;

struct Server {
    directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
    http: reqwest::Client,
    base: String,
    workspace: WorkspaceId,
    branch: BranchId,
    mounts: Arc<MemoryMountDriver>,
    engine: Arc<Engine>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = initialize(
            &directory.path().join("config"),
            "127.0.0.1:7421".parse().unwrap(),
        )
        .unwrap();
        let settings = ConfigStore::open(&path).unwrap();
        let local = Arc::new(LocalBackend::open(directory.path().join("data")).unwrap());
        let remote = Arc::new(MemoryRemote::default());
        let mounts = Arc::new(MemoryMountDriver::default());
        let engine = Engine::open(
            Adapters {
                state: local.clone(),
                objects: local.clone(),
                working: local,
                remote_objects: Some(remote.clone()),
                remote_refs: Some(remote),
                clock: Arc::new(ManualClock::default()),
                mounts: mounts.clone(),
                directories: Arc::new(NoHostDirectories),
            },
            EngineConfig {
                live: settings.live(),
                identity: LocalIdentity {
                    uid: 1000,
                    gid: 1000,
                },
            },
        )
        .await
        .unwrap();
        let actor: Principal = "owner".to_owned().try_into().unwrap();
        let operation = engine
            .services
            .workspace
            .create(
                RequestContext {
                    principal: actor.clone(),
                    request_id: "create".into(),
                },
                CreateWorkspace {
                    name: "shared".into(),
                    grants: vec![Grant {
                        principal: "worker".to_owned().try_into().unwrap(),
                        branch: None,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                    max_file_bytes: None,
                    max_working_bytes: None,
                    max_inodes: None,
                },
            )
            .await
            .unwrap();
        let Some(CommandResult::Workspace { workspace }) = operation.result else {
            panic!("workspace was not created")
        };
        engine.sync_pending().await.unwrap();
        let branch = engine
            .services
            .workspace
            .status(&actor, workspace.id)
            .await
            .unwrap()
            .branches[0]
            .id;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(
            engine.services.clone(),
            GatewayConfig {
                allowed_hosts: vec![address.to_string()],
                settings,
                cancellation: CancellationToken::new(),
            },
        )
        .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            task,
            http: reqwest::Client::new(),
            base: format!("http://{address}"),
            workspace: workspace.id,
            branch,
            mounts,
            engine,
        }
    }

    async fn attach(&self) -> Value {
        self.attach_with_automatic(true).await
    }

    async fn attach_with_automatic(&self, automatic: bool) -> Value {
        let response = self
            .http
            .post(format!("{}/api/lifecycle", self.base))
            .json(&json!({
                "event":"attach","request_id":format!("attach-{automatic}"),"workspace":self.workspace,
                "branch":self.branch,"automatic":automatic,
                "mount_path":self.directory.path().join("config").join("mounts").join("shared"),
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let result: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{result}");
        result["binding"].clone()
    }

    async fn set_collaboration_enabled(&self, enabled: bool) {
        let mut settings: Value = self
            .http
            .get(format!("{}/api/settings", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        settings["runtime"]["collaboration"]["enabled"] = enabled.into();
        let response = self
            .http
            .put(format!("{}/api/settings", self.base))
            .json(&json!({"revision":settings["revision"],"runtime":settings["runtime"]}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let result: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{result}");
        assert_eq!(result["runtime"]["collaboration"]["enabled"], enabled);
    }
}

#[tokio::test]
async fn collaboration_routes_enforce_binding_generation() {
    let server = Server::new().await;
    let mount = server.attach_with_automatic(false).await;
    let path = format!(
        "{}/api/workspaces/{}/collaboration",
        server.base, server.workspace
    );

    let status: Value = server
        .http
        .get(&path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        status
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["settings"]["enabled"] == true)
    );
    let input = json!({"mount":mount["id"],"expected_binding_generation":mount["generation"].as_u64().unwrap()+1,
        "enabled":true});
    let stale = server
        .http
        .put(format!("{}/api/collaboration", server.base))
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
}

#[tokio::test]
async fn codex_events_require_registration_and_replay_terminal_outcomes() {
    let server = Server::new().await;
    let mount = server.attach().await;
    let url = format!("{}/api/collaboration/codex", server.base);
    let mut event = json!({"mount":mount["id"],"source":"codex-test","method":"turn/completed",
        "params":{"threadId":"thread-123","turn":{"id":"turn-456","status":"completed"}}});
    let unknown = server.http.post(&url).json(&event).send().await.unwrap();
    assert!(unknown.status().is_client_error());
    event["method"] = json!("turn/started");
    event["params"]["turn"]["status"] = json!("inProgress");
    for _ in 0..2 {
        let response = server.http.post(&url).json(&event).send().await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{body}");
        assert_eq!(body["active_executions"], 1);
    }
    event["method"] = json!("turn/completed");
    event["params"]["turn"]["status"] = json!("interrupted");
    for _ in 0..2 {
        let response = server.http.post(&url).json(&event).send().await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{body}");
        assert_eq!(body["active_executions"], 0);
    }
    event["params"]["turn"]["status"] = json!("inProgress");
    assert_eq!(
        server
            .http
            .post(&url)
            .json(&event)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn unsupported_drivers_reject_automatic_mounts_and_configuration() {
    let server = Server::new().await;
    let mount = server.attach().await;
    server
        .mounts
        .automatic_collaboration_disabled
        .store(true, Ordering::SeqCst);
    let response = server.http.post(format!("{}/api/lifecycle", server.base))
        .json(&json!({"event":"attach","request_id":"unsupported-automatic","workspace":server.workspace,
            "branch":server.branch,"automatic":true,
            "mount_path":server.directory.path().join("config").join("mounts").join("unsupported")}))
        .send().await.unwrap();
    assert!(response.status().is_client_error());
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "UNSUPPORTED"
    );
    let response = server
        .http
        .put(format!("{}/api/collaboration", server.base))
        .json(
            &json!({"mount":mount["id"],"expected_binding_generation":mount["generation"],
            "enabled":true}),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "UNSUPPORTED"
    );
}

#[tokio::test]
async fn global_settings_control_collaboration_and_branch_configuration_cannot_override_them() {
    let server = Server::new().await;
    let mount = server.attach().await;
    server.set_collaboration_enabled(false).await;
    let response = server
        .http
        .put(format!("{}/api/collaboration", server.base))
        .json(
            &json!({"mount":mount["id"],"expected_binding_generation":mount["generation"],
            "enabled":true}),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let response = server
        .http
        .put(format!("{}/api/collaboration", server.base))
        .json(
            &json!({"mount":mount["id"],"expected_binding_generation":mount["generation"],
            "enabled":false}),
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    assert_eq!(body["settings"]["enabled"], false);
    assert_eq!(body["phase"], "disabled");
    let status: Value = server
        .http
        .get(format!(
            "{}/api/workspaces/{}/archive",
            server.base, server.workspace
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["capabilities"]["collaboration_enabled"], false);
    server.set_collaboration_enabled(true).await;
    server.engine.collaborate_due().await.unwrap();
    let statuses: Value = server
        .http
        .get(format!(
            "{}/api/workspaces/{}/collaboration",
            server.base, server.workspace
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        statuses
            .as_array()
            .unwrap()
            .iter()
            .any(|status| status["settings"]["enabled"] == true)
    );
}
