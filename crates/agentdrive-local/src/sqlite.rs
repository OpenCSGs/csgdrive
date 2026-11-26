use crate::{LocalBackend, node_index};
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde::{Serialize, de::DeserializeOwned};

pub(crate) fn sql_error(error: rusqlite::Error) -> Error {
    let code = match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::ConstraintViolation) => ErrorCode::Conflict,
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            ErrorCode::Busy
        }
        Some(rusqlite::ErrorCode::DiskFull) => ErrorCode::CapacityExceeded,
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            ErrorCode::Integrity
        }
        _ => ErrorCode::Io,
    };
    Error::new(code, error.to_string())
}

const META_DB_SCHEMA_VERSION: u32 = 2;

pub(crate) fn initialize(connection: &mut Connection) -> Result<LocationId> {
    if has_metadata(connection)? {
        let location: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='location_id'",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        return location.parse();
    }
    let transaction = connection.transaction().map_err(sql_error)?;
    transaction
        .execute_batch(include_str!("schema.sql"))
        .map_err(sql_error)?;
    transaction
        .execute_batch(include_str!("object_usage.sql"))
        .map_err(sql_error)?;
    node_index::migrate(&transaction)?;
    let location = LocationId::new();
    transaction
        .execute(
            "INSERT INTO metadata(key,value) VALUES('schema_version',?1),('location_id',?2)",
            params![META_DB_SCHEMA_VERSION, location.to_string()],
        )
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;
    Ok(location)
}

fn has_metadata(connection: &Connection) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='metadata')",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)
}

pub(crate) fn validate_format(connection: &Connection) -> Result<()> {
    if has_metadata(connection)? {
        let stored: Option<u32> = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if stored != Some(META_DB_SCHEMA_VERSION) {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "unsupported local database schema version",
            ));
        }
    }
    Ok(())
}

pub(crate) fn db_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::invalid("value exceeds the persistent integer range"))
}

pub(crate) fn unsigned_integer(value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::integrity("database contains a negative unsigned integer"))
}

pub(crate) fn read_one<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    args: &[String],
) -> Result<Option<T>> {
    let bytes: Option<Vec<u8>> = connection
        .prepare_cached(sql)
        .map_err(sql_error)?
        .query_row(params_from_iter(args), |row| row.get(0))
        .optional()
        .map_err(sql_error)?;
    bytes.map(|value| decode(&value)).transpose()
}

pub(crate) fn read_many<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    args: &[String],
) -> Result<Vec<T>> {
    let mut statement = connection.prepare_cached(sql).map_err(sql_error)?;
    let rows = statement
        .query_map(params_from_iter(args), |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql_error)?;
    rows.map(|row| decode(&row.map_err(sql_error)?)).collect()
}

impl LocalBackend {
    async fn one<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<String>,
    ) -> Result<Option<T>> {
        self.db(move |connection| read_one(connection, sql, &args))
            .await
    }
    async fn many<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<String>,
    ) -> Result<Vec<T>> {
        self.db(move |connection| read_many(connection, sql, &args))
            .await
    }
}

fn pin_key(pin: &CachePin) -> Result<String> {
    Ok(object_ref(
        ObjectKind::RecordIndex,
        &encode(&(pin.workspace, pin.revision, &pin.paths))?,
    )
    .id
    .to_string())
}

fn ensure_root(
    transaction: &Transaction<'_>,
    workspace: WorkspaceId,
    root: &ObjectRef,
) -> Result<()> {
    let found: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE workspace_id=?1 AND hash=?2 AND size=?3)",
            params![
                workspace.to_string(),
                root.id.as_str(),
                db_integer(root.size)?
            ],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if !found {
        return Err(Error::integrity(
            "a local commit refers to an uninstalled root object",
        ));
    }
    Ok(())
}

fn check_guard(transaction: &Transaction<'_>, guard: &BranchGuard) -> Result<()> {
    let branch: Branch = read_one(
        transaction,
        "SELECT data FROM branch_records WHERE branch_id=?1",
        &[guard.branch.to_string()],
    )?
    .ok_or_else(|| Error::new(ErrorCode::NotFound, "branch does not exist"))?;
    if branch.authority_epoch != guard.authority_epoch {
        return Err(Error::new(
            ErrorCode::StaleAuthority,
            "branch authority changed before commit",
        ));
    }
    if branch.generation != guard.generation
        || branch.formal_head != guard.expected_head
        || guard
            .mutation_seq
            .is_some_and(|expected| expected != branch.mutation_seq)
    {
        return Err(Error::new(
            ErrorCode::TargetMoved,
            "branch changed before commit",
        ));
    }
    Ok(())
}

