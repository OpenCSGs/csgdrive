//! Store file chunks through Scion's repository-scoped Git LFS interface.
use agentdrive_config::{RemoteConfig, ScionConfig};
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::stream;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{ops::Range, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

// Persisted format: SQLite holds consecutive raw SHA-256 chunk IDs in file order.
const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const LFS_JSON: &str = "application/vnd.git-lfs+json";

#[derive(Clone)]
pub struct ScionBackend {
    client: Client,
    config: ScionConfig,
    target: String,
    objects_url: String,
    records: Arc<dyn ObjectVerificationStore>,
}

impl std::fmt::Debug for ScionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScionBackend")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Batch {
    objects: Vec<BatchObject>,
}

#[derive(Deserialize)]
struct BatchObject {
    oid: String,
    size: u64,
    error: Option<BatchError>,
    #[serde(default)]
    actions: Actions,
}

#[derive(Default, Deserialize)]
struct Actions {
    upload: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct BatchError {
    code: u16,
}

impl ScionBackend {
    pub fn open(
        config: ScionConfig,
        records: Arc<dyn ObjectVerificationStore>,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let objects_url = format!(
            "{}/{}.git/info/lfs/objects",
            config.endpoint.trim_end_matches('/'),
            config.repository
        );
        let target = RemoteConfig::Scion(config.clone()).storage_target()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(transport)?;
        Ok(Arc::new(Self {
            client,
            config,
            records,
            target,
            objects_url,
        }))
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let request = if self.config.token.is_empty() {
            request
        } else {
            request.bearer_auth(&self.config.token)
        };
        let response = request.send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(http_error(response.status()));
        }
        Ok(response)
    }

    async fn batch(&self, operation: &str, objects: &[(String, u64)]) -> Result<Vec<BatchObject>> {
        let payload: Vec<_> = objects
            .iter()
            .map(|(oid, size)| serde_json::json!({"oid": oid, "size": size}))
            .collect();
        let request = self.client.post(format!("{}/batch", self.objects_url))
            .header("Accept", LFS_JSON).header("Content-Type", LFS_JSON)
            .json(&serde_json::json!({"operation": operation, "transfers":["basic"], "objects": payload}));
        let result: Batch = self.send(request).await?.json().await.map_err(transport)?;
        if result.objects.len() != objects.len() {
            return Err(Error::integrity("Scion returned an incomplete LFS batch"));
        }
        for (entry, (oid, size)) in result.objects.iter().zip(objects) {
            if &entry.oid != oid || entry.size != *size {
                return Err(Error::integrity(
                    "Scion returned another chunk in the LFS batch",
                ));
            }
            if let Some(error) = &entry.error {
                if operation == "download" && error.code == 404 {
                    continue;
                }
                return Err(http_error(
                    StatusCode::from_u16(error.code).unwrap_or(StatusCode::BAD_GATEWAY),
                ));
            }
        }
        Ok(result.objects)
    }

    async fn manifest(
        &self,
        workspace: WorkspaceId,
        file: &ObjectRef,
        chunks: Range<u64>,
    ) -> Result<Option<Bytes>> {
        let expected = (chunks.end - chunks.start) * 32;
        let Some(proof) = self
            .records
            .verification(
                &self.target,
                workspace,
                &file.id,
                chunks.start * 32..chunks.end * 32,
            )
            .await?
        else {
            return Ok(None);
        };
        if file.kind != ObjectKind::File
            || proof.size != file.size
            || proof.hashes.len() as u64 != expected
        {
            return Err(Error::integrity("invalid Scion file chunk list"));
        }
        Ok(Some(proof.hashes))
    }

