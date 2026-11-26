//! SQLite records ownership. A per-branch file lock orders transfers and publications
//! across connections without holding a database write transaction during file I/O.
use super::*;
use rusqlite::{Connection, Row, TransactionBehavior, params};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub(super) struct Database(Arc<PathBuf>);
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        let kind = match e.sqlite_error_code() {
            Some(
                rusqlite::ErrorCode::DatabaseBusy
                | rusqlite::ErrorCode::DatabaseLocked
                | rusqlite::ErrorCode::ConstraintViolation,
            ) => ErrorKind::Conflict,
            _ => ErrorKind::Internal,
        };
        Error::new(kind, e.to_string())
    }
}

pub(super) struct WriteGuard {
    _reservation: std::fs::File,
    pub binding: MountSession,
}

impl WriteReservation for WriteGuard {
    fn session(&self) -> &MountSession {
        &self.binding
    }
}

impl Database {
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let path = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(path.as_ref())?;
            conn.busy_timeout(Duration::from_secs(30))?;
            conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
            f(conn)
        })
        .await
        .map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?
    }

    async fn reserve(&self, branch: BranchId) -> Result<std::fs::File> {
        let mut filename = self.0.as_os_str().to_owned();
        filename.push(format!(".{branch}.lock"));
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(filename)
            .await
            .map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?
            .into_std()
            .await;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(Error::new(ErrorKind::Internal, e.to_string()));
                }
            }
        }
    }

    pub async fn open(path: &Path) -> Result<Self> {
        let db = Self(Arc::new(path.into()));
        db.run(initialize).await?;
        Ok(db)
    }
}

const SESSION_SCHEMA_VERSION: i32 = 2;

fn initialize(mut db: Connection) -> Result<()> {
    // The write reservation serializes concurrent opens of a new, empty file.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == SESSION_SCHEMA_VERSION || version == 1 {
        check_session_schema(&tx, version)?;
        if version == 1 {
            tx.execute_batch("ALTER TABLE drive_sessions ADD COLUMN revision TEXT;
                              ALTER TABLE drive_sessions ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0 CHECK(read_only IN (0,1))")?;
            tx.pragma_update(None, "user_version", SESSION_SCHEMA_VERSION)?;
        }
    } else {
        let empty: bool = tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sqlite_schema)",
            [],
            |row| row.get(0),
        )?;
        if version != 0 || !empty {
            return Err(Error::new(
                ErrorKind::Invalid,
                format!(
                    "unsupported S3 session database schema version {version}, expected {SESSION_SCHEMA_VERSION}"
                ),
            ));
        }
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.pragma_update(None, "user_version", SESSION_SCHEMA_VERSION)?;
    }
    tx.commit()?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    Ok(())
}

