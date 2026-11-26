use crate::{Runtime, files::read_content};
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

const PAGE_SIZE: usize = 200;
const READ_LIMIT: u32 = 256 * 1024;

pub(crate) struct BranchFileService {
    runtime: Arc<Runtime>,
    content: Arc<dyn ContentResolver>,
    working: Arc<dyn WorkingTreeStore>,
    queries: Arc<dyn TreeQuery>,
}

impl std::fmt::Debug for BranchFileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchFileService").finish_non_exhaustive()
    }
}

impl BranchFileService {
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        content: Arc<dyn ContentResolver>,
        working: Arc<dyn WorkingTreeStore>,
        queries: Arc<dyn TreeQuery>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            content,
            working,
            queries,
        })
    }

    async fn authorize(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        expected: Option<&str>,
    ) -> Result<String> {
        let (_, branch) = self.readable_branch(actor, workspace, branch).await?;
        let version = format!(
            "{}:{}:{}:{}",
            branch.id, branch.authority_epoch, branch.generation, branch.mutation_seq
        );
        if expected.is_some_and(|expected| expected != version) {
            return Err(Error::new(
                ErrorCode::TargetMoved,
                "Branch contents changed; refresh the file browser.",
            ));
        }
        Ok(version)
    }

    async fn readable_branch(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<(Workspace, Branch)> {
        let (record, branch) = self
            .runtime
            .authorize_branch(actor, workspace, branch, Permission::Read)
            .await?;
        if branch.state == BranchState::Deleted {
            return Err(Error::new(ErrorCode::NotFound, "branch has been deleted"));
        }
        Ok((record, branch))
    }

    async fn node(&self, branch: BranchId, path: &WorkspacePath) -> Result<Node> {
        self.runtime
            .state
            .node_at(branch, path)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "path does not exist"))
    }
}

#[async_trait]
impl BranchFilesApi for BranchFileService {
    async fn list(&self, actor: &Principal, request: ListBranchFiles) -> Result<BranchFiles> {
        let _gate = self.runtime.lock_branch(request.branch).await;
        let version = self
            .authorize(
                actor,
                request.workspace,
                request.branch,
                request.version.as_deref(),
            )
            .await?;
        if request.after.is_some() && request.version.is_none() {
            return Err(Error::invalid("directory pagination requires a version"));
        }
        if request.after.as_ref().is_some_and(|after| {
            after.parent().map(|parent| parent.lookup_key()) != Some(request.path.lookup_key())
        }) {
            return Err(Error::invalid(
                "cursor must be a direct child of the directory",
            ));
        }
        let node = self.node(request.branch, &request.path).await?;
        if node.inode.kind != FileKind::Directory {
            return Err(Error::invalid("path is not a directory"));
        }
        let mut entries = self
            .runtime
            .state
            .children_page(
                request.branch,
                &request.path,
                request.after.as_ref(),
                PAGE_SIZE + 1,
            )
            .await?;
        let next = if entries.len() > PAGE_SIZE {
            entries.truncate(PAGE_SIZE);
            entries.last().map(|node| node.path.clone())
        } else {
            None
        };
        Ok(BranchFiles {
            version,
            entries,
            next,
        })
    }

    async fn read(&self, actor: &Principal, request: ReadBranchFile) -> Result<Bytes> {
        if request.size == 0 || request.size > READ_LIMIT {
            return Err(Error::invalid(
                "file reads must contain between 1 and 262144 bytes",
            ));
        }
        let gate = self.runtime.lock_branch(request.branch).await;
        self.authorize(
            actor,
            request.workspace,
            request.branch,
            Some(&request.version),
        )
        .await?;
        let node = self.node(request.branch, &request.path).await?;
        if node.inode.kind != FileKind::File {
            return Err(Error::invalid("path is not a file"));
        }
        if request.offset >= node.inode.size {
            return Ok(Bytes::new());
        }
        if node.dirty {
            return self
                .working
                .read(
                    WorkingFile {
                        workspace: request.workspace,
                        branch: request.branch,
                        inode: node.inode.id,
                    },
                    request.offset,
                    request.size,
                )
                .await;
        }
        let _objects = self.runtime.objects.read().await;
        // The immutable object remains protected while a remote read is in flight.
        drop(gate);
        read_content(
            self.content.as_ref(),
            request.workspace,
            node.inode
                .content
                .as_ref()
                .ok_or_else(|| Error::integrity("file content is missing"))?,
            request.offset,
            request.size,
        )
        .await
    }
}

