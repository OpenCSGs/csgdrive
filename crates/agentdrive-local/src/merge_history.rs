use crate::{LocalBackend, sqlite::sql_error};
use agentdrive_model::*;
use agentdrive_ports::*;
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value};
use serde::Deserialize;

const RECORDS: &str = include_str!("merge_history.sql");

const ENTRY: &str = r#"json_object(
    'id',id,'merge_id',merge_id,'source_branch',source_branch,
    'source_name',coalesce((SELECT json_extract(CAST(b.data AS TEXT),'$.name') FROM branches b WHERE b.workspace=?1 AND b.id=records.source_branch),
                         (SELECT json_extract(CAST(r.data AS TEXT),'$.metadata.name') FROM remote_refs r WHERE r.workspace=?1 AND r.id=records.source_branch)),
    'source_revision',source_revision,'target_branch',target_branch,
    'target_name',coalesce((SELECT json_extract(CAST(b.data AS TEXT),'$.name') FROM branches b WHERE b.workspace=?1 AND b.id=records.target_branch),
                         (SELECT json_extract(CAST(r.data AS TEXT),'$.metadata.name') FROM remote_refs r WHERE r.workspace=?1 AND r.id=records.target_branch)),
    'target_revision',target_revision,'base_revision',base_revision,'candidate_revision',candidate_revision,
    'applied_revision',applied_revision,'principal',json_extract(first_data,'$.principal'),
    'origin_location',origin_location,'created_ns',created_ns,'updated_ns',updated_ns,'status',status,
    'remote_confirmed',json(CASE WHEN applied_data IS NULL THEN NULL WHEN json_extract(applied_data,'$.remote_confirmed')=1 THEN 'true' ELSE 'false' END),
    'mount_ready',json(CASE WHEN json_type(coalesce(applied_data,latest_data),'$.mount_ready')='true' THEN 'true'
                           WHEN json_type(coalesce(applied_data,latest_data),'$.mount_ready')='false' THEN 'false' END),
    'history_complete',json(CASE WHEN complete=1 AND json_extract(first_data,'$.name')='merge_prepare' THEN 'true' ELSE 'false' END),
    'error',json(json_extract(latest_data,'$.error')),'conflict_count',conflict_count
)"#;

