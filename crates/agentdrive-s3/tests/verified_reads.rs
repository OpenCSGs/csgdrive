use agentdrive_config::{S3Config, S3Credentials};
use agentdrive_engine::ContentService;
use agentdrive_local::{DatabaseStorage, LocalBackend};
use agentdrive_model::*;
use agentdrive_ports::*;
use agentdrive_s3::S3Backend;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{Method, Response, StatusCode},
};
use std::sync::{Arc, Mutex};

const BLOCK: usize = 256 * 1024;

#[derive(Default)]
struct Bucket {
    file: Option<Bytes>,
    etag: Option<String>,
    version: Option<String>,
    range_supported: bool,
    corrupt_range: bool,
    requests: Vec<(Method, Option<String>, Option<String>)>,
}

fn success(
    bucket: &Bucket,
    body: Bytes,
    code: StatusCode,
    size: usize,
    range: Option<String>,
) -> Response<Body> {
    let mut r = Response::builder()
        .status(code)
        .header("last-modified", "Mon, 07 Nov 2022 12:00:00 GMT")
        .header("content-length", size.to_string());
    if let Some(v) = &bucket.etag {
        r = r.header("etag", v);
    }
    if let Some(v) = &bucket.version {
        r = r.header("x-amz-version-id", v);
    }
    if let Some(v) = range {
        r = r.header("content-range", v);
    }
    r.body(Body::from(body)).unwrap()
}

async fn handle(State(state): State<Arc<Mutex<Bucket>>>, request: Request) -> Response<Body> {
    let (parts, payload) = request.into_parts();
    let header = |k: &str| {
        parts
            .headers
            .get(k)
            .map(|h| h.to_str().unwrap().to_string())
    };
    let range = header("range");
    let matched = header("if-match");
    let payload = axum::body::to_bytes(payload, 16 * 1024 * 1024)
        .await
        .unwrap();
    let mut b = state.lock().unwrap();
    b.requests
        .push((parts.method.clone(), range.clone(), matched.clone()));
    if parts.method == Method::PUT {
        assert_eq!(header("if-none-match").as_deref(), Some("*"));
        b.file = Some(payload);
        return success(&b, Bytes::new(), StatusCode::OK, 0, None);
    }
    let Some(body) = b.file.clone() else {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    };
    if parts.method == Method::HEAD {
        return success(&b, Bytes::new(), StatusCode::OK, body.len(), None);
    }
    assert_eq!(parts.method, Method::GET);
    if matched.is_some() && matched != b.etag {
        return Response::builder().status(412).body(Body::empty()).unwrap();
    }
    if let Some(range) = range
        && b.range_supported
    {
        let span = range.strip_prefix("bytes=").unwrap();
        let (a, z) = span.split_once('-').unwrap();
        let (a, z) = (a.parse::<usize>().unwrap(), z.parse::<usize>().unwrap());
        let mut bytes = body[a..=z].to_vec();
        if b.corrupt_range {
            bytes[0] ^= 1;
        }
        let len = bytes.len();
        return success(
            &b,
            bytes.into(),
            StatusCode::PARTIAL_CONTENT,
            len,
            Some(format!("bytes {a}-{z}/{}", body.len())),
        );
    }
    success(&b, body.clone(), StatusCode::OK, body.len(), None)
}

fn source(reference: &ObjectRef, data: Bytes) -> ObjectStream {
    ObjectStream {
        reference: reference.clone(),
        reader: Box::pin(std::io::Cursor::new(data)),
    }
}

