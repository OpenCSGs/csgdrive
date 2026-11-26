use super::*;
use std::{collections::VecDeque, ops::Range, sync::Mutex};

const BLOCK_BYTES: usize = 256 * 1024;
const DIGEST_BYTES: u64 = 32;
const MEMORY_PROOF_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct BlockHashes {
    hashes: Vec<u8>,
    digest: sha2::Sha256,
    current: usize,
}

impl BlockHashes {
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let count = (BLOCK_BYTES - self.current).min(data.len());
            self.digest.update(&data[..count]);
            self.current += count;
            if self.current == BLOCK_BYTES {
                self.hashes.extend(self.digest.finalize_reset());
                self.current = 0;
            }
            data = &data[count..];
        }
    }

    fn finish(mut self) -> Bytes {
        if self.current > 0 {
            self.hashes.extend(self.digest.finalize());
        }
        self.hashes.into()
    }
}

type Entry = ((WorkspaceId, ObjectId), ObjectVerification);

pub(super) struct Proofs {
    target: String,
    database: Option<Arc<dyn ObjectVerificationStore>>,
    // Standalone clients have no server database. Bound both the entry count and digest data.
    recent: Mutex<VecDeque<Entry>>,
}

fn strong_etag(value: &str) -> bool {
    value.len() > 2 && value.starts_with('"') && value.ends_with('"')
}

impl Proofs {
    pub fn new(target: String, database: Option<Arc<dyn ObjectVerificationStore>>) -> Self {
        Self {
            target,
            database,
            recent: Mutex::default(),
        }
    }

    async fn load(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        range: Range<u64>,
    ) -> Result<Option<ObjectVerification>> {
        if let Some(db) = &self.database {
            return db
                .verification(&self.target, workspace, &object.id, range)
                .await;
        }
        let entries = self.recent.lock().expect("proof cache mutex");
        let key = (workspace, object.id.clone());
        Ok(entries.iter().find(|(k, _)| *k == key).map(|(_, proof)| {
            let mut proof = proof.clone();
            let len = proof.hashes.len() as u64;
            proof.hashes = proof
                .hashes
                .slice(range.start.min(len) as usize..range.end.min(len) as usize);
            proof
        }))
    }

    pub async fn save(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        etag: Option<String>,
        version: Option<String>,
        blocks: BlockHashes,
    ) -> Result<()> {
        let proof = ObjectVerification {
            etag: etag.filter(|s| strong_etag(s)),
            version: version.filter(|v| !v.is_empty() && v != "null"),
            size: object.size,
            hashes: blocks.finish(),
        };
        if let Some(db) = &self.database {
            return db
                .save_verification(&self.target, workspace, &object.id, proof)
                .await;
        }
        let mut entries = self.recent.lock().expect("proof cache mutex");
        let key = (workspace, object.id.clone());
        entries.retain(|(k, _)| k != &key);
        let mut used =
            entries.iter().map(|(_, p)| p.hashes.len()).sum::<usize>() + proof.hashes.len();
        while entries.len() >= 256 || used > MEMORY_PROOF_BYTES {
            let Some((_, old)) = entries.pop_front() else {
                return Ok(());
            };
            used -= old.hashes.len();
        }
        entries.push_back((key, proof));
        Ok(())
    }

    pub async fn matches(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        head: &object_store::ObjectMeta,
    ) -> Result<bool> {
        let Some(proof) = self.load(workspace, object, 0..0).await? else {
            return Ok(false);
        };
        let etag = head.e_tag.as_deref().filter(|s| strong_etag(s));
        let version = head
            .version
            .as_deref()
            .filter(|v| !v.is_empty() && *v != "null");
        Ok(proof.size == object.size
            && (etag.is_some() || version.is_some())
            && proof.etag.as_deref() == etag
            && proof.version.as_deref() == version)
    }
}

