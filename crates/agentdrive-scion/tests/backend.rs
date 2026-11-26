use agentdrive_config::ScionConfig;
use agentdrive_local::LocalBackend;
use agentdrive_model::*;
use agentdrive_ports::{ObjectStream, RemoteObjectStore};
use agentdrive_scion::ScionBackend;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncReadExt;

type Objects = Arc<Mutex<HashMap<String, Bytes>>>;

async fn upload(
    State(objects): State<Objects>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> StatusCode {
    if hex::encode(Sha256::digest(&bytes)) != id {
        return StatusCode::BAD_REQUEST;
    }
    objects.lock().unwrap().insert(id, bytes);
    StatusCode::OK
}

async fn download(
    State(objects): State<Objects>,
    Path(id): Path<String>,
) -> std::result::Result<Bytes, StatusCode> {
    objects
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

async fn batch(State(objects): State<Objects>, Json(body): Json<Value>) -> Json<Value> {
    let mut results = body["objects"].as_array().unwrap().clone();
    for object in &mut results {
        if !objects
            .lock()
            .unwrap()
            .contains_key(object["oid"].as_str().unwrap())
        {
            if body["operation"] == "download" {
                object["error"] = json!({"code":404});
            } else {
                object["actions"] = json!({"upload":{"href":"local"}});
            }
        }
    }
    Json(json!({"objects":results}))
}

async fn roundtrip(
    config: ScionConfig,
) -> (WorkspaceId, ObjectRef, Arc<ScionBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let open = || {
        ScionBackend::open(
            config.clone(),
            Arc::new(LocalBackend::open(dir.path()).unwrap()),
        )
        .unwrap()
    };
    let backend = open();
    let workspace = WorkspaceId::new();
    let mut data = vec![19; 4 * 1024 * 1024];
    data.extend_from_slice(b"tail");
    let file = object_ref(ObjectKind::File, &data);
    backend.check_file_storage().await.unwrap();
    assert!(!backend.contains(workspace, &file).await.unwrap());
    backend
        .upload(
            workspace,
            ObjectStream {
                reference: file.clone(),
                reader: Box::pin(std::io::Cursor::new(data.clone())),
            },
        )
        .await
        .unwrap();
    drop(backend);
    let backend = open();
    assert!(
        backend
            .verified_file_exists(workspace, &file)
            .await
            .unwrap()
    );
    assert_eq!(
        backend
            .read_verified_range(workspace, &file, file.size - 5, 16)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"\x13tail"
    );
    let mut bytes = Vec::new();
    backend
        .download(workspace, &file)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    assert_eq!(data, bytes);
    (workspace, file, backend, dir)
}

#[tokio::test]
async fn sqlite_reopens_verified_multi_chunk_files_and_detects_corruption() {
    let objects = Objects::default();
    let app = Router::new()
        .route("/org/repo.git/info/lfs/objects/batch", post(batch))
        .route(
            "/org/repo.git/info/lfs/objects/{id}",
            get(download).put(upload),
        )
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(objects.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (workspace, file, backend, _dir) = roundtrip(ScionConfig {
        enabled: true,
        endpoint,
        repository: "org/repo".into(),
        token: "test".into(),
        allow_http: true,
    })
    .await;
    objects.lock().unwrap().insert(
        hex::encode(Sha256::digest(b"tail")),
        Bytes::from_static(b"fail"),
    );
    let error = backend
        .read_verified_range(workspace, &file, file.size - 1, 1)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Integrity);
    objects.lock().unwrap().clear();
    assert!(!backend.contains(workspace, &file).await.unwrap());
    task.abort();
}

#[tokio::test]
#[ignore = "requires a writable Scion repository"]
async fn scion_repository_roundtrip() {
    roundtrip(ScionConfig {
        enabled: true,
        endpoint: std::env::var("SCION_ENDPOINT").unwrap(),
        repository: std::env::var("SCION_REPOSITORY").unwrap(),
        token: std::env::var("SCION_TOKEN").unwrap_or_default(),
        allow_http: true,
    })
    .await;
}
