//! S3 and RustFS storage with immutable content and conditional reference updates.
mod verification;
use agentdrive_config::{RemoteConfig, S3Config};
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, StaticCredentialProvider, UpdateVersion,
    aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey, AwsCredential, S3ConditionalPut},
    multipart::{MultipartStore, PartId},
    path::Path,
    signer::{HeaderName, HeaderValue, Method, SignedUrlOptions, Signer},
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;
use verification::{BlockHashes, Proofs};

const MIN_PART_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

pub struct S3Backend {
    client: AmazonS3,
    http: reqwest::Client,
    prefix: String,
    proofs: Arc<Proofs>,
}
impl std::fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Backend")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Version {
    etag: String,
    version: Option<String>,
}

impl S3Backend {
    pub fn new(config: S3Config) -> Result<Arc<Self>> {
        Self::open(config, None)
    }

    pub fn file_store(
        config: S3Config,
        proofs: Arc<dyn ObjectVerificationStore>,
    ) -> Result<Arc<Self>> {
        Self::open(config, Some(proofs))
    }

    fn open(
        config: S3Config,
        proofs: Option<Arc<dyn ObjectVerificationStore>>,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let target = RemoteConfig::S3(config.clone()).storage_target()?;
        if config.bucket.is_empty() {
            return Err(Error::invalid("S3 bucket is required"));
        }
        let prefix = Path::parse(config.prefix.trim_matches('/'))
            .map_err(|error| Error::invalid(error.to_string()))?
            .to_string();
        if prefix.is_empty() {
            return Err(Error::invalid("S3 namespace prefix must not be empty"));
        }
        let endpoint = config.effective_endpoint();
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(config.bucket)
            .with_region(config.region)
            .with_allow_http(config.allow_http)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .with_virtual_hosted_style_request(false);
        if let Some(endpoint) = endpoint {
            let url = object_store::signer::Url::parse(&endpoint)
                .map_err(|_| Error::invalid("invalid S3 endpoint"))?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::invalid(
                    "S3 endpoint must not contain credentials, a query or a fragment",
                ));
            }
            builder = builder.with_config(AmazonS3ConfigKey::S3Endpoint, endpoint);
        }
        if let Some(credentials) = config.credentials {
            builder =
                builder.with_credentials(Arc::new(StaticCredentialProvider::new(AwsCredential {
                    key_id: credentials.access_key_id,
                    secret_key: credentials.secret_access_key,
                    token: credentials.session_token,
                })));
        }
        let client = builder.build().map_err(store_error)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(http_error)?;
        Ok(Arc::new(Self {
            client,
            http,
            prefix,
            proofs: Arc::new(Proofs::new(target, proofs)),
        }))
    }

    /// 使用独立的临时对象检查读取、条件写入和删除权限。
    pub async fn check_connection(&self) -> Result<()> {
        let key = Path::from(format!(
            "{}/.connection-check/{}",
            self.prefix,
            OperationId::new()
        ));
        let result = async {
            let created = self
                .client
                .put_opts(
                    &key,
                    Bytes::from_static(b"agentdrive").into(),
                    object_store::PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                )
                .await
                .map_err(store_error)?;
            let contents = self
                .client
                .get(&key)
                .await
                .map_err(store_error)?
                .bytes()
                .await
                .map_err(store_error)?;
            if contents.as_ref() != b"agentdrive" {
                return Err(Error::integrity(
                    "S3 connection check returned different contents",
                ));
            }
            self.client
                .put_opts(
                    &key,
                    Bytes::from_static(b"checked").into(),
                    object_store::PutOptions {
                        mode: PutMode::Update(UpdateVersion {
                            e_tag: created.e_tag,
                            version: created.version,
                        }),
                        ..Default::default()
                    },
                )
                .await
                .map_err(store_error)?;
            Ok(())
        }
        .await;
        let cleanup = self.client.delete(&key).await.map_err(store_error);
        result?;
        cleanup
    }

    fn object_key(&self, workspace: WorkspaceId, object: &ObjectId) -> Path {
        Path::from(format!("{}/{workspace}/objects/{object}", self.prefix))
    }
    fn ref_key(&self, key: &RefKey) -> Path {
        Path::from(match key {
            RefKey::Workspace(workspace) => format!("{}/{workspace}/control.json", self.prefix),
            RefKey::Branch { workspace, branch } => {
                format!("{}/{workspace}/branches/{branch}.json", self.prefix)
            }
        })
    }

    async fn verify_remote(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<()> {
        self.object_stream(workspace, object, true)
            .await?
            .try_for_each(|_| async { Ok(()) })
            .await
    }

    async fn object_stream(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        require_digest: bool,
    ) -> Result<futures::stream::BoxStream<'static, Result<Bytes>>> {
        use futures::StreamExt;
        let path = self.object_key(workspace, &object.id);
        let result = self.client.get(&path).await.map_err(store_error)?;
        if result.meta.size != object.size {
            return Err(Error::integrity(
                "remote object length differs from its reference",
            ));
        }
        let etag = result.meta.e_tag.clone();
        let version = result.meta.version.clone();
        let proofs = self.proofs.clone();
        let object = object.clone();
        let mut chunks = result.into_stream();
        Ok(async_stream::try_stream! {
            let mut digest = object_hasher(object.kind);
            let mut blocks = BlockHashes::default();
            let mut total = 0u64;
            while let Some(bytes) = chunks.try_next().await.map_err(store_error)? {
                total = total.checked_add(bytes.len() as u64)
                    .filter(|n| *n <= object.size)
                    .ok_or_else(|| Error::integrity("remote object exceeded its declared size"))?;
                digest.update(&bytes);
                blocks.update(&bytes);
                yield bytes;
            }
            if total == object.size && finish_digest(digest) == object.id {
                proofs.save(workspace, &object, etag, version, blocks).await?;
            } else if require_digest {
                Err(Error::integrity("existing remote object failed verification"))?;
            }
        }
        .boxed())
    }

    async fn complete_immutable(
        &self,
        path: &Path,
        upload: &str,
        parts: Vec<PartId>,
    ) -> Result<(Option<String>, Option<String>)> {
        let header = HeaderName::from_static("if-none-match");
        let options = SignedUrlOptions::new()
            .with_query([("uploadId", upload)])
            .with_signed_header(header.clone(), HeaderValue::from_static("*"));
        let url = self
            .client
            .signed_url_opts(Method::POST, path, Duration::from_secs(300), &options)
            .await
            .map_err(store_error)?;
        let mut body = String::from("<CompleteMultipartUpload>");
        for (index, part) in parts.into_iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                index + 1,
                quick_xml::escape::escape(&part.content_id)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let response = self
            .http
            .post(url)
            .header(header, "*")
            .header("content-type", "application/xml")
            .body(body)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if status.as_u16() == 412 {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "remote object already exists",
            ));
        }
        if !status.is_success() {
            return Err(Error::new(
                ErrorCode::RemoteUnknown,
                format!("multipart completion returned HTTP {}", status.as_u16()),
            )
            .retryable());
        }
        let version = response
            .headers()
            .get("x-amz-version-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(bytes) = stream.try_next().await.map_err(http_error)? {
            if body.len() + bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::integrity(
                    "multipart completion response is too large",
                ));
            }
            body.extend_from_slice(&bytes);
        }
        let mut reader = quick_xml::Reader::from_reader(body.as_slice());
        let mut root = false;
        let mut etag = None;
        loop {
            use quick_xml::events::Event;
            match reader.read_event() {
                Ok(Event::Start(tag)) if !root => {
                    if tag.local_name().as_ref() != b"CompleteMultipartUploadResult" {
                        return Err(Error::new(
                            ErrorCode::RemoteUnknown,
                            "multipart completion returned an embedded error",
                        )
                        .retryable());
                    }
                    root = true;
                }
                Ok(Event::Start(tag)) if tag.local_name().as_ref() == b"ETag" => {
                    let raw = reader
                        .read_text(tag.name())
                        .map_err(|_| Error::integrity("invalid completed ETag"))?;
                    etag = Some(
                        quick_xml::escape::unescape(&raw)
                            .map_err(|_| Error::integrity("invalid completed ETag"))?
                            .into_owned(),
                    );
                }
                Ok(Event::End(tag))
                    if tag.local_name().as_ref() == b"CompleteMultipartUploadResult" =>
                {
                    return Ok((etag, version));
                }
                Ok(Event::Eof) | Err(_) => {
                    return Err(Error::new(
                        ErrorCode::RemoteUnknown,
                        "multipart completion response is incomplete",
                    )
                    .retryable());
                }
                _ => (),
            }
        }
    }

    async fn upload_multipart(
        &self,
        workspace: WorkspaceId,
        mut source: ObjectStream,
    ) -> Result<()> {
        let part_size = source
            .reference
            .size
            .div_ceil(10_000)
            .max(MIN_PART_BYTES)
            .next_multiple_of(1024 * 1024);
        if part_size > MAX_PART_BYTES {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "file exceeds the configured multipart memory boundary",
            ));
        }
        let path = self.object_key(workspace, &source.reference.id);
        let upload = self
            .client
            .create_multipart(&path)
            .await
            .map_err(store_error)?;
        let result = async {
            let mut blocks = BlockHashes::default();
            let mut parts = Vec::new();
            let mut remaining = source.reference.size;
            let mut digest = object_hasher(source.reference.kind);
            while remaining != 0 {
                let size = remaining.min(part_size) as usize;
                let mut buffer = vec![0; size];
                source.reader.read_exact(&mut buffer).await?;
                digest.update(&buffer);
                blocks.update(&buffer);
                let part = self
                    .client
                    .put_part(&path, &upload, parts.len(), Bytes::from(buffer).into())
                    .await
                    .map_err(store_error)?;
                parts.push(part);
                remaining -= size as u64;
            }
            let mut extra = [0u8; 1];
            if source.reader.read(&mut extra).await? != 0
                || finish_digest(digest) != source.reference.id
            {
                return Err(Error::integrity("upload source failed digest verification"));
            }
            let (etag, version) = self.complete_immutable(&path, &upload, parts).await?;
            self.proofs
                .save(workspace, &source.reference, etag, version, blocks)
                .await
        }
        .await;
        if result.is_err() {
            let _ = self.client.abort_multipart(&path, &upload).await;
            if matches!(
                result.as_ref().err().map(|error| error.code),
                Some(ErrorCode::AlreadyExists | ErrorCode::RemoteUnknown | ErrorCode::Unavailable)
            ) {
                match self.verify_remote(workspace, &source.reference).await {
                    Ok(()) => return Ok(()),
                    Err(error) if error.code == ErrorCode::Integrity => return Err(error),
                    Err(_) => (),
                }
            }
        }
        result
    }
}

