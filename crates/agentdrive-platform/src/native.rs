use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(any(target_os = "linux", all(target_os = "macos", feature = "macos-mount")))]
#[path = "fuse.rs"]
mod implementation;
#[cfg(windows)]
#[path = "winfsp.rs"]
mod implementation;

pub struct NativeMountDriver {
    roots: Vec<PathBuf>,
    #[cfg(any(
        target_os = "linux",
        windows,
        all(target_os = "macos", feature = "macos-mount")
    ))]
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
    pub fn new(roots: Vec<PathBuf>) -> Result<Arc<Self>> {
        let mut canonical = Vec::new();
        for root in roots {
            std::fs::create_dir_all(&root)?;
            canonical.push(root.canonicalize()?);
        }
        Ok(Arc::new(Self {
            roots: canonical,
            #[cfg(any(
                target_os = "linux",
                windows,
                all(target_os = "macos", feature = "macos-mount")
            ))]
            implementation: implementation::Driver::new()?,
        }))
    }
}

#[async_trait]
impl MountDriver for NativeMountDriver {
    fn supports_automatic_collaboration(&self) -> bool {
        cfg!(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))
    }

    fn requires_remount_for_rebind(&self) -> bool {
        // FUSE-T's NFS backend does not support inode or directory-entry invalidation.
        cfg!(all(target_os = "macos", feature = "macos-fuse-t"))
    }

    fn handle_tracking_scope(&self) -> agentdrive_ports::HandleTrackingScope {
        if cfg!(any(
            target_os = "linux",
            windows,
            all(
                target_os = "macos",
                feature = "macos-mount",
                not(feature = "macos-fuse-t")
            )
        )) {
            agentdrive_ports::HandleTrackingScope::Native
        } else {
            agentdrive_ports::HandleTrackingScope::Protocol
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
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.mount(spec, file_system).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = file_system;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mounts require FUSE-T with macos-fuse-t, or macFUSE with macos-mount",
            ))
        }
    }

    async fn open_handles(&self, binding: &MountBinding) -> Result<usize> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.open_handles(binding).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = binding;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mount support is not enabled",
            ))
        }
    }

    async fn rebind(&self, previous: &MountBinding, next: &MountBinding) -> Result<()> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.rebind(previous, next).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = (previous, next);
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mount support is not enabled",
            ))
        }
    }

    async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.unmount(binding).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = binding;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mount support is not enabled",
            ))
        }
    }

    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.status(binding).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = binding;
            Ok(NativeMountState::Absent)
        }
    }

    fn capability(&self) -> &'static str {
        #[cfg(target_os = "linux")]
        {
            "linux-fuse"
        }
        #[cfg(windows)]
        {
            "windows-winfsp"
        }
        #[cfg(all(target_os = "macos", feature = "macos-fuse-t"))]
        {
            "macos-fuse-t"
        }
        #[cfg(all(
            target_os = "macos",
            feature = "macos-mount",
            not(feature = "macos-fuse-t")
        ))]
        {
            "macos-macfuse"
        }
        #[cfg(all(target_os = "macos", not(feature = "macos-mount")))]
        {
            "macos-macfuse-feature-required"
        }
    }
}

#[cfg(test)]
mod collaboration_tests {
    use super::*;

    #[test]
    fn only_fuse_t_requires_a_new_session_for_rebind() {
        let driver = NativeMountDriver::new(Vec::new()).unwrap();
        assert_eq!(
            driver.requires_remount_for_rebind(),
            cfg!(all(target_os = "macos", feature = "macos-fuse-t"))
        );
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        windows,
        all(target_os = "macos", feature = "macos-mount")
    ))]
    fn compiled_native_drivers_support_automatic_collaboration() {
        let driver = NativeMountDriver::new(Vec::new()).unwrap();
        assert!(driver.supports_automatic_collaboration());
    }

    #[test]
    #[cfg(all(target_os = "macos", not(feature = "macos-mount")))]
    fn unavailable_native_driver_does_not_support_automatic_collaboration() {
        let driver = NativeMountDriver::new(Vec::new()).unwrap();
        assert!(!driver.supports_automatic_collaboration());
    }

    #[cfg(all(
        target_os = "macos",
        any(feature = "macos-fuse-t", not(feature = "macos-mount"))
    ))]
    #[test]
    fn protocol_or_unavailable_driver_does_not_claim_native_handle_tracking() {
        let driver = NativeMountDriver::new(Vec::new()).unwrap();
        assert_eq!(
            driver.handle_tracking_scope(),
            HandleTrackingScope::Protocol
        );
    }
}
