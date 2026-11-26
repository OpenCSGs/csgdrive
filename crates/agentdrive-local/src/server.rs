use crate::{LocalBackend, sqlite::sql_error};
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::BTreeMap, sync::Arc};
use tokio::io::AsyncReadExt;

pub(crate) fn configure_layout(db: &mut Connection, server: bool) -> Result<()> {
    let tx = db.transaction().map_err(sql_error)?;
    let stored: Option<bool> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='database_metadata'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let populated: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM objects) OR EXISTS(SELECT 1 FROM workspaces)",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if stored.unwrap_or(false) != server && (stored.is_some() || populated) {
        return Err(Error::new(
            ErrorCode::Conflict,
            "data directory uses another metadata layout",
        ));
    }
    tx.execute(
        "INSERT OR IGNORE INTO metadata(key,value) VALUES('database_metadata',?1)",
        [server],
    )
    .map_err(sql_error)?;
    if server {
        tx.execute_batch(include_str!("server.sql"))
            .map_err(sql_error)?;
    }
    tx.commit().map_err(sql_error)
}

/// Persisted server settings and each workspace's content backend configuration.
#[derive(Debug)]
pub struct ServerSettings {
    pub runtime: Vec<u8>,
    pub workspaces: BTreeMap<WorkspaceId, Vec<u8>>,
}

impl LocalBackend {
    pub fn server_settings(&self) -> Result<Option<ServerSettings>> {
        let db = self.inner.connection.lock();
        let runtime = db
            .query_row(
                "SELECT runtime FROM server_settings WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        let Some(runtime) = runtime else {
            return Ok(None);
        };
        let mut stmt = db
            .prepare("SELECT workspace, config FROM workspace_storage")
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))
            .map_err(sql_error)?;
        let workspaces = rows
            .map(|row| {
                let (workspace, config) = row.map_err(sql_error)?;
                Ok((workspace.parse()?, config))
            })
            .collect::<Result<_>>()?;
        Ok(Some(ServerSettings {
            runtime,
            workspaces,
        }))
    }

    pub fn save_server_settings(&self, settings: ServerSettings) -> Result<()> {
        let mut db = self.inner.connection.lock();
        let tx = db.transaction().map_err(sql_error)?;
        tx.execute(
            "INSERT INTO server_settings(id,runtime) VALUES(1,?1)
             ON CONFLICT(id) DO UPDATE SET runtime=excluded.runtime",
            [settings.runtime],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM workspace_storage", [])
            .map_err(sql_error)?;
        for (workspace, config) in settings.workspaces {
            tx.execute(
                "INSERT INTO workspace_storage(workspace,config) VALUES(?1,?2)",
                params![workspace.to_string(), config],
            )
            .map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)
    }

    pub(crate) async fn metadata_bytes(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<Bytes> {
        let object = object.clone();
        self.db(move |db| {
            let bytes: Vec<u8> = db
                .query_row(
                    "SELECT data FROM metadata_objects WHERE workspace=?1 AND hash=?2",
                    params![workspace.to_string(), object.id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "metadata object is absent"))?;
            verify_object(&object, &bytes)?;
            Ok(Bytes::from(bytes))
        })
        .await
    }

    pub(crate) async fn open_metadata(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<ObjectStream> {
        let bytes = self.metadata_bytes(workspace, object).await?;
        Ok(ObjectStream {
            reference: object.clone(),
            reader: Box::pin(std::io::Cursor::new(bytes)),
        })
    }

    pub(crate) async fn has_metadata(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
    ) -> Result<bool> {
        let hash = object.id.clone();
        self.db(move |db| {
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata_objects WHERE workspace=?1 AND hash=?2)",
                params![workspace.to_string(), hash.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)
        })
        .await
    }
}

/// Route file payloads to the selected backend and published AgentDrive state to SQLite.
pub struct DatabaseStorage {
    db: LocalBackend,
    target: String,
    files: Arc<dyn RemoteObjectStore>,
}

impl std::fmt::Debug for DatabaseStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseStorage")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl DatabaseStorage {
    pub fn new(
        db: LocalBackend,
        target: String,
        files: Arc<dyn RemoteObjectStore>,
    ) -> Result<Arc<Self>> {
        if !db.inner.database_metadata {
            return Err(Error::invalid("server metadata requires a server database"));
        }
        Ok(Arc::new(Self { db, target, files }))
    }
}

#[async_trait]
impl RemoteObjectStore for DatabaseStorage {
    async fn upload(&self, workspace: WorkspaceId, source: ObjectStream) -> Result<()> {
        if source.reference.kind == ObjectKind::File {
            return self.files.upload(workspace, source).await;
        }
        let mut data = Vec::new();
        source
            .reader
            .take(MAX_METADATA_BYTES as u64 + 1)
            .read_to_end(&mut data)
            .await?;
        if data.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid("metadata exceeds format limit"));
        }
        verify_object(&source.reference, &data)?;
        self.db
            .db(move |db| {
                db.execute(
                    "INSERT INTO metadata_objects(workspace,hash,data,published) VALUES(?1,?2,?3,1)
                 ON CONFLICT(workspace,hash) DO UPDATE SET published=1",
                    params![workspace.to_string(), source.reference.id.as_str(), data],
                )
                .map_err(sql_error)?;
                Ok(())
            })
            .await
    }
    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream> {
        if object.kind == ObjectKind::File {
            self.files.download(workspace, object).await
        } else {
            self.db.open_metadata(workspace, object).await
        }
    }
    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool> {
        if object.kind == ObjectKind::File {
            self.files.contains(workspace, object).await
        } else {
            self.db.has_metadata(workspace, object).await
        }
    }
}