fn upsert<T: Serialize>(
    transaction: &Transaction<'_>,
    table: &str,
    id_column: &str,
    id: String,
    workspace: WorkspaceId,
    value: &T,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO {table}({id_column},workspace_id,data) VALUES(?1,?2,?3) ON CONFLICT({id_column}) DO UPDATE SET data=excluded.data"
    );
    transaction
        .execute(&sql, params![id, workspace.to_string(), encode(value)?])
        .map_err(sql_error)?;
    Ok(())
}

#[async_trait]
impl LocalStateStore for LocalBackend {
    fn set_tree_reader(&self, reader: std::sync::Weak<dyn RevisionReader>) {
        *self.inner.tree_reader.lock() = Some(reader);
    }

    async fn compact_node_views(&self) -> Result<()> {
        self.db(|db| {
            let tx = db.transaction().map_err(sql_error)?;
            let changed = tx
                .execute(
                    "DELETE FROM metadata WHERE key='node_compaction_pending'",
                    [],
                )
                .map_err(sql_error)?;
            if changed == 0 {
                return Ok(());
            }
            tx.execute_batch(include_str!("node_compact.sql"))
                .map_err(sql_error)?;
            tx.commit().map_err(sql_error)
        })
        .await
    }
    async fn has_node_view(&self, branch: BranchId) -> Result<bool> {
        self.db(move |db| {
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM branch_node_views WHERE branch_id=?1)",
                [branch.to_string()],
                |r| r.get(0),
            )
            .map_err(sql_error)
        })
        .await
    }

    async fn collaboration(&self, branch: BranchId) -> Result<Option<CollaborationRecord>> {
        self.one(
            "SELECT data FROM collaborations WHERE branch_id=?1",
            vec![branch.to_string()],
        )
        .await
    }
    async fn collaborations(&self, workspace: WorkspaceId) -> Result<Vec<CollaborationRecord>> {
        self.many(
            "SELECT data FROM collaborations WHERE workspace_id=?1 ORDER BY branch_id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn relationship_snapshot(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Option<RelationshipSnapshot>> {
        self.read_relationship_snapshot(workspace).await
    }
    async fn file_view(&self, mount: MountId, query: FileQuery) -> Result<Option<FileView>> {
        self.read_file_view(mount, query).await
    }

    async fn retention_state(&self) -> Result<RetentionState> {
        self.read_retention_state().await
    }

    async fn selected_branch(&self, workspace: WorkspaceId) -> Result<Option<BranchId>> {
        self.db(move |connection| {
            let branch: Option<String> = connection
                .query_row(
                    "SELECT branch_id FROM workspace_selections WHERE workspace_id=?1",
                    [workspace.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            branch.map(|value| value.parse()).transpose()
        })
        .await
    }
    async fn location(&self) -> Result<LocationId> {
        Ok(self.inner.location)
    }
    async fn workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        self.one(
            "SELECT data FROM workspace_records WHERE workspace_id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.many(
            "SELECT data FROM workspace_records ORDER BY workspace_id",
            vec![],
        )
        .await
    }
    async fn branch(&self, id: BranchId) -> Result<Option<Branch>> {
        self.one(
            "SELECT data FROM branch_records WHERE branch_id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn branches(&self, workspace: WorkspaceId) -> Result<Vec<Branch>> {
        self.many(
            "SELECT data FROM branch_records WHERE workspace_id=?1 ORDER BY branch_id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn nodes(&self, branch: BranchId) -> Result<Vec<Node>> {
        self.with_node_view(branch, move |db| {
            node_index::many(db, "1", "ORDER BY n.path", &[branch.to_string()])
        })
        .await
    }
    async fn node_deltas(&self, branch: BranchId) -> Result<Vec<NodeDelta>> {
        self.with_node_view(branch, move |db| {
            let mut query = db
                .prepare("SELECT path,previous FROM node_deltas WHERE branch_id=?1 ORDER BY path")
                .map_err(sql_error)?;
            let b = branch.to_string();
            let rows = query
                .query_map([&b], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
                })
                .map_err(sql_error)?;
            rows.map(|r| {
                let (path, before) = r.map_err(sql_error)?;
                let after = node_index::one(db, "n.path=?2", &[b.clone(), path])?;
                Ok(NodeDelta {
                    before: before.map(|bytes| decode(&bytes)).transpose()?,
                    after,
                })
            })
            .collect()
        })
        .await
    }
    async fn history_index(&self, branch: BranchId) -> Result<Option<ObjectRef>> {
        self.one(
            "SELECT root FROM branch_history WHERE branch_id=?1",
            vec![branch.to_string()],
        )
        .await
    }
    async fn children(&self, branch: BranchId, parent: &WorkspacePath) -> Result<Vec<Node>> {
        let args = [branch.to_string(), parent.lookup_key()];
        self.with_node_view(branch, move |db| {
            node_index::many(db, "n.parent=?2", "ORDER BY n.path", &args)
        })
        .await
    }
    async fn children_page(
        &self,
        branch: BranchId,
        parent: &WorkspacePath,
        after: Option<&WorkspacePath>,
        limit: usize,
    ) -> Result<Vec<Node>> {
        let args = [
            branch.to_string(),
            parent.lookup_key(),
            after.map(WorkspacePath::lookup_key).unwrap_or_default(),
            limit.to_string(),
        ];
        self.with_node_view(branch, move |db| {
            node_index::many(
                db,
                "n.parent=?2 AND n.path>?3",
                "ORDER BY n.path LIMIT ?4",
                &args,
            )
        })
        .await
    }
    async fn subtree(&self, branch: BranchId, path: &WorkspacePath) -> Result<Vec<Node>> {
        if path.is_root() {
            return self.nodes(branch).await;
        }
        let key = path.lookup_key();
        let args = [
            branch.to_string(),
            key.clone(),
            format!("{key}/"),
            format!("{key}0"),
        ];
        self.with_node_view(branch, move |db| {
            node_index::many(
                db,
                "n.path=?2 OR (n.path>=?3 AND n.path<?4)",
                "ORDER BY n.path",
                &args,
            )
        })
        .await
    }
    async fn usage(&self, branch: BranchId) -> Result<WorkingTreeUsage> {
        self.with_node_view(branch, move |db| {
            let (inodes, bytes) = db
                .query_row(
                    "SELECT inodes,bytes FROM branch_node_views WHERE branch_id=?1",
                    [branch.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_error)?
                .unwrap_or_default();
            Ok(WorkingTreeUsage {
                inodes: unsigned_integer(inodes)?,
                bytes: unsigned_integer(bytes)?,
            })
        })
        .await
    }
    async fn node(&self, branch: BranchId, inode: InodeId) -> Result<Option<Node>> {
        self.with_node_view(branch, move |db| {
            node_index::one(db, "n.inode=?2", &[branch.to_string(), inode.0.to_string()])
        })
        .await
    }
    async fn node_at(&self, branch: BranchId, path: &WorkspacePath) -> Result<Option<Node>> {
        let args = [branch.to_string(), path.lookup_key()];
        self.with_node_view(branch, move |db| node_index::one(db, "n.path=?2", &args))
            .await
    }
    async fn revision(&self, id: RevisionId) -> Result<Option<Revision>> {
        self.one(
            "SELECT data FROM revision_records WHERE revision_id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn revisions(&self, workspace: WorkspaceId) -> Result<Vec<Revision>> {
        self.many(
            "SELECT data FROM revision_records WHERE workspace_id=?1 ORDER BY revision_id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn revision_metadata(
        &self,
        workspace: WorkspaceId,
        ids: Vec<RevisionId>,
    ) -> Result<Vec<Revision>> {
        self.read_revision_metadata(workspace, ids).await
    }
    async fn revision_history(&self, query: RevisionHistoryScan) -> Result<(Vec<Revision>, usize)> {
        self.read_revision_history(query).await
    }
    async fn operation(&self, id: OperationId) -> Result<Option<OperationRecord>> {
        self.one(
            "SELECT data FROM operations WHERE id=?1 UNION ALL SELECT data FROM replicated_operations WHERE id=?1 LIMIT 1",
            vec![id.to_string()],
        )
        .await
    }
    async fn operation_by_request(
        &self,
        actor: &Principal,
        request_id: &str,
    ) -> Result<Option<OperationRecord>> {
        self.one(
            "SELECT data FROM operations WHERE principal=?1 AND request_id=?2",
            vec![actor.as_str().to_owned(), request_id.to_owned()],
        )
        .await
    }
    async fn operations(&self, workspace: WorkspaceId) -> Result<Vec<OperationRecord>> {
        self.many(
            "SELECT data FROM (SELECT id,data FROM operations WHERE workspace_id=?1 UNION ALL SELECT id,data FROM replicated_operations WHERE workspace_id=?1 AND id NOT IN (SELECT id FROM operations)) ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn pending_merge_history(&self, workspace: WorkspaceId) -> Result<Vec<OperationRecord>> {
        self.many(
            "SELECT o.data FROM operations o LEFT JOIN merge_history_publications p ON p.operation=o.id \
             WHERE o.workspace_id=?1 AND json_extract(CAST(o.data AS TEXT),'$.merge_history') IS NOT NULL \
             AND json_array_length(CAST(o.data AS TEXT),'$.merge_history.events') > coalesce(p.sequence,0) \
             ORDER BY json_extract(CAST(o.data AS TEXT),'$.created_ns'),o.id LIMIT 100",
            vec![workspace.to_string()],
        ).await
    }
    async fn merge_history_page(
        &self,
        query: MergeHistoryScan,
    ) -> Result<(Vec<MergeHistoryEntry>, usize)> {
        self.read_merge_history_page(query).await
    }
    async fn merge_history_entry(
        &self,
        workspace: WorkspaceId,
        record: &str,
    ) -> Result<Option<MergeHistoryEntry>> {
        self.read_merge_history_entry(workspace, record).await
    }
    async fn merge_history_events(
        &self,
        query: MergeEventsScan,
    ) -> Result<(Vec<MergeHistoryAction>, usize)> {
        self.read_merge_history_events(query).await
    }
    async fn mount(&self, id: MountId) -> Result<Option<MountBinding>> {
        self.one("SELECT data FROM mounts WHERE id=?1", vec![id.to_string()])
            .await
    }
    async fn mounts(&self) -> Result<Vec<MountBinding>> {
        self.many("SELECT data FROM mounts ORDER BY id", vec![])
            .await
    }
    async fn merge(&self, id: MergeId) -> Result<Option<MergeCandidate>> {
        self.one("SELECT data FROM merges WHERE id=?1", vec![id.to_string()])
            .await
    }
    async fn merges(&self, workspace: WorkspaceId) -> Result<Vec<MergeCandidate>> {
        self.many(
            "SELECT data FROM merges WHERE workspace_id=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn sync_jobs(&self, workspace: WorkspaceId) -> Result<Vec<SyncJob>> {
        self.many(
            "SELECT data FROM sync_jobs WHERE workspace_id=?1 ORDER BY branch_id,source_generation",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn remote_refs(&self, workspace: WorkspaceId) -> Result<Vec<BranchRef>> {
        self.many(
            "SELECT data FROM remote_refs WHERE workspace_id=?1 ORDER BY branch_id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn cache_pins(&self, workspace: WorkspaceId) -> Result<Vec<CachePin>> {
        self.many(
            "SELECT data FROM cache_pins WHERE workspace_id=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }

    async fn reserve_operation(&self, record: OperationRecord) -> Result<ReservedOperation> {
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let existing: Option<OperationRecord> = read_one(
                &transaction,
                "SELECT data FROM operations WHERE principal=?1 AND request_id=?2",
                &[
                    record.principal.as_str().to_owned(),
                    record.request_id.clone(),
                ],
            )?;
            if let Some(existing) = existing {
                if existing.fingerprint != record.fingerprint || existing.name != record.name {
                    return Err(Error::new(
                        ErrorCode::RequestIdConflict,
                        "request_id was already used with different parameters",
                    ));
                }
                return Ok(ReservedOperation {
                    record: existing,
                    created: false,
                });
            }
            put_operation(&transaction, &record)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ReservedOperation {
                record,
                created: true,
            })
        })
        .await
    }

    async fn commit(&self, commit: LocalCommit) -> Result<()> {
        let edited = commit
            .node_changes
            .iter()
            .filter_map(|change| match change {
                NodeChange::Put { branch, .. } | NodeChange::Remove { branch, .. } => Some(*branch),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        for branch in edited {
            self.ensure_node_view(branch).await?;
        }
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            for guard in &commit.guards { check_guard(&transaction, guard)?; }
            for record in &commit.collaborations {
                upsert(&transaction, "collaborations", "branch_id", record.branch.to_string(), record.workspace, record)?;
            }
            for guard in &commit.operation_guards {
                let current: Option<OperationRecord> = read_one(&transaction, "SELECT data FROM operations WHERE id=?1", &[guard.id.to_string()])?;
                if current.as_ref() != Some(guard) {
                    return Err(Error::new(ErrorCode::TargetMoved, "operation receipt changed before commit").retryable());
                }
            }
            for workspace in &commit.new_workspaces {
                transaction.execute("INSERT INTO workspace_records(data) VALUES(?1)", [encode(workspace)?]).map_err(sql_error)?;
            }
            for workspace in &commit.workspaces {
                let previous: Option<Workspace> = read_one(&transaction, "SELECT data FROM workspace_records WHERE workspace_id=?1", &[workspace.id.to_string()])?;
                if previous.is_some_and(|w| w.deleted) && !workspace.deleted {
                    return Err(Error::new(ErrorCode::NotFound, "workspace has been deleted"));
                }
                transaction.execute("UPDATE workspace_records SET data=?2 WHERE workspace_id=?1", params![workspace.id.to_string(), encode(workspace)?]).map_err(sql_error)?;
            }
            for selection in &commit.selections {
                transaction.execute("INSERT INTO workspace_selections(workspace_id,branch_id) VALUES(?1,?2) ON CONFLICT(workspace_id) DO UPDATE SET branch_id=excluded.branch_id", params![selection.workspace.to_string(), selection.branch.to_string()]).map_err(sql_error)?;
            }
            for branch in &commit.new_branches {
                ensure_root(&transaction, branch.workspace, &branch.working_root)?;
                transaction.execute("INSERT INTO branch_records(data) VALUES(?1)", [encode(branch)?]).map_err(sql_error)?;
            }
            for branch in &commit.branches {
                ensure_root(&transaction, branch.workspace, &branch.working_root)?;
                let exists: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM branches WHERE branch_id=?1)", [branch.id.to_string()], |row| row.get(0)).map_err(sql_error)?;
                if exists {
                    transaction.execute("UPDATE branch_records SET data=?2 WHERE branch_id=?1", params![branch.id.to_string(), encode(branch)?]).map_err(sql_error)?;
                } else {
                    transaction.execute("INSERT INTO branch_records(data) VALUES(?1)", [encode(branch)?]).map_err(sql_error)?;
                }
            }
            if !commit.node_changes.is_empty() {
                node_index::schedule_compaction(&transaction)?;
            }
            for change in &commit.node_changes {
                match change {
                    NodeChange::Put {branch,node} => node_index::change(&transaction,*branch,&node.path,Some(node))?,
                    NodeChange::Remove {branch,path} => node_index::change(&transaction,*branch,path,None)?,
                    NodeChange::Share {branch,workspace,root} => node_index::share(&transaction,*branch,*workspace,root)?,
                    NodeChange::Replace {branch,nodes} => node_index::replace(&transaction,*branch,nodes)?,
                }
            }
            for branch in commit.new_branches.iter().chain(&commit.branches) {
                node_index::save(&transaction,branch)?;
            }
            for branch in &commit.checkpointed_branches {
                transaction.execute("DELETE FROM node_deltas WHERE branch_id=?1", [branch.to_string()]).map_err(sql_error)?;
            }
            for revision in &commit.revisions {
                ensure_root(&transaction, revision.workspace, &revision.root)?;
                put_revision(&transaction, revision)?;
            }
            for operation in &commit.operations { put_operation(&transaction, operation)?; }
            for operation in &commit.replicated_operations {
                put_replicated_operation(&transaction, operation)?;
            }
            for mount in &commit.mounts {
                let active = mount.state != MountState::Released;
                let writable = active && mount.access == AccessMode::ReadWrite;
                transaction.execute("INSERT INTO mounts(id,path,branch_id,active,writable,data) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET path=excluded.path,branch_id=excluded.branch_id,active=excluded.active,writable=excluded.writable,data=excluded.data",
                    params![mount.id.to_string(), mount.path.to_string_lossy(), mount.branch.to_string(), active, writable, encode(mount)?]).map_err(sql_error)?;
            }
            for merge in &commit.merges { upsert(&transaction, "merges", "id", merge.id.to_string(), merge.workspace, merge)?; }
            for job in &commit.sync_jobs {
                if job.expected_authority.is_none() { put_history(&transaction, &job.reference)?; }
                transaction.execute("INSERT INTO sync_jobs(id,workspace_id,branch_id,source_generation,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                    params![job.operation.to_string(), job.reference.workspace.to_string(), job.reference.branch.to_string(), db_integer(job.reference.source_generation)?, encode(job)?]).map_err(sql_error)?;
            }
            for id in &commit.completed_jobs { transaction.execute("DELETE FROM sync_jobs WHERE id=?1", [id.to_string()]).map_err(sql_error)?; }
            for version in &commit.published_merge_history {
                transaction.execute("INSERT INTO merge_history_publications(operation,sequence) VALUES(?1,?2) ON CONFLICT(operation) DO UPDATE SET sequence=max(sequence,excluded.sequence)",
                    params![version.operation.to_string(), db_integer(version.sequence)?]).map_err(sql_error)?;
            }
            for observation in &commit.remote_refs {
                let reference = &observation.reference;
                put_history(&transaction, reference)?;
                let existing: Option<BranchRef> = read_one(&transaction, "SELECT data FROM remote_refs WHERE branch_id=?1", &[reference.branch.to_string()])?;
                if existing.as_ref().is_some_and(|old| old.source_generation > reference.source_generation) { continue; }
                if existing.as_ref().is_some_and(|old| old.source_generation == reference.source_generation && old != reference) {
                    return Err(Error::integrity("remote reference reused a publication sequence"));
                }
                upsert(&transaction, "remote_refs", "branch_id", reference.branch.to_string(), reference.workspace, observation)?;
            }
            for update in &commit.pin_updates {
                let key = pin_key(&update.pin)?;
                if update.enabled { upsert(&transaction, "cache_pins", "id", key, update.pin.workspace, &update.pin)?; }
                else { transaction.execute("DELETE FROM cache_pins WHERE id=?1", [key]).map_err(sql_error)?; }
            }
            transaction.commit().map_err(sql_error)?;
            Ok(())
        }).await
    }
}

fn put_revision(transaction: &Transaction<'_>, revision: &Revision) -> Result<()> {
    let id = revision.id.to_string();
    if let Some(existing) = read_one::<Revision>(
        transaction,
        "SELECT data FROM revision_records WHERE revision_id=?1",
        std::slice::from_ref(&id),
    )? {
        if existing != *revision {
            return Err(Error::integrity("immutable revision was changed"));
        }
        return Ok(());
    }
    transaction
        .execute(
            "INSERT INTO revision_records(data) VALUES(?1)",
            [encode(revision)?],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn put_history(transaction: &Transaction<'_>, reference: &BranchRef) -> Result<()> {
    transaction.execute(
        "INSERT INTO branch_history(branch_id,source_generation,root) VALUES(?1,?2,?3) ON CONFLICT(branch_id) DO UPDATE SET source_generation=excluded.source_generation,root=excluded.root WHERE excluded.source_generation>branch_history.source_generation",
        params![reference.branch.to_string(), db_integer(reference.source_generation)?, encode(&reference.record_index)?],
    ).map_err(sql_error)?;
    Ok(())
}

fn put_replicated_operation(transaction: &Transaction<'_>, record: &OperationRecord) -> Result<()> {
    let local: Option<OperationRecord> = read_one(
        transaction,
        "SELECT data FROM operations WHERE id=?1",
        &[record.id.to_string()],
    )?;
    if let Some(local) = local {
        check_operation_identity(Some(&local), record)?;
        return Ok(());
    }
    let existing: Option<OperationRecord> = read_one(
        transaction,
        "SELECT data FROM replicated_operations WHERE id=?1",
        &[record.id.to_string()],
    )?;
    check_operation_identity(existing.as_ref(), record)?;
    if let Some(existing) = existing {
        if existing.workspace != record.workspace
            || existing.branch != record.branch
            || existing.created_ns != record.created_ns
        {
            return Err(Error::integrity("replicated operation context changed"));
        }
        let Some(next) = &record.merge_history else {
            return Ok(());
        };
        if let Some(previous) = &existing.merge_history {
            if previous.location != next.location
                || previous.source != next.source
                || previous.source_branch != next.source_branch
                || previous.target != next.target
                || previous.candidate_before != next.candidate_before
                || previous.resolutions != next.resolutions
                || previous.merge.zip(next.merge).is_some_and(|(a, b)| a != b)
                || previous.base.zip(next.base).is_some_and(|(a, b)| a != b)
            {
                return Err(Error::integrity("merge history identity changed"));
            }
            let shared = previous.events.len().min(next.events.len());
            if previous.events[..shared] != next.events[..shared] {
                return Err(Error::integrity("merge history changed an existing event"));
            }
            if previous.events.len() >= next.events.len() {
                return Ok(());
            }
            if (previous.merge.is_some() && next.merge.is_none())
                || (previous.base.is_some() && next.base.is_none())
            {
                return Err(Error::integrity("merge history lost its candidate context"));
            }
        }
    }
    transaction.execute("INSERT INTO replicated_operations(id,workspace_id,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
        params![record.id.to_string(), record.workspace.map(|id| id.to_string()), encode(record)?]).map_err(sql_error)?;
    Ok(())
}

fn put_operation(transaction: &Transaction<'_>, record: &OperationRecord) -> Result<()> {
    let existing: Option<OperationRecord> = read_one(
        transaction,
        "SELECT data FROM operations WHERE id=?1 UNION ALL SELECT data FROM replicated_operations WHERE id=?1 LIMIT 1",
        &[record.id.to_string()],
    )?;
    check_operation_identity(existing.as_ref(), record)?;
    transaction.execute("INSERT INTO operations(id,principal,request_id,workspace_id,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id,data=excluded.data",
        params![record.id.to_string(), record.principal.as_str(), record.request_id, record.workspace.map(|id| id.to_string()), encode(record)?]).map_err(sql_error)?;
    Ok(())
}

fn check_operation_identity(
    existing: Option<&OperationRecord>,
    record: &OperationRecord,
) -> Result<()> {
    if existing.is_some_and(|old| {
        old.principal != record.principal
            || old.request_id != record.request_id
            || old.fingerprint != record.fingerprint
            || old.name != record.name
    }) {
        return Err(Error::new(
            ErrorCode::RequestIdConflict,
            "operation identity is immutable",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod query_tests {
    use super::*;

    fn node(name: &str, id: u64, size: u64) -> Node {
        Node {
            path: name.to_owned().try_into().unwrap(),
            inode: Inode {
                id: InodeId(id),
                kind: FileKind::File,
                mode: 0o600,
                uid: 1,
                gid: 1,
                size,
                created_ns: 1,
                modified_ns: 1,
                content: None,
            },
            dirty: true,
        }
    }

    fn test_database() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", true).unwrap();
        initialize(&mut db).unwrap();
        db
    }

    fn create_branch(db: &mut Connection) -> (BranchId, Revision) {
        let workspace = WorkspaceId::new();
        let branch = BranchId::new();
        let revision = Revision {
            message: None,
            id: RevisionId::new(),
            workspace,
            branch: Some(branch),
            parents: Vec::new(),
            root: object_ref(ObjectKind::Tree, b"root"),
            kind: RevisionKind::Commit,
            actor: "owner".to_owned().try_into().unwrap(),
            created_ns: 0,
            operation: OperationId::new(),
        };
        let tx = db.transaction().unwrap();
        // The workspace, branch and initial revision are published together at commit.
        tx.execute(
            "INSERT INTO workspace_records(data) VALUES(?1)",
            params![
                encode(&serde_json::json!({
                    "id": workspace, "name": "workspace", "initial_revision": revision.id,
                }))
                .unwrap()
            ],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO branch_records(data) VALUES(?1)",
            params![
            encode(&serde_json::json!({
                "id": branch, "workspace": workspace, "name": "main", "base_revision": revision.id,
                "formal_head": revision.id, "working_root": revision.root,
            })).unwrap()
        ],
        )
        .unwrap();
        put_revision(&tx, &revision).unwrap();
        tx.commit().unwrap();
        (branch, revision)
    }

    #[test]
    fn core_payloads_store_each_promoted_field_only_in_its_column() {
        let mut db = test_database();
        let (branch_id, initial) = create_branch(&mut db);
        for (table, fields) in [
            ("workspaces", &["id", "name", "initial_revision"][..]),
            (
                "branches",
                &[
                    "id",
                    "workspace",
                    "name",
                    "source_branch",
                    "base_revision",
                    "formal_head",
                    "autosave_head",
                ][..],
            ),
            (
                "revisions",
                &[
                    "id",
                    "workspace",
                    "branch",
                    "created_ns",
                    "operation",
                    "message",
                    "parents",
                ][..],
            ),
        ] {
            let sql = format!("SELECT data FROM {table}");
            let payload: serde_json::Value = read_one(&db, &sql, &[]).unwrap().unwrap();
            for field in fields {
                assert!(
                    payload.get(field).is_none(),
                    "{table}.data contains {field}"
                );
            }
        }
        let source = BranchId::new();
        db.execute(
            "UPDATE branches SET name='renamed', source_branch_id=?1 WHERE branch_id=?2",
            params![source.to_string(), branch_id.to_string()],
        )
        .unwrap();
        let branch: serde_json::Value = read_one(
            &db,
            "SELECT data FROM branch_records WHERE branch_id=?1",
            &[branch_id.to_string()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(branch["name"], "renamed");
        assert_eq!(branch["source_branch"], source.to_string());
        assert_eq!(branch["formal_head"], initial.id.to_string());
        db.execute(
            "UPDATE revisions SET message=?1, created_ns=7 WHERE revision_id=?2",
            params!["updated metadata", initial.id.to_string()],
        )
        .unwrap();
        let revision: Revision = read_one(
            &db,
            "SELECT data FROM revision_records WHERE revision_id=?1",
            &[initial.id.to_string()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            revision,
            Revision {
                message: Some("updated metadata".to_owned()),
                created_ns: 7,
                ..initial
            }
        );
        assert!(db.execute("UPDATE workspaces SET data=CAST(json_set(CAST(data AS TEXT),'$.name','duplicate') AS BLOB)", []).is_err());
    }

    #[test]
    fn parent_order_survives_replay_and_remote_only_history() {
        let mut db = test_database();
        let (_, initial) = create_branch(&mut db);
        let remote_branch = BranchId::new();
        let mut merge = Revision {
            id: RevisionId::new(),
            kind: RevisionKind::Commit,
            branch: Some(remote_branch),
            parents: vec![initial.id, RevisionId::new()],
            ..initial.clone()
        };
        let tx = db.transaction().unwrap();
        put_revision(&tx, &merge).unwrap();
        put_revision(&tx, &merge).unwrap();
        tx.commit().unwrap();
        let (workspace, branch, count): (String, String, i64) = db.query_row(
            "SELECT workspace_id,branch_id,(SELECT count(*) FROM revision_parents WHERE revision_id=r.revision_id)
             FROM revisions r WHERE revision_id=?1 AND json_type(CAST(data AS TEXT),'$.parents') IS NULL",
            [merge.id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).unwrap();
        assert_eq!(
            (workspace, branch, count),
            (merge.workspace.to_string(), remote_branch.to_string(), 2)
        );
        let stored: Revision = read_one(
            &db,
            "SELECT data FROM revision_records WHERE revision_id=?1",
            &[merge.id.to_string()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(stored, merge);
        merge.parents.reverse();
        let tx = db.transaction().unwrap();
        assert_eq!(
            put_revision(&tx, &merge).unwrap_err().code,
            ErrorCode::Integrity
        );
    }

    #[test]
    fn local_relationships_reject_a_cross_workspace_branch_or_orphan_revision_edge() {
        let mut db = test_database();
        let (branch, _) = create_branch(&mut db);
        let (_, other) = create_branch(&mut db);
        let tx = db.transaction().unwrap();
        tx.execute(
            "INSERT INTO workspace_selections(workspace_id,branch_id) VALUES(?1,?2)",
            params![other.workspace.to_string(), branch.to_string()],
        )
        .unwrap();
        assert_eq!(
            tx.commit().unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO revision_parents(revision_id,parent_order,parent_revision_id) VALUES(?1,0,?2)",
            params![RevisionId::new().to_string(),other.id.to_string()]).unwrap();
        assert_eq!(
            tx.commit().unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }

    #[test]
    fn node_usage_tracks_atomic_updates_and_reopen() {
        let mut connection = test_database();
        let (branch, _) = create_branch(&mut connection);
        let first = node("/Src/CAFÉ", 2, 5);
        let transaction = connection.transaction().unwrap();
        node_index::replace(&transaction, branch, std::slice::from_ref(&first)).unwrap();
        transaction.commit().unwrap();
        let parent: String = connection
            .query_row("SELECT parent FROM node_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(parent, "/src");
        let usage = |db: &Connection| -> (i64, i64) {
            db.query_row("SELECT inodes,bytes FROM branch_node_views", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
        };
        assert_eq!(usage(&connection), (1, 5));
        let tx = connection.transaction().unwrap();
        let updated = node("/Src/CAFÉ", 2, 9);
        node_index::change(&tx, branch, &updated.path, Some(&updated)).unwrap();
        let other = node("/other", 3, 7);
        node_index::change(&tx, branch, &other.path, Some(&other)).unwrap();
        assert_eq!(usage(&tx), (2, 16));
        let duplicate = node("/duplicate", 3, 99);
        assert!(node_index::change(&tx, branch, &duplicate.path, Some(&duplicate)).is_err());
        tx.rollback().unwrap();
        assert_eq!(usage(&connection), (1, 5));
        let tx = connection.transaction().unwrap();
        node_index::change(&tx, branch, &first.path, None).unwrap();
        let new = node("/new", 4, 12);
        node_index::change(&tx, branch, &new.path, Some(&new)).unwrap();
        tx.commit().unwrap();
        initialize(&mut connection).unwrap();
        assert_eq!(usage(&connection), (1, 12));
    }
}
