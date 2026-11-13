use agentdrive_model::{Error, ErrorCode, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostMode {
    Kvm,
    Pvm,
}

impl HostMode {
    pub(super) fn detect(modules: &Path) -> Result<Self> {
        if modules.join("kvm_pvm").try_exists()? {
            Ok(Self::Pvm)
        } else if modules.join("kvm").try_exists()? {
            Ok(Self::Kvm)
        } else {
            Err(Error::new(
                ErrorCode::Unavailable,
                "宿主机尚未启用 KVM/PVM，请加载对应内核模块后启动 Sandbox",
            ))
        }
    }

    pub(super) fn select_kernel(self, configured: &Path) -> Result<PathBuf> {
        if !configured.is_absolute() {
            return Err(Error::invalid("Sandbox 内核必须使用绝对路径"));
        }
        let name = match self {
            Self::Kvm => "vmlinux-bm",
            Self::Pvm => "vmlinux-pvm",
        };
        let candidate = configured.with_file_name(name);
        if candidate.try_exists()? {
            if !candidate.is_file() {
                return Err(Error::invalid(format!(
                    "Sandbox 内核必须是文件：{}",
                    candidate.display()
                )));
            }
            return Ok(candidate);
        }
        // 兼容旧 KVM 配置，同时防止将指向 PVM 内核的符号链接用于 KVM。
        if self == Self::Kvm && configured.try_exists()? {
            let target = configured.canonicalize()?;
            if target.is_file() && target.file_name().is_some_and(|name| name != "vmlinux-pvm") {
                return Ok(configured.to_path_buf());
            }
        }
        Err(Error::new(
            ErrorCode::Unavailable,
            format!(
                "宿主机运行模式为 {self:?}，缺少对应 Sandbox 内核：{}",
                candidate.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_loaded_backend_and_rejects_missing_modules() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            HostMode::detect(root.path()).unwrap_err().code,
            ErrorCode::Unavailable
        );
        std::fs::create_dir(root.path().join("kvm")).unwrap();
        assert_eq!(HostMode::detect(root.path()).unwrap(), HostMode::Kvm);
        std::fs::create_dir(root.path().join("kvm_pvm")).unwrap();
        assert_eq!(HostMode::detect(root.path()).unwrap(), HostMode::Pvm);
    }

    #[test]
    fn selects_matching_kernel_and_preserves_legacy_kvm_support() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("vmlinux");
        std::fs::write(&legacy, "legacy").unwrap();
        assert_eq!(HostMode::Kvm.select_kernel(&legacy).unwrap(), legacy);
        assert_eq!(
            HostMode::Pvm.select_kernel(&legacy).unwrap_err().code,
            ErrorCode::Unavailable
        );
        let pvm = root.path().join("vmlinux-pvm");
        let kvm = root.path().join("vmlinux-bm");
        std::fs::write(&pvm, "pvm").unwrap();
        std::fs::write(&kvm, "kvm").unwrap();
        std::fs::remove_file(&legacy).unwrap();
        assert_eq!(HostMode::Kvm.select_kernel(&legacy).unwrap(), kvm);
        assert_eq!(HostMode::Pvm.select_kernel(&legacy).unwrap(), pvm);
        assert_eq!(HostMode::Kvm.select_kernel(&pvm).unwrap(), kvm);
        std::fs::remove_file(&kvm).unwrap();
        assert!(HostMode::Kvm.select_kernel(&pvm).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_pvm_symlink_as_legacy_kvm_kernel() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("vmlinux");
        let pvm = root.path().join("vmlinux-pvm");
        std::fs::write(&pvm, "pvm").unwrap();
        std::os::unix::fs::symlink(&pvm, &legacy).unwrap();
        assert!(HostMode::Kvm.select_kernel(&legacy).is_err());
        assert_eq!(HostMode::Pvm.select_kernel(&legacy).unwrap(), pvm);
    }
}