fn store_error(error: object_store::Error) -> Error {
    let code = match error {
        object_store::Error::NotFound { .. } => ErrorCode::NotFound,
        object_store::Error::AlreadyExists { .. } => ErrorCode::AlreadyExists,
        object_store::Error::Precondition { .. } | object_store::Error::NotModified { .. } => {
            ErrorCode::Conflict
        }
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => ErrorCode::PermissionDenied,
        object_store::Error::NotSupported { .. } | object_store::Error::NotImplemented { .. } => {
            ErrorCode::Unsupported
        }
        object_store::Error::InvalidPath { .. }
        | object_store::Error::UnknownConfigurationKey { .. } => ErrorCode::InvalidArgument,
        _ => ErrorCode::Unavailable,
    };
    let mut mapped = Error::new(code, error.to_string());
    mapped.retryable = code == ErrorCode::Unavailable;
    mapped
}
fn http_error(error: reqwest::Error) -> Error {
    Error::new(ErrorCode::RemoteUnknown, error.without_url().to_string()).retryable()
}

#[async_trait]
impl RemoteObjectStore for S3Backend {
    async fn check_file_storage(&self) -> Result<()> {
        let path = Path::from(format!(
            "{}/.connection-check/{}",
            self.prefix,
            OperationId::new()
        ));
        match self.client.head(&path).await {
            Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }

    async fn upload(&self, workspace: WorkspaceId, mut source: ObjectStream) -> Result<()> {
        let path = self.object_key(workspace, &source.reference.id);
        if self.contains(workspace, &source.reference).await? {
            return Ok(());
        }
        if source.reference.size > MIN_PART_BYTES {
            return self.upload_multipart(workspace, source).await;
        }
        let mut bytes = Vec::with_capacity(source.reference.size as usize);
        (&mut source.reader)
            .take(source.reference.size + 1)
            .read_to_end(&mut bytes)
            .await?;
        verify_object(&source.reference, &bytes)?;
        let mut blocks = BlockHashes::default();
        blocks.update(&bytes);
        match self
            .client
            .put_opts(&path, Bytes::from(bytes).into(), PutMode::Create.into())
            .await
        {
            Ok(result) => {
                self.proofs
                    .save(
                        workspace,
                        &source.reference,
                        result.e_tag,
                        result.version,
                        blocks,
                    )
                    .await
            }
            Err(object_store::Error::AlreadyExists { .. }) => {
                self.verify_remote(workspace, &source.reference).await
            }
            Err(error) => Err(store_error(error)),
        }
    }

    async fn verified_file_exists(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<bool> {
        self.contains(workspace, object).await
    }

    async fn read_verified_range(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Option<Bytes>> {
        self.verified_range(workspace, object, offset, size).await
    }

    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream> {
        // LocalObjectStore::install checks the digest before exposing the downloaded file.
        let stream = self.object_stream(workspace, object, false).await?;
        let reader = StreamReader::new(stream.map_err(std::io::Error::other));
        Ok(ObjectStream {
            reference: object.clone(),
            reader: Box::pin(reader),
        })
    }

    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool> {
        let head = match self
            .client
            .head(&self.object_key(workspace, &object.id))
            .await
        {
            Ok(head) => head,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(store_error(error)),
        };
        if head.size != object.size {
            return Err(Error::integrity(
                "remote object length differs from its reference",
            ));
        }
        if self.proofs.matches(workspace, object, &head).await? {
            return Ok(true);
        }
        self.verify_remote(workspace, object).await?;
        Ok(true)
    }
}

#[async_trait]
impl RemoteRefStore for S3Backend {
    async fn check_connection(&self) -> Result<()> {
        S3Backend::check_connection(self).await
    }
    async fn get(&self, key: &RefKey) -> Result<Option<VersionedRef>> {
        let result = match self.client.get(&self.ref_key(key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(store_error(error)),
        };
        if result.meta.size > MAX_METADATA_BYTES as u64 {
            return Err(Error::integrity("remote reference exceeds format limit"));
        }
        let version = Version {
            etag: result
                .meta
                .e_tag
                .clone()
                .ok_or_else(|| Error::integrity("S3 response omitted ETag"))?,
            version: result.meta.version.clone(),
        };
        let size = result.meta.size;
        let mut reader =
            StreamReader::new(result.into_stream().map_err(std::io::Error::from)).take(size + 1);
        let mut bytes = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut bytes).await?;
        if bytes.len() as u64 != size {
            return Err(Error::integrity("remote reference length mismatch"));
        }
        Ok(Some(VersionedRef {
            version: serde_json::to_string(&version)?,
            bytes: bytes.into(),
        }))
    }

    async fn compare_exchange(&self, update: RefUpdate) -> Result<CasOutcome> {
        if update.bytes.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid("remote reference exceeds format limit"));
        }
        let mode = if let Some(version) = update.expected_version {
            let version: Version = serde_json::from_str(&version)?;
            PutMode::Update(UpdateVersion {
                e_tag: Some(version.etag),
                version: version.version,
            })
        } else {
            PutMode::Create
        };
        match self
            .client
            .put_opts(&self.ref_key(&update.key), update.bytes.into(), mode.into())
            .await
        {
            Ok(result) => Ok(CasOutcome::Applied {
                version: serde_json::to_string(&Version {
                    etag: result
                        .e_tag
                        .ok_or_else(|| Error::integrity("S3 write response omitted ETag"))?,
                    version: result.version,
                })?,
            }),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. }
                | object_store::Error::NotModified { .. },
            ) => Ok(CasOutcome::Conflict),
            Err(error) => {
                let error = store_error(error);
                if error.retryable {
                    Ok(CasOutcome::Unknown)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceId>> {
        let prefix = Path::from(format!("{}/", self.prefix));
        let listed = self
            .client
            .list_with_delimiter(Some(&prefix))
            .await
            .map_err(store_error)?;
        let mut ids = std::collections::BTreeSet::new();
        for path in listed.common_prefixes {
            if let Some(name) = path.filename()
                && let Ok(id) = name.parse::<WorkspaceId>()
            {
                ids.insert(id);
            }
        }
        Ok(ids.into_iter().collect())
    }
    async fn list_branches(&self, workspace: WorkspaceId) -> Result<Vec<BranchId>> {
        let prefix = Path::from(format!("{}/{workspace}/branches/", self.prefix));
        let mut stream = self.client.list(Some(&prefix));
        let mut branches = Vec::new();
        while let Some(object) = stream.try_next().await.map_err(store_error)? {
            if let Some(name) = object
                .location
                .filename()
                .and_then(|name| name.strip_suffix(".json"))
            {
                branches.push(name.parse()?);
            }
        }
        branches.sort();
        branches.dedup();
        Ok(branches)
    }
}
