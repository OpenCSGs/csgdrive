use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataOperation {
    Lookup,
    Getattr,
}

struct PausedReply {
    started: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

struct PausedMetadata {
    inner: Arc<dyn FileSystem>,
    operation: MetadataOperation,
    inode: InodeId,
    pause: std::sync::Mutex<Option<PausedReply>>,
}

impl PausedMetadata {
    async fn pause_reply(&self, operation: MetadataOperation) {
        if self.operation == operation {
            let pause = self.pause.lock().unwrap().take();
            if let Some(pause) = pause {
                let _ = pause.started.send(());
                let _ = pause.resume.await;
            }
        }
    }
}

#[async_trait]
impl FileSystem for PausedMetadata {
    async fn lookup(&self, context: &FsContext, parent: InodeId, name: &str) -> Result<Inode> {
        let result = self.inner.lookup(context, parent, name).await;
        if name == "probe.txt" && result.is_ok() {
            self.pause_reply(MetadataOperation::Lookup).await;
        }
        result
    }

    async fn getattr(&self, context: &FsContext, inode: InodeId) -> Result<Inode> {
        let result = self.inner.getattr(context, inode).await;
        if inode == self.inode && result.is_ok() {
            self.pause_reply(MetadataOperation::Getattr).await;
        }
        result
    }

    async fn readdir(&self, context: &FsContext, inode: InodeId) -> Result<Vec<DirectoryItem>> {
        self.inner.readdir(context, inode).await
    }

    async fn create(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        kind: FileKind,
        mode: u32,
    ) -> Result<Inode> {
        self.inner.create(context, parent, name, kind, mode).await
    }

    async fn setattr(
        &self,
        context: &FsContext,
        inode: InodeId,
        attributes: AttributeUpdate,
    ) -> Result<Inode> {
        self.inner.setattr(context, inode, attributes).await
    }

    async fn open(
        &self,
        context: &FsContext,
        inode: InodeId,
        mode: OpenMode,
    ) -> Result<FileHandle> {
        self.inner.open(context, inode, mode).await
    }

    async fn read(
        &self,
        context: &FsContext,
        handle: HandleId,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        self.inner.read(context, handle, offset, size).await
    }

    async fn write(
        &self,
        context: &FsContext,
        handle: HandleId,
        offset: u64,
        data: Bytes,
    ) -> Result<u32> {
        self.inner.write(context, handle, offset, data).await
    }

    async fn release(&self, context: &FsContext, handle: HandleId) -> Result<()> {
        self.inner.release(context, handle).await
    }

    async fn rename(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
        replace: bool,
    ) -> Result<()> {
        self.inner
            .rename(context, parent, name, new_parent, new_name, replace)
            .await
    }

    async fn can_unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
        closing_handle: Option<HandleId>,
    ) -> Result<()> {
        self.inner
            .can_unlink(context, parent, name, directory, closing_handle)
            .await
    }

    async fn unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
    ) -> Result<()> {
        self.inner.unlink(context, parent, name, directory).await
    }

    async fn fsync(&self, context: &FsContext) -> Result<DurabilityReceipt> {
        self.inner.fsync(context).await
    }

    async fn statfs(&self, context: &FsContext) -> Result<VolumeStats> {
        self.inner.statfs(context).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_rebind_waits_until_metadata_reply_is_sent() {
    for operation in [MetadataOperation::Lookup, MetadataOperation::Getattr] {
        metadata_rebind(operation).await;
    }
}

async fn metadata_rebind(operation: MetadataOperation) {
    let harness = Harness::open().await;
    let context_fs = FsContext {
        mount: harness.binding.id,
        generation: harness.binding.generation,
        identity: harness.binding.identity,
    };
    let inode = harness
        .engine
        .services
        .files
        .create(
            &context_fs,
            InodeId::ROOT,
            "probe.txt",
            FileKind::File,
            0o644,
        )
        .await
        .unwrap();
    harness.wait_for_handles(0).await;
    harness.driver.unmount(&harness.binding).await.unwrap();
    let (started, observed) = oneshot::channel();
    let (resume, waiting) = oneshot::channel();
    let files = Arc::new(PausedMetadata {
        inner: harness.engine.services.files.clone(),
        operation,
        inode: inode.id,
        pause: std::sync::Mutex::new(Some(PausedReply {
            started,
            resume: waiting,
        })),
    });
    harness
        .driver
        .mount(
            MountSpec {
                binding: harness.binding.clone(),
            },
            files,
        )
        .await
        .unwrap();
    let path = harness.binding.path.join("probe.txt");
    let reader = tokio::task::spawn_blocking(move || {
        // A second stat reaches getattr after the first lookup populated the native path.
        std::fs::metadata(&path)?;
        std::fs::metadata(path)
    });
    tokio::time::timeout(Duration::from_secs(10), observed)
        .await
        .unwrap()
        .unwrap();
    let mut next = harness.binding.clone();
    next.generation += 1;
    let result = harness.driver.rebind(&harness.binding, &next).await;
    resume.send(()).unwrap();
    let original = reader.await.unwrap();
    harness.wait_for_handles(0).await;
    harness
        .driver
        .rebind(&harness.binding, &next)
        .await
        .unwrap();
    harness
        .local
        .commit(LocalCommit {
            mounts: vec![next.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    let path = harness.binding.path.join("probe.txt");
    let after = tokio::task::spawn_blocking(move || std::fs::metadata(path))
        .await
        .unwrap();
    harness.driver.unmount(&next).await.unwrap();
    assert_eq!(
        result.unwrap_err().code,
        ErrorCode::MountBusy,
        "{operation:?}"
    );
    assert!(original.is_ok(), "{operation:?}: {original:?}");
    assert!(after.is_ok(), "{operation:?}: {after:?}");
}
