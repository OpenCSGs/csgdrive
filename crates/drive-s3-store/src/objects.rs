use super::*;
use agentdrive_engine::BranchFileReader;
use agentdrive_engine::BranchObjectChange;
use agentdrive_model::{FileKind, Inode, ObjectKind, Permission, WorkspacePath, object_ref};
use bytes::{Bytes, BytesMut};
use serde::Serialize;
use std::ops::Range;

#[derive(Clone, Debug, Serialize)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// Opaque entity tag without HTTP quoting.
    pub etag: String,
    pub modified_ms: i64,
}

fn path_and_kind(key: &str) -> Result<(WorkspacePath, FileKind)> {
    let (name, kind) = match key.strip_suffix('/') {
        Some(s) => (s, FileKind::Directory),
        None => (key, FileKind::File),
    };
    if name.is_empty() || name.starts_with('/') {
        return Err(Error::new(
            ErrorKind::Invalid,
            "expected a relative nonempty object key",
        ));
    }
    Ok((WorkspacePath::try_from(format!("/{name}"))?, kind))
}

fn metadata(path: &WorkspacePath, inode: &Inode, dirty: bool, version: &str) -> ObjectMeta {
    let mut key = path.as_str()[1..].to_owned();
    let etag = if inode.kind == FileKind::Directory {
        key.push('/');
        object_ref(ObjectKind::File, &[]).id.to_string()
    } else if !dirty && let Some(ref content) = inode.content {
        content.id.to_string()
    } else {
        // Native writes are versioned before they are sealed into the object store.
        object_ref(
            ObjectKind::File,
            format!("{version}:{}", inode.id.0).as_bytes(),
        )
        .id
        .to_string()
    };
    ObjectMeta {
        key,
        size: inode.size,
        etag,
        modified_ms: inode.modified_ns.div_euclid(1_000_000),
    }
}

#[derive(Debug)]
pub struct ObjectReader {
    pub meta: ObjectMeta,
    source: BranchFileReader,
}

impl ObjectReader {
    pub async fn chunk(&self, offset: u64, remaining: u64) -> Result<Bytes> {
        let size = remaining.min(256 * 1024) as u32;
        let bytes = self.source.read(offset, size).await?;
        if bytes.len() != size as usize {
            return Err(Error::new(ErrorKind::Internal, "unexpected object length"));
        }
        Ok(bytes)
    }

    pub async fn bytes(&self, range: Range<u64>) -> Result<Bytes> {
        if range.start > range.end || range.end > self.meta.size {
            return Err(Error::new(ErrorKind::Invalid, "invalid object range"));
        }
        let mut buf = BytesMut::new();
        let mut offset = range.start;
        while offset < range.end {
            let bytes = self.chunk(offset, range.end - offset).await?;
            offset += bytes.len() as u64;
            buf.extend_from_slice(&bytes);
        }
        Ok(buf.freeze())
    }
}

impl Store {
    pub async fn reader(&self, bucket: &str, key: &str) -> Result<ObjectReader> {
        let (path, kind) = path_and_kind(key)?;
        let session = self.leases.status(bucket).await?;
        let source = match session.revision {
            Some(revision) => {
                self.engine
                    .open_revision_file(
                        &self.principal,
                        session.workspace_id,
                        session.branch_id,
                        revision,
                        &path,
                    )
                    .await?
            }
            None => {
                self.engine
                    .open_branch_file(
                        &self.principal,
                        session.workspace_id,
                        session.branch_id,
                        &path,
                    )
                    .await?
            }
        };
        let node = &source.node;
        if node.path != path || node.inode.kind != kind {
            return Err(Error::new(ErrorKind::NoSuchKey, "object does not exist"));
        }
        let meta = metadata(&path, &node.inode, node.dirty, &source.version);
        Ok(ObjectReader { meta, source })
    }

    pub async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta> {
        Ok(self.reader(bucket, key).await?.meta)
    }

    pub async fn get(&self, bucket: &str, key: &str) -> Result<Bytes> {
        let reader = self.reader(bucket, key).await?;
        reader.bytes(0..reader.meta.size).await
    }

    pub async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let s = self.leases.status(bucket).await?;
        if let Some(revision) = s.revision {
            let version = revision.to_string();
            let tree = self
                .engine
                .revision_tree(&self.principal, s.workspace_id, s.branch_id, revision)
                .await?;
            let mut entries: Vec<_> = tree
                .into_iter()
                .filter(|(path, _)| !path.is_root())
                .map(|(path, inode)| metadata(&path, &inode, false, &version))
                .filter(|meta| meta.key.starts_with(prefix))
                .collect();
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            return Ok(entries);
        }
        let runtime = &self.engine.runtime;
        let _gate = runtime.lock_branch(s.branch_id).await;
        let (_, b) = self
            .authorized_branch(s.workspace_id, s.branch_id, Permission::Read)
            .await?;
        let version = format!(
            "{}:{}:{}:{}",
            b.id, b.authority_epoch, b.generation, b.mutation_seq
        );
        let mut entries: Vec<_> = runtime
            .state
            .nodes(b.id)
            .await?
            .into_iter()
            .filter(|n| !n.path.is_root())
            .map(|n| metadata(&n.path, &n.inode, n.dirty, &version))
            .filter(|n| n.key.starts_with(prefix))
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    pub async fn put(&self, bucket: &str, key: &str, data: Bytes) -> Result<ObjectMeta> {
        let (path, kind) = path_and_kind(key)?;
        let change = if kind == FileKind::File {
            BranchObjectChange::Put(data)
        } else {
            if !data.is_empty() {
                return Err(Error::new(
                    ErrorKind::Invalid,
                    "a directory marker must be empty",
                ));
            }
            BranchObjectChange::Directory
        };
        self.publish(bucket, path, change)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Internal, "AgentDrive did not publish the object"))
    }

    pub async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.delete_if(bucket, key, ObjectConditions::default())
            .await
    }

    pub async fn delete_if(
        &self,
        bucket: &str,
        key: &str,
        conditions: ObjectConditions,
    ) -> Result<()> {
        let (path, kind) = path_and_kind(key)?;
        self.publish(bucket, path, BranchObjectChange::Delete(kind, conditions))
            .await?;
        Ok(())
    }

    async fn publish(
        &self,
        bucket: &str,
        path: WorkspacePath,
        change: BranchObjectChange,
    ) -> Result<Option<ObjectMeta>> {
        let store = self.clone();
        let bucket = bucket.to_owned();
        // A disconnected HTTP client cannot drop the guard while AgentDrive's SQLite commit is running.
        tokio::spawn(async move {
            let guard = store.leases.guard(&bucket).await?;
            let session = &guard.binding;
            let inode = store
                .engine
                .update_branch_object(
                    &store.principal,
                    session.workspace_id,
                    session.branch_id,
                    path.clone(),
                    change,
                )
                .await?;
            drop(guard);
            Ok(inode.map(|n| metadata(&path, &n, false, "")))
        })
        .await
        .map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?
    }
}
