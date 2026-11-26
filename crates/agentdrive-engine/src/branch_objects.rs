use crate::trees::{directory_key, inode_key};
use crate::{Engine, runtime::increment};
use agentdrive_model::*;
use agentdrive_ports::{LocalCommit, NodeChange, RevisionReader, TreeChanges, TreeEditor};
use bytes::Bytes;
use std::collections::{BTreeMap, BTreeSet};

/// A whole-object update for transports that publish immutable file contents.
pub enum BranchObjectChange {
    Put(Bytes),
    Directory,
    Delete(FileKind, ObjectConditions),
}

#[derive(Debug, Default)]
pub struct ObjectConditions {
    /// An asterisk requires that the path exists.
    pub etag: Option<String>,
    pub size: Option<u64>,
}

impl ObjectConditions {
    fn check(&self, inode: Option<&Inode>) -> Result<()> {
        let matches_etag = self.etag.as_deref().is_none_or(|tag| {
            inode.is_some_and(|inode| {
                tag == "*"
                    || if inode.kind == FileKind::Directory {
                        object_ref(ObjectKind::File, &[]).id.as_str() == tag
                    } else {
                        inode
                            .content
                            .as_ref()
                            .is_some_and(|object| object.id.as_str() == tag)
                    }
            })
        });
        if !matches_etag
            || inode.is_some_and(|inode| self.size.is_some_and(|size| size != inode.size))
        {
            return Err(Error::new(
                ErrorCode::PreconditionFailed,
                "object delete condition did not match",
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for BranchObjectChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Put(data) => f.debug_tuple("Put").field(&data.len()).finish(),
            Self::Directory => write!(f, "Directory"),
            Self::Delete(kind, _) => f.debug_tuple("Delete").field(kind).finish(),
        }
    }
}

impl Engine {
    /// Publish contents, parent directories and the restartable tree root in one local commit.
    pub async fn update_branch_object(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch_id: BranchId,
        path: WorkspacePath,
        change: BranchObjectChange,
    ) -> Result<Option<Inode>> {
        if path.is_root() {
            return Err(Error::invalid("cannot replace the branch root"));
        }
        let _branch_gate = self.runtime.lock_branch(branch_id).await;
        self.runtime
            .authorize(actor, workspace, Some(branch_id), Permission::Write)
            .await?;
        let mut branch = self.runtime.branch(branch_id).await?;
        branch.check_write(self.runtime.location, branch.authority_epoch)?;
        self.runtime.check_mutation(branch_id)?;
        self.runtime.check_sandbox(branch_id)?;
        if branch.saved_seq != branch.mutation_seq
            || self
                .runtime
                .state
                .collaboration(branch_id)
                .await?
                .is_some_and(|c| c.settings.enabled)
            || self.runtime.state.mounts().await?.iter().any(|m| {
                m.branch == branch_id
                    && m.access == AccessMode::ReadWrite
                    && m.state != MountState::Released
            })
        {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "branch has another writable working view",
            ));
        }
        let _objects = self.runtime.objects.read().await;
        let trees = &self.revisions.trees;
        // Only the target and its ancestors can change in a single-key publication.
        let mut tree = BTreeMap::new();
        let mut cursor = Some(path.clone());
        while let Some(current) = cursor {
            cursor = current.parent();
            if let Some(node) = self.runtime.state.node_at(branch_id, &current).await? {
                if node.path == current {
                    tree.insert(current, node.inode);
                } else if !matches!(change, BranchObjectChange::Delete(..)) {
                    return Err(Error::new(
                        ErrorCode::NameConflict,
                        "object name conflicts with the branch name policy",
                    ));
                }
            }
        }
        let mut edits = TreeChanges::default();
        let guard = BranchGuard::from(&branch);
        let mut changed = BTreeSet::new();
        let now = self.runtime.clock.now_ns();
        let (kind, new_size) = match &change {
            BranchObjectChange::Put(data) => (FileKind::File, data.len() as u64),
            BranchObjectChange::Directory => (FileKind::Directory, 0),
            BranchObjectChange::Delete(kind, _) => (*kind, 0),
        };
        if let BranchObjectChange::Delete(_, conditions) = &change {
            let target = tree.get(&path).filter(|inode| inode.kind == kind);
            conditions.check(target)?;
            let Some(inode) = target else {
                return Ok(None);
            };
            if kind == FileKind::Directory
                && !self
                    .runtime
                    .state
                    .children_page(branch_id, &path, None, 1)
                    .await?
                    .is_empty()
            {
                return Err(Error::new(
                    ErrorCode::DirectoryNotEmpty,
                    "directory still contains objects",
                ));
            }
            edits.inodes.insert(inode_key(inode.id), None);
            let parent = path
                .parent()
                .and_then(|p| tree.get(&p))
                .ok_or_else(|| Error::integrity("missing parent directory"))?;
            let entry = DirectoryEntry {
                parent: parent.id,
                name: path.name().into(),
                inode: inode.id,
            };
            edits.directories.insert(directory_key(&entry)?, None);
            tree.remove(&path);
            changed.insert(path.clone());
        } else {
            let initial_node_count = tree.len();
            let old_size = tree.get(&path).map_or(0, |inode| inode.size);
            let mut current = WorkspacePath::root();
            for component in path.as_str()[1..].split('/') {
                let parent = current;
                current = parent.join(component)?;
                let target_kind = if current == path {
                    kind
                } else {
                    FileKind::Directory
                };
                if let Some(inode) = tree.get(&current) {
                    if inode.kind != target_kind {
                        return Err(Error::new(
                            ErrorCode::NameConflict,
                            "file and directory share the same path",
                        ));
                    }
                    if current == path && kind == FileKind::Directory {
                        return Ok(Some(inode.clone()));
                    }
                    continue;
                }
                let parent_node = tree
                    .get_mut(&parent)
                    .ok_or_else(|| Error::integrity("missing parent directory"))?;
                parent_node.modified_ns = now;
                let inode = Inode {
                    id: InodeId(branch.next_inode),
                    kind: target_kind,
                    mode: if target_kind == FileKind::Directory {
                        0o755
                    } else {
                        0o644
                    },
                    uid: parent_node.uid,
                    gid: parent_node.gid,
                    created_ns: now,
                    modified_ns: now,
                    size: 0,
                    content: None,
                };
                branch.next_inode = increment(branch.next_inode)?;
                let entry = DirectoryEntry {
                    parent: parent_node.id,
                    name: current.name().into(),
                    inode: inode.id,
                };
                edits
                    .directories
                    .insert(directory_key(&entry)?, Some(entry));
                tree.insert(current.clone(), inode);
                changed.insert(parent);
                changed.insert(current.clone());
            }
            self.runtime
                .check_capacity(
                    &branch,
                    old_size,
                    new_size,
                    (tree.len() - initial_node_count) as u64,
                )
                .await?;
            if let BranchObjectChange::Put(data) = change {
                let object = trees
                    .content
                    .local
                    .put(workspace, ObjectKind::File, data)
                    .await?;
                if let Some(remote) = trees.content.remote(workspace) {
                    trees
                        .content
                        .upload_object(remote.as_ref(), workspace, &object)
                        .await?;
                    trees
                        .content
                        .local
                        .confirm_remote(workspace, &object)
                        .await?;
                }
                let inode = tree
                    .get_mut(&path)
                    .ok_or_else(|| Error::integrity("missing target file"))?;
                inode.modified_ns = now;
                inode.size = object.size;
                inode.content = Some(object);
                changed.insert(path.clone());
            }
        }
        if let Some(parent) = path.parent() {
            if let Some(inode) = tree.get_mut(&parent) {
                inode.modified_ns = now;
            }
            changed.insert(parent);
        }
        for key in &changed {
            if let Some(inode) = tree.get(key) {
                edits
                    .inodes
                    .insert(inode_key(inode.id), Some(inode.clone()));
            }
        }
        let batch = trees.content.local.begin_batch(workspace).await?;
        branch.working_root = trees
            .edit_tree(workspace, &branch.working_root, edits, batch.as_ref())
            .await?;
        batch.finish().await?;
        branch.mutation_seq = increment(branch.mutation_seq)?;
        branch.saved_seq = branch.mutation_seq;
        branch.dirty =
            branch.working_root != trees.revision(workspace, branch.formal_head).await?.root;
        let node_changes = changed
            .into_iter()
            .map(|path| match tree.get(&path) {
                Some(inode) => NodeChange::Put {
                    branch: branch_id,
                    node: Node {
                        path,
                        inode: inode.clone(),
                        dirty: false,
                    },
                },
                None => NodeChange::Remove {
                    branch: branch_id,
                    path,
                },
            })
            .collect();
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![guard],
                branches: vec![branch.clone()],
                checkpointed_branches: vec![branch_id],
                node_changes,
                ..Default::default()
            })
            .await?;
        self.runtime.mark_dirty(&branch);
        Ok(tree.remove(&path))
    }
}
