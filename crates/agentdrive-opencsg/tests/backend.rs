mod support;
use agentdrive_model::*;
use agentdrive_ports::*;
use bytes::Bytes;
use sha2::Digest;
use support::Server;
use tokio::io::AsyncReadExt;

fn stream(bytes: Vec<u8>) -> ObjectStream {
    ObjectStream {
        reference: object_ref(ObjectKind::File, &bytes),
        reader: Box::pin(std::io::Cursor::new(bytes)),
    }
}

#[tokio::test]
async fn binary_objects_survive_chunking_restart_and_repeated_uploads() {
    let server = Server::new().await;
    let backend = server.backend();
    let workspace = WorkspaceId::new();
    let bytes: Vec<u8> = (0..4 * 1024 * 1024 + 37)
        .map(|index| (index % 256) as u8)
        .collect();
    let reference = object_ref(ObjectKind::File, &bytes);
    assert!(!backend.contains(workspace, &reference).await.unwrap());
    backend
        .upload(workspace, stream(bytes.clone()))
        .await
        .unwrap();
    assert!(backend.contains(workspace, &reference).await.unwrap());
    let before = server.data.lock().unwrap().generation;
    backend
        .upload(workspace, stream(bytes.clone()))
        .await
        .unwrap();
    assert_eq!(server.data.lock().unwrap().generation, before);
    let second = server.backend();
    let mut restored = Vec::new();
    second
        .download(workspace, &reference)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut restored)
        .await
        .unwrap();
    assert_eq!(restored, bytes);
    let empty = stream(Vec::new());
    let reference = empty.reference.clone();
    backend.upload(workspace, empty).await.unwrap();
    let mut output = Vec::new();
    second
        .download(workspace, &reference)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut output)
        .await
        .unwrap();
    assert!(output.is_empty());
}

#[tokio::test]
async fn invalid_uploads_do_not_publish_or_poison_object_identities() {
    let server = Server::new().await;
    let backend = server.backend();
    let workspace = WorkspaceId::new();
    let mut invalid = stream(b"expected".to_vec());
    let reference = invalid.reference.clone();
    invalid.reader = Box::pin(std::io::Cursor::new(b"modified".to_vec()));
    assert_eq!(
        backend.upload(workspace, invalid).await.unwrap_err().code,
        ErrorCode::Integrity
    );
    assert!(!backend.contains(workspace, &reference).await.unwrap());
    backend
        .upload(workspace, stream(b"expected".to_vec()))
        .await
        .unwrap();
    {
        let mut data = server.data.lock().unwrap();
        let id = format!("{:x}", sha2::Sha256::digest(b"expected"));
        let chunk = format!("agentdrive/v1/{workspace}/chunks/{id}");
        for files in data.binary_snapshots.values_mut() {
            if let Some(bytes) = files.get_mut(&chunk) {
                *bytes = b"corrupt!".to_vec();
            }
        }
    }
    let mut output = Vec::new();
    assert!(
        backend
            .download(workspace, &reference)
            .await
            .unwrap()
            .reader
            .read_to_end(&mut output)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn conditional_references_reject_stale_writers_and_recover_lost_acknowledgements() {
    let server = Server::new().await;
    let first = server.backend();
    let second = server.backend();
    first.check_connection().await.unwrap();
    second.check_connection().await.unwrap();
    let key = RefKey::Workspace(WorkspaceId::new());
    let outcome = first
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: None,
            bytes: Bytes::from_static(b"initial"),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, CasOutcome::Applied { .. }));
    let version = first.get(&key).await.unwrap().unwrap().version;
    let (a, b) = tokio::join!(
        first.compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(version.clone()),
            bytes: Bytes::from_static(b"first")
        }),
        second.compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(version),
            bytes: Bytes::from_static(b"second")
        }),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| matches!(v, CasOutcome::Applied { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| matches!(v, CasOutcome::Conflict))
            .count(),
        1
    );
    let version = first.get(&key).await.unwrap().unwrap().version;
    server.data.lock().unwrap().lose_write_response = true;
    let outcome = first
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(version),
            bytes: Bytes::from_static(b"confirmed"),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, CasOutcome::Applied { .. }));
    assert_eq!(second.get(&key).await.unwrap().unwrap().bytes, "confirmed");

    let version = first.get(&key).await.unwrap().unwrap().version;
    server.data.lock().unwrap().lose_confirmation = true;
    let outcome = first
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(version),
            bytes: Bytes::from_static(b"uncertain"),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, CasOutcome::Unknown));
    assert_eq!(second.get(&key).await.unwrap().unwrap().bytes, "uncertain");

    let version = first.get(&key).await.unwrap().unwrap().version;
    server.data.lock().unwrap().advance_after_write = true;
    let outcome = first
        .compare_exchange(RefUpdate {
            key: key.clone(),
            expected_version: Some(version),
            bytes: Bytes::from_static(b"intermediate"),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, CasOutcome::Unknown));
    assert_eq!(second.get(&key).await.unwrap().unwrap().bytes, "advanced");
}

