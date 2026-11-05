//! Per-mount NFSv3 server for the built-in macOS NFS client.
use crate::{binding::NativeBinding, nfs_files::NfsFiles};
use agentdrive_model::*;
use agentdrive_ports::{FileSystem, MountSpec, NativeMountState};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    ffi::{CStr, OsStr},
    os::unix::ffi::OsStrExt,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{io::AsyncWriteExt, task::JoinHandle};

struct Session {
    binding: Arc<NativeBinding>,
    source: String,
    revoked: Arc<AtomicBool>,
    server: JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The listener spawns separate RPC tasks. Invalidate their handles before stopping accepts.
        self.revoked.store(true, Ordering::Release);
        self.server.abort();
    }
}

pub(super) struct Driver {
    sessions: Mutex<BTreeMap<MountId, Session>>,
}

impl Driver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            sessions: Mutex::new(BTreeMap::new()),
        })
    }

    pub async fn mount(&self, spec: MountSpec, files: Arc<dyn FileSystem>) -> Result<()> {
        let binding = spec.binding;
        if volume_source(&binding.path)?.is_some() {
            return Err(
                Error::new(ErrorCode::MountBusy, "mount path is already in use").retryable(),
            );
        }
        std::fs::create_dir_all(&binding.path)?;
        let _underlying_directory = crate::nfs_mount_point::prepare(&binding.path)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let source = format!("127.0.0.1:/{token}");
        let native = NativeBinding::new(binding.clone());
        let revoked = Arc::new(AtomicBool::new(false));
        let accepting_mount = Arc::new(AtomicBool::new(true));
        let fs = NfsFiles::new(
            native.clone(),
            files,
            token,
            revoked.clone(),
            accepting_mount.clone(),
        );
        let listener = NFSTcpListener::bind("127.0.0.1:0", fs).await?;
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move {
            if let Err(error) = listener.handle_forever().await {
                tracing::error!(%error, "NFS listener stopped");
            }
        });
        let session = Session {
            binding: native,
            source,
            revoked,
            server,
        };
        let access = if binding.access == AccessMode::ReadOnly {
            "ro"
        } else {
            "rw"
        };
        // A local CREATE may wait for remote write authority. Keep the RPC pending
        // until the server replies, while allowing the caller to receive termination signals.
        let options = format!(
            "vers=3,tcp,port={port},mountport={port},locallocks,noresvport,intr,retrycnt=0,noac,nonegnamecache,sync,nobrowse,nodev,nosuid,{access}"
        );
        // Pass the one-use export capability to Darwin's mount helper through a pipe.
        // Process arguments and the final kernel mount table are visible to other local users.
        let mut command = tokio::process::Command::new("/sbin/mount_nfs")
            .args(["-o", &options, "-F", "/dev/stdin"])
            .arg(&binding.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = command
            .stdin
            .take()
            .ok_or_else(|| Error::integrity("missing mount helper stdin"))?;
        input.write_all(session.source.as_bytes()).await?;
        drop(input);
        let output = command.wait_with_output().await?;
        accepting_mount.store(false, Ordering::Release);
        if !output.status.success() {
            // Do not expose the export token even on an unsuccessful mount.
            let detail =
                String::from_utf8_lossy(&output.stderr).replace(&session.source, "AgentDrive");
            return Err(Error::new(
                ErrorCode::Io,
                format!("mount_nfs: {}", detail.trim()),
            ));
        }
        self.sessions.lock().insert(binding.id, session);
        Ok(())
    }

    pub async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        let Some(source) = volume_source(&binding.path)? else {
            return Ok(NativeMountState::Absent);
        };
        let sessions = self.sessions.lock();
        let own = sessions.get(&binding.id).is_some_and(|session| {
            let current = session.binding.snapshot();
            current.generation == binding.generation
                && current.path == binding.path
                && source == session.source
                && !session.server.is_finished()
        });
        Ok(if own {
            NativeMountState::Mounted
        } else {
            NativeMountState::Unknown
        })
    }

    pub async fn open_handles(&self, binding: &MountBinding) -> Result<usize> {
        // Native NFSv3 counts active RPCs. macOS detects idle file descriptors on unmount.
        Ok(self
            .sessions
            .lock()
            .get(&binding.id)
            .map_or(0, |s| s.binding.open_handles()))
    }

    pub async fn rebind(&self, previous: &MountBinding, next: &MountBinding) -> Result<()> {
        let sessions = self.sessions.lock();
        let session = sessions.get(&previous.id).ok_or_else(|| {
            Error::new(ErrorCode::StaleBinding, "NFS session is no longer mounted")
        })?;
        if session.server.is_finished() {
            return Err(Error::new(ErrorCode::Io, "NFS listener has stopped"));
        }
        session.binding.rebind(previous, next)
    }

    pub async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        match self.status(binding).await? {
            NativeMountState::Unknown => {
                return Err(Error::new(
                    ErrorCode::Conflict,
                    "mount path belongs to another filesystem or generation",
                ));
            }
            NativeMountState::Absent => (),
            NativeMountState::Mounted => {
                let native = self
                    .sessions
                    .lock()
                    .get(&binding.id)
                    .map(|session| session.binding.clone())
                    .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "NFS session changed"))?;
                // Allow short metadata requests to finish naturally before asking the kernel
                // to detach. Report a stalled request while the current mount is still usable.
                tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    native.wait_until_idle(),
                )
                .await
                .map_err(|_| {
                    Error::new(
                        ErrorCode::MountBusy,
                        "NFS is still processing file requests",
                    )
                    .retryable()
                })?;
                let path = binding.path.clone();
                tokio::task::spawn_blocking(move || {
                    nix::mount::unmount(&path, nix::mount::MntFlags::empty()).map_err(|error| {
                        if error == nix::errno::Errno::EBUSY {
                            Error::new(
                                ErrorCode::MountBusy,
                                "close open files and leave the NFS directory before unmounting",
                            )
                            .retryable()
                        } else {
                            Error::from(std::io::Error::from(error))
                        }
                    })
                })
                .await
                .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
            }
        }
        let native = self.sessions.lock().get(&binding.id).map(|session| {
            session.revoked.store(true, Ordering::Release);
            session.binding.clone()
        });
        if let Some(native) = native {
            // Normal unmount can flush more writes. Finish accepted RPCs before dropping the server.
            native.wait_until_idle().await;
            self.sessions.lock().remove(&binding.id);
        }
        Ok(())
    }
}

/// Read the kernel mount table without issuing a request to the filesystem itself.
/// A status query can run under the same branch lock needed to service NFS requests.
#[allow(unsafe_code)]
fn volume_source(path: &Path) -> Result<Option<String>> {
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
