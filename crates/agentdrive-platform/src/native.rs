use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "geesefs.rs"]
mod implementation;
#[cfg(windows)]
#[path = "winfsp.rs"]
mod implementation;

pub struct NativeMountDriver {
    roots: Vec<PathBuf>,
    #[cfg(agentdrive_native_mount)]
    implementation: implementation::Driver,
}
impl std::fmt::Debug for NativeMountDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMountDriver")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}
impl NativeMountDriver {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn configure_geesefs(
        &self,
        endpoint: String,
        admin_endpoint: String,
        access_key: String,
        secret_key: String,
    ) -> Result<()> {
        self.implementation
            .configure(endpoint, admin_endpoint, access_key, secret_key)
    }

    pub fn new(roots: Vec<PathBuf>) -> Result<Arc<Self>> {
        let mut canonical = Vec::new();
        for root in roots {
            std::fs::create_dir_all(&root)?;
            canonical.push(root.canonicalize()?);
        }
        Ok(Arc::new(Self {
            roots: canonical,
            #[cfg(agentdrive_native_mount)]
            implementation: implementation::Driver::new()?,
        }))
    }
}

#[async_trait]
impl MountDriver for NativeMountDriver {
    fn supports_shared_writable_mounts(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "macos"))
    }

    fn supports_automatic_collaboration(&self) -> bool {
        cfg!(windows)
    }

    fn handle_tracking_scope(&self) -> HandleTrackingScope {
        if cfg!(windows) {
            HandleTrackingScope::Native
        } else {
            HandleTrackingScope::Protocol
        }
    }

    async fn normalize_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::invalid("mount path must be absolute and normalized"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::invalid("cannot mount over a filesystem root"))?
            .canonicalize()?;
        let name = path
            .file_name()
            .ok_or_else(|| Error::invalid("mount point needs a directory name"))?;
        let path = parent.join(name);
        if !self
            .roots
            .iter()
            .any(|root| path.starts_with(root) && &path != root)
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "mount path is outside the configured mount roots",
            ));
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            #[cfg(windows)]
            if self.implementation.owns_mount_path(&path) {
                return Ok(path);
            }
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "mount path must not be a symbolic link",
            ));
        }
        Ok(path)
    }

    async fn mount(&self, spec: MountSpec, file_system: Arc<dyn FileSystem>) -> Result<()> {
        self.normalize_path(&spec.binding.path).await?;
        #[cfg(windows)]
        {
            self.implementation.mount(spec, file_system).await
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = file_system;
            self.implementation.mount(&spec.binding).await
        }
        #[cfg(not(agentdrive_native_mount))]
        {
            let _ = file_system;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mounts are unavailable on this platform",
            ))
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn flush(&self, binding: &MountBinding) -> Result<()> {
        self.implementation.flush(binding).await
    }

    #[cfg(windows)]
    async fn open_handles(&self, binding: &MountBinding) -> Result<usize> {
        self.implementation.open_handles(binding).await
    }

    #[cfg(windows)]
    async fn rebind(&self, previous: &MountBinding, next: &MountBinding) -> Result<()> {
        self.implementation.rebind(previous, next).await
    }

    async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        #[cfg(agentdrive_native_mount)]
        {
            self.implementation.unmount(binding).await
        }
        #[cfg(not(agentdrive_native_mount))]
        {
            let _ = binding;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mount support is not enabled",
            ))
        }
    }

    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        #[cfg(agentdrive_native_mount)]
        {
            self.implementation.status(binding).await
        }
        #[cfg(not(agentdrive_native_mount))]
        {
            let _ = binding;
            Ok(NativeMountState::Absent)
        }
    }

    async fn detach_disconnected(&self, binding: &MountBinding) -> Result<bool> {
        self.normalize_path(&binding.path).await?;
        #[cfg(target_os = "linux")]
        {
            self.implementation.detach_disconnected(binding).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(false)
        }
    }

    fn capability(&self) -> &'static str {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            "geesefs"
        }
        #[cfg(windows)]
        {
            "windows-winfsp"
        }
        #[cfg(not(agentdrive_native_mount))]
        {
            "unsupported"
        }
    }
}