#[tokio::test]
async fn incompatible_services_are_rejected_before_publishing_data() {
    for (ignore_start_sha, overwrite_created, ignore_create_start_sha) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let server = Server::new().await;
        {
            let mut data = server.data.lock().unwrap();
            data.ignore_start_sha = ignore_start_sha;
            data.overwrite_created = overwrite_created;
            data.ignore_create_start_sha = ignore_create_start_sha;
        }
        let error = server.backend().check_connection().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported);
    }
}

#[tokio::test]
async fn connection_probe_retries_concurrent_commits_and_rejects_inconclusive_checks() {
    let server = Server::new().await;
    server.data.lock().unwrap().probe_conflicts = 2;
    server.backend().check_connection().await.unwrap();
    assert_eq!(server.data.lock().unwrap().probe_conflicts, 0);

    let backend = server.backend();
    server.data.lock().unwrap().fail_stale_probe = true;
    assert_eq!(
        backend.check_connection().await.unwrap_err().code,
        ErrorCode::Unavailable
    );
    server.data.lock().unwrap().fail_stale_probe = false;
    backend.check_connection().await.unwrap();
}

#[tokio::test]
async fn redacted_commit_errors_preserve_conditional_write_verification() {
    let server = Server::new().await;
    server.data.lock().unwrap().redact_commit_errors = true;
    server.backend().check_connection().await.unwrap();

    for (ignore_start_sha, overwrite_created, ignore_create_start_sha) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        {
            let mut data = server.data.lock().unwrap();
            data.ignore_start_sha = ignore_start_sha;
            data.overwrite_created = overwrite_created;
            data.ignore_create_start_sha = ignore_create_start_sha;
        }
        assert_eq!(
            server.backend().check_connection().await.unwrap_err().code,
            ErrorCode::Unsupported
        );
    }
}

#[tokio::test]
async fn branch_discovery_reads_all_pages_and_errors_do_not_expose_tokens() {
    let server = Server::new().await;
    let backend = server.backend();
    let workspace = WorkspaceId::new();
    let mut expected = std::collections::BTreeSet::new();
    {
        let mut data = server.data.lock().unwrap();
        let mut files = data.files().clone();
        for _ in 0..235 {
            let branch = BranchId::new();
            expected.insert(branch);
            files.insert(
                format!("agentdrive/v1/{workspace}/branches/{branch}.json"),
                "{}".into(),
            );
        }
        data.publish(files);
    }
    assert_eq!(
        backend.list_branches(workspace).await.unwrap(),
        expected.into_iter().collect::<Vec<_>>()
    );
    server.data.lock().unwrap().unavailable = true;
    let error = backend
        .get(&RefKey::Workspace(workspace))
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("private-test-token"));
}

#[tokio::test]
async fn workspace_discovery_reads_directory_pages_and_ignores_unrelated_names() {
    let server = Server::new().await;
    let backend = server.backend();
    let mut expected = std::collections::BTreeSet::new();
    {
        let mut data = server.data.lock().unwrap();
        let mut files = data.files().clone();
        for _ in 0..235 {
            let workspace = WorkspaceId::new();
            expected.insert(workspace);
            files.insert(
                format!("agentdrive/v1/{workspace}/control.json"),
                "{}".into(),
            );
        }
        files.insert("agentdrive/v1/README.md".into(), "catalog".into());
        files.insert("agentdrive/v1/unrelated/file.json".into(), "{}".into());
        data.publish(files);
    }
    assert_eq!(
        backend.list_workspaces().await.unwrap(),
        expected.into_iter().collect::<Vec<_>>()
    );
    server.data.lock().unwrap().unavailable = true;
    assert!(backend.list_workspaces().await.is_err());
}

