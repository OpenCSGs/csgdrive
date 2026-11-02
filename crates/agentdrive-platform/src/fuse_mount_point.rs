//! Keep FUSE-T's underlying directory protected while its native session is absent.
use agentdrive_model::{Error, ErrorCode, Result};
use std::{
    fs::{File, OpenOptions, Permissions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub(crate) fn prepare(path: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    ensure_empty(path)?;
    protect(&directory)?;
    // A writer can create a host file before the permission change takes effect.
    // Preserve it and reject the mount instead of hiding it under the workspace.
    ensure_empty(path)?;
    let actual = std::fs::symlink_metadata(path)?;
    let opened = directory.metadata()?;
    if actual.dev() != opened.dev() || actual.ino() != opened.ino() {
        return Err(Error::new(
            ErrorCode::MountBusy,
            "mount directory changed during preparation",
        ));
    }
    Ok(directory)
}

fn ensure_empty(path: &Path) -> Result<()> {
    if std::fs::read_dir(path)?.next().transpose()?.is_some() {
        return Err(Error::new(
            ErrorCode::MountBusy,
            "mount directory must be empty",
        ));
    }
    Ok(())
}

fn protect(directory: &File) -> Result<()> {
    acl::check(directory)?;
    let mode = directory.metadata()?.permissions().mode() & !0o222;
    // Keep this mode after unmount and process exit: restoring write permission
    // would permit successful host writes during a replacement native session.
    directory.set_permissions(Permissions::from_mode(mode))?;
    Ok(())
}

#[allow(unsafe_code)]
mod acl {
    use super::*;
    use std::{ffi::c_void, os::fd::AsRawFd, ptr};

    // Darwin's extended ACL API is not exposed by libc or rustix.
    const ACL_TYPE_EXTENDED: i32 = 0x100;
    const ACL_EXTENDED_ALLOW: i32 = 1;
    const ACL_FIRST_ENTRY: i32 = 0;
    const ACL_NEXT_ENTRY: i32 = -1;
    const ACL_ENTRY_ONLY_INHERIT: i32 = 1 << 8;
    const DIRECTORY_WRITE: u64 = (1 << 2) | (1 << 5) | (1 << 6);

    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: i32, entry: *mut *mut c_void) -> i32;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut i32) -> i32;
        fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> i32;
        fn acl_get_flagset_np(entry: *mut c_void, flags: *mut *mut c_void) -> i32;
        fn acl_get_flag_np(flags: *mut c_void, flag: i32) -> i32;
        fn acl_free(acl: *mut c_void) -> i32;
    }

    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: This pointer was allocated by acl_get_fd_np and is freed once.
            unsafe { acl_free(self.0) };
        }
    }

    pub(super) fn check(directory: &File) -> Result<()> {
        // SAFETY: directory owns an open descriptor for the duration of this call.
        let raw = unsafe { acl_get_fd_np(directory.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(error.into());
        }
        let acl = Acl(raw);
        let mut entry_id = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: acl remains allocated and entry is a writable output pointer.
            if unsafe { acl_get_entry(acl.0, entry_id, &mut entry) } != 0 {
                let error = std::io::Error::last_os_error();
                // Darwin reports the end of a valid ACL with EINVAL.
                if error.raw_os_error() == Some(libc::EINVAL) {
                    return Ok(());
                }
                return Err(error.into());
            }
            entry_id = ACL_NEXT_ENTRY;
            let mut tag = 0;
            let mut mask = 0;
            let mut flags = ptr::null_mut();
            // SAFETY: entry belongs to the live acl; all outputs are valid pointers.
            if unsafe {
                acl_get_tag_type(entry, &mut tag) != 0
                    || acl_get_permset_mask_np(entry, &mut mask) != 0
                    || acl_get_flagset_np(entry, &mut flags) != 0
            } {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: flags was returned from the live entry above.
            let inherit_only = unsafe { acl_get_flag_np(flags, ACL_ENTRY_ONLY_INHERIT) };
            if inherit_only < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if tag == ACL_EXTENDED_ALLOW && mask & DIRECTORY_WRITE != 0 && inherit_only == 0 {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "mount directory contains write-granting ACL entries; choose an empty directory without these entries",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(path: &Path, rule: &str) -> std::process::Output {
        let result = std::process::Command::new("/bin/chmod")
            .args(["+a", rule])
            .arg(path)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        std::process::Command::new("/bin/ls")
            .args(["-lde"])
            .arg(path)
            .output()
            .unwrap()
    }

    #[test]
    fn protection_survives_descriptor_close_and_preserves_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mount");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("existing"), b"preserve").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(prepare(&path).unwrap_err().code, ErrorCode::MountBusy);
        assert_eq!(std::fs::read(path.join("existing")).unwrap(), b"preserve");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode(), mode);
        std::fs::remove_file(path.join("existing")).unwrap();
        drop(prepare(&path).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o222,
            0
        );
        if !rustix::process::geteuid().is_root() {
            assert_eq!(
                std::fs::write(path.join("host"), b"denied")
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        drop(prepare(&path).unwrap());
    }

    #[test]
    fn file_created_before_protection_is_preserved_and_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        let directory = File::open(path).unwrap();
        ensure_empty(path).unwrap();
        std::fs::write(path.join("racing-writer"), b"preserve").unwrap();
        protect(&directory).unwrap();
        assert_eq!(ensure_empty(path).unwrap_err().code, ErrorCode::MountBusy);
        assert_eq!(
            std::fs::read(path.join("racing-writer")).unwrap(),
            b"preserve"
        );
        directory
            .set_permissions(Permissions::from_mode(0o700))
            .unwrap();
    }

    #[test]
    fn write_granting_acls_for_any_user_are_preserved_and_rejected() {
        for rule in [
            "everyone allow add_file,add_subdirectory,delete_child",
            "user:nobody allow add_file,add_subdirectory",
        ] {
            let root = tempfile::tempdir().unwrap();
            let before = acl(root.path(), rule);
            let mode = std::fs::metadata(root.path()).unwrap().permissions().mode();
            let error = prepare(root.path()).unwrap_err();
            assert_eq!(error.code, ErrorCode::PermissionDenied);
            assert!(error.message.contains("ACL"));
            assert_eq!(
                std::fs::metadata(root.path()).unwrap().permissions().mode(),
                mode
            );
            let after = std::process::Command::new("/bin/ls")
                .args(["-lde"])
                .arg(root.path())
                .output()
                .unwrap();
            assert_eq!(after.stdout, before.stdout);
        }
    }

    #[test]
    fn read_only_deny_and_inherit_only_acls_allow_protection() {
        for rule in [
            "everyone allow list,search,readattr",
            "everyone deny add_file,add_subdirectory",
            "everyone allow add_file,add_subdirectory,file_inherit,directory_inherit,only_inherit",
        ] {
            let root = tempfile::tempdir().unwrap();
            acl(root.path(), rule);
            drop(prepare(root.path()).unwrap());
            assert_eq!(
                std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o222,
                0
            );
        }
    }
}