impl LocalBackend {
    pub(crate) async fn read_merge_history_page(
        &self,
        query: MergeHistoryScan,
    ) -> Result<(Vec<MergeHistoryEntry>, usize)> {
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let mut args = vec![Value::Text(query.workspace.to_string())];
            let mut conditions = filters(&query, &mut args)?;
            let total = transaction
                .query_row(
                    &format!("{RECORDS} SELECT count(*) FROM records WHERE {}", conditions.join(" AND ")),
                    params_from_iter(&args),
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sql_error)?;
            if let Some((at, id)) = query.before {
                let timestamp = bind(&mut args, Value::Integer(at));
                let id = bind(&mut args, Value::Text(id));
                conditions.push(format!("(created_ns < {timestamp} OR (created_ns={timestamp} AND id < {id}))"));
            }
            let limit = bind(&mut args, Value::Integer(query.limit as i64));
            let sql = format!("{RECORDS} SELECT {ENTRY} FROM records WHERE {} ORDER BY created_ns DESC,id DESC LIMIT {limit}", conditions.join(" AND "));
            let entries = read_entries(&transaction, &sql, &args)?;
            Ok((entries, usize::try_from(total).map_err(|_| Error::integrity("invalid merge history count"))?))
        }).await
    }

    pub(crate) async fn read_merge_history_entry(
        &self,
        workspace: WorkspaceId,
        record: &str,
    ) -> Result<Option<MergeHistoryEntry>> {
        let record = record.to_owned();
        self.db(move |connection| {
            let sql = format!("{RECORDS} SELECT {ENTRY} FROM records WHERE id=?2");
            let data: Option<String> = connection
                .query_row(&sql, [workspace.to_string(), record], |row| row.get(0))
                .optional()
                .map_err(sql_error)?;
            data.map(|data| decode(data.as_bytes())).transpose()
        })
        .await
    }

    pub(crate) async fn read_merge_history_events(
        &self,
        query: MergeEventsScan,
    ) -> Result<(Vec<MergeHistoryAction>, usize)> {
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let mut args = vec![Value::Text(query.workspace.to_string()), Value::Text(query.record)];
            let events = format!(r#"{RECORDS}, events AS (
                SELECT r.id operation_id,r.data operation_data,e.value event_data,
                       r.id || '/' || printf('%020d',json_extract(e.value,'$.sequence')) event_id,
                       json_extract(e.value,'$.at_ns') at_ns,1 complete
                FROM receipts r,json_each(r.data,'$.merge_history.events') e WHERE r.record_id=?2
                UNION ALL
                SELECT r.id,r.data,json_object('phase',json_extract(r.data,'$.phase'),
                       'local_saved',json_extract(r.data,'$.local_saved'),'remote_confirmed',json_extract(r.data,'$.remote_confirmed'),
                       'mount_ready',json_extract(r.data,'$.mount_ready'),'error',json(json_extract(r.data,'$.error')),
                       'candidate',json_extract(r.data,'$.result.candidate.revision'),
                       'result_revision',coalesce(json_extract(r.data,'$.result.revision'),json_extract(r.data,'$.result.candidate.applied_revision')),
                       'conflict_count',json_array_length(json_extract(r.data,'$.result.candidate.conflicts'))),
                       r.id || '/legacy',r.created_ns,0
                FROM receipts r WHERE r.record_id=?2 AND coalesce(json_array_length(json_extract(r.data,'$.merge_history.events')),0)=0
            )"#);
            let total = transaction.query_row(&format!("{events} SELECT count(*) FROM events"), params_from_iter(&args), |row| row.get::<_, i64>(0)).map_err(sql_error)?;
            let mut predicate = "1".to_owned();
            if let Some((at, id)) = query.after {
                let at = bind(&mut args, Value::Integer(at));
                let id = bind(&mut args, Value::Text(id));
                predicate = format!("(at_ns > {at} OR (at_ns={at} AND event_id>{id}))");
            }
            let limit = bind(&mut args, Value::Integer(query.limit as i64));
            let sql = format!("{events} SELECT event_id,json_object('id',operation_id,'request_id',json_extract(operation_data,'$.request_id'),'name',json_extract(operation_data,'$.name'),'principal',json_extract(operation_data,'$.principal'),'location',json_extract(operation_data,'$.merge_history.location'),'candidate_before',json_extract(operation_data,'$.merge_history.candidate_before'),'resolutions',json(coalesce(json_extract(operation_data,'$.merge_history.resolutions'),'[]'))),event_data,at_ns,complete FROM events WHERE {predicate} ORDER BY at_ns,event_id LIMIT {limit}");
            let mut statement = transaction.prepare(&sql).map_err(sql_error)?;
            let rows = statement.query_map(params_from_iter(&args), |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?,row.get::<_, i64>(3)?,row.get::<_, bool>(4)?))).map_err(sql_error)?;
            let mut entries = Vec::new();
            for row in rows {
                let (id, operation, event, at_ns, complete) = row.map_err(sql_error)?;
                let operation: ActionOperation = decode(operation.as_bytes())?;
                let event: serde_json::Value = serde_json::from_str(&event).map_err(|error| Error::integrity(error.to_string()))?;
                entries.push(action(id, operation, event, at_ns, complete)?);
            }
            Ok((entries, usize::try_from(total).map_err(|_| Error::integrity("invalid merge history count"))?))
        }).await
    }
}

fn bind(args: &mut Vec<Value>, value: Value) -> String {
    args.push(value);
    format!("?{}", args.len())
}