#[tokio::test]
async fn credentials_are_not_forwarded_to_redirects() {
    let server = Server::new().await;
    let destination = Server::new().await;
    server.data.lock().unwrap().redirect = Some(destination.config.endpoint.clone());
    assert!(server.backend().check_connection().await.is_err());
    assert_eq!(destination.data.lock().unwrap().requests, 0);
}

/// Exercises the Rust adapter and the real hf-xet native library with a separately
/// running XNet server. Supply AGENTDRIVE_TEST_XNET_TOKEN_FILE (scoped JSON token)
/// and AGENTDRIVE_TEST_XNET_DOWNLOAD_URL (the test server's signed URL with {oid}).
#[tokio::test]
#[ignore = "requires a running XNet CAS, repository credentials, a download URL and hf-xet"]
async fn codes_xnet_recovers_a_lost_pointer_response_and_reads_binary_content() {
    let mut server = Server::new().await;
    server.config.xnet = true;
    let token_file = std::env::var_os("AGENTDRIVE_TEST_XNET_TOKEN_FILE").unwrap();
    let token = serde_json::from_slice(&std::fs::read(token_file).unwrap()).unwrap();
    {
        let mut data = server.data.lock().unwrap();
        data.xnet_token = Some(token);
        data.xnet_upload_mode = Some("lfs".into());
        data.lose_pointer_response = true;
        data.xnet_download = Some(std::env::var("AGENTDRIVE_TEST_XNET_DOWNLOAD_URL").unwrap());
    }
    let workspace = WorkspaceId::new();
    let mut bytes = vec![0; 3 * 1024 * 1024 + 31];
    bytes[..16].copy_from_slice(&workspace.to_string().as_bytes()[..16]);
    let reference = object_ref(ObjectKind::File, &bytes);
    let backend = server.backend();
    backend
        .upload(workspace, stream(bytes.clone()))
        .await
        .unwrap();
    let pointer_path = format!("agentdrive/v1/{workspace}/files/{}", reference.id);
    let hash = sha2::Sha256::digest(&bytes);
    {
        let data = server.data.lock().unwrap();
        assert_eq!(
            data.xnet_paths.as_slice(),
            std::slice::from_ref(&pointer_path)
        );
        assert_eq!(
            data.files()[&pointer_path],
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{hash:x}\nsize {}\n",
                bytes.len()
            )
        );
    }
    let mut restored = Vec::new();
    server
        .backend()
        .download(workspace, &reference)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut restored)
        .await
        .unwrap();
    assert_eq!(restored, bytes);
    assert!(backend.contains(workspace, &reference).await.unwrap());

    server.data.lock().unwrap().xnet_upload_mode = Some("regular".into());
    let small = b"normal Codes storage".to_vec();
    let small_ref = object_ref(ObjectKind::File, &small);
    backend
        .upload(workspace, stream(small.clone()))
        .await
        .unwrap();
    let mut result = Vec::new();
    backend
        .download(workspace, &small_ref)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut result)
        .await
        .unwrap();
    assert_eq!(result, small);

    server.data.lock().unwrap().xnet_upload_mode = Some("unexpected".into());
    assert_eq!(
        backend
            .upload(workspace, stream(b"x".to_vec()))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Integrity
    );
}

