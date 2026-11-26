use crate::s3::{MAX_OBJECT, error};
use bytes::{Bytes, BytesMut};
use drive_s3_store::{ObjectMeta, Store};
use md5::{Digest, Md5};
use s3s::{
    S3Result,
    dto::{CompletedPart, ETag, MultipartUpload},
    s3_error,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    time::Instant,
};
use uuid::Uuid;

const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
type Entries = Arc<Mutex<HashMap<String, Arc<Mutex<Upload>>>>>;

pub struct Uploads {
    entries: Entries,
    cleanup: tokio::task::JoinHandle<()>,
    ttl: Duration,
}

struct Upload {
    bucket: String,
    key: String,
    parts: BTreeMap<i32, (ETag, Bytes)>,
    active: Instant,
    finished: bool,
}

impl Drop for Uploads {
    fn drop(&mut self) {
        self.cleanup.abort();
    }
}

fn prune(entries: &mut HashMap<String, Arc<Mutex<Upload>>>, ttl: Duration) {
    entries.retain(|_, entry| {
        entry.try_lock().map_or(true, |upload| {
            !upload.finished && upload.active.elapsed() < ttl
        })
    });
}

impl Uploads {
    pub fn new() -> Self {
        Self::with_timeout(IDLE_TIMEOUT, Duration::from_secs(60))
    }

    fn with_timeout(ttl: Duration, interval: Duration) -> Self {
        let entries: Entries = Arc::default();
        let registry = entries.clone();
        let cleanup = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                prune(&mut *registry.lock().await, ttl);
            }
        });
        Self {
            entries,
            cleanup,
            ttl,
        }
    }

    async fn upload(&self, id: &str, bucket: &str, key: &str) -> S3Result<OwnedMutexGuard<Upload>> {
        let entry = self
            .entries
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| s3_error!(NoSuchUpload))?;
        let upload = entry.lock_owned().await;
        if upload.bucket != bucket
            || upload.key != key
            || upload.finished
            || upload.active.elapsed() >= self.ttl
        {
            return Err(s3_error!(NoSuchUpload));
        }
        Ok(upload)
    }

    pub async fn create(&self, bucket: &str, key: &str) -> S3Result<String> {
        let mut entries = self.entries.lock().await;
        prune(&mut entries, self.ttl);
        if entries.len() >= 8 {
            return Err(s3_error!(SlowDown, "too many unfinished uploads"));
        }
        let id = Uuid::new_v4().to_string();
        entries.insert(
            id.clone(),
            Arc::new(Mutex::new(Upload {
                bucket: bucket.into(),
                key: key.into(),
                parts: BTreeMap::new(),
                active: Instant::now(),
                finished: false,
            })),
        );
        Ok(id)
    }

    pub async fn part(
        &self,
        id: &str,
        bucket: &str,
        key: &str,
        number: i32,
        bytes: Bytes,
    ) -> S3Result<ETag> {
        if !(1..=10_000).contains(&number) {
            return Err(s3_error!(InvalidPart));
        }
        let mut upload = self.upload(id, bucket, key).await?;
        let others: usize = upload
            .parts
            .iter()
            .filter(|(n, _)| **n != number)
            .map(|(_, (_, b))| b.len())
            .sum();
        if bytes.len() > MAX_OBJECT - others {
            return Err(s3_error!(EntityTooLarge));
        }
        let tag = ETag::Strong(hex::encode(Md5::digest(&bytes)));
        upload.parts.insert(number, (tag.clone(), bytes));
        upload.active = Instant::now();
        Ok(tag)
    }
    pub async fn complete(
        &self,
        store: &Store,
        id: &str,
        bucket: &str,
        key: &str,
        parts: Vec<CompletedPart>,
    ) -> S3Result<ObjectMeta> {
        let mut upload = self.upload(id, bucket, key).await?;
        if parts.is_empty() {
            return Err(s3_error!(InvalidPart));
        }
        let mut data = BytesMut::new();
        let mut previous = 0;
        let count = parts.len();
        for (index, part) in parts.into_iter().enumerate() {
            let number = part.part_number.ok_or_else(|| s3_error!(InvalidPart))?;
            if number <= previous {
                return Err(s3_error!(InvalidPartOrder));
            }
            let (tag, bytes) = upload
                .parts
                .get(&number)
                .ok_or_else(|| s3_error!(InvalidPart))?;
            if part.e_tag.as_ref() != Some(tag) {
                return Err(s3_error!(InvalidPart));
            }
            if index + 1 < count && bytes.len() < 5 * 1024 * 1024 {
                return Err(s3_error!(EntityTooSmall));
            }
            previous = number;
            data.extend_from_slice(bytes);
        }
        let store = store.clone();
        let entries = self.entries.clone();
        let id = id.to_owned();
        // Finish processing if the HTTP client disconnects after publication starts.
        tokio::spawn(async move {
            let result = store
                .put(&upload.bucket, &upload.key, data.freeze())
                .await
                .map_err(error);
            upload.active = Instant::now();
            if result.is_ok() {
                upload.finished = true;
                entries.lock().await.remove(&id);
            }
            result
        })
        .await
        .map_err(|e| s3_error!(InternalError, "{e}"))?
    }

    pub async fn abort(&self, id: &str, bucket: &str, key: &str) -> S3Result<()> {
        let mut upload = self.upload(id, bucket, key).await?;
        upload.finished = true;
        self.entries.lock().await.remove(id);
        Ok(())
    }

    pub async fn list(&self, bucket: &str, prefix: &str) -> Vec<MultipartUpload> {
        let candidates = self.entries.lock().await.clone();
        let mut result = Vec::new();
        for (id, entry) in candidates {
            let upload = entry.lock().await;
            if !upload.finished
                && upload.active.elapsed() < self.ttl
                && upload.bucket == bucket
                && upload.key.starts_with(prefix)
            {
                result.push(MultipartUpload {
                    key: Some(upload.key.clone()),
                    upload_id: Some(id),
                    ..Default::default()
                });
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn abandoned_parts_expire_and_recent_progress_preserves_the_upload() {
        let uploads = Uploads::with_timeout(Duration::from_secs(60), Duration::from_secs(10));
        let mut ids = Vec::new();
        for _ in 0..8 {
            ids.push(uploads.create("a", "key").await.unwrap());
        }
        assert!(uploads.create("b", "key").await.is_err());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(45)).await;
        uploads
            .part(&ids[0], "a", "key", 1, Bytes::from_static(b"ongoing"))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(uploads.entries.lock().await.len(), 1);
        assert_eq!(
            uploads.list("a", "").await[0].upload_id.as_ref(),
            Some(&ids[0])
        );
        assert!(
            uploads
                .part(&ids[1], "a", "key", 1, Bytes::new())
                .await
                .is_err()
        );
        uploads.create("b", "key").await.unwrap();
    }
}