#[tokio::test]
async fn repeated_upload_and_cold_range_verify_the_remote_representation() {
    let fixture = Arc::new(Mutex::new(Bucket {
        etag: Some("\"v1\"".into()),
        version: Some("one".into()),
        range_supported: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(handle).with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = S3Config {
        enabled: true,
        bucket: "bucket".into(),
        prefix: "prefix".into(),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        allow_http: true,
        credentials: Some(S3Credentials {
            access_key_id: "fixture".into(),
            secret_access_key: "fixture-test-secret".into(),
            session_token: None,
        }),
    };
    let temp = tempfile::tempdir().unwrap();
    let db = LocalBackend::open(temp.path()).unwrap();
    let remote = S3Backend::file_store(cfg.clone(), Arc::new(db.clone())).unwrap();
    let workspace = WorkspaceId::new();
    let data = Bytes::from(
        (0..3 * BLOCK + 123)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let file = object_ref(ObjectKind::File, &data);
    remote
        .upload(workspace, source(&file, data.clone()))
        .await
        .unwrap();
    drop(remote);
    drop(db);
    let db = LocalBackend::open(temp.path()).unwrap();
    let remote = S3Backend::file_store(cfg, Arc::new(db.clone())).unwrap();
    fixture.lock().unwrap().requests.clear();
    remote
        .upload(workspace, source(&file, data.clone()))
        .await
        .unwrap();
    assert!(remote.contains(workspace, &file).await.unwrap());
    assert_eq!(
        fixture.lock().unwrap().requests,
        [(Method::HEAD, None, None), (Method::HEAD, None, None)]
    );

    let adapter = DatabaseStorage::new(db.clone(), "backend".into(), remote.clone()).unwrap();
    let resolver = ContentService::new(Arc::new(db.clone()), Some(adapter.clone()), Some(adapter));
    fixture.lock().unwrap().requests.clear();
    let start = BLOCK - 7;
    assert_eq!(
        resolver
            .read_range(workspace, &file, start as u64, 30)
            .await
            .unwrap(),
        &data[start..start + 30]
    );
    assert_eq!(
        fixture.lock().unwrap().requests,
        [(
            Method::GET,
            Some(format!("bytes=0-{}", 2 * BLOCK - 1)),
            Some("\"v1\"".into())
        )]
    );
    assert!(!db.contains(workspace, &file).await.unwrap());

    fixture.lock().unwrap().corrupt_range = true;
    assert_eq!(
        resolver
            .read_range(workspace, &file, 0, 1)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Integrity
    );
    fixture.lock().unwrap().corrupt_range = false;
    fixture.lock().unwrap().version = Some("two".into());
    assert!(
        remote
            .read_verified_range(workspace, &file, 0, 1)
            .await
            .unwrap()
            .is_none()
    );
    fixture.lock().unwrap().etag = Some("\"v2\"".into());
    assert!(
        remote
            .read_verified_range(workspace, &file, 0, 1)
            .await
            .unwrap()
            .is_none()
    );
    fixture.lock().unwrap().requests.clear();
    assert!(remote.contains(workspace, &file).await.unwrap());
    assert!(remote.contains(workspace, &file).await.unwrap());
    assert_eq!(
        fixture.lock().unwrap().requests,
        [
            (Method::HEAD, None, None),
            (Method::GET, None, None),
            (Method::HEAD, None, None)
        ]
    );

    fixture.lock().unwrap().range_supported = false;
    fixture.lock().unwrap().requests.clear();
    assert_eq!(
        resolver.read_range(workspace, &file, 9, 3).await.unwrap(),
        &data[9..12]
    );
    assert!(db.contains(workspace, &file).await.unwrap());
    assert_eq!(
        fixture.lock().unwrap().requests,
        [
            (
                Method::GET,
                Some(format!("bytes=0-{}", BLOCK - 1)),
                Some("\"v2\"".into())
            ),
            (Method::GET, None, None)
        ]
    );
    fixture.lock().unwrap().requests.clear();
    assert_eq!(
        resolver.read_range(workspace, &file, 8, 6).await.unwrap(),
        &data[8..14]
    );
    assert!(fixture.lock().unwrap().requests.is_empty());

    // A different object has no saved proof. Its first complete transfer records the block hashes.
    let legacy = Bytes::from_static(b"legacy object");
    let legacy_ref = object_ref(ObjectKind::File, &legacy);
    {
        let mut bucket = fixture.lock().unwrap();
        bucket.file = Some(legacy.clone());
        bucket.etag = Some("W/\"weak\"".into());
        bucket.version = None;
        bucket.range_supported = true;
        bucket.requests.clear();
    }
    assert_eq!(
        resolver
            .read_range(workspace, &legacy_ref, 0, 1)
            .await
            .unwrap(),
        &legacy[0..1]
    );
    assert_eq!(
        fixture.lock().unwrap().requests,
        [(Method::GET, None, None)]
    );
    assert_eq!(
        remote
            .read_verified_range(workspace, &legacy_ref, 5, 2)
            .await
            .unwrap()
            .unwrap(),
        &legacy[5..7]
    );
    assert_eq!(
        fixture.lock().unwrap().requests.last().unwrap(),
        &(Method::GET, Some("bytes=0-12".into()), None)
    );
    // A weak validator cannot establish full remote identity. Recheck the bytes even at the same length.
    fixture.lock().unwrap().file = Some(Bytes::from_static(b"damaged data!"));
    assert_eq!(
        remote
            .contains(workspace, &legacy_ref)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Integrity
    );
    server.abort();
}