#[tokio::test]
async fn native_pointer_reads_use_the_fixed_revision_and_keep_credentials_at_the_hub() {
    use axum::{Router, body::Bytes as HttpBytes, http::HeaderMap, routing::get};
    let server = Server::new().await;
    let storage = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = storage.local_addr().unwrap();
    let expected = b"content through XNet";
    let workspace = WorkspaceId::new();
    let object = object_ref(ObjectKind::File, expected);
    let oid = format!("{:x}", sha2::Sha256::digest(expected));
    let file_path = format!("agentdrive/v1/{workspace}/files/{}", object.id);
    let revision = {
        let mut data = server.data.lock().unwrap();
        let mut files = data.files().clone();
        files.insert(
            file_path.clone(),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
                expected.len()
            ),
        );
        data.publish(files);
        data.xnet_download = Some(format!("http://{address}/object"));
        data.head()
    };
    let directory = tempfile::tempdir().unwrap();
    let db =
        std::sync::Arc::new(agentdrive_local::LocalBackend::open_server(directory.path()).unwrap());
    db.save_file_placement(
        workspace,
        &object.id,
        serde_json::to_vec(&serde_json::json!({
            "object": object, "revision": revision, "format":"git_lfs", "oid":oid, "path":file_path
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let app = Router::new()
        .route(
            "/object",
            get(move |headers: HeaderMap| async move {
                assert!(headers.get("Authorization").is_none());
                HttpBytes::from_static(expected)
            }),
        )
        .route("/corrupt", get(|| async { "invalid" }));
    let service = tokio::spawn(async move { axum::serve(storage, app).await.unwrap() });
    let backend =
        agentdrive_opencsg::OpenCsgBackend::file_store(server.config.clone(), db).unwrap();
    let mut restored = Vec::new();
    backend
        .download(workspace, &object)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut restored)
        .await
        .unwrap();
    assert_eq!(restored, expected);
    server.data.lock().unwrap().xnet_download = Some(format!("http://{address}/corrupt"));
    let error = backend.download(workspace, &object).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Integrity);
    service.abort();
}

#[tokio::test]
async fn server_keeps_branch_refs_tree_pages_and_chunk_manifests_in_sql() {
    use agentdrive_local::{DatabaseStorage, LocalBackend};
    use agentdrive_opencsg::OpenCsgBackend;
    use std::sync::Arc;

    let server = Server::new().await;
    let directory = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let branch = BranchId::new();
    let key = RefKey::Branch { workspace, branch };
    let payload: Vec<u8> = (0..4 * 1024 * 1024 + 1).map(|i| (i % 251) as u8).collect();
    let file = object_ref(ObjectKind::File, &payload);
    let tree = object_ref(ObjectKind::Tree, b"tree");
    let target = agentdrive_config::RemoteConfig::OpenCsg(server.config.clone())
        .storage_target()
        .unwrap();
    {
        let db = LocalBackend::open_server(directory.path()).unwrap();
        let codes =
            OpenCsgBackend::file_store(server.config.clone(), Arc::new(db.clone())).unwrap();
        let backend = DatabaseStorage::new(db.clone(), target.clone(), codes).unwrap();
        db.put(workspace, ObjectKind::Tree, Bytes::from_static(b"tree"))
            .await
            .unwrap();
        assert!(
            backend
                .publish_stored_metadata(workspace, &tree)
                .await
                .unwrap()
        );
        assert!(
            backend
                .publish_stored_metadata(workspace, &tree)
                .await
                .unwrap()
        );
        let missing = object_ref(ObjectKind::Tree, b"absent");
        assert!(
            backend
                .publish_stored_metadata(workspace, &missing)
                .await
                .is_err()
        );
        backend
            .upload(workspace, stream(payload.clone()))
            .await
            .unwrap();
        let initial = backend
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: None,
                bytes: Bytes::from_static(b"branch-head"),
            })
            .await
            .unwrap();
        assert!(matches!(initial, CasOutcome::Applied { .. }));
        let duplicate = backend
            .compare_exchange(RefUpdate {
                key: key.clone(),
                expected_version: None,
                bytes: Bytes::from_static(b"stale"),
            })
            .await
            .unwrap();
        assert!(matches!(duplicate, CasOutcome::Conflict));
        assert!(
            !directory
                .path()
                .join("objects")
                .join(workspace.to_string())
                .join(tree.id.as_str())
                .exists()
        );
        let data = server.data.lock().unwrap();
        let stored: Vec<_> = data
            .files()
            .keys()
            .filter(|key| key.as_str() != "README.md")
            .collect();
        assert_eq!(stored.len(), 2);
        assert!(stored.iter().all(|key| key.contains("/chunks/")));
    }
    let db = LocalBackend::open_server(directory.path()).unwrap();
    let codes = OpenCsgBackend::file_store(server.config.clone(), Arc::new(db.clone())).unwrap();
    let backend = DatabaseStorage::new(db, target, codes).unwrap();
    assert_eq!(
        backend.get(&key).await.unwrap().unwrap().bytes.as_ref(),
        b"branch-head"
    );
    assert_eq!(backend.list_branches(workspace).await.unwrap(), [branch]);
    for (reference, expected) in [(file, payload), (tree, b"tree".to_vec())] {
        let mut result = Vec::new();
        backend
            .download(workspace, &reference)
            .await
            .unwrap()
            .reader
            .read_to_end(&mut result)
            .await
            .unwrap();
        assert_eq!(result, expected);
    }
}

