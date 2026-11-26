//! Each branch has a mutable layer whose parent is pinned to an immutable version.
//! Changed paths shadow the same paths in older layers, including deletions.
use crate::{
    LocalBackend,
    sqlite::{db_integer, read_many, read_one, sql_error},
};
use agentdrive_model::*;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::Weak;

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(include_str!("node_layers.sql"))
        .map_err(sql_error)
}

pub(super) fn schedule_compaction(db: &Connection) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO metadata(key,value) VALUES('node_compaction_pending',1)",
        [],
    )
    .map_err(sql_error)?;
    Ok(())
}

const LINEAGE: &str = "WITH RECURSIVE lineage(id,version,depth) AS (
    SELECT l.id,l.version,0 FROM branch_node_views b JOIN node_layers l ON l.id=b.layer WHERE b.branch_id=?1
    UNION ALL
    SELECT p.id,l.parent_version,a.depth+1 FROM lineage a JOIN node_layers l ON l.id=a.id JOIN node_layers p ON p.id=l.parent
)";

// Keep the short list of ancestor layers on the outside of both indexed path lookups.
fn query(predicate: &str, tail: &str) -> String {
    format!("{LINEAGE}
        SELECT n.data FROM lineage a CROSS JOIN node_versions n ON n.layer=a.id
        WHERE ({predicate}) AND n.data IS NOT NULL AND n.since<=a.version AND (n.until IS NULL OR n.until>a.version)
        AND NOT EXISTS(SELECT 1 FROM lineage b CROSS JOIN node_versions h ON h.layer=b.id AND h.path=n.path
          WHERE b.depth<a.depth AND h.since<=b.version AND (h.until IS NULL OR h.until>b.version))
        {tail}")
}

pub(super) fn one(db: &Connection, predicate: &str, args: &[String]) -> Result<Option<Node>> {
    read_one(db, &query(predicate, "LIMIT 1"), args)
}

pub(super) fn many(
    db: &Connection,
    predicate: &str,
    tail: &str,
    args: &[String],
) -> Result<Vec<Node>> {
    read_many(db, &query(predicate, tail), args)
}

fn layer(db: &Connection, parent: Option<(i64, i64)>) -> Result<i64> {
    db.query_row(
        "INSERT INTO node_layers(parent,parent_version) VALUES(?1,?2) RETURNING id",
        params![parent.map(|p| p.0), parent.map(|p| p.1)],
        |r| r.get(0),
    )
    .map_err(sql_error)
}

fn insert(
    db: &Connection,
    layer: i64,
    version: i64,
    path: &WorkspacePath,
    node: Option<&Node>,
) -> Result<()> {
    db.execute(
        "INSERT INTO node_versions(layer,path,since,parent,inode,data) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            layer,
            path.lookup_key(),
            version,
            path.parent().map(|p| p.lookup_key()),
            node.map(|n| db_integer(n.inode.id.0)).transpose()?,
            node.map(encode).transpose()?
        ],
    )
    .map_err(sql_error)?;
    Ok(())
}

