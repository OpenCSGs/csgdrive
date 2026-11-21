use crate::{OpenCsgBackend, http::Repository};
use agentdrive_config::OpenCsgConfig;
use agentdrive_model::*;
use agentdrive_ports::ObjectStream;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio, time::Duration};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

pub(crate) struct Xnet {
    pub(crate) enabled: bool,
    base: Url,
    token: String,
    download: Client,
    allow_http: bool,
}

impl Xnet {
    pub fn new(config: &OpenCsgConfig) -> Result<Self> {
        let mut base =
            Url::parse(&config.endpoint).map_err(|_| Error::invalid("invalid OpenCSG URL"))?;
        base.path_segments_mut()
            .map_err(|_| Error::invalid("invalid OpenCSG URL"))?
            .clear()
            .extend(["hf", "codes"])
            .extend(config.repository.split('/'));
        Ok(Self {
            enabled: config.xnet,
            base,
            token: config.token.clone(),
            download: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(60))
                .build()
                .map_err(|_| Error::invalid("invalid Codes download client"))?,
            allow_http: config.allow_http,
        })
    }

    pub async fn check(&self, revision: &str) -> Result<()> {
        self.upload_file(revision, None, 0).await
    }

    async fn upload_file(&self, revision: &str, file: Option<&Path>, size: u64) -> Result<()> {
        let mut token_url = self.base.clone();
        token_url
            .path_segments_mut()
            .expect("validated URL")
            .extend(["xet-write-token", revision]);
        let request = json!({ "action": if file.is_some() { "upload" } else { "check" },
                              "token_url": token_url.as_str(), "token": self.token, "allow_http": self.allow_http,
                              "path": file, "size": size });
        let python = std::env::var_os("AGENTDRIVE_XNET_PYTHON").unwrap_or_else(|| "python3".into());
        let mut child = tokio::process::Command::new(python)
            .args(["-c", include_str!("xnet_transfer.py")])
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                Error::new(
                    ErrorCode::Unsupported,
                    "XNet requires Python with hf-xet 1.1.10 (AGENTDRIVE_XNET_PYTHON)",
                )
            })?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(&serde_json::to_vec(&request)?).await?;
        drop(stdin);
        let status = child.wait().await?;
        match status.code() {
            Some(0) => Ok(()),
            Some(10) => Err(Error::new(
                ErrorCode::Unsupported,
                "install hf-xet 1.1.10 in the XNet Python environment",
            )),
            Some(11) => Err(Error::new(
                ErrorCode::PermissionDenied,
                "access to Codes XNet transfer was denied",
            )),
            Some(12) => Err(Error::new(
                ErrorCode::Unsupported,
                "Codes XNet token endpoint is unavailable",
            )),
            Some(14) => Err(Error::integrity("invalid XNet transfer response")),
            _ => Err(Error::new(ErrorCode::Unavailable, "Codes XNet transfer failed").retryable()),
        }
    }
}

#[derive(Deserialize)]
struct UploadModes {
    files: Vec<UploadMode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadMode {
    path: String,
    upload_mode: String,
    should_ignore: bool,
    is_dir: bool,
}

impl Repository {
    pub async fn uses_lfs(&self, path: &str, size: u64) -> Result<bool> {
        let request = self
            .request(Method::POST, &format!("preupload/{}", self.branch()))
            .json(&json!({"files": [{"path": path, "size": size, "sample": ""}]}));
        let response: UploadModes = self.response(request).await?;
        let [file] = response.files.as_slice() else {
            return Err(Error::integrity("unexpected Codes preupload response"));
        };
        if file.path != path || file.is_dir || file.should_ignore {
            return Err(Error::invalid("Codes preupload rejected the object path"));
        }
        match file.upload_mode.as_str() {
            "lfs" => Ok(true),
            "regular" => Ok(false),
            _ => Err(Error::integrity("unknown Codes upload mode")),
        }
    }

    async fn commit_pointer(&self, path: &str, pointer: &[u8]) -> Result<()> {
        let body = json!({"message":"chore: persist AgentDrive file object", "files":[{
            "path":path, "action":"create", "content":STANDARD.encode(pointer)
        }]});
        self.response::<serde_json::Value>(
            self.request(Method::POST, &format!("commit/{}", self.branch()))
                .json(&body),
        )
        .await?;
        Ok(())
    }
}

impl OpenCsgBackend {
    pub(crate) fn file_object_path(&self, workspace: WorkspaceId, object: &ObjectRef) -> String {
        format!("{}/{workspace}/files/{}", self.prefix, object.id)
    }

