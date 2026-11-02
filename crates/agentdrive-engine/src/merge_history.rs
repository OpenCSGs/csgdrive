use crate::Runtime;
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Debug)]
pub(crate) struct MergeHistoryService {
    runtime: Arc<Runtime>,
}

impl MergeHistoryService {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Arc<Self> {
        Arc::new(Self { runtime })
    }

    async fn workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        self.runtime
            .state
            .workspace(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))
    }

    async fn entry(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        record: &str,
    ) -> Result<MergeHistoryEntry> {
        validate_record(record)?;
        let workspace_record = self.workspace(workspace).await?;
        readable_branches(&workspace_record, actor)?;
        let entry = self
            .runtime
            .state
            .merge_history_entry(workspace, record)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "merge history does not exist"))?;
        authorize_entry(&workspace_record, actor, &entry)?;
        Ok(entry)
    }

    async fn revisions(
        &self,
        actor: &Principal,
        workspace: &Workspace,
        ids: impl IntoIterator<Item = RevisionId>,
    ) -> Result<Vec<Revision>> {
        let ids: BTreeSet<_> = ids.into_iter().collect();
        Ok(self
            .runtime
            .state
            .revision_metadata(workspace.id, ids.into_iter().collect())
            .await?
            .into_iter()
            .filter(|revision| {
                workspace
                    .authorize(actor, revision.branch, Permission::Read)
                    .is_ok()
            })
            .collect())
    }
}

#[async_trait]
impl MergeHistoryApi for MergeHistoryService {
    async fn list(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        mut filter: MergeHistoryFilter,
    ) -> Result<MergeHistoryPage> {
        let workspace_record = self.workspace(workspace).await?;
        let readable = readable_branches(&workspace_record, actor)?;
        for branch in [filter.branch, filter.source_branch, filter.target_branch]
            .into_iter()
            .flatten()
        {
            workspace_record.authorize(actor, Some(branch), Permission::Read)?;
        }
        if filter.from_ns.zip(filter.to_ns).is_some_and(|(a, b)| a > b) {
            return Err(Error::invalid(
                "merge history start must not follow its end",
            ));
        }
        let limit = page_size(filter.limit)?;
        let cursor = filter.cursor.take();
        filter.limit = None;
        let fingerprint = object_ref(
            ObjectKind::RecordIndex,
            &encode(&(actor, workspace, &filter, &readable))?,
        )
        .id;
        let before = decode_cursor(cursor, &fingerprint)?;
        let (mut entries, total) = self
            .runtime
            .state
            .merge_history_page(MergeHistoryScan {
                workspace,
                filter,
                readable_branches: readable,
                before,
                limit: limit + 1,
            })
            .await?;
        let more = entries.len() > limit;
        entries.truncate(limit);
        let next = more.then(|| {
            let last = entries.last().expect("nonempty bounded merge history page");
            format!("{fingerprint}:{}:{}", last.created_ns, last.id)
        });
        Ok(MergeHistoryPage {
            workspace,
            location: self.runtime.location,
            observed_ns: self.runtime.clock.now_ns(),
            entries,
            total,
            next,
        })
    }

    async fn detail(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        record: &str,
    ) -> Result<MergeHistoryDetail> {
        let entry = self.entry(actor, workspace, record).await?;
        let workspace_record = self.workspace(workspace).await?;
        let revisions = self
            .revisions(
                actor,
                &workspace_record,
                [
                    entry.source_revision,
                    entry.target_revision,
                    entry.base_revision,
                    entry.candidate_revision,
                    entry.applied_revision,
                ]
                .into_iter()
                .flatten(),
            )
            .await?;
        let events = self
            .events(actor, workspace, record, MergeEventsQuery::default())
            .await?;
        Ok(MergeHistoryDetail {
            entry,
            revisions,
            events,
            location: self.runtime.location,
            observed_ns: self.runtime.clock.now_ns(),
        })
    }

    async fn events(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        record: &str,
        query: MergeEventsQuery,
    ) -> Result<MergeHistoryEvents> {
        self.entry(actor, workspace, record).await?;
        let workspace_record = self.workspace(workspace).await?;
        let readable = readable_branches(&workspace_record, actor)?;
        let limit = page_size(query.limit)?;
        let fingerprint = object_ref(
            ObjectKind::RecordIndex,
            &encode(&(actor, workspace, record, &readable))?,
        )
        .id;
        let after = decode_cursor(query.cursor, &fingerprint)?;
        let (mut entries, total) = self
            .runtime
            .state
            .merge_history_events(MergeEventsScan {
                workspace,
                record: record.to_owned(),
                after,
                limit: limit + 1,
            })
            .await?;
        let more = entries.len() > limit;
        entries.truncate(limit);
        let next = more.then(|| {
            let last = entries.last().expect("nonempty bounded merge event page");
            format!("{fingerprint}:{}:{}", last.created_ns, last.id)
        });
        let ids = entries.iter().flat_map(|event| {
            [
                event.candidate_before,
                event.candidate_revision,
                event.applied_revision,
            ]
            .into_iter()
            .flatten()
        });
        let revisions = self.revisions(actor, &workspace_record, ids).await?;
        Ok(MergeHistoryEvents {
            entries,
            total,
            next,
            revisions,
        })
    }
}

fn page_size(limit: Option<usize>) -> Result<usize> {
    let limit = limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(Error::invalid(
            "merge history page size must be between 1 and 200",
        ));
    }
    Ok(limit)
}

fn readable_branches(workspace: &Workspace, actor: &Principal) -> Result<Option<Vec<BranchId>>> {
    if workspace.authorize(actor, None, Permission::Read).is_ok() {
        return Ok(None);
    }
    let branches: BTreeSet<_> = workspace
        .grants
        .iter()
        .filter(|grant| {
            grant.principal == *actor
                && (grant.permissions.contains(&Permission::Read)
                    || grant.permissions.contains(&Permission::Manage))
        })
        .filter_map(|grant| grant.branch)
        .collect();
    if branches.is_empty() {
        return Err(Error::new(
            ErrorCode::PermissionDenied,
            "workspace has no readable merge history",
        ));
    }
    Ok(Some(branches.into_iter().collect()))
}

fn authorize_entry(
    workspace: &Workspace,
    actor: &Principal,
    entry: &MergeHistoryEntry,
) -> Result<()> {
    workspace.authorize(actor, entry.target_branch, Permission::Read)?;
    workspace.authorize(actor, entry.source_branch, Permission::Read)
}

fn validate_record(record: &str) -> Result<()> {
    record.parse::<MergeId>().map(|_| ())
}

fn decode_cursor(cursor: Option<String>, fingerprint: &ObjectId) -> Result<Option<(i64, String)>> {
    cursor
        .map(|value| {
            if value.len() > 256 {
                return Err(Error::invalid("invalid merge history cursor"));
            }
            let mut fields = value.splitn(3, ':');
            let hash = fields.next().unwrap_or_default();
            let at = fields.next().and_then(|value| value.parse().ok());
            let id = fields
                .next()
                .filter(|id| !id.is_empty() && !id.chars().any(char::is_control));
            match (at, id) {
                (Some(at), Some(id)) if hash == fingerprint.to_string() => Ok((at, id.to_owned())),
                _ => Err(Error::invalid(
                    "merge history cursor does not match the query",
                )),
            }
        })
        .transpose()
}
