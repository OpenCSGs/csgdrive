WITH operation_rows AS (
    SELECT id, CAST(data AS TEXT) data FROM operations
    WHERE workspace=?1 AND (json_extract(CAST(data AS TEXT),'$.name') IN ('merge_prepare','merge_resolve','merge_apply')
        OR (json_extract(CAST(data AS TEXT),'$.name')='mount_release' AND json_extract(CAST(data AS TEXT),'$.merge_history') IS NOT NULL))
    UNION ALL
    SELECT id, CAST(data AS TEXT) data FROM replicated_operations
    WHERE workspace=?1 AND (json_extract(CAST(data AS TEXT),'$.name') IN ('merge_prepare','merge_resolve','merge_apply')
        OR (json_extract(CAST(data AS TEXT),'$.name')='mount_release' AND json_extract(CAST(data AS TEXT),'$.merge_history') IS NOT NULL))
      AND id NOT IN (SELECT id FROM operations)
), linked AS (
    SELECT id,data,coalesce(
        json_extract(data,'$.merge_history.merge'),
        json_extract(data,'$.result.candidate.id'),
        json_extract(data,'$.pending.merge'),
        CASE WHEN json_extract(data,'$.name')='merge_apply' THEN (
            SELECT m.id FROM merges m WHERE m.workspace=?1
              AND json_extract(CAST(m.data AS TEXT),'$.applied_revision')=json_extract(operation_rows.data,'$.result.revision')
            LIMIT 1
        ) END
    ) merge_id FROM operation_rows
), receipts AS MATERIALIZED (
    SELECT *,coalesce(merge_id,id) record_id,
        json_extract(data,'$.created_ns') created_ns,
        coalesce(json_extract(data,'$.merge_history.events[#-1].at_ns'),json_extract(data,'$.created_ns')) updated_ns
    FROM linked
), ordered AS MATERIALIZED (
    SELECT *,row_number() OVER (PARTITION BY record_id ORDER BY
        CASE WHEN json_extract(data,'$.name')='merge_prepare' THEN 0 ELSE 1 END,created_ns,id) first_rank,
        row_number() OVER (PARTITION BY record_id ORDER BY updated_ns DESC,created_ns DESC,id DESC) latest_rank
    FROM receipts
), groups AS (
    SELECT record_id id,min(created_ns) created_ns,max(updated_ns) updated_ns,
        min(CASE WHEN json_array_length(json_extract(data,'$.merge_history.events'))>0 THEN 1 ELSE 0 END) complete,
        max(CASE WHEN first_rank=1 THEN data END) first_data,
        max(CASE WHEN latest_rank=1 THEN data END) latest_data
    FROM ordered GROUP BY record_id
), candidate_parents AS (
    SELECT DISTINCT record_id,json_extract(data,'$.merge_history.candidate_before') revision
    FROM receipts WHERE json_extract(data,'$.result.kind')='merge'
), candidates AS MATERIALIZED (
    SELECT r.record_id,json_extract(r.data,'$.result.candidate') data,
        row_number() OVER (PARTITION BY r.record_id ORDER BY (p.revision IS NOT NULL),r.updated_ns DESC,r.created_ns DESC,r.id DESC) rank
    FROM receipts r LEFT JOIN candidate_parents p ON p.record_id=r.record_id
        AND p.revision=json_extract(r.data,'$.result.candidate.revision')
    WHERE json_extract(r.data,'$.result.kind')='merge'
), applications AS MATERIALIZED (
    SELECT r.record_id,r.data,row_number() OVER (PARTITION BY r.record_id ORDER BY
        CASE WHEN r.id=json_extract(CAST(v.data AS TEXT),'$.operation') THEN 0 ELSE 1 END,r.created_ns,r.id) rank
    FROM receipts r LEFT JOIN revisions v ON v.workspace=?1 AND v.id=json_extract(r.data,'$.result.revision')
    WHERE json_extract(r.data,'$.name')='merge_apply' AND json_extract(r.data,'$.local_saved')=1
        AND json_extract(r.data,'$.result.kind')='revision'
), heads AS MATERIALIZED (
    SELECT g.*,coalesce(c.data,CAST(m.data AS TEXT)) candidate_data,a.data applied_data
    FROM groups g LEFT JOIN candidates c ON c.record_id=g.id AND c.rank=1
    LEFT JOIN applications a ON a.record_id=g.id AND a.rank=1
    LEFT JOIN merges m ON m.workspace=?1 AND m.id=g.id
), versions AS MATERIALIZED (
    SELECT *,
        coalesce(json_extract(first_data,'$.merge_history.merge'),json_extract(candidate_data,'$.id'),
                 json_extract(latest_data,'$.merge_history.merge')) merge_id,
        coalesce(json_extract(first_data,'$.merge_history.source'),json_extract(candidate_data,'$.source'),
                 (SELECT json_extract(CAST(v.data AS TEXT),'$.parents[1]') FROM revisions v WHERE v.workspace=?1
                   AND v.id=json_extract(applied_data,'$.result.revision'))) source_revision,
        coalesce(json_extract(first_data,'$.merge_history.target.branch'),json_extract(candidate_data,'$.target.branch'),
                 json_extract(first_data,'$.branch')) target_branch,
        coalesce(json_extract(first_data,'$.merge_history.target.expected_head'),json_extract(candidate_data,'$.target.expected_head'),
                 (SELECT json_extract(CAST(v.data AS TEXT),'$.parents[0]') FROM revisions v WHERE v.workspace=?1
                   AND v.id=json_extract(applied_data,'$.result.revision'))) target_revision,
        coalesce(json_extract(candidate_data,'$.base'),json_extract(first_data,'$.merge_history.base')) base_revision,
        json_extract(candidate_data,'$.revision') candidate_revision,
        coalesce(json_extract(candidate_data,'$.applied_revision'),json_extract(applied_data,'$.result.revision')) applied_revision,
        json_array_length(json_extract(candidate_data,'$.conflicts')) conflict_count
    FROM heads
), records AS (
    SELECT *,
        coalesce(json_extract(first_data,'$.merge_history.source_branch'),
                 (SELECT json_extract(CAST(v.data AS TEXT),'$.branch') FROM revisions v WHERE v.workspace=?1 AND v.id=versions.source_revision)) source_branch,
        json_extract(first_data,'$.merge_history.location') origin_location,
        CASE
            WHEN applied_revision IS NOT NULL THEN 'applied'
            WHEN json_extract(latest_data,'$.phase')='recovery_required' THEN 'recovery_required'
            WHEN json_extract(latest_data,'$.phase') IN ('waiting_for_quiesce','committed_mount_pending') THEN 'waiting_for_mount'
            WHEN json_extract(latest_data,'$.phase') IN ('failed','cancelled') THEN 'failed'
            WHEN merge_id IS NULL AND json_extract(latest_data,'$.name')='merge_prepare'
                 AND json_extract(latest_data,'$.local_saved')=1 AND json_extract(latest_data,'$.result.kind')='revision' THEN 'already_included'
            WHEN conflict_count>0 THEN 'conflicts'
            ELSE 'pending'
        END status
    FROM versions
)