    pub(crate) async fn upload_lfs(&self, path: &str, mut source: ObjectStream) -> Result<()> {
        let temp = NamedTempFile::new()?;
        let mut file = tokio::fs::File::from_std(temp.reopen()?);
        let mut digest = object_hasher(source.reference.kind);
        let mut raw = Sha256::new();
        let mut remaining = source.reference.size;
        let mut buffer = vec![0; 128 * 1024];
        while remaining > 0 {
            let n = remaining.min(buffer.len() as u64) as usize;
            source.reader.read_exact(&mut buffer[..n]).await?;
            digest.update(&buffer[..n]);
            raw.update(&buffer[..n]);
            file.write_all(&buffer[..n]).await?;
            remaining -= n as u64;
        }
        if source.reader.read(&mut buffer[..1]).await? != 0
            || finish_digest(digest) != source.reference.id
        {
            return Err(Error::integrity(
                "uploaded content differs from its object reference",
            ));
        }
        file.flush().await?;
        drop(file);
        let oid = format!("{:x}", raw.finalize());
        let size = source.reference.size;
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n");
        let head = self.repository.head().await?;
        if let Some(stored) = self.repository.read(path, &head).await? {
            return verify_pointer(&stored, pointer.as_bytes());
        }
        self.xnet
            .upload_file(self.repository.branch(), Some(temp.path()), size)
            .await?;
        // The Codes commit handler verifies the LFS object against XNet before creating its pointer.
        let _write = self.writes.lock().await;
        let head = self.repository.head().await?;
        if let Some(stored) = self.repository.read(path, &head).await? {
            return verify_pointer(&stored, pointer.as_bytes());
        }
        let result = self
            .repository
            .commit_pointer(path, pointer.as_bytes())
            .await;
        let head = self.repository.head().await?;
        if let Some(stored) = self.repository.read(path, &head).await? {
            return verify_pointer(&stored, pointer.as_bytes());
        }
        result?;
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "Codes LFS commit remains unconfirmed",
        )
        .retryable())
    }

    pub(crate) async fn native_oid(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        head: &str,
    ) -> Result<Option<String>> {
        if object.kind != ObjectKind::File || object.size == 0 {
            return Ok(None);
        }
        let path = self.file_object_path(workspace, object);
        let Some(content) = self.repository.read(&path, head).await? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&content)
            .map_err(|_| Error::integrity("invalid Codes LFS pointer"))?;
        let oid = text
            .strip_prefix("version https://git-lfs.github.com/spec/v1\noid sha256:")
            .and_then(|s| s.strip_suffix(&format!("\nsize {}\n", object.size)))
            .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| Error::integrity("invalid Codes LFS pointer"))?;
        Ok(Some(oid.to_owned()))
    }

    pub(crate) async fn download_lfs(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        revision: &str,
        oid: &str,
    ) -> Result<ObjectStream> {
        let mut url = self.xnet.base.clone();
        url.path_segments_mut()
            .expect("validated URL")
            .extend(["resolve", revision])
            .extend(self.file_object_path(workspace, object).split('/'));
        // Resolve the exact Git commit with repository credentials. Signed data URLs
        // receive no Authorization, including redirects back to the repository host.
        let client = &self.xnet.download;
        let mut request = client.get(url.clone()).bearer_auth(&self.xnet.token);
        let mut response = None;
        for _ in 0..6 {
            let result = request.send().await.map_err(|_| download_error())?;
            if !result.status().is_redirection() {
                response = Some(result);
                break;
            }
            let location = result
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|header| header.to_str().ok())
                .ok_or_else(download_error)?;
            url = url.join(location).map_err(|_| download_error())?;
            let allowed =
                url.scheme() == "https" || (url.scheme() == "http" && self.xnet.allow_http);
            if !allowed
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(Error::invalid("invalid Codes file redirect"));
            }
            request = client.get(url.clone());
        }
        let mut response = response.ok_or_else(download_error)?;
        match response.status().as_u16() {
            200 => (),
            401 | 403 => {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "Codes file access denied",
                ));
            }
            404 => {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "Codes file not found at requested commit",
                ));
            }
            _ => return Err(download_error()),
        }
        let temp = NamedTempFile::new()?;
        let mut file = tokio::fs::File::from_std(temp.reopen()?);
        let mut digest = object_hasher(object.kind);
        let mut sha256 = Sha256::new();
        let mut remaining = object.size;
        while let Some(bytes) = response.chunk().await.map_err(|_| download_error())? {
            remaining = remaining
                .checked_sub(bytes.len() as u64)
                .ok_or_else(|| Error::integrity("XNet file exceeds its expected length"))?;
            digest.update(&bytes);
            sha256.update(&bytes);
            file.write_all(&bytes).await?;
        }
        if remaining != 0
            || format!("{:x}", sha256.finalize()) != oid
            || finish_digest(digest) != object.id
        {
            return Err(Error::integrity("XNet file failed AgentDrive verification"));
        }
        file.flush().await?;
        let reader = tokio::fs::File::from_std(temp.reopen()?);
        Ok(ObjectStream {
            reference: object.clone(),
            reader: Box::pin(reader),
        })
    }
}

fn verify_pointer(stored: &[u8], expected: &[u8]) -> Result<()> {
    if stored != expected {
        return Err(Error::integrity(
            "Codes LFS pointer differs from its object reference",
        ));
    }
    Ok(())
}

fn download_error() -> Error {
    Error::new(ErrorCode::Unavailable, "Codes file transfer failed").retryable()
}
