use agentdrive_model::*;
use agentdrive_ports::*;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use tokio::{io::AsyncReadExt, sync::Mutex as AsyncMutex};

type DownloadGates = BTreeMap<(WorkspaceId, ObjectId), Weak<AsyncMutex<()>>>;

#[derive(Clone)]
pub struct RemoteStorage {
    pub objects: Arc<dyn RemoteObjectStore>,
    pub refs: Arc<dyn RemoteRefStore>,
}
impl std::fmt::Debug for RemoteStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteStorage").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default)]
pub struct StorageRoutes {
    pub(crate) default: Option<Arc<RemoteStorage>>,
    workspaces: BTreeMap<WorkspaceId, (String, Arc<RemoteStorage>)>,
}

impl StorageRoutes {
    pub(crate) fn with_default(default: Option<Arc<RemoteStorage>>) -> Self {
        Self {
            default,
            ..Self::default()
        }
    }

    pub fn bind(&mut self, workspace: WorkspaceId, target: String, remote: Arc<RemoteStorage>) {
        self.workspaces.insert(workspace, (target, remote));
    }

    pub(crate) fn workspace_ids(&self) -> impl Iterator<Item = WorkspaceId> + '_ {
        self.workspaces.keys().copied()
    }

    pub async fn check_existing(&self, state: &dyn LocalStateStore) -> Result<()> {
        for workspace in state.workspaces().await? {
            if workspace.deleted && !self.workspaces.contains_key(&workspace.id) {
                continue;
            }
            self.check_target(workspace.id, workspace.storage_target.as_deref())?;
        }
        Ok(())
    }

    pub(crate) fn check_target(&self, id: WorkspaceId, target: Option<&str>) -> Result<()> {
        if self.workspaces.get(&id).map(|(key, _)| key.as_str()) != target {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!(
                    "storage for workspace {id} was fixed at creation; use its original destination"
                ),
            ));
        }
        Ok(())
    }

    pub fn for_workspace(&self, workspace: WorkspaceId) -> Option<Arc<RemoteStorage>> {
        self.workspaces
            .get(&workspace)
            .map(|(_, remote)| remote.clone())
            .or_else(|| self.default.clone())
    }

    fn has_remote(&self) -> bool {
        self.default.is_some() || !self.workspaces.is_empty()
    }
}

pub struct ContentService {
    pub(crate) local: Arc<dyn LocalObjectStore>,
    pub(crate) storage: ArcSwap<StorageRoutes>,
    downloads: Mutex<DownloadGates>,
}

impl std::fmt::Debug for ContentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentService")
            .field("remote", &self.remote_available())
            .finish_non_exhaustive()
    }
}

impl ContentService {
    pub fn new(
        local: Arc<dyn LocalObjectStore>,
        remote: Option<Arc<dyn RemoteObjectStore>>,
        refs: Option<Arc<dyn RemoteRefStore>>,
    ) -> Arc<Self> {
        let default = remote
            .zip(refs)
            .map(|(objects, refs)| Arc::new(RemoteStorage { objects, refs }));
        Self::with_routes(local, StorageRoutes::with_default(default))
    }

