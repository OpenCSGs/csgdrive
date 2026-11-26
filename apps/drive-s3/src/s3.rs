use crate::{listing, multipart::Uploads};
use bytes::{Bytes, BytesMut};
use drive_s3_store::{ErrorKind, ObjectConditions, ObjectMeta, Store};
use futures::TryStreamExt;
use s3s::{S3, S3Error, S3Request, S3Response, S3Result, dto::*, s3_error};
use std::{
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

pub(crate) const MAX_OBJECT: usize = 64 * 1024 * 1024;
pub struct Gateway {
    store: Arc<Store>,
    uploads: Uploads,
}

impl Gateway {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            uploads: Uploads::new(),
        }
    }
}

pub(crate) fn timestamp(ms: i64) -> Timestamp {
    (UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)).into()
}

pub(crate) fn error(err: drive_s3_store::Error) -> S3Error {
    match err.kind {
        ErrorKind::PreconditionFailed => s3_error!(PreconditionFailed, "{err}"),
        ErrorKind::AccessDenied => s3_error!(AccessDenied, "{err}"),
        ErrorKind::NoSuchBucket => s3_error!(NoSuchBucket, "{err}"),
        ErrorKind::NoSuchKey => s3_error!(NoSuchKey, "{err}"),
        ErrorKind::Invalid => s3_error!(InvalidArgument, "{err}"),
        ErrorKind::Conflict => s3_error!(OperationAborted, "{err}"),
        ErrorKind::Internal => {
            tracing::error!(%err,"AgentDrive storage error");
            s3_error!(InternalError)
        }
    }
}

async fn data(body: Option<StreamingBlob>) -> S3Result<Bytes> {
    let mut buf = BytesMut::new();
    if let Some(mut body) = body {
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| s3_error!(IncompleteBody, "{e}"))?
        {
            if chunk.len() > MAX_OBJECT - buf.len() {
                return Err(s3_error!(EntityTooLarge));
            }
            buf.extend_from_slice(&chunk);
        }
    }
    Ok(buf.freeze())
}

fn source(src: &CopySource) -> S3Result<(&str, &str)> {
    match src {
        CopySource::Bucket {
            bucket,
            key,
            version_id: None,
        } => Ok((bucket, key)),
        _ => Err(s3_error!(NotImplemented)),
    }
}

fn check_read_target(version: Option<&str>, part: Option<i32>) -> S3Result<()> {
    if version.is_some_and(|v| v != "null") {
        return Err(s3_error!(NoSuchVersion));
    }
    if part.is_some() {
        return Err(s3_error!(NotImplemented, "individual multipart reads"));
    }
    Ok(())
}

fn read_conditions(
    meta: &ObjectMeta,
    matches: Option<&ETagCondition>,
    differs: Option<&ETagCondition>,
    modified_since: Option<&Timestamp>,
    unmodified_since: Option<&Timestamp>,
) -> S3Result<()> {
    let modified = timestamp(meta.modified_ms.div_euclid(1000) * 1000);
    // HTTP entity tag conditions take precedence over their corresponding dates.
    let unchanged = matches.map_or_else(
        || unmodified_since.is_none_or(|t| modified <= *t),
        |tag| tag.is_any() || tag.as_etag().and_then(ETag::as_strong) == Some(meta.etag.as_str()),
    );
    let changed = differs.map_or_else(
        || modified_since.is_none_or(|t| modified > *t),
        |tag| !tag.is_any() && tag.as_etag().map(ETag::value) != Some(meta.etag.as_str()),
    );
    if !unchanged {
        return Err(s3_error!(PreconditionFailed));
    }
    if !changed {
        return Err(s3_error!(NotModified));
    }
    Ok(())
}