pub(super) fn change(
    tx: &Transaction<'_>,
    branch: BranchId,
    path: &WorkspacePath,
    node: Option<&Node>,
) -> Result<()> {
    let b = branch.to_string();
    let before = one(tx, "n.path=?2", &[b.clone(), path.lookup_key()])?;
    if let Some(node) = node
        && before.as_ref().map(|previous| previous.inode.id) != Some(node.inode.id)
        && one(tx, "n.inode=?2", &[b.clone(), node.inode.id.0.to_string()])?.is_some()
    {
        return Err(Error::new(
            ErrorCode::Conflict,
            "inode already exists in this branch",
        ));
    }
    tx.execute(
        "INSERT OR IGNORE INTO node_deltas(branch_id,path,previous) VALUES(?1,?2,?3)",
        params![
            b,
            path.lookup_key(),
            before.as_ref().map(encode).transpose()?
        ],
    )
    .map_err(sql_error)?;
    let (id, version): (i64, i64) = tx.query_row(
        "UPDATE node_layers SET version=version+1 WHERE id=(SELECT layer FROM branch_node_views WHERE branch_id=?1)
         RETURNING id,version", [&b],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(sql_error)?;
    tx.execute(
        "UPDATE node_versions SET until=?1 WHERE layer=?2 AND path=?3 AND until IS NULL",
        params![version, id, path.lookup_key()],
    )
    .map_err(sql_error)?;
    insert(tx, id, version, path, node)?;
    let bytes = node.map_or(0, |n| n.inode.size);
    let old_bytes = before.as_ref().map_or(0, |n| n.inode.size);
    tx.execute(
        "UPDATE branch_node_views SET inodes=inodes+?1,bytes=bytes+?2 WHERE branch_id=?3",
        params![
            i64::from(node.is_some()) - i64::from(before.is_some()),
            db_integer(bytes)? - db_integer(old_bytes)?,
            b
        ],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn bind(
    db: &Connection,
    branch: BranchId,
    id: i64,
    count: Option<i64>,
    bytes: Option<i64>,
) -> Result<()> {
    db.execute("INSERT INTO branch_node_views(branch_id,layer,inodes,bytes) VALUES(?1,?2,?3,?4)
        ON CONFLICT(branch_id) DO UPDATE SET layer=excluded.layer,inodes=excluded.inodes,bytes=excluded.bytes",
        params![branch.to_string(),id,count,bytes]).map_err(sql_error)?;
    db.execute(
        "DELETE FROM node_deltas WHERE branch_id=?1",
        [branch.to_string()],
    )
    .map_err(sql_error)?;
    Ok(())
}

pub(super) fn share(
    db: &Connection,
    branch: BranchId,
    workspace: WorkspaceId,
    root: &ObjectRef,
) -> Result<()> {
    type View = (i64, i64, Option<i64>, Option<i64>);
    let frozen: Option<View> = db.query_row(
        "SELECT layer,version,inodes,bytes FROM saved_node_views WHERE workspace_id=?1 AND hash=?2",
        params![workspace.to_string(),root.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional().map_err(sql_error)?;
    let (parent, version, count, bytes) = match frozen {
        Some(v) => v,
        None => {
            // Imported revisions may not have a local working index yet. Build it once on first access.
            let parent = layer(db, None)?;
            db.execute("INSERT INTO saved_node_views(workspace_id,hash,root,layer,version) VALUES(?1,?2,?3,?4,0)",
                params![workspace.to_string(),root.id.as_str(),encode(root)?,parent]).map_err(sql_error)?;
            (parent, 0, None, None)
        }
    };
    bind(
        db,
        branch,
        layer(db, Some((parent, version)))?,
        count,
        bytes,
    )
}

pub(super) fn replace(db: &Connection, branch: BranchId, nodes: &[Node]) -> Result<()> {
    let id = layer(db, None)?;
    let mut bytes = 0i64;
    for node in nodes {
        bytes = bytes
            .checked_add(db_integer(node.inode.size)?)
            .ok_or_else(|| Error::integrity("tree byte count overflow"))?;
        insert(db, id, 0, &node.path, Some(node))?;
    }
    bind(
        db,
        branch,
        id,
        Some(db_integer(nodes.len() as u64)?),
        Some(bytes),
    )
}

pub(super) fn save(db: &Connection, branch: &Branch) -> Result<()> {
    if branch.saved_seq == branch.mutation_seq {
        db.execute("INSERT OR IGNORE INTO saved_node_views(workspace_id,hash,root,layer,version,inodes,bytes)
            SELECT ?1,?2,?3,l.id,l.version,b.inodes,b.bytes FROM branch_node_views b JOIN node_layers l ON l.id=b.layer WHERE b.branch_id=?4",
            params![branch.workspace.to_string(),branch.working_root.id.as_str(),encode(&branch.working_root)?,branch.id.to_string()]).map_err(sql_error)?;
    }
    Ok(())
}

fn pending_base(
    db: &Connection,
    branch: BranchId,
) -> Result<Option<(WorkspaceId, ObjectRef, i64)>> {
    let record: Option<(String,Vec<u8>,i64)> = db.query_row(
        "SELECT s.workspace_id,s.root,s.layer FROM branch_node_views b JOIN node_layers l ON l.id=b.layer
         JOIN saved_node_views s ON s.layer=l.parent AND s.version=l.parent_version WHERE b.branch_id=?1 AND b.inodes IS NULL",
        [branch.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql_error)?;
    record
        .map(|(workspace, root, id)| Ok((workspace.parse()?, decode(&root)?, id)))
        .transpose()
}

impl LocalBackend {
    pub(super) async fn with_node_view<T, F>(&self, branch: BranchId, mut read: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        loop {
            // Readiness and the query use the same connection lock, including during a concurrent restore.
            let attempt = self
                .db(move |db| {
                    if pending_base(db, branch)?.is_some() {
                        Ok(Err(read))
                    } else {
                        read(db).map(Ok)
                    }
                })
                .await?;
            match attempt {
                Ok(value) => return Ok(value),
                Err(retry) => {
                    self.ensure_node_view(branch).await?;
                    read = retry;
                }
            }
        }
    }

    pub(super) async fn ensure_node_view(&self, branch: BranchId) -> Result<()> {
        if self.db(move |db| pending_base(db, branch)).await?.is_none() {
            return Ok(());
        }
        // Imported roots share one baseline. Only their first read needs to traverse the immutable tree.
        let _hydration = self.inner.node_hydration.lock().await;
        while let Some((workspace, root, id)) = self.db(move |db| pending_base(db, branch)).await? {
            let reader = self
                .inner
                .tree_reader
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or_else(|| Error::integrity("missing tree reader for imported revision"))?;
            let tree = reader.tree(workspace, &root).await?;
            self.db(move |db| {
                let tx = db.transaction().map_err(sql_error)?;
                let still_needed: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM saved_node_views WHERE layer=?1 AND inodes IS NULL)",
                    [id], |row| row.get(0),
                ).map_err(sql_error)?;
                if still_needed {
                    let count = db_integer(tree.len() as u64)?;
                    let mut bytes = 0i64;
                    for (path,inode) in tree {
                        bytes = bytes.checked_add(db_integer(inode.size)?)
                            .ok_or_else(|| Error::integrity("tree byte count overflow"))?;
                        let node = Node {path,inode,dirty:false};
                        insert(&tx,id,0,&node.path,Some(&node))?;
                    }
                    tx.execute("UPDATE saved_node_views SET inodes=?1,bytes=?2 WHERE layer=?3 AND version=0",
                        params![count,bytes,id]).map_err(sql_error)?;
                    tx.execute("UPDATE branch_node_views SET inodes=?1,bytes=?2 WHERE inodes IS NULL AND layer IN
                        (SELECT id FROM node_layers WHERE parent=?3 AND parent_version=0)",params![count,bytes,id]).map_err(sql_error)?;
                }
                tx.commit().map_err(sql_error)
            }).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_use_layer_path_parent_and_inode_indexes() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::sqlite::initialize(&mut db).unwrap();
        for (predicate, index) in [
            ("n.path=?2", "layer=? AND path=?"),
            ("n.parent=?2", "versioned_children"),
            ("n.inode=?2", "versioned_inodes"),
        ] {
            let plan = format!("EXPLAIN QUERY PLAN {}", query(predicate, "LIMIT 1"));
            let mut statement = db.prepare(&plan).unwrap();
            let steps = statement
                .query_map(["branch", "value"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(
                steps
                    .lines()
                    .any(|step| step.contains("SEARCH n USING INDEX") && step.contains(index)),
                "{steps}"
            );
            assert!(
                steps
                    .lines()
                    .any(|step| step.contains("SEARCH h USING")
                        && step.contains("layer=? AND path=?")),
                "{steps}"
            );
        }
    }
}