fn check_session_schema(db: &Connection, version: i32) -> Result<()> {
    // Check the application's tables even if another database uses the same SQLite user_version.
    for (table, required) in [
        (
            "drive_branches",
            &["branch_id", "epoch", "holder", "serialized", "workspace_id"][..],
        ),
        (
            "drive_sessions",
            if version == 1 {
                &["branch_id", "bucket", "retired", "successor"][..]
            } else {
                &[
                    "branch_id",
                    "bucket",
                    "read_only",
                    "retired",
                    "revision",
                    "successor",
                ][..]
            },
        ),
    ] {
        let mut query = db.prepare(
            "SELECT p.name FROM sqlite_schema s JOIN pragma_table_info(s.name) p
             WHERE s.type='table' AND s.name=?1 ORDER BY p.name",
        )?;
        let columns = query
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns != required {
            return Err(Error::new(
                ErrorKind::Invalid,
                format!(
                    "unsupported S3 session database layout for {table} in schema version {version}"
                ),
            ));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionRepository for Database {
    async fn bind(
        &self,
        bucket: &str,
        workspace: WorkspaceId,
        branch: BranchId,
        mode: WriteMode,
        revision: Option<RevisionId>,
        read_only: bool,
    ) -> Result<MountSession> {
        let bucket = bucket.to_owned();
        self.run(move |c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            c.execute(
                "INSERT INTO drive_branches(branch_id,workspace_id,serialized) VALUES (?1,?2,?3) ON CONFLICT(branch_id) DO NOTHING",
                params![branch.to_string(), workspace.to_string(), mode == WriteMode::Serialized],
            )?;
            c.execute(
                "INSERT INTO drive_sessions(bucket,branch_id,revision,read_only) VALUES(?1,?2,?3,?4) ON CONFLICT(bucket) DO NOTHING",
                params![bucket, branch.to_string(), revision.map(|id| id.to_string()), read_only],
            )?;
            let result = status(&c, &bucket)?;
            if result.branch_id != branch || result.workspace_id != workspace || (!read_only && result.write_mode != mode) || result.revision != revision || result.read_only != read_only {
                return Err(Error::new(ErrorKind::Conflict, "existing binding or branch write mode differs"));
            }
            c.execute_batch("COMMIT")?;
            Ok(result)
        }).await
    }

    async fn branch_mounts(
        &self,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Vec<MountSession>> {
        self.run(move |c| {
            let mut query = c.prepare(&format!(
                "{SESSION_SELECT} WHERE a.workspace_id=?1 AND a.branch_id=?2 ORDER BY s.bucket"
            ))?;
            let mut rows = query.query(params![workspace.to_string(), branch.to_string()])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(session(row)?);
            }
            Ok(sessions)
        })
        .await
    }

    async fn status(&self, bucket: &str) -> Result<MountSession> {
        let bucket = bucket.to_owned();
        self.run(move |c| status(&c, &bucket)).await
    }
    async fn guard(&self, bucket: &str) -> Result<Box<dyn WriteReservation>> {
        let branch = self.status(bucket).await?.branch_id;
        let reservation = self.reserve(branch).await?;
        // Bindings are immutable. Recheck ownership after preceding transfers finish.
        let binding = self.status(bucket).await?;
        binding.require_writer()?;
        Ok(Box::new(WriteGuard {
            _reservation: reservation,
            binding,
        }))
    }

    async fn change(&self, bucket: &str, change: SessionChange) -> Result<MountSession> {
        let branch = self.status(bucket).await?.branch_id;
        let reservation = self.reserve(branch).await?;
        let bucket = bucket.to_owned();
        self.run(move |c| {
            let _reservation = reservation;
            c.execute_batch("BEGIN IMMEDIATE")?;
            let previous = status(&c, &bucket)?;
            if previous.read_only {
                return Err(Error::new(ErrorKind::AccessDenied, "read-only mount"));
            }
            if previous.write_mode == WriteMode::Serialized {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "this branch serializes each write without a designated holder",
                ));
            }
            let target = match change {
                SessionChange::Acquire => {
                    if previous.retired {
                        return Err(Error::new(
                            ErrorKind::AccessDenied,
                            "mount session has been fenced",
                        ));
                    }
                    if let Some(ref holder) = previous.holder {
                        if holder != &bucket {
                            return Err(Error::new(
                                ErrorKind::Conflict,
                                "branch already has a writer",
                            ));
                        }
                    } else {
                        set_holder(&c, previous.branch_id, Some(&bucket))?;
                    }
                    bucket
                }
                SessionChange::Release => {
                    if !previous.retired {
                        previous.require_writer()?;
                        retire(&c, &bucket, None)?;
                        set_holder(&c, previous.branch_id, None)?;
                    }
                    bucket
                }
                SessionChange::Handoff(to) => {
                    let next = status(&c, &to)?;
                    if bucket == to
                        || previous.branch_id != next.branch_id
                        || previous.workspace_id != next.workspace_id
                        || next.read_only
                        || next.retired
                    {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            "handoff requires another live session on the same branch",
                        ));
                    }
                    if previous.retired && next.writable {
                        let successor: Option<String> = c.query_row(
                            "SELECT successor FROM drive_sessions WHERE bucket=?1",
                            [&bucket],
                            |r| r.get(0),
                        )?;
                        if successor.as_ref() == Some(&to) {
                            return Ok(next);
                        }
                    }
                    previous.require_writer()?;
                    retire(&c, &bucket, Some(&to))?;
                    set_holder(&c, previous.branch_id, Some(&to))?;
                    to
                }
            };
            let result = status(&c, &target)?;
            c.execute_batch("COMMIT")?;
            Ok(result)
        })
        .await
    }
}

fn set_holder(c: &Connection, branch: BranchId, bucket: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE drive_branches SET holder=?1, epoch=epoch+1 WHERE branch_id=?2",
        params![bucket, branch.to_string()],
    )?;
    Ok(())
}

fn retire(c: &Connection, bucket: &str, next: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE drive_sessions SET retired=1,successor=?1 WHERE bucket=?2",
        params![next, bucket],
    )?;
    Ok(())
}

const SESSION_SELECT: &str =
    "SELECT s.bucket,a.workspace_id,s.branch_id,a.epoch,a.holder,s.retired,a.serialized,s.revision,s.read_only
    FROM drive_sessions s JOIN drive_branches a ON a.branch_id=s.branch_id";

fn status(c: &Connection, bucket: &str) -> Result<MountSession> {
    let mut query = c.prepare(&format!("{SESSION_SELECT} WHERE s.bucket=?1"))?;
    let mut rows = query.query([bucket])?;
    session(
        rows.next()?
            .ok_or_else(|| Error::new(ErrorKind::NoSuchBucket, "unknown mount session"))?,
    )
}

fn session(row: &Row<'_>) -> Result<MountSession> {
    let bucket: String = row.get(0)?;
    let holder: Option<String> = row.get(4)?;
    let retired: bool = row.get(5)?;
    let serialized: bool = row.get(6)?;
    Ok(MountSession {
        workspace_id: row.get::<_, String>(1)?.parse()?,
        branch_id: row.get::<_, String>(2)?.parse()?,
        epoch: u64::try_from(row.get::<_, i64>(3)?)
            .map_err(|_| Error::new(ErrorKind::Internal, "invalid lock epoch"))?,
        write_mode: if serialized {
            WriteMode::Serialized
        } else {
            WriteMode::Exclusive
        },
        revision: row
            .get::<_, Option<String>>(7)?
            .map(|s| s.parse())
            .transpose()?,
        read_only: row.get(8)?,
        writable: !row.get::<_, bool>(8)?
            && !retired
            && (serialized || holder.as_ref() == Some(&bucket)),
        bucket,
        holder,
        retired,
    })
}
