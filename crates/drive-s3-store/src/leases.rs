//! SQLite records ownership. A per-branch file lock orders transfers and publications
//! across connections without holding a database write transaction during file I/O.
use super::*;
use rusqlite::{Connection, OptionalExtension, params};
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
    pub binding: SessionStatus,
}

impl WriteReservation for WriteGuard {
    fn session(&self) -> &SessionStatus {
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
        db.run(|c| {
            c.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS drive_branches(
                branch_id TEXT PRIMARY KEY, epoch INTEGER NOT NULL DEFAULT 0,
                serialized INTEGER NOT NULL DEFAULT 0 CHECK(serialized IN (0,1)),
                holder TEXT UNIQUE REFERENCES drive_sessions(bucket));
            CREATE TABLE IF NOT EXISTS drive_sessions(
                bucket TEXT PRIMARY KEY, workspace TEXT NOT NULL, branch TEXT NOT NULL,
                workspace_id TEXT NOT NULL, branch_id TEXT NOT NULL REFERENCES drive_branches(branch_id),
                retired INTEGER NOT NULL DEFAULT 0, successor TEXT REFERENCES drive_sessions(bucket));")?;
            let has_mode: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('drive_branches') WHERE name='serialized')",
                [], |row| row.get(0)
            )?;
            if !has_mode {
                c.execute("ALTER TABLE drive_branches ADD COLUMN serialized INTEGER NOT NULL DEFAULT 0 CHECK(serialized IN (0,1))", [])?;
            }
            Ok(())
        }).await?;
        Ok(db)
    }
}

#[async_trait::async_trait]
impl SessionRepository for Database {
    async fn bind(
        &self,
        bucket: &str,
        workspace: &str,
        branch: &str,
        w: WorkspaceId,
        b: BranchId,
        mode: WriteMode,
    ) -> Result<SessionStatus> {
        let (bucket, workspace, branch) =
            (bucket.to_owned(), workspace.to_owned(), branch.to_owned());
        self.run(move |c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            c.execute(
                "INSERT INTO drive_branches(branch_id,serialized) VALUES (?1,?2) ON CONFLICT(branch_id) DO NOTHING",
                params![b.to_string(), mode == WriteMode::Serialized],
            )?;
            c.execute(
                "INSERT INTO drive_sessions(bucket,workspace,branch,workspace_id,branch_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(bucket) DO NOTHING",
                params![bucket, workspace, branch, w.to_string(), b.to_string()],
            )?;
            let result = status(&c, &bucket)?;
            if result.branch_id != b || result.workspace_id != w || result.write_mode != mode {
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
    ) -> Result<Vec<SessionStatus>> {
        self.run(move |c| {
            let mut query = c.prepare(
                "SELECT bucket FROM drive_sessions WHERE workspace_id=?1 AND branch_id=?2 ORDER BY bucket"
            )?;
            query.query_map(params![workspace.to_string(), branch.to_string()], |row| row.get::<_, String>(0))?
                .map(|bucket| status(&c, &bucket?)).collect()
        }).await
    }

    async fn status(&self, bucket: &str) -> Result<SessionStatus> {
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

    async fn change(&self, bucket: &str, change: SessionChange) -> Result<SessionStatus> {
        let branch = self.status(bucket).await?.branch_id;
        let reservation = self.reserve(branch).await?;
        let bucket = bucket.to_owned();
        self.run(move |c| {
            let _reservation = reservation;
            c.execute_batch("BEGIN IMMEDIATE")?;
            let previous = status(&c, &bucket)?;
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

fn status(c: &Connection, bucket: &str) -> Result<SessionStatus> {
    let (workspace, branch, w, b, epoch, holder, retired, serialized): (
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        bool,
        bool,
    ) = c
        .query_row(
            "SELECT s.workspace,s.branch,s.workspace_id,s.branch_id,a.epoch,a.holder,s.retired,a.serialized
        FROM drive_sessions s JOIN drive_branches a ON a.branch_id=s.branch_id WHERE s.bucket=?1",
            [bucket],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::new(ErrorKind::NoSuchBucket, "unknown mount session"))?;
    Ok(SessionStatus {
        bucket: bucket.into(),
        workspace,
        branch,
        workspace_id: w.parse()?,
        branch_id: b.parse()?,
        epoch: u64::try_from(epoch)
            .map_err(|_| Error::new(ErrorKind::Internal, "invalid lock epoch"))?,
        write_mode: if serialized {
            WriteMode::Serialized
        } else {
            WriteMode::Exclusive
        },
        writable: !retired && (serialized || holder.as_deref() == Some(bucket)),
        holder,
        retired,
    })
}