    pub(crate) fn with_routes(
        local: Arc<dyn LocalObjectStore>,
        routes: StorageRoutes,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            storage: ArcSwap::from_pointee(routes),
            downloads: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn remote(&self, workspace: WorkspaceId) -> Option<Arc<dyn RemoteObjectStore>> {
        self.storage
            .load()
            .for_workspace(workspace)
            .map(|storage| storage.objects.clone())
    }

    pub(crate) fn refs(&self, workspace: WorkspaceId) -> Option<Arc<dyn RemoteRefStore>> {
        self.storage
            .load()
            .for_workspace(workspace)
            .map(|storage| storage.refs.clone())
    }

    pub(crate) async fn control(
        &self,
        workspace: WorkspaceId,
        mutate: impl Fn(&mut WorkspaceControl) -> Result<bool> + Send,
    ) -> Result<()> {
        let refs = self.refs(workspace).ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let key = RefKey::Workspace(workspace);
        for _ in 0..16 {
            let current = refs
                .get(&key)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace is not published"))?;
            let mut control = crate::replica::decode_workspace_control(workspace, &current.bytes)?;
            if !mutate(&mut control)? {
                return Ok(());
            }
            let bytes = encode(&control)?;
            if bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::new(
                    ErrorCode::CapacityExceeded,
                    "workspace control exceeds format limit",
                ));
            }
            match refs
                .compare_exchange(RefUpdate {
                    key: key.clone(),
                    expected_version: Some(current.version),
                    bytes: bytes.into(),
                })
                .await?
            {
                CasOutcome::Applied { .. } => return Ok(()),
                CasOutcome::Conflict | CasOutcome::Unknown => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "workspace control update remains unconfirmed",
        )
        .retryable())
    }

    pub(crate) async fn upload_object(
        &self,
        remote: &dyn RemoteObjectStore,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<()> {
        let stream = match self.local.open(workspace, object).await {
            Ok(stream) => stream,
            Err(error) if error.code == ErrorCode::NotFound => {
                if object.kind == ObjectKind::File
                    && remote.verified_file_exists(workspace, object).await?
                {
                    return Ok(());
                }
                self.open(workspace, object).await?
            }
            Err(error) => return Err(error),
        };
        remote.upload(workspace, stream).await
    }

    pub(crate) async fn register_publication(
        &self,
        workspace: WorkspaceId,
        operation: OperationId,
        roots: Vec<ObjectRef>,
    ) -> Result<()> {
        self.control(workspace, move |control| {
            if control.workspace.deleted {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "workspace has been deleted",
                ));
            }
            if control.active_operations.get(&operation) == Some(&roots) {
                return Ok(false);
            }
            if control.phase != GcPhase::Open {
                return Err(
                    Error::new(ErrorCode::Busy, "workspace reclamation barrier is active")
                        .retryable(),
                );
            }
            control.active_operations.insert(operation, roots.clone());
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl ContentResolver for ContentService {
    fn remote_available(&self) -> bool {
        self.storage.load().has_remote()
    }

    fn remote_available_for(&self, workspace: WorkspaceId) -> bool {
        self.storage.load().for_workspace(workspace).is_some()
    }

    async fn open(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<ObjectStream> {
        match self.local.open(workspace, reference).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.code == ErrorCode::NotFound => (),
            Err(error) => return Err(error),
        }
        let remote = self.remote(workspace).ok_or_else(|| {
            Error::new(
                ErrorCode::Unavailable,
                "object is not cached and remote storage is unavailable",
            )
            .retryable()
        })?;
        let gate = {
            let mut downloads = self.downloads.lock();
            let key = (workspace, reference.id.clone());
            if let Some(gate) = downloads.get(&key).and_then(Weak::upgrade) {
                gate
            } else {
                downloads.retain(|_, gate| gate.strong_count() != 0);
                let gate = Arc::new(AsyncMutex::new(()));
                downloads.insert(key, Arc::downgrade(&gate));
                gate
            }
        };
        let _guard = gate.lock().await;
        for _ in 0..3 {
            match self.local.open(workspace, reference).await {
                Ok(stream) => return Ok(stream),
                Err(error) if error.code == ErrorCode::NotFound => (),
                Err(error) => return Err(error),
            }
            let stream = remote.download(workspace, reference).await?;
            if stream.reference != *reference {
                return Err(Error::integrity(
                    "remote object identity differs from request",
                ));
            }
            self.local.install(workspace, stream).await?;
        }
        self.local.open(workspace, reference).await
    }

    async fn metadata(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<Bytes> {
        if reference.kind == ObjectKind::File || reference.size > MAX_METADATA_BYTES as u64 {
            return Err(Error::integrity("invalid metadata object"));
        }
        let stream = self.open(workspace, reference).await?;
        let mut data = Vec::with_capacity(reference.size as usize);
        stream
            .reader
            .take(reference.size + 1)
            .read_to_end(&mut data)
            .await?;
        verify_object(reference, &data)?;
        Ok(data.into())
    }

    async fn read_range(
        &self,
        workspace: WorkspaceId,
        reference: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        match self
            .local
            .read_range(workspace, reference, offset, size)
            .await
        {
            Ok(bytes) => return Ok(bytes),
            Err(error) if error.code == ErrorCode::NotFound => (),
            Err(error) => return Err(error),
        }
        if let Some(remote) = self.remote(workspace)
            && let Some(bytes) = remote
                .read_verified_range(workspace, reference, offset, size)
                .await?
        {
            if bytes.len() as u64 != reference.size.saturating_sub(offset).min(u64::from(size)) {
                return Err(Error::integrity(
                    "verified range length differs from the request",
                ));
            }
            return Ok(bytes);
        }
        let _lease = self.open(workspace, reference).await?;
        self.local
            .read_range(workspace, reference, offset, size)
            .await
    }

    async fn protect_remote(
        &self,
        workspace: WorkspaceId,
        operation: OperationId,
        roots: Vec<ObjectRef>,
    ) -> Result<()> {
        if self.refs(workspace).is_none() {
            return Ok(());
        }
        self.control(workspace, move |control| {
            if control.workspace.deleted {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "workspace has been deleted",
                ));
            }
            let pin = HistoryPin {
                operation,
                roots: roots.clone(),
            };
            if control.history_pins.get(&operation) == Some(&pin) {
                return Ok(false);
            }
            if control.phase != GcPhase::Open {
                return Err(
                    Error::new(ErrorCode::Busy, "workspace reclamation barrier is active")
                        .retryable(),
                );
            }
            control.history_pins.insert(operation, pin);
            Ok(true)
        })
        .await
    }

    async fn release_remote(&self, workspace: WorkspaceId, operation: OperationId) -> Result<()> {
        if self.refs(workspace).is_none() {
            return Ok(());
        }
        self.control(workspace, move |control| {
            let published = control.active_operations.remove(&operation).is_some();
            let pinned = control.history_pins.remove(&operation).is_some();
            Ok(published || pinned)
        })
        .await
    }
}