    async fn fetch(&self, hash: &[u8], size: u64) -> Result<Bytes> {
        let mut response = self
            .send(
                self.client
                    .get(format!("{}/{}", self.objects_url, hex::encode(hash))),
            )
            .await?;
        let mut bytes = BytesMut::with_capacity(size as usize);
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if (bytes.len() + chunk.len()) as u64 > size {
                return Err(Error::integrity("Scion chunk is larger than recorded"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != size || Sha256::digest(&bytes)[..] != *hash {
            return Err(Error::integrity("Scion chunk size or digest differs"));
        }
        Ok(bytes.freeze())
    }
}

fn transport(error: reqwest::Error) -> Error {
    Error::new(
        ErrorCode::Unavailable,
        format!("Scion request: {}", error.without_url()),
    )
    .retryable()
}

fn http_error(status: StatusCode) -> Error {
    let code = match status.as_u16() {
        401 | 403 => ErrorCode::PermissionDenied,
        404 => ErrorCode::NotFound,
        413 => ErrorCode::CapacityExceeded,
        _ => ErrorCode::Unavailable,
    };
    let error = Error::new(code, format!("Scion returned HTTP {status}"));
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        error.retryable()
    } else {
        error
    }
}

fn chunk_size(total: u64, index: u64) -> u64 {
    (total - index * CHUNK_SIZE).min(CHUNK_SIZE)
}

#[async_trait]
impl RemoteObjectStore for ScionBackend {
    async fn check_file_storage(&self) -> Result<()> {
        self.batch("download", &[]).await?;
        self.batch("upload", &[]).await?;
        Ok(())
    }

    async fn upload(&self, workspace: WorkspaceId, mut stream: ObjectStream) -> Result<()> {
        let file = stream.reference;
        if file.kind != ObjectKind::File {
            return Err(Error::invalid("Scion expects a file"));
        }
        let mut hashes = BytesMut::new();
        let mut digest = object_hasher(file.kind);
        for index in 0..file.size.div_ceil(CHUNK_SIZE) {
            let mut bytes = vec![0; chunk_size(file.size, index) as usize];
            stream.reader.read_exact(&mut bytes).await?;
            digest.update(&bytes);
            let hash = Sha256::digest(&bytes);
            let oid = hex::encode(hash);
            let batch = self
                .batch("upload", &[(oid.clone(), bytes.len() as u64)])
                .await?;
            if batch[0].actions.upload.is_some() {
                let request = self
                    .client
                    .put(format!("{}/{oid}", self.objects_url))
                    .header("Content-Type", "application/octet-stream")
                    .body(bytes);
                self.send(request).await?;
            }
            hashes.extend_from_slice(&hash);
        }
        if stream.reader.read(&mut [0]).await? != 0 || finish_digest(digest) != file.id {
            return Err(Error::integrity(
                "uploaded bytes differ from the file reference",
            ));
        }
        self.records
            .save_verification(
                &self.target,
                workspace,
                &file.id,
                ObjectVerification {
                    size: file.size,
                    hashes: hashes.freeze(),
                    version: None,
                    etag: None,
                },
            )
            .await
    }

    async fn contains(&self, workspace: WorkspaceId, file: &ObjectRef) -> Result<bool> {
        let count = file.size.div_ceil(CHUNK_SIZE);
        let Some(hashes) = self.manifest(workspace, file, 0..count).await? else {
            return Ok(false);
        };
        for (group, hashes) in hashes.chunks(32 * 128).enumerate() {
            let objects: Vec<_> = hashes
                .as_chunks::<32>()
                .0
                .iter()
                .enumerate()
                .map(|(i, hash)| {
                    (
                        hex::encode(hash),
                        chunk_size(file.size, (group * 128 + i) as u64),
                    )
                })
                .collect();
            if self
                .batch("download", &objects)
                .await?
                .iter()
                .any(|entry| entry.error.is_some())
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn verified_file_exists(&self, workspace: WorkspaceId, file: &ObjectRef) -> Result<bool> {
        self.contains(workspace, file).await
    }

    async fn download(&self, workspace: WorkspaceId, file: &ObjectRef) -> Result<ObjectStream> {
        let count = file.size.div_ceil(CHUNK_SIZE);
        let hashes = self
            .manifest(workspace, file, 0..count)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "Scion file mapping is absent"))?;
        let size = file.size;
        let this = self.clone();
        let stream = stream::try_unfold((this, hashes, 0), move |(this, hashes, i)| async move {
            if i == count {
                return Ok(None);
            }
            let start = i as usize * 32;
            let bytes = this
                .fetch(&hashes[start..start + 32], chunk_size(size, i))
                .await
                .map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>(Some((bytes, (this, hashes, i + 1))))
        });
        Ok(ObjectStream {
            reference: file.clone(),
            reader: Box::pin(StreamReader::new(Box::pin(stream))),
        })
    }

    async fn read_verified_range(
        &self,
        workspace: WorkspaceId,
        file: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Option<Bytes>> {
        let end = offset.saturating_add(u64::from(size)).min(file.size);
        if offset >= end {
            return Ok(Some(Bytes::new()));
        }
        let first = offset / CHUNK_SIZE;
        let Some(hashes) = self
            .manifest(workspace, file, first..end.div_ceil(CHUNK_SIZE))
            .await?
        else {
            return Ok(None);
        };
        let mut bytes = BytesMut::with_capacity((end - offset) as usize);
        for (i, hash) in hashes.as_chunks::<32>().0.iter().enumerate() {
            let index = first + i as u64;
            let start = index * CHUNK_SIZE;
            let chunk = self.fetch(hash, chunk_size(file.size, index)).await?;
            bytes.extend_from_slice(
                &chunk[offset.saturating_sub(start) as usize
                    ..(end - start).min(chunk.len() as u64) as usize],
            );
        }
        Ok(Some(bytes.freeze()))
    }
}
