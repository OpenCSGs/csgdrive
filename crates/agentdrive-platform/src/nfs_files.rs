use crate::binding::{NativeBinding, NativeHandle};
use crate::inodes::InodeNamespace;
use agentdrive_model::{
    AccessMode, Error, ErrorCode, FileKind, FsContext, IO_BUFFER_BYTES, Inode, InodeId,
    LocalIdentity, OpenMode,
};
use agentdrive_ports::{AttributeUpdate, FileSystem};
use async_trait::async_trait;
use bytes::Bytes;
use nfsserve::{
    nfs::{self, fattr3, filename3, nfs_fh3, nfsstat3, nfstime3, sattr3},
    vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities},
};
use parking_lot::Mutex;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

type NfsResult<T> = Result<T, nfsstat3>;

struct RootTimestamp {
    generation: u64,
    original_ns: i64,
    reported_ns: i64,
}

pub(crate) struct NfsFiles {
    binding: Arc<NativeBinding>,
    inodes: Mutex<InodeNamespace>,
    root_mtime: Mutex<Option<RootTimestamp>>,
    files: Arc<dyn FileSystem>,
    export_path: String,
    session: Uuid,
    revoked: Arc<AtomicBool>,
    accepting_mount: Arc<AtomicBool>,
}

impl NfsFiles {
    pub(crate) fn new(
        binding: Arc<NativeBinding>,
        files: Arc<dyn FileSystem>,
        export_token: String,
        revoked: Arc<AtomicBool>,
        accepting_mount: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inodes: Mutex::new(InodeNamespace::new(binding.snapshot().generation)),
            root_mtime: Mutex::new(None),
            binding,
            files,
            export_path: format!("/{export_token}"),
            session: Uuid::new_v4(),
            revoked,
            accepting_mount,
        }
    }

    fn active(&self) -> NfsResult<()> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        Ok(())
    }

    fn request(&self) -> NfsResult<(FsContext, NativeHandle)> {
        self.active()?;
        let context = self.binding.context(None);
        let handle = self.binding.reserve(context.generation).map_err(status)?;
        self.active()?;
        Ok((context, handle))
    }

    fn inode(&self, context: &FsContext, id: u64) -> NfsResult<InodeId> {
        self.inodes
            .lock()
            .decode(context.generation, id)
            .map_err(status)
    }

    fn fileid(&self, context: &FsContext, inode: InodeId) -> NfsResult<u64> {
        self.inodes
            .lock()
            .encode(context.generation, inode)
            .map_err(status)
    }

    fn file_system_id(&self) -> u64 {
        self.session.as_u64_pair().0
    }

    fn attr(&self, context: &FsContext, inode: Inode) -> NfsResult<fattr3> {
        let fileid = self.fileid(context, inode.id)?;
        let is_directory = inode.kind == FileKind::Directory;
        // The kernel keeps the export root handle across turns. Give it a new
        // directory verifier even when consecutive revisions share their root timestamp.
        let modified = if inode.id == InodeId::ROOT {
            let mut root = self.root_mtime.lock();
            let reported = match root.as_ref() {
                Some(last) if last.generation > context.generation => {
                    return Err(nfsstat3::NFS3ERR_STALE);
                }
                Some(last)
                    if last.generation == context.generation
                        && last.original_ns == inode.modified_ns =>
                {
                    last.reported_ns
                }
                Some(last)
                    if last.reported_ns / 1_000_000_000 == inode.modified_ns / 1_000_000_000 =>
                {
                    inode.modified_ns.saturating_add(1_000_000_000)
                }
                _ => inode.modified_ns,
            };
            *root = Some(RootTimestamp {
                generation: context.generation,
                original_ns: inode.modified_ns,
                reported_ns: reported,
            });
            timestamp(reported)
        } else {
            timestamp(inode.modified_ns)
        };
        Ok(fattr3 {
            ftype: if is_directory {
                nfs::ftype3::NF3DIR
            } else {
                nfs::ftype3::NF3REG
            },
            mode: inode.mode,
            nlink: if is_directory { 2 } else { 1 },
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            used: inode.size,
            fsid: self.file_system_id(),
            fileid,
            atime: modified,
            mtime: modified,
            ctime: modified,
            ..Default::default()
        })
    }

    async fn make(
        &self,
        context: &FsContext,
        parent: u64,
        name: &filename3,
        kind: FileKind,
        mode: u32,
    ) -> NfsResult<Inode> {
        self.files
            .create(
                context,
                self.inode(context, parent)?,
                utf8(name)?,
                kind,
                mode,
            )
            .await
            .map_err(status)
    }
}

