use super::*;
use agentdrive_config::{LiveConfig, RuntimeConfig};
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_test_support::{ManualClock, MemoryMountDriver, NoHostDirectories};

async fn serve(services: Services, actor: Principal) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let app = api(services).layer(Extension(actor));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn merge_history_http_is_read_only_and_reports_mcp_receipts() {
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
            json!({"request_id":OperationId::new(),"request":{"name":"history"}}),
        )
        .await
        .unwrap();
    let workspace: Workspace =
        serde_json::from_value(created["result"]["workspace"].clone()).unwrap();
    let initial = local
        .revision(workspace.initial_revision)
        .await
        .unwrap()
        .unwrap();
    let branch = local
        .branch(initial.branch.unwrap())
        .await
        .unwrap()
        .unwrap();
    let prepared = server
        .dispatch(
            "merge_prepare",
            json!({
                "request_id":OperationId::new(),
                "request": {
                    "workspace":workspace.id,
                    "source":initial.id,
                    "target":BranchGuard::from(&branch),
                }
            }),
        )
        .await
        .unwrap();
    assert!(prepared["error"].is_null(), "{prepared}");
    let (address, task) = serve(engine.services.clone(), owner).await;
    let client = reqwest::Client::new();
    let endpoint = format!("{address}/api/workspaces/{}/merges", workspace.id);
    let response = client
        .get(format!("{endpoint}?limit=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: MergeHistoryPage = response.json().await.unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].status, MergeHistoryStatus::AlreadyIncluded);
    let record = &page.entries[0].id;
    let detail: MergeHistoryDetail = client
        .get(format!("{endpoint}/{record}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail.entry.source_revision, Some(initial.id));
    assert!(detail.entry.history_complete);
    let events: MergeHistoryEvents = client
        .get(format!("{endpoint}/{record}/events?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events.entries.len(), 1);
    assert!(events.next.is_some());
    for query in [
        "limit=0",
        "limit=201",
        "status=unknown",
        "unexpected=yes",
        "cursor=bad",
    ] {
        assert_eq!(
            client
                .get(format!("{endpoint}?{query}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    assert_eq!(
        client
            .get(format!("{endpoint}/invalid-id"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!("{endpoint}/{}", MergeId::new()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .post(&endpoint)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        client
            .post(format!("{address}/api/commands/merge_apply"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let visitor: Principal = "visitor".to_owned().try_into().unwrap();
    let (guest, guest_task) = serve(engine.services.clone(), visitor).await;
    for path in [
        format!("/api/workspaces/{}/merges", workspace.id),
        format!("/api/workspaces/{}/merges/{record}/events", workspace.id),
    ] {
        assert_eq!(
            client
                .get(format!("{guest}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    task.abort();
    guest_task.abort();
}