fn delete_conditions(
    version: Option<&String>,
    tag: Option<&ETagCondition>,
    size: Option<i64>,
    modified: Option<&Timestamp>,
) -> S3Result<ObjectConditions> {
    if version.is_some() || modified.is_some() {
        return Err(s3_error!(
            NotImplemented,
            "versioned or timestamp-conditional delete"
        ));
    }
    let etag = match tag {
        Some(ETagCondition::Any) => Some("*".to_owned()),
        Some(ETagCondition::ETag(etag)) => Some(
            etag.as_strong()
                .ok_or_else(|| s3_error!(PreconditionFailed))?
                .to_owned(),
        ),
        None => None,
    };
    let size = size
        .map(u64::try_from)
        .transpose()
        .map_err(|_| s3_error!(InvalidArgument, "negative object size"))?;
    Ok(ObjectConditions { etag, size })
}

fn max_keys(n: Option<i32>) -> S3Result<i32> {
    let n = n.unwrap_or(1000);
    if n < 0 {
        return Err(s3_error!(InvalidArgument, "negative max-keys"));
    }
    Ok(n.min(1000))
}

#[async_trait::async_trait]
impl S3 for Gateway {
    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        self.store
            .check_bucket(&req.input.bucket)
            .await
            .map_err(error)?;
        Ok(S3Response::new(HeadBucketOutput::default()))
    }
    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        self.store
            .check_bucket(&req.input.bucket)
            .await
            .map_err(error)?;
        Ok(S3Response::new(GetBucketLocationOutput::default()))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let i = req.input;
        let limit = max_keys(i.max_keys)?;
        let prefix = i.prefix.as_deref().unwrap_or_default();
        let url = i.encoding_type.is_some();
        let rows = self.store.list(&i.bucket, prefix).await.map_err(error)?;
        let page = listing::page(
            rows,
            prefix,
            i.delimiter.as_deref().unwrap_or_default(),
            i.marker.as_deref().unwrap_or_default(),
            limit as usize,
            url,
        );
        Ok(S3Response::new(ListObjectsOutput {
            name: Some(i.bucket),
            prefix: Some(listing::encoded(prefix, url)),
            delimiter: i.delimiter.map(|d| listing::encoded(&d, url)),
            max_keys: Some(limit),
            encoding_type: i.encoding_type,
            marker: i.marker.map(|m| listing::encoded(&m, url)),
            is_truncated: Some(page.next.is_some()),
            next_marker: page.next.map(|m| listing::encoded(&m, url)),
            common_prefixes: Some(page.prefixes),
            contents: Some(page.objects),
            ..Default::default()
        }))
    }
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let i = req.input;
        let limit = max_keys(i.max_keys)?;
        let prefix = i.prefix.as_deref().unwrap_or_default();
        let url = i.encoding_type.is_some();
        let token = i
            .continuation_token
            .as_deref()
            .map(|v| {
                hex::decode(v)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or_else(|| s3_error!(InvalidArgument, "invalid continuation token"))
            })
            .transpose()?;
        let after = token
            .as_deref()
            .or(i.start_after.as_deref())
            .unwrap_or_default();
        let rows = self.store.list(&i.bucket, prefix).await.map_err(error)?;
        let page = listing::page(
            rows,
            prefix,
            i.delimiter.as_deref().unwrap_or_default(),
            after,
            limit as usize,
            url,
        );
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(i.bucket),
            prefix: Some(listing::encoded(prefix, url)),
            delimiter: i.delimiter.map(|d| listing::encoded(&d, url)),
            max_keys: Some(limit),
            encoding_type: i.encoding_type,
            continuation_token: i.continuation_token,
            start_after: i.start_after.map(|m| listing::encoded(&m, url)),
            is_truncated: Some(page.next.is_some()),
            next_continuation_token: page.next.map(hex::encode),
            key_count: Some((page.objects.len() + page.prefixes.len()) as i32),
            common_prefixes: Some(page.prefixes),
            contents: Some(page.objects),
            ..Default::default()
        }))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let i = req.input;
        check_read_target(i.version_id.as_deref(), i.part_number)?;
        let m = self.store.head(&i.bucket, &i.key).await.map_err(error)?;
        read_conditions(
            &m,
            i.if_match.as_ref(),
            i.if_none_match.as_ref(),
            i.if_modified_since.as_ref(),
            i.if_unmodified_since.as_ref(),
        )?;
        Ok(S3Response::new(HeadObjectOutput {
            content_length: Some(m.size as i64),
            e_tag: Some(ETag::Strong(m.etag)),
            last_modified: Some(timestamp(m.modified_ms)),
            accept_ranges: Some("bytes".into()),
            ..Default::default()
        }))
    }
    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let i = req.input;
        check_read_target(i.version_id.as_deref(), i.part_number)?;
        let reader = self.store.reader(&i.bucket, &i.key).await.map_err(error)?;
        let m = reader.meta.clone();
        read_conditions(
            &m,
            i.if_match.as_ref(),
            i.if_none_match.as_ref(),
            i.if_modified_since.as_ref(),
            i.if_unmodified_since.as_ref(),
        )?;
        let partial = i.range.is_some();
        let range = match i.range {
            Some(requested) => requested
                .check(m.size)
                .ok()
                .filter(|r| !r.is_empty())
                .ok_or_else(|| s3_error!(InvalidRange))?,
            None => 0..m.size,
        };
        let content_range =
            partial.then(|| format!("bytes {}-{}/{}", range.start, range.end - 1, m.size));
        let length = range.end - range.start;
        let chunks =
            futures::stream::try_unfold((reader, range), |(reader, mut range)| async move {
                if range.is_empty() {
                    return Ok::<_, drive_s3_store::Error>(None);
                }
                let bytes = reader.chunk(range.start, range.end - range.start).await?;
                range.start += bytes.len() as u64;
                Ok(Some((bytes, (reader, range))))
            });
        let body = s3s::Body::http_body_unsync(axum::body::Body::from_stream(chunks));
        let mut response = S3Response::new(GetObjectOutput {
            content_length: Some(length as i64),
            content_range,
            body: Some(body.into()),
            last_modified: Some(timestamp(m.modified_ms)),
            e_tag: Some(ETag::Strong(m.etag)),
            accept_ranges: Some("bytes".into()),
            ..Default::default()
        });
        if partial {
            response.status = Some(axum::http::StatusCode::PARTIAL_CONTENT);
        }
        Ok(response)
    }
    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let i = req.input;
        if i.if_match.is_some() || i.if_none_match.is_some() {
            return Err(s3_error!(NotImplemented, "conditional PUT"));
        }
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let bytes = data(i.body).await?;
        let m = self
            .store
            .put(&i.bucket, &i.key, bytes)
            .await
            .map_err(error)?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(ETag::Strong(m.etag)),
            ..Default::default()
        }))
    }
    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let i = req.input;
        let (bucket, key) = source(&i.copy_source)?;
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        if i.if_match.is_some() || i.if_none_match.is_some() {
            return Err(s3_error!(NotImplemented, "conditional copy destination"));
        }
        let reader = self.store.reader(bucket, key).await.map_err(error)?;
        read_conditions(
            &reader.meta,
            i.copy_source_if_match.as_ref(),
            i.copy_source_if_none_match.as_ref(),
            i.copy_source_if_modified_since.as_ref(),
            i.copy_source_if_unmodified_since.as_ref(),
        )
        .map_err(|_| s3_error!(PreconditionFailed))?;
        if reader.meta.size > MAX_OBJECT as u64 {
            return Err(s3_error!(EntityTooLarge));
        }
        let bytes = reader.bytes(0..reader.meta.size).await.map_err(error)?;
        drop(reader);
        let m = self
            .store
            .put(&i.bucket, &i.key, bytes)
            .await
            .map_err(error)?;
        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: Some(ETag::Strong(m.etag)),
                last_modified: Some(timestamp(m.modified_ms)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let i = req.input;
        let conditions = delete_conditions(
            i.version_id.as_ref(),
            i.if_match.as_ref(),
            i.if_match_size,
            i.if_match_last_modified_time.as_ref(),
        )?;
        self.store
            .delete_if(&i.bucket, &i.key, conditions)
            .await
            .map_err(error)?;
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let i = req.input;
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for obj in i.delete.objects {
            let tag = obj.e_tag.map(ETagCondition::ETag);
            let result = match delete_conditions(
                obj.version_id.as_ref(),
                tag.as_ref(),
                obj.size,
                obj.last_modified_time.as_ref(),
            ) {
                Ok(conditions) => self
                    .store
                    .delete_if(&i.bucket, &obj.key, conditions)
                    .await
                    .map_err(error),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) if !i.delete.quiet.unwrap_or(false) => deleted.push(DeletedObject {
                    key: Some(obj.key),
                    ..Default::default()
                }),
                Ok(()) => {}
                Err(e) => {
                    errors.push(s3s::dto::Error {
                        key: Some(obj.key),
                        code: Some(e.code().as_str().to_string()),
                        message: e.message().map(str::to_owned),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: Some(deleted),
            errors: Some(errors),
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let i = req.input;
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let id = self.uploads.create(&i.bucket, &i.key).await?;
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(i.bucket),
            key: Some(i.key),
            upload_id: Some(id),
            ..Default::default()
        }))
    }
    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let i = req.input;
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let etag = self
            .uploads
            .part(
                &i.upload_id,
                &i.bucket,
                &i.key,
                i.part_number,
                data(i.body).await?,
            )
            .await?;
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(etag),
            ..Default::default()
        }))
    }
    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let i = req.input;
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let (bucket, key) = source(&i.copy_source)?;
        let reader = self.store.reader(bucket, key).await.map_err(error)?;
        read_conditions(
            &reader.meta,
            i.copy_source_if_match.as_ref(),
            i.copy_source_if_none_match.as_ref(),
            i.copy_source_if_modified_since.as_ref(),
            i.copy_source_if_unmodified_since.as_ref(),
        )
        .map_err(|_| s3_error!(PreconditionFailed))?;
        let range = match i.copy_source_range {
            Some(range) => Range::parse(&range)
                .map_err(|_| s3_error!(InvalidRange))?
                .check(reader.meta.size)
                .map_err(|_| s3_error!(InvalidRange))?,
            None => 0..reader.meta.size,
        };
        if range.end - range.start > MAX_OBJECT as u64 {
            return Err(s3_error!(EntityTooLarge));
        }
        let bytes = reader.bytes(range).await.map_err(error)?;
        drop(reader);
        let etag = self
            .uploads
            .part(&i.upload_id, &i.bucket, &i.key, i.part_number, bytes)
            .await?;
        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(etag),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let i = req.input;
        if i.if_match.is_some() || i.if_none_match.is_some() {
            return Err(s3_error!(NotImplemented, "conditional completion"));
        }
        self.store.check_writer(&i.bucket).await.map_err(error)?;
        let parts = i
            .multipart_upload
            .and_then(|m| m.parts)
            .ok_or_else(|| s3_error!(InvalidPart))?;
        let m = self
            .uploads
            .complete(&self.store, &i.upload_id, &i.bucket, &i.key, parts)
            .await?;
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(i.bucket),
            key: Some(i.key),
            e_tag: Some(ETag::Strong(m.etag)),
            ..Default::default()
        }))
    }
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let i = req.input;
        self.store
            .authorize_upload_abort(&i.bucket)
            .await
            .map_err(error)?;
        self.uploads.abort(&i.upload_id, &i.bucket, &i.key).await?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }
    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let i = req.input;
        self.store.check_bucket(&i.bucket).await.map_err(error)?;
        let uploads = self
            .uploads
            .list(&i.bucket, i.prefix.as_deref().unwrap_or_default())
            .await;
        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(i.bucket),
            uploads: Some(uploads),
            is_truncated: Some(false),
            ..Default::default()
        }))
    }
}