fn filters(query: &MergeHistoryScan, args: &mut Vec<Value>) -> Result<Vec<String>> {
    let mut conditions = vec!["1".to_owned()];
    for (column, value) in [
        (
            "source_branch",
            query.filter.source_branch.map(|id| id.to_string()),
        ),
        (
            "target_branch",
            query.filter.target_branch.map(|id| id.to_string()),
        ),
        (
            "origin_location",
            query.filter.location.map(|id| id.to_string()),
        ),
    ] {
        if let Some(value) = value {
            let parameter = bind(args, Value::Text(value));
            conditions.push(format!("{column}={parameter}"));
        }
    }
    if let Some(branch) = query.filter.branch {
        let parameter = bind(args, Value::Text(branch.to_string()));
        conditions.push(format!(
            "(source_branch={parameter} OR target_branch={parameter})"
        ));
    }
    if let Some(status) = query.filter.status {
        let value =
            serde_json::to_value(status).map_err(|error| Error::integrity(error.to_string()))?;
        let parameter = bind(
            args,
            Value::Text(value.as_str().unwrap_or_default().to_owned()),
        );
        conditions.push(format!("status={parameter}"));
    }
    if let Some(revision) = query.filter.revision {
        let parameter = bind(args, Value::Text(revision.to_string()));
        conditions.push(format!("(source_revision={parameter} OR target_revision={parameter} OR base_revision={parameter} OR candidate_revision={parameter} OR applied_revision={parameter} OR EXISTS(SELECT 1 FROM receipts r,json_tree(r.data,'$.merge_history') t WHERE r.record_id=records.id AND t.type='text' AND t.value={parameter}))"));
    }
    for (operator, value) in [(">=", query.filter.from_ns), ("<=", query.filter.to_ns)] {
        if let Some(value) = value {
            let parameter = bind(args, Value::Integer(value));
            conditions.push(format!("created_ns{operator}{parameter}"));
        }
    }
    if let Some(branches) = &query.readable_branches {
        if branches.is_empty() {
            conditions.push("0".to_owned());
        } else {
            let parameters: Vec<_> = branches
                .iter()
                .map(|id| bind(args, Value::Text(id.to_string())))
                .collect();
            let parameters = parameters.join(",");
            conditions.push(format!(
                "(target_branch IN ({parameters}) AND source_branch IN ({parameters}))"
            ));
        }
    }
    Ok(conditions)
}

fn read_entries(
    connection: &Connection,
    sql: &str,
    args: &[Value],
) -> Result<Vec<MergeHistoryEntry>> {
    let mut statement = connection.prepare(sql).map_err(sql_error)?;
    let rows = statement
        .query_map(params_from_iter(args), |row| row.get::<_, String>(0))
        .map_err(sql_error)?;
    rows.map(|row| decode(row.map_err(sql_error)?.as_bytes()))
        .collect()
}

#[derive(Deserialize)]
struct ActionOperation {
    id: OperationId,
    request_id: String,
    name: String,
    principal: Principal,
    location: Option<LocationId>,
    candidate_before: Option<RevisionId>,
    resolutions: Vec<MergeResolutionSummary>,
}

fn action(
    id: String,
    operation: ActionOperation,
    event: serde_json::Value,
    at_ns: i64,
    complete: bool,
) -> Result<MergeHistoryAction> {
    let get = |key: &str| event.get(key).cloned().unwrap_or(serde_json::Value::Null);
    let parse = |key: &str| {
        serde_json::from_value::<Option<RevisionId>>(get(key))
            .map_err(|error| Error::integrity(error.to_string()))
    };
    let boolean = |key: &str| {
        event.get(key).and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.as_i64().map(|value| value != 0))
        })
    };
    Ok(MergeHistoryAction {
        id,
        operation: operation.id,
        request_id: operation.request_id,
        name: operation.name,
        principal: operation.principal,
        location: operation.location,
        created_ns: at_ns,
        phase: serde_json::from_value(get("phase"))
            .map_err(|error| Error::integrity(error.to_string()))?,
        local_saved: boolean("local_saved").unwrap_or(false),
        remote_confirmed: boolean("remote_confirmed").unwrap_or(false),
        mount_ready: boolean("mount_ready"),
        error: serde_json::from_value(get("error"))
            .map_err(|error| Error::integrity(error.to_string()))?,
        candidate_before: operation.candidate_before,
        candidate_revision: parse("candidate")?,
        applied_revision: parse("result_revision")?,
        conflict_count: event
            .get("conflict_count")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize),
        paths: operation
            .resolutions
            .into_iter()
            .map(|resolution| MergeHistoryPath {
                path: resolution.path,
                choice: resolution.choice,
            })
            .collect(),
        history_complete: complete,
    })
}