fn key_parts(key: &RefKey) -> (String, String) {
    match key {
        RefKey::Workspace(id) => (id.to_string(), String::new()),
        RefKey::Branch { workspace, branch } => (workspace.to_string(), branch.to_string()),
    }
}

#[async_trait]
impl RemoteRefStore for DatabaseStorage {
    async fn check_connection(&self) -> Result<()> {
        self.files.check_file_storage().await
    }
    async fn get(&self, key: &RefKey) -> Result<Option<VersionedRef>> {
        let target = self.target.clone();
        let (workspace, branch) = key_parts(key);
        self.db
            .db(move |db| {
                db.query_row(
                    "SELECT version,data FROM published_refs
                 WHERE target=?1 AND workspace=?2 AND branch=?3",
                    params![target, workspace, branch],
                    |row| {
                        Ok(VersionedRef {
                            version: row.get::<_, i64>(0)?.to_string(),
                            bytes: Bytes::from(row.get::<_, Vec<u8>>(1)?),
                        })
                    },
                )
                .optional()
                .map_err(sql_error)
            })
            .await
    }

    async fn compare_exchange(&self, update: RefUpdate) -> Result<CasOutcome> {
        if update.bytes.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid("reference exceeds format limit"));
        }
        let target = self.target.clone();
        let (workspace, branch) = key_parts(&update.key);
        self.db
            .db(move |db| {
                let bytes = update.bytes.as_ref();
                let (changed, version) =
                    match update.expected_version {
                        None => {
                            let changed = db.execute(
                        "INSERT INTO published_refs(target,workspace,branch,version,data)
                         VALUES(?1,?2,?3,1,?4) ON CONFLICT DO NOTHING",
                        params![target, workspace, branch, bytes],
                    ).map_err(sql_error)?;
                            (changed, 1)
                        }
                        Some(expected) => {
                            let expected = expected
                                .parse::<i64>()
                                .map_err(|_| Error::invalid("invalid reference version"))?;
                            let next = expected
                                .checked_add(1)
                                .ok_or_else(|| Error::integrity("reference version exhausted"))?;
                            let changed = db
                                .execute(
                                    "UPDATE published_refs SET version=?5,data=?6
                         WHERE target=?1 AND workspace=?2 AND branch=?3 AND version=?4",
                                    params![target, workspace, branch, expected, next, bytes],
                                )
                                .map_err(sql_error)?;
                            (changed, next)
                        }
                    };
                Ok(if changed == 1 {
                    CasOutcome::Applied {
                        version: version.to_string(),
                    }
                } else {
                    CasOutcome::Conflict
                })
            })
            .await
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceId>> {
        let target = self.target.clone();
        self.db
            .db(move |db| {
                let mut stmt = db
                    .prepare(
                        "SELECT workspace FROM published_refs WHERE target=?1 AND branch=''
                 ORDER BY workspace",
                    )
                    .map_err(sql_error)?;
                let rows = stmt
                    .query_map([target], |row| row.get::<_, String>(0))
                    .map_err(sql_error)?;
                rows.map(|row| row.map_err(sql_error)?.parse()).collect()
            })
            .await
    }

    async fn list_branches(&self, workspace: WorkspaceId) -> Result<Vec<BranchId>> {
        let target = self.target.clone();
        self.db
            .db(move |db| {
                let mut stmt = db.prepare(
                "SELECT branch FROM published_refs WHERE target=?1 AND workspace=?2 AND branch<>''
                 ORDER BY branch"
            ).map_err(sql_error)?;
                let rows = stmt
                    .query_map(params![target, workspace.to_string()], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(sql_error)?;
                rows.map(|row| row.map_err(sql_error)?.parse()).collect()
            })
            .await
    }
}

#[async_trait]
impl FileManifestStore for LocalBackend {
    async fn file_manifest(
        &self,
        workspace: WorkspaceId,
        file: &ObjectId,
    ) -> Result<Option<Vec<u8>>> {
        let id = file.clone();
        self.db(move |db| {
            db.query_row(
                "SELECT manifest FROM file_manifests WHERE workspace=?1 AND hash=?2",
                params![workspace.to_string(), id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)
        })
        .await
    }

    async fn save_file_manifest(
        &self,
        workspace: WorkspaceId,
        file: &ObjectId,
        manifest: Vec<u8>,
    ) -> Result<()> {
        let id = file.clone();
        self.db(move |db| {
            let changed = db
                .execute(
                    "INSERT INTO file_manifests(workspace,hash,manifest) VALUES(?1,?2,?3)
                 ON CONFLICT(workspace,hash) DO UPDATE SET manifest=excluded.manifest
                 WHERE manifest=excluded.manifest",
                    params![workspace.to_string(), id.as_str(), manifest],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(Error::integrity(
                    "file manifest differs from its stored value",
                ));
            }
            Ok(())
        })
        .await
    }
}
