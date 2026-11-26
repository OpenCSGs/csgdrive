use agentdrive_config::OpenCsgConfig;
use agentdrive_opencsg::OpenCsgBackend;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct Data {
    pub generation: u64,
    pub xnet_token: Option<Value>,
    pub binary_snapshots: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub commit_sizes: Vec<usize>,
    pub fail_batch_before_commit: bool,
    pub resolved_paths: Vec<String>,
    pub xnet_download: Option<String>,
    pub xnet_upload_mode: Option<String>,
    pub xnet_paths: Vec<String>,
    pub lose_pointer_response: bool,
    pub snapshots: BTreeMap<String, BTreeMap<String, String>>,
    pub ignore_start_sha: bool,
    pub ignore_create_start_sha: bool,
    pub overwrite_created: bool,
    pub lose_write_response: bool,
    pub lose_read_response: bool,
    pub lose_confirmation: bool,
    pub advance_after_write: bool,
    pub probe_conflicts: usize,
    pub fail_stale_probe: bool,
    pub redact_commit_errors: bool,
    pub unavailable: bool,
    pub redirect: Option<String>,
    pub requests: usize,
}
impl Data {
    pub fn head(&self) -> String {
        format!("{:040x}", self.generation)
    }
    pub fn files(&self) -> &BTreeMap<String, String> {
        self.snapshots.get(&self.head()).unwrap()
    }
    pub fn publish(&mut self, files: BTreeMap<String, String>) {
        let binaries = self
            .binary_snapshots
            .get(&self.head())
            .cloned()
            .unwrap_or_default();
        self.generation += 1;
        self.binary_snapshots.insert(self.head(), binaries);
        self.snapshots.insert(self.head(), files);
    }
}

pub struct Server {
    pub data: Arc<Mutex<Data>>,
    pub config: OpenCsgConfig,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn new() -> Self {
        let mut data = Data::default();
        data.publish(BTreeMap::from([(
            "README.md".into(),
            "Storage repository".into(),
        )]));
        let data = Arc::new(Mutex::new(data));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .fallback(handle)
            .with_state(data.clone())
            .layer(DefaultBodyLimit::max(16 * 1024 * 1024));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = OpenCsgConfig {
            enabled: true,
            endpoint: format!("http://{address}"),
            repository: "test/storage".into(),
            branch: "main".into(),
            prefix: "agentdrive/v1".into(),
            token: "private-test-token".into(),
            allow_http: true,
            xnet: false,
        };
        Self { data, config, task }
    }
    pub fn backend(&self) -> Arc<OpenCsgBackend> {
        OpenCsgBackend::new(self.config.clone()).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn ok(data: Value) -> Response {
    axum::Json(json!({"msg":"OK", "data":data})).into_response()
}
fn error(status: StatusCode) -> Response {
    (
        status,
        axum::Json(json!({"msg":"private-test-token must never appear in client diagnostics"})),
    )
        .into_response()
}

fn commit_error(data: &Data, detail: &str) -> Response {
    let message = if data.redact_commit_errors {
        "GIT-ERR-22: Commit git files failed"
    } else {
        detail
    };
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"code":"GIT-ERR-22", "msg":message})),
    )
        .into_response()
}