impl S3Backend {
    pub(super) async fn verified_range(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Option<Bytes>> {
        if object.kind != ObjectKind::File {
            return Ok(None);
        }
        if size as usize > IO_BUFFER_BYTES * 16 {
            return Err(Error::invalid("read request exceeds the buffer limit"));
        }
        let length = object.size.saturating_sub(offset).min(u64::from(size));
        if length == 0 {
            return Ok(Some(Bytes::new()));
        }
        let end = offset + length;
        let first = offset / BLOCK_BYTES as u64;
        let last = end.div_ceil(BLOCK_BYTES as u64);
        let Some(proof) = self
            .proofs
            .load(workspace, object, first * DIGEST_BYTES..last * DIGEST_BYTES)
            .await?
        else {
            return Ok(None);
        };
        if proof.size != object.size || proof.hashes.len() as u64 != (last - first) * DIGEST_BYTES {
            return Err(Error::integrity("invalid stored range verification data"));
        }
        let start = first * BLOCK_BYTES as u64;
        let stop = last.saturating_mul(BLOCK_BYTES as u64).min(object.size);
        let range = format!("bytes={start}-{}", stop - 1);
        let path = self.object_key(workspace, &object.id);
        let mut opts = SignedUrlOptions::new().with_signed_header(
            HeaderName::from_static("range"),
            HeaderValue::from_str(&range).map_err(|_| Error::invalid("invalid range"))?,
        );
        if let Some(etag) = &proof.etag {
            opts = opts.with_signed_header(
                HeaderName::from_static("if-match"),
                HeaderValue::from_str(etag).map_err(|_| Error::integrity("invalid stored ETag"))?,
            );
        }
        let url = self
            .client
            .signed_url_opts(Method::GET, &path, Duration::from_secs(300), &opts)
            .await
            .map_err(store_error)?;
        let mut request = self.http.get(url).header("Range", range);
        if let Some(etag) = &proof.etag {
            request = request.header("If-Match", etag);
        }
        let response = request.send().await.map_err(http_error)?;
        match response.status().as_u16() {
            206 => (),
            200 | 400 | 405 | 412 | 501 => return Ok(None),
            404 => return Err(Error::new(ErrorCode::NotFound, "S3 object does not exist")),
            401 | 403 => {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "S3 range read denied",
                ));
            }
            status => {
                return Err(Error::new(
                    ErrorCode::Unavailable,
                    format!("S3 range read returned HTTP {status}"),
                )
                .retryable());
            }
        }
        let headers = response.headers();
        let received_etag = headers.get("etag").and_then(|v| v.to_str().ok());
        let received_version = headers
            .get("x-amz-version-id")
            .and_then(|v| v.to_str().ok());
        if proof
            .etag
            .as_deref()
            .is_some_and(|e| received_etag != Some(e))
            || proof
                .version
                .as_deref()
                .is_some_and(|v| received_version != Some(v))
        {
            return Ok(None);
        }
        let expected = format!("bytes {start}-{}/{}", stop - 1, object.size);
        if headers.get("content-range").and_then(|v| v.to_str().ok()) != Some(&expected) {
            return Err(Error::integrity("invalid S3 Content-Range"));
        }
        let mut stream = response.bytes_stream();
        let mut data = bytes::BytesMut::with_capacity((stop - start) as usize);
        while let Some(block) = stream.try_next().await.map_err(http_error)? {
            if block.len() > (stop - start) as usize - data.len() {
                return Err(Error::integrity("S3 range exceeded its declared length"));
            }
            data.extend_from_slice(&block);
        }
        if data.len() as u64 != stop - start {
            return Err(Error::integrity("incomplete S3 range"));
        }
        let (hashes, _) = proof.hashes.as_chunks::<{ DIGEST_BYTES as usize }>();
        for (block, hash) in data.chunks(BLOCK_BYTES).zip(hashes) {
            if sha2::Sha256::digest(block).as_slice() != hash {
                return Err(Error::integrity(
                    "S3 range block failed digest verification",
                ));
            }
        }
        Ok(Some(
            data.freeze()
                .slice((offset - start) as usize..(end - start) as usize),
        ))
    }
}
