use crate::{OpenCsgBackend, http::validate_commit, xnet::Xnet};
use agentdrive_model::*;
use agentdrive_ports::ObjectStream;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

// Two 4 MiB chunks fit in an 11 MiB JSON request, including Base64 and paths.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const BATCH_BYTES: usize = 2 * CHUNK_BYTES;
const LFS_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Codes interprets this prefix when committing blobs. One extra byte keeps literal pointers readable.
fn git_blob(bytes: &Bytes) -> Bytes {
    if !bytes.starts_with(LFS_HEADER) {
        return bytes.clone();
    }
    let mut blob = Vec::with_capacity(bytes.len() + 1);
    blob.push(0);
    blob.extend_from_slice(bytes);
    blob.into()
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FilePlacement {
    object: ObjectRef,
    revision: String,
    #[serde(flatten)]
    location: Location,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
enum Location {
    GitChunks { chunks: Vec<String> },
    GitLfs { path: String, oid: String },
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_hexdigit() && !v.is_ascii_uppercase())
}

impl OpenCsgBackend {
    fn object_path(&self, workspace: WorkspaceId, object: &ObjectRef) -> String {
        format!("{}/{workspace}/objects/{}.json", self.prefix, object.id)
    }

    pub(crate) async fn file_placement(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<Option<FilePlacement>> {
        let data = match &self.placements {
            Some(db) => db.file_placement(workspace, &object.id).await?,
            None => {
                let revision = self.repository.head().await?;
                let path = self.object_path(workspace, object);
                self.repository.read(&path, &revision).await?
            }
        };
        let Some(data) = data else {
            return Ok(None);
        };
        if data.len() > MAX_METADATA_BYTES {
            return Err(Error::integrity(
                "Codes file placement exceeds the size limit",
            ));
        }
        let placement: FilePlacement = decode(&data)?;
        validate_commit(&placement.revision)?;
        let valid = match &placement.location {
            Location::GitChunks { chunks } => {
                chunks.len() as u64 == object.size.div_ceil(CHUNK_BYTES as u64)
                    && chunks.iter().all(|id| valid_sha256(id))
            }
            Location::GitLfs { path, oid } => {
                path == &self.file_object_path(workspace, object) && valid_sha256(oid)
            }
        };
        if !valid || placement.object != *object {
            return Err(Error::integrity(
                "Codes placement differs from its file reference",
            ));
        }
        Ok(Some(placement))
    }

    pub(crate) async fn upload_file_object(
        &self,
        workspace: WorkspaceId,
        mut source: ObjectStream,
    ) -> Result<()> {
        let object = source.reference.clone();
        if self.placements.is_some() && object.kind != ObjectKind::File {
            return Err(Error::invalid(
                "Codes file storage accepts only file contents",
            ));
        }
        if self.file_placement(workspace, &object).await?.is_some() {
            // Verify the caller's input even when this content already has a durable location.
            let mut digest = object_hasher(object.kind);
            let mut size = 0u64;
            let mut buffer = vec![0; IO_BUFFER_BYTES];
            loop {
                let count = source.reader.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                size = size
                    .checked_add(count as u64)
                    .filter(|v| *v <= object.size)
                    .ok_or_else(|| Error::integrity("file exceeds its reference length"))?;
                digest.update(&buffer[..count]);
            }
            if size != object.size || finish_digest(digest) != object.id {
                return Err(Error::integrity("file differs from its reference"));
            }
            return Ok(());
        }
        self.check_connection().await?;
        let path = self.file_object_path(workspace, &object);
        let placement = if self.xnet.enabled
            && object.kind == ObjectKind::File
            && object.size > 0
            && self.repository.uses_lfs(&path, object.size).await?
        {
            let (revision, oid) = self.upload_lfs(&path, source).await?;
            FilePlacement {
                object: object.clone(),
                revision,
                location: Location::GitLfs { path, oid },
            }
        } else {
            self.upload_git_chunks(workspace, source).await?
        };
        let bytes = encode(&placement)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid(
                "Codes file placement exceeds the size limit",
            ));
        }
        let result = match &self.placements {
            Some(db) => db.save_file_placement(workspace, &object.id, bytes).await,
            None => {
                self.create(&self.object_path(workspace, &object), &bytes)
                    .await
            }
        };
        match result {
            Ok(()) => Ok(()),
            // Two uploads may verify and publish the same bytes at different Git revisions.
            Err(error) if error.code == ErrorCode::Conflict => {
                if self.file_placement(workspace, &object).await?.is_some() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn upload_git_chunks(
        &self,
        workspace: WorkspaceId,
        mut source: ObjectStream,
    ) -> Result<FilePlacement> {
        let object = source.reference;
        if object.size.div_ceil(CHUNK_BYTES as u64) > (MAX_METADATA_BYTES / 70) as u64 {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "file exceeds the Git chunk index capacity",
            ));
        }
        let mut chunks = Vec::new();
        let mut seen = BTreeSet::new();
        let mut batch = BTreeMap::new();
        let mut batch_bytes = 0;
        let mut remaining = object.size;
        let mut digest = object_hasher(object.kind);
        while remaining > 0 {
            let size = remaining.min(CHUNK_BYTES as u64) as usize;
            let mut bytes = vec![0; size];
            source.reader.read_exact(&mut bytes).await?;
            digest.update(&bytes);
            let id = format!("{:x}", Sha256::digest(&bytes));
            if seen.insert(id.clone()) {
                batch.insert(id.clone(), Bytes::from(bytes));
                batch_bytes += size;
            }
            chunks.push(id);
            remaining -= size as u64;
            if batch_bytes >= BATCH_BYTES && remaining > 0 {
                self.commit_chunk_batch(workspace, &batch).await?;
                batch.clear();
                batch_bytes = 0;
            }
        }
        if finish_digest(digest) != object.id || source.reader.read(&mut [0]).await? != 0 {
            return Err(Error::integrity(
                "Git upload differs from its file reference",
            ));
        }
        if !batch.is_empty() {
            self.commit_chunk_batch(workspace, &batch).await?;
        }
        let revision = self.repository.head().await?;
        Ok(FilePlacement {
            object,
            revision,
            location: Location::GitChunks { chunks },
        })
    }

    async fn commit_chunk_batch(
        &self,
        workspace: WorkspaceId,
        batch: &BTreeMap<String, Bytes>,
    ) -> Result<()> {
        struct Chunk<'a> {
            path: String,
            digest: &'a str,
            size: usize,
            blob: Bytes,
        }
        let files: Vec<_> = batch
            .iter()
            .map(|(digest, bytes)| Chunk {
                path: format!("{}/{workspace}/chunks/{digest}", self.prefix),
                digest,
                size: bytes.len(),
                blob: git_blob(bytes),
            })
            .collect();
        let _write = self.writes.lock().await;
        for attempt in 0..3 {
            match self
                .repository
                .commit_bytes(
                    files
                        .iter()
                        .map(|chunk| (chunk.path.as_str(), chunk.blob.as_ref())),
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) if error.retryable || error.code == ErrorCode::AlreadyExists => {
                    let mut complete = true;
                    let head = self.repository.head().await?;
                    for chunk in &files {
                        match chunk_bytes(&self.xnet, &head, &chunk.path, chunk.size, chunk.digest)
                            .await
                        {
                            Ok(_) => (),
                            Err(e) if e.code == ErrorCode::NotFound => {
                                complete = false;
                                break;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    if complete {
                        return Ok(());
                    }
                    if attempt == 2 {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(20 * (attempt + 1))).await;
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "Git batch outcome could not be confirmed",
        )
        .retryable())
    }

    pub(crate) async fn open_file_object(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<ObjectStream> {
        let placement = self
            .file_placement(workspace, object)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "Codes file has no stored location"))?;
        match placement.location {
            Location::GitLfs { path, oid } => {
                self.download_lfs(&path, object, &placement.revision, &oid)
                    .await
            }
            Location::GitChunks { chunks } => {
                if chunks.is_empty() {
                    verify_object(object, &[])?;
                    return Ok(ObjectStream {
                        reference: object.clone(),
                        reader: Box::pin(Cursor::new(Vec::new())),
                    });
                }
                let input = Arc::new(ChunkInput {
                    chunks,
                    revision: placement.revision,
                    prefix: format!("{}/{workspace}/chunks", self.prefix),
                    client: self.xnet.clone(),
                    object: object.clone(),
                });
                let stream = futures::stream::try_unfold(
                    (0, object_hasher(object.kind)),
                    move |(index, mut digest)| {
                        let input = input.clone();
                        async move {
                            if index == input.chunks.len() {
                                return Ok(None);
                            }
                            let bytes = input.read(index).await.map_err(std::io::Error::other)?;
                            digest.update(&bytes);
                            if index + 1 == input.chunks.len()
                                && finish_digest(digest.clone()) != input.object.id
                            {
                                return Err(std::io::Error::other(Error::integrity(
                                    "Codes chunks differ from their file reference",
                                )));
                            }
                            Ok(Some((bytes, (index + 1, digest))))
                        }
                    },
                );
                Ok(ObjectStream {
                    reference: object.clone(),
                    reader: Box::pin(StreamReader::new(Box::pin(stream))),
                })
            }
        }
    }

    pub(crate) async fn git_range(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Option<Bytes>> {
        // Full reads populate the shared cache and remain available when Codes is offline.
        if offset == 0 && u64::from(size) >= object.size {
            return Ok(None);
        }
        let placement = self
            .file_placement(workspace, object)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "Codes file has no stored location"))?;
        let Location::GitChunks { chunks } = placement.location else {
            return Ok(None);
        };
        let start = offset.min(object.size);
        let end = offset.saturating_add(u64::from(size)).min(object.size);
        let mut bytes = Vec::with_capacity((end - start) as usize);
        let input = ChunkInput {
            chunks,
            revision: placement.revision,
            prefix: format!("{}/{workspace}/chunks", self.prefix),
            client: self.xnet.clone(),
            object: object.clone(),
        };
        if start < end {
            for index in (start / CHUNK_BYTES as u64)..=((end - 1) / CHUNK_BYTES as u64) {
                let chunk = input.read(index as usize).await?;
                let base = index * CHUNK_BYTES as u64;
                let from = start.saturating_sub(base) as usize;
                let to = (end - base).min(chunk.len() as u64) as usize;
                bytes.extend_from_slice(&chunk[from..to]);
            }
        }
        Ok(Some(Bytes::from(bytes)))
    }
}

struct ChunkInput {
    chunks: Vec<String>,
    revision: String,
    prefix: String,
    client: Xnet,
    object: ObjectRef,
}

impl ChunkInput {
    async fn read(&self, index: usize) -> Result<Bytes> {
        let id = &self.chunks[index];
        let expected =
            (self.object.size - index as u64 * CHUNK_BYTES as u64).min(CHUNK_BYTES as u64) as usize;
        chunk_bytes(
            &self.client,
            &self.revision,
            &format!("{}/{id}", self.prefix),
            expected,
            id,
        )
        .await
    }
}

async fn chunk_bytes(
    client: &Xnet,
    revision: &str,
    path: &str,
    expected: usize,
    id: &str,
) -> Result<Bytes> {
    let mut response = client.resolve(revision, path).await?;
    if response
        .content_length()
        .is_some_and(|v| v < expected as u64 || v > (expected + 1) as u64)
    {
        return Err(Error::integrity("Git chunk length mismatch"));
    }
    let mut data = Vec::with_capacity(expected);
    while let Some(bytes) = response
        .chunk()
        .await
        .map_err(|_| Error::new(ErrorCode::Unavailable, "Git chunk transfer failed").retryable())?
    {
        if data.len().saturating_add(bytes.len()) > expected + 1 {
            return Err(Error::integrity("Git chunk exceeds its recorded length"));
        }
        data.extend_from_slice(&bytes);
    }
    let mut data = Bytes::from(data);
    if data.len() == expected + 1 && data[0] == 0 && data[1..].starts_with(LFS_HEADER) {
        data = data.slice(1..);
    }
    if data.len() != expected || format!("{:x}", Sha256::digest(&data)) != id {
        return Err(Error::integrity("Git chunk failed digest verification"));
    }
    Ok(data)
}