/// An open read is tied to the selected file contents throughout the transfer.
pub struct BranchFileReader {
    pub node: Node,
    pub version: String,
    source: FileSource,
}

impl std::fmt::Debug for BranchFileReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchFileReader")
            .field("path", &self.node.path)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

enum FileSource {
    Empty,
    Stored {
        content: Arc<dyn ContentResolver>,
        workspace: WorkspaceId,
        object: ObjectRef,
        _retention: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Working {
        store: Arc<dyn WorkingTreeStore>,
        file: WorkingFile,
        _branch: tokio::sync::OwnedMutexGuard<()>,
    },
}

impl crate::Engine {
    pub async fn revision_tree(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        revision: RevisionId,
    ) -> Result<FileTree> {
        let (record, _) = self
            .branch_files
            .readable_branch(actor, workspace, branch)
            .await?;
        let _retention = self.runtime.objects.read().await;
        let trees = &self.revisions.trees;
        let revision = trees.readable_revision(actor, &record, revision).await?;
        trees.tree(workspace, &revision.root).await
    }

    pub async fn open_revision_file(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        revision: RevisionId,
        path: &WorkspacePath,
    ) -> Result<BranchFileReader> {
        let (record, _) = self
            .branch_files
            .readable_branch(actor, workspace, branch)
            .await?;
        let retention = self.runtime.objects.clone().read_owned().await;
        let trees = &self.revisions.trees;
        let revision = trees.readable_revision(actor, &record, revision).await?;
        let node = self
            .branch_files
            .queries
            .lookup_node(workspace, &revision.root, path)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "path does not exist in revision"))?;
        let source = if node.inode.kind == FileKind::Directory {
            FileSource::Empty
        } else {
            FileSource::Stored {
                content: self.branch_files.content.clone(),
                workspace,
                object: node
                    .inode
                    .content
                    .clone()
                    .ok_or_else(|| Error::integrity("file content is missing"))?,
                _retention: retention,
            }
        };
        Ok(BranchFileReader {
            node,
            version: revision.id.to_string(),
            source,
        })
    }

    pub async fn open_branch_file(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        path: &WorkspacePath,
    ) -> Result<BranchFileReader> {
        let service = &self.branch_files;
        let gate = self.runtime.lock_branch(branch).await;
        let version = service.authorize(actor, workspace, branch, None).await?;
        let node = service.node(branch, path).await?;
        let source = if node.inode.kind == FileKind::Directory {
            FileSource::Empty
        } else if node.dirty {
            FileSource::Working {
                store: service.working.clone(),
                file: WorkingFile {
                    workspace,
                    branch,
                    inode: node.inode.id,
                },
                _branch: gate,
            }
        } else {
            FileSource::Stored {
                content: service.content.clone(),
                workspace,
                object: node
                    .inode
                    .content
                    .clone()
                    .ok_or_else(|| Error::integrity("file content is missing"))?,
                _retention: self.runtime.objects.clone().read_owned().await,
            }
        };
        Ok(BranchFileReader {
            node,
            version,
            source,
        })
    }
}

impl BranchFileReader {
    pub async fn read(&self, offset: u64, size: u32) -> Result<Bytes> {
        if size > READ_LIMIT {
            return Err(Error::invalid("file read exceeds chunk size"));
        }
        match &self.source {
            FileSource::Empty => Ok(Bytes::new()),
            FileSource::Stored {
                content,
                workspace,
                object,
                ..
            } => read_content(content.as_ref(), *workspace, object, offset, size).await,
            FileSource::Working { store, file, .. } => store.read(*file, offset, size).await,
        }
    }
}