async fn handle(
    State(state): State<Arc<Mutex<Data>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut data = state.lock().unwrap();
    data.requests += 1;
    if let Some(location) = &data.redirect {
        return (StatusCode::FOUND, [("location", location)]).into_response();
    }
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some("Bearer private-test-token")
    {
        return error(StatusCode::UNAUTHORIZED);
    }
    if data.unavailable {
        return error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if method == Method::GET && data.lose_read_response {
        data.lose_read_response = false;
        return error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if uri.path().starts_with("/hf/codes/test/storage/xet-") {
        return match &data.xnet_token {
            Some(token) => axum::Json(token.clone()).into_response(),
            None => error(StatusCode::NOT_FOUND),
        };
    }
    if let Some(rest) = uri.path().strip_prefix("/hf/codes/test/storage/resolve/") {
        let (revision, path) = rest.split_once('/').unwrap();
        data.resolved_paths.push(path.to_owned());
        if let Some(bytes) = data
            .binary_snapshots
            .get(revision)
            .and_then(|items| items.get(path))
        {
            return bytes.clone().into_response();
        }
        let Some(pointer) = data
            .snapshots
            .get(revision)
            .and_then(|files| files.get(path))
        else {
            return error(StatusCode::NOT_FOUND);
        };
        let oid = pointer
            .lines()
            .find_map(|line| line.strip_prefix("oid sha256:"))
            .unwrap();
        let location = data.xnet_download.as_ref().unwrap().replace("{oid}", oid);
        return (StatusCode::FOUND, [("location", location)]).into_response();
    }
    let Some(path) = uri.path().strip_prefix("/api/v1/codes/test/storage/") else {
        return error(StatusCode::NOT_FOUND);
    };
    let url = url::Url::parse(&format!("http://localhost{uri}")).unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    if path == "preupload/main" && method == Method::POST {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let path = request["files"][0]["path"].as_str().unwrap();
        data.xnet_paths.push(path.to_owned());
        return ok(
            json!({"files":[{"path":path, "uploadMode":data.xnet_upload_mode,
                          "shouldIgnore":false, "isDir":false}]}),
        );
    }
    if path == "commit/main" && method == Method::POST {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let entries = request["files"].as_array().unwrap();
        data.commit_sizes.push(entries.len());
        if std::mem::take(&mut data.fail_batch_before_commit) {
            return error(StatusCode::SERVICE_UNAVAILABLE);
        }
        let mut files = data.files().clone();
        let mut binaries = Vec::new();
        for file in entries {
            let path = file["path"].as_str().unwrap();
            let bytes = STANDARD.decode(file["content"].as_str().unwrap()).unwrap();
            if path.contains("/chunks/") {
                // Codes checks pointer-shaped payloads against its LFS object store at commit time.
                if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\noid sha256:") {
                    return error(StatusCode::UNPROCESSABLE_ENTITY);
                }
                files.insert(path.to_owned(), "<binary>".to_owned());
                binaries.push((path.to_owned(), bytes));
            } else {
                files.insert(path.to_owned(), String::from_utf8(bytes).unwrap());
            }
        }
        data.publish(files);
        let head = data.head();
        data.binary_snapshots
            .get_mut(&head)
            .unwrap()
            .extend(binaries);
        if std::mem::take(&mut data.lose_pointer_response) {
            return error(StatusCode::SERVICE_UNAVAILABLE);
        }
        return ok(Value::Null);
    }
    if path == "branches" {
        return ok(json!([{"name":"main", "commit":{"id":data.head()}}]));
    }
    if let Some(rest) = path.strip_prefix("refs/") {
        let Some((commit, directory)) = rest.split_once("/tree/") else {
            return error(StatusCode::NOT_FOUND);
        };
        let Some(files) = data.snapshots.get(commit) else {
            return error(StatusCode::NOT_FOUND);
        };
        let prefix = format!("{directory}/");
        let mut entries = BTreeMap::new();
        for name in files.keys().filter_map(|path| path.strip_prefix(&prefix)) {
            let (name, kind) = match name.split_once('/') {
                Some((name, _)) => (name, "dir"),
                None => (name, "file"),
            };
            entries.insert(name.to_string(), json!({"name":name,"type":kind}));
        }
        let offset = query
            .get("cursor")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = query
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100);
        let cursor = if offset + limit < entries.len() {
            (offset + limit).to_string()
        } else {
            String::new()
        };
        return ok(
            json!({"Files":entries.into_values().skip(offset).take(limit).collect::<Vec<_>>(),"Cursor":cursor}),
        );
    }
    let Some(path) = path.strip_prefix("raw/") else {
        return error(StatusCode::NOT_FOUND);
    };
    if method == Method::GET {
        let head = data.head();
        let commit = query.get("ref").unwrap_or(&head);
        return match data.snapshots.get(commit).and_then(|files| files.get(path)) {
            Some(content) => ok(content.clone().into()),
            None => error(StatusCode::NOT_FOUND),
        };
    }
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(_) => return error(StatusCode::BAD_REQUEST),
    };
    if method == Method::DELETE {
        let mut files = data.files().clone();
        files.remove(path);
        data.publish(files);
        return ok(Value::Null);
    }
    let content = STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
    if content == b"updated" && data.probe_conflicts > 0 {
        data.probe_conflicts -= 1;
        let files = data.files().clone();
        data.publish(files);
    }
    if content == b"stale" && data.fail_stale_probe {
        return error(StatusCode::SERVICE_UNAVAILABLE);
    }
    let ignore_start_sha =
        data.ignore_start_sha || (method == Method::POST && data.ignore_create_start_sha);
    if !ignore_start_sha && body["start_sha"].as_str() != Some(data.head().as_str()) {
        return commit_error(&data, "reference does not point to expected object");
    }
    let mut files = data.files().clone();
    if method == Method::POST && files.contains_key(path) && !data.overwrite_created {
        return commit_error(&data, "rpc error: A file with this name already exists");
    }
    if method == Method::PUT && !files.contains_key(path) {
        return error(StatusCode::NOT_FOUND);
    }
    files.insert(path.into(), String::from_utf8(content).unwrap());
    data.publish(files);
    if data.advance_after_write {
        data.advance_after_write = false;
        let mut files = data.files().clone();
        files.insert(path.into(), "advanced".into());
        data.publish(files);
    }
    if data.lose_confirmation {
        data.lose_confirmation = false;
        data.lose_read_response = true;
    }
    if data.lose_write_response {
        data.lose_write_response = false;
        return error(StatusCode::SERVICE_UNAVAILABLE);
    }
    ok(Value::Null)
}