#[tokio::test]
async fn git_batches_recover_lost_replies_and_range_reads_verify_only_requested_chunks() {
    use agentdrive_local::LocalBackend;
    use agentdrive_opencsg::OpenCsgBackend;
    use std::sync::Arc;

    const CHUNK: usize = 4 * 1024 * 1024;
    let server = Server::new().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(LocalBackend::open_server(dir.path()).unwrap());
    let workspace = WorkspaceId::new();
    let mut bytes = vec![0xFF; CHUNK];
    bytes.extend(std::iter::repeat_n(0x82, CHUNK));
    bytes.extend([0, 1, 0xFF, 0xFE]);
    let object = object_ref(ObjectKind::File, &bytes);
    let backend = OpenCsgBackend::file_store(server.config.clone(), db.clone()).unwrap();
    {
        let mut service = server.data.lock().unwrap();
        service.fail_batch_before_commit = true;
        service.lose_pointer_response = true;
    }
    backend
        .upload(workspace, stream(bytes.clone()))
        .await
        .unwrap();
    let placement: serde_json::Value = serde_json::from_slice(
        &db.file_placement(workspace, &object.id)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(placement["format"], "git_chunks");
    let revision = placement["revision"].as_str().unwrap();
    {
        let service = server.data.lock().unwrap();
        assert_eq!(service.commit_sizes, [2, 2, 1]);
        let snapshot = &service.binary_snapshots[revision];
        for (index, digest) in placement["chunks"].as_array().unwrap().iter().enumerate() {
            let path = format!(
                "agentdrive/v1/{workspace}/chunks/{}",
                digest.as_str().unwrap()
            );
            assert_eq!(
                &snapshot[&path],
                &bytes[index * CHUNK..((index + 1) * CHUNK).min(bytes.len())]
            );
        }
    }
    let new_policy = OpenCsgBackend::file_store(
        agentdrive_config::OpenCsgConfig {
            xnet: true,
            ..server.config.clone()
        },
        db,
    )
    .unwrap();
    new_policy
        .upload(workspace, stream(bytes.clone()))
        .await
        .unwrap();
    {
        let mut service = server.data.lock().unwrap();
        assert!(service.xnet_paths.is_empty());
        assert_eq!(service.commit_sizes, [2, 2, 1]);
        service.resolved_paths.clear();
    }
    let range = new_policy
        .read_verified_range(workspace, &object, (CHUNK + 1) as u64, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(range.as_ref(), &bytes[CHUNK + 1..CHUNK + 4]);
    assert_eq!(server.data.lock().unwrap().resolved_paths.len(), 1);
    let range = new_policy
        .read_verified_range(workspace, &object, (CHUNK - 1) as u64, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(range.as_ref(), &bytes[CHUNK - 1..CHUNK + 1]);
    let digest = placement["chunks"][1].as_str().unwrap();
    let path = format!("agentdrive/v1/{workspace}/chunks/{digest}");
    server
        .data
        .lock()
        .unwrap()
        .binary_snapshots
        .get_mut(revision)
        .unwrap()
        .get_mut(&path)
        .unwrap()[0] = 0;
    let error = new_policy
        .read_verified_range(workspace, &object, (CHUNK + 1) as u64, 3)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Integrity);
}

#[cfg(unix)]
#[tokio::test]
async fn sql_lfs_placement_survives_a_change_to_regular_git_writes() {
    use agentdrive_local::LocalBackend;
    use agentdrive_opencsg::OpenCsgBackend;
    use axum::{Router, routing::get};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    // Start an isolated test process so its Python transfer helper cannot affect other tests.
    let Ok(test_file) = std::env::var("AGENTDRIVE_TEST_STORED_CAS_FILE") else {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("xnet-helper");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, shutil, sys
request = json.load(sys.stdin)
if request["action"] == "upload":
    shutil.copyfile(request["path"], os.environ["AGENTDRIVE_TEST_STORED_CAS_FILE"])
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sql_lfs_placement_survives_a_change_to_regular_git_writes",
            ])
            .env(
                "AGENTDRIVE_TEST_STORED_CAS_FILE",
                dir.path().join("cas-file"),
            )
            .env("AGENTDRIVE_XNET_PYTHON", script)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(LocalBackend::open_server(dir.path()).unwrap());
    let server = Server::new().await;
    let workspace = WorkspaceId::new();
    let bytes = b"stored using a mocked XNet CAS\x00\xff";
    let object = object_ref(ObjectKind::File, bytes);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/object", listener.local_addr().unwrap());
    let service = tokio::spawn(async move {
        let app = Router::new().route(
            "/object",
            get(move || async move { std::fs::read(test_file).unwrap() }),
        );
        axum::serve(listener, app).await.unwrap()
    });
    {
        let mut data = server.data.lock().unwrap();
        data.xnet_upload_mode = Some("lfs".into());
        data.xnet_download = Some(url);
        data.lose_pointer_response = true;
    }
    let writer = OpenCsgBackend::file_store(
        agentdrive_config::OpenCsgConfig {
            xnet: true,
            ..server.config.clone()
        },
        db.clone(),
    )
    .unwrap();
    writer
        .upload(workspace, stream(bytes.to_vec()))
        .await
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(
        &db.file_placement(workspace, &object.id)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["format"], "git_lfs");
    assert_eq!(record["oid"], format!("{:x}", sha2::Sha256::digest(bytes)));
    assert_eq!(
        record["path"],
        format!("agentdrive/v1/{workspace}/files/{}", object.id)
    );
    assert_eq!(server.data.lock().unwrap().xnet_paths.len(), 1);
    let reader = OpenCsgBackend::file_store(server.config.clone(), db).unwrap();
    assert!(
        reader
            .read_verified_range(workspace, &object, 0, 3)
            .await
            .unwrap()
            .is_none()
    );
    let mut actual = Vec::new();
    reader
        .download(workspace, &object)
        .await
        .unwrap()
        .reader
        .read_to_end(&mut actual)
        .await
        .unwrap();
    assert_eq!(actual, bytes);
    reader
        .upload(workspace, stream(bytes.to_vec()))
        .await
        .unwrap();
    assert_eq!(server.data.lock().unwrap().commit_sizes, [1]);
    service.abort();
}

#[tokio::test]
async fn git_chunks_preserve_literal_lfs_pointers_and_binary_prefixes() {
    use agentdrive_local::LocalBackend;
    use agentdrive_opencsg::OpenCsgBackend;
    use std::sync::Arc;
    let server = Server::new().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(LocalBackend::open_server(dir.path()).unwrap());
    let workspace = WorkspaceId::new();
    let backend = OpenCsgBackend::file_store(server.config.clone(), db).unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 8\n",
        "a".repeat(64)
    )
    .into_bytes();
    for input in [pointer.clone(), [&[0][..], &pointer].concat()] {
        let reference = object_ref(ObjectKind::File, &input);
        server.data.lock().unwrap().lose_pointer_response = true;
        backend
            .upload(workspace, stream(input.clone()))
            .await
            .unwrap();
        let mut data = Vec::new();
        backend
            .download(workspace, &reference)
            .await
            .unwrap()
            .reader
            .read_to_end(&mut data)
            .await
            .unwrap();
        assert_eq!(data, input);
        let range = backend
            .read_verified_range(workspace, &reference, 1, 12)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(range.as_ref(), &input[1..13]);
    }
}
