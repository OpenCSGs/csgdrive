use agentdrive_model::{AccessMode, Error, ErrorCode, MountBinding, Result};
use agentdrive_ports::NativeMountState;
use std::{path::Path, process::Stdio, sync::OnceLock, time::Duration};
use tokio::process::Command;

pub(super) struct Driver {
    gateway: OnceLock<Gateway>,
}
struct Gateway {
    endpoint: String,
    admin_endpoint: String,
    access_key: String,
    secret_key: String,
    client: reqwest::Client,
}

fn alias(binding: &MountBinding) -> String {
    format!("drive-{}-{:x}", binding.id, binding.generation)
}

impl Driver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            gateway: OnceLock::new(),
        })
    }

    pub fn configure(
        &self,
        endpoint: String,
        admin_endpoint: String,
        access_key: String,
        secret_key: String,
    ) -> Result<()> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::new(ErrorCode::Io, e.to_string()))?;
        self.gateway
            .set(Gateway {
                endpoint,
                admin_endpoint,
                access_key,
                secret_key,
                client,
            })
            .map_err(|_| Error::invalid("GeeseFS gateway is already configured"))
    }

    pub async fn mount(&self, binding: &MountBinding) -> Result<()> {
        let gateway = self
            .gateway
            .get()
            .ok_or_else(|| Error::invalid("GeeseFS requires a configured local S3 gateway"))?;
        tokio::fs::create_dir_all(&binding.path).await?;
        if tokio::fs::read_dir(&binding.path)
            .await?
            .next_entry()
            .await?
            .is_some()
        {
            return Err(Error::invalid("mount directory must be empty"));
        }
        let bucket = alias(binding);
        let response = gateway.client.post(format!("{}/mounts", gateway.admin_endpoint.trim_end_matches('/')))
            .json(&serde_json::json!({
                "action": "bind_ids", "bucket": bucket, "workspace_id": binding.workspace,
                "branch_id": binding.branch, "revision": binding.revision, "read_only": binding.access == AccessMode::ReadOnly
            })).send().await.map_err(|e| Error::new(ErrorCode::Io, format!("bind GeeseFS volume: {e}")))?;
        if !response.status().is_success() {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!(
                    "bind GeeseFS volume: {} {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                ),
            ));
        }
        let mut command = Command::new("geesefs");
        command
            .env("AWS_ACCESS_KEY_ID", &gateway.access_key)
            .env("AWS_SECRET_ACCESS_KEY", &gateway.secret_key)
            .env_remove("AWS_SESSION_TOKEN")
            .args([
                "--endpoint",
                &gateway.endpoint,
                "--region",
                "us-east-1",
                "--list-type",
                "2",
                "--stat-cache-ttl",
                "0s",
                "--fsync-on-close",
            ]);
        if binding.access == AccessMode::ReadOnly {
            command.args(["-o", "ro"]);
        }
        // GeeseFS's parent exits after the child has mounted the directory.
        let output = command
            .arg(&bucket)
            .arg(&binding.path)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| Error::new(ErrorCode::Io, format!("start geesefs: {e}")))?;
        if !output.status.success() {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "GeeseFS mount failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(())
    }

    pub async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        let Some(source) = mount_source(&binding.path)? else {
            return Ok(NativeMountState::Absent);
        };
        if source != alias(binding) {
            return Ok(NativeMountState::Unknown);
        }
        #[cfg(target_os = "linux")]
        if disconnected(&binding.path) {
            return Ok(NativeMountState::Unknown);
        }
        Ok(NativeMountState::Mounted)
    }

    pub async fn flush(&self, binding: &MountBinding) -> Result<()> {
        if binding.access == AccessMode::ReadOnly {
            return Ok(());
        }
        if self.status(binding).await? != NativeMountState::Mounted {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "GeeseFS mount is unavailable",
            ));
        }
        let path = binding.path.clone();
        tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
            .await
            .map_err(|e| Error::new(ErrorCode::Io, e.to_string()))??;
        Ok(())
    }

    pub async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        match self.status(binding).await? {
            NativeMountState::Absent => return Ok(()),
            NativeMountState::Unknown => {
                return Err(Error::new(
                    ErrorCode::Conflict,
                    "mount directory belongs to another filesystem",
                ));
            }
            NativeMountState::Mounted => self.flush(binding).await?,
        }
        detach(&binding.path).await
    }

    #[cfg(target_os = "linux")]
    pub async fn detach_disconnected(&self, binding: &MountBinding) -> Result<bool> {
        if mount_source(&binding.path)?.as_deref() != Some(alias(binding).as_str())
            || !disconnected(&binding.path)
        {
            return Ok(false);
        }
        detach(&binding.path).await?;
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
fn disconnected(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|e| e.raw_os_error() == Some(libc::ENOTCONN))
}

async fn detach(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let (program, flag) = ("fusermount3", "-uz");
    #[cfg(target_os = "macos")]
    let (program, flag) = ("/sbin/umount", "-f");
    let output = Command::new(program).arg(flag).arg(path).output().await?;
    if !output.status.success() {
        return Err(Error::new(
            ErrorCode::Io,
            format!(
                "{program} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn mount_source(path: &Path) -> Result<Option<String>> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: A null buffer asks for the mount count without writing records.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut capacity = count as usize + 8;
    loop {
        let mut mounts: Vec<libc::statfs> = Vec::with_capacity(capacity);
        let bytes = i32::try_from(capacity * size_of::<libc::statfs>())
            .map_err(|_| Error::integrity("mount table is too large"))?;
        // SAFETY: The allocation has space for bytes bytes; getfsstat initializes each record it returns.
        let filled = unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if filled < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if filled as usize >= capacity {
            capacity *= 2;
            continue;
        }
        // SAFETY: Only the first filled records were written by getfsstat.
        unsafe {
            mounts.set_len(filled as usize);
        }
        for record in mounts {
            // SAFETY: Darwin's statfs stores both mount names as nul-terminated strings.
            let target = unsafe { CStr::from_ptr(record.f_mntonname.as_ptr()) };
            if path.as_os_str() == OsStr::from_bytes(target.to_bytes()) {
                // SAFETY: As above for the mounted-from name.
                let source = unsafe { CStr::from_ptr(record.f_mntfromname.as_ptr()) };
                return Ok(Some(source.to_string_lossy().into_owned()));
            }
        }
        return Ok(None);
    }
}

#[cfg(target_os = "linux")]
fn mount_source(path: &Path) -> Result<Option<String>> {
    use std::os::unix::ffi::OsStrExt;
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    for line in table.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mut decoded = Vec::new();
        let bytes = fields[4].as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\'
                && index + 3 < bytes.len()
                && bytes[index + 1..index + 4]
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'7'))
            {
                decoded.push(
                    (bytes[index + 1] - b'0') * 64
                        + (bytes[index + 2] - b'0') * 8
                        + bytes[index + 3]
                        - b'0',
                );
                index += 4;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        if decoded == path.as_os_str().as_bytes() {
            let mut fields = right.split_whitespace();
            let kind = fields.next();
            let source = fields.next().unwrap_or("");
            return Ok(Some(if kind == Some("fuse.geesefs") {
                source.to_owned()
            } else {
                String::new()
            }));
        }
    }
    Ok(None)
}