fn utf8(name: &filename3) -> NfsResult<&str> {
    std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

fn timestamp(ns: i64) -> nfstime3 {
    let ns = ns.max(0) as u64;
    nfstime3 {
        seconds: (ns / 1_000_000_000).min(u64::from(u32::MAX)) as u32,
        nseconds: (ns % 1_000_000_000) as u32,
    }
}

fn updates(attr: sattr3, identity: LocalIdentity) -> NfsResult<AttributeUpdate> {
    if matches!(attr.uid, nfs::set_uid3::uid(uid) if uid != identity.uid)
        || matches!(attr.gid, nfs::set_gid3::gid(gid) if gid != identity.gid)
    {
        return Err(nfsstat3::NFS3ERR_PERM);
    }
    let modified_ns = match attr.mtime {
        nfs::set_mtime::DONT_CHANGE => None,
        nfs::set_mtime::SET_TO_SERVER_TIME => {
            let elapsed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            Some(elapsed.as_nanos().min(i64::MAX as u128) as i64)
        }
        nfs::set_mtime::SET_TO_CLIENT_TIME(t) if t.nseconds < 1_000_000_000 => {
            Some(i64::from(t.seconds) * 1_000_000_000 + i64::from(t.nseconds))
        }
        nfs::set_mtime::SET_TO_CLIENT_TIME(_) => return Err(nfsstat3::NFS3ERR_INVAL),
    };
    Ok(AttributeUpdate {
        mode: match attr.mode {
            nfs::set_mode3::Void => None,
            nfs::set_mode3::mode(value) => Some(value),
        },
        size: match attr.size {
            nfs::set_size3::Void => None,
            nfs::set_size3::size(value) => Some(value),
        },
        modified_ns,
        created_ns: None,
    })
}

fn status(error: Error) -> nfsstat3 {
    tracing::debug!(code = ?error.code, message = %error.message, "NFS request failed");
    match error.code {
        ErrorCode::NotFound => nfsstat3::NFS3ERR_NOENT,
        ErrorCode::AlreadyExists | ErrorCode::NameConflict => nfsstat3::NFS3ERR_EXIST,
        ErrorCode::NotDirectory => nfsstat3::NFS3ERR_NOTDIR,
        ErrorCode::IsDirectory => nfsstat3::NFS3ERR_ISDIR,
        ErrorCode::DirectoryNotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        ErrorCode::ReadOnly => nfsstat3::NFS3ERR_ROFS,
        ErrorCode::PermissionDenied
        | ErrorCode::BranchBusy
        | ErrorCode::MountBusy
        | ErrorCode::Busy => nfsstat3::NFS3ERR_ACCES,
        ErrorCode::InvalidArgument => nfsstat3::NFS3ERR_INVAL,
        ErrorCode::CapacityExceeded => nfsstat3::NFS3ERR_NOSPC,
        ErrorCode::Unsupported => nfsstat3::NFS3ERR_NOTSUPP,
        ErrorCode::StaleBinding | ErrorCode::StaleAuthority | ErrorCode::TargetMoved => {
            nfsstat3::NFS3ERR_STALE
        }
        _ => nfsstat3::NFS3ERR_IO,
    }
}

#[async_trait]
impl NFSFileSystem for NfsFiles {
    fn root_dir(&self) -> u64 {
        InodeId::ROOT.0
    }

    fn capabilities(&self) -> VFSCapabilities {
        match self.binding.snapshot().access {
            AccessMode::ReadOnly => VFSCapabilities::ReadOnly,
            AccessMode::ReadWrite => VFSCapabilities::ReadWrite,
        }
    }

    async fn path_to_id(&self, path: &[u8]) -> NfsResult<u64> {
        self.active()?;
        if path != self.export_path.as_bytes()
            || !self.accepting_mount.swap(false, Ordering::AcqRel)
        {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        Ok(self.root_dir())
    }

    fn id_to_fh(&self, id: u64) -> nfs_fh3 {
        // Inode numbers can repeat after switching branches. Bind the wire handle
        // to one mount instance, including its root handle.
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(self.session.as_bytes());
        data.extend_from_slice(&id.to_be_bytes());
        nfs_fh3 { data }
    }

    fn fh_to_id(&self, handle: &nfs_fh3) -> NfsResult<u64> {
        self.active()?;
        let Some(id) = handle.data.strip_prefix(self.session.as_bytes().as_slice()) else {
            return Err(nfsstat3::NFS3ERR_STALE);
        };
        Ok(u64::from_be_bytes(
            id.try_into().map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?,
        ))
    }

    fn serverid(&self) -> nfs::cookieverf3 {
        self.file_system_id().to_be_bytes()
    }

    async fn lookup(&self, parent: u64, name: &filename3) -> NfsResult<u64> {
        let (context, _active) = self.request()?;
        self.files
            .lookup(&context, self.inode(&context, parent)?, utf8(name)?)
            .await
            .map_err(status)
            .and_then(|inode| self.fileid(&context, inode.id))
    }

    async fn getattr(&self, id: u64) -> NfsResult<fattr3> {
        let (context, _active) = self.request()?;
        self.files
            .getattr(&context, self.inode(&context, id)?)
            .await
            .map_err(status)
            .and_then(|inode| self.attr(&context, inode))
    }

    async fn setattr(&self, id: u64, attr: sattr3) -> NfsResult<fattr3> {
        let (context, _active) = self.request()?;
        let update = updates(attr, context.identity)?;
        self.files
            .setattr(&context, self.inode(&context, id)?, update)
            .await
            .map_err(status)
            .and_then(|inode| self.attr(&context, inode))
    }

    async fn read(&self, id: u64, offset: u64, count: u32) -> NfsResult<(Vec<u8>, bool)> {
        let (context, _active) = self.request()?;
        let mode = OpenMode {
            read: true,
            write: false,
            append: false,
        };
        let handle = self
            .files
            .open(&context, self.inode(&context, id)?, mode)
            .await
            .map_err(status)?;
        let result = self.files.read(&context, handle.id, offset, count).await;
        let release = self.files.release(&context, handle.id).await;
        let data = result.map_err(status)?;
        release.map_err(status)?;
        let inode = self
            .files
            .getattr(&context, self.inode(&context, id)?)
            .await
            .map_err(status)?;
        let eof = offset.saturating_add(data.len() as u64) >= inode.size;
        Ok((data.to_vec(), eof))
    }

    async fn write(&self, id: u64, offset: u64, data: &[u8]) -> NfsResult<fattr3> {
        let (context, _active) = self.request()?;
        let mode = OpenMode {
            read: false,
            write: true,
            append: false,
        };
        let handle = self
            .files
            .open(&context, self.inode(&context, id)?, mode)
            .await
            .map_err(status)?;
        let result = self
            .files
            .write(&context, handle.id, offset, Bytes::copy_from_slice(data))
            .await;
        let release = self.files.release(&context, handle.id).await;
        let written = result.map_err(status)?;
        release.map_err(status)?;
        if written as usize != data.len() {
            return Err(nfsstat3::NFS3ERR_IO);
        }
        // nfsserve acknowledges each successful WRITE with FILE_SYNC.
        self.files.fsync(&context).await.map_err(status)?;
        self.files
            .getattr(&context, self.inode(&context, id)?)
            .await
            .map_err(status)
            .and_then(|inode| self.attr(&context, inode))
    }

    async fn create(
        &self,
        parent: u64,
        name: &filename3,
        attr: sattr3,
    ) -> NfsResult<(u64, fattr3)> {
        let (context, _active) = self.request()?;
        let mut update = updates(attr, context.identity)?;
        let mode = update.mode.take().unwrap_or(0o644);
        let mut inode = self
            .make(&context, parent, name, FileKind::File, mode)
            .await?;
        if update.size.unwrap_or(0) != 0 || update.modified_ns.is_some() {
            inode = self
                .files
                .setattr(&context, inode.id, update)
                .await
                .map_err(status)?;
        }
        let attr = self.attr(&context, inode)?;
        Ok((attr.fileid, attr))
    }

    async fn create_exclusive(&self, parent: u64, name: &filename3) -> NfsResult<u64> {
        let (context, _active) = self.request()?;
        self.make(&context, parent, name, FileKind::File, 0o600)
            .await
            .map(|inode| inode.id.0)
    }

    async fn mkdir(&self, parent: u64, name: &filename3) -> NfsResult<(u64, fattr3)> {
        let (context, _active) = self.request()?;
        let inode = self
            .make(&context, parent, name, FileKind::Directory, 0o755)
            .await?;
        let attr = self.attr(&context, inode)?;
        Ok((attr.fileid, attr))
    }

    async fn remove(&self, parent: u64, name: &filename3) -> NfsResult<()> {
        let (context, _active) = self.request()?;
        let name = utf8(name)?;
        let inode = self
            .files
            .lookup(&context, self.inode(&context, parent)?, name)
            .await
            .map_err(status)?;
        self.files
            .unlink(
                &context,
                self.inode(&context, parent)?,
                name,
                inode.kind == FileKind::Directory,
            )
            .await
            .map_err(status)
    }

    async fn rename(
        &self,
        parent: u64,
        from: &filename3,
        new_parent: u64,
        to: &filename3,
    ) -> NfsResult<()> {
        let (context, _active) = self.request()?;
        self.files
            .rename(
                &context,
                self.inode(&context, parent)?,
                utf8(from)?,
                self.inode(&context, new_parent)?,
                utf8(to)?,
                true,
            )
            .await
            .map_err(status)
    }

    async fn readdir(&self, parent: u64, after: u64, limit: usize) -> NfsResult<ReadDirResult> {
        let (context, _active) = self.request()?;
        let mut children = self
            .files
            .readdir(&context, self.inode(&context, parent)?)
            .await
            .map_err(status)?;
        // Use monotonic inode IDs as NFS cookies so deleting the previous page's
        // last item cannot reset pagination or hide the remaining directory.
        children.sort_unstable_by_key(|entry| entry.inode.id.0);
        let cookie = if after == 0 {
            0
        } else {
            self.inode(&context, after)?.0
        };
        let start = children.partition_point(|item| item.inode.id.0 <= cookie);
        let end = children.len().saturating_sub(start) <= limit;
        let entries = children
            .into_iter()
            .skip(start)
            .take(limit)
            .map(|item| {
                let attr = self.attr(&context, item.inode)?;
                Ok(DirEntry {
                    fileid: attr.fileid,
                    name: item.name.into_bytes().into(),
                    attr,
                })
            })
            .collect::<NfsResult<_>>()?;
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        _: u64,
        _: &filename3,
        _: &nfs::nfspath3,
        _: &sattr3,
    ) -> NfsResult<(u64, fattr3)> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _: u64) -> NfsResult<nfs::nfspath3> {
        Err(nfsstat3::NFS3ERR_INVAL)
    }

    async fn fsinfo(&self, root: u64) -> NfsResult<nfs::fsinfo3> {
        let attr = self.getattr(root).await?;
        let size = IO_BUFFER_BYTES as u32;
        Ok(nfs::fsinfo3 {
            obj_attributes: nfs::post_op_attr::attributes(attr),
            rtmax: size,
            rtpref: size,
            rtmult: 4096,
            wtmax: size,
            wtpref: size,
            wtmult: 4096,
            dtpref: 32768,
            maxfilesize: i64::MAX as u64,
            time_delta: nfstime3 {
                seconds: 0,
                nseconds: 1,
            },
            properties: nfs::FSF_HOMOGENEOUS | nfs::FSF_CANSETTIME,
        })
    }
}
