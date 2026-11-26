CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value NOT NULL);
CREATE TABLE IF NOT EXISTS workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    data BLOB NOT NULL,
    name TEXT NOT NULL,
    initial_revision_id TEXT NOT NULL,
    CHECK(json(CAST(data AS TEXT)) = json_remove(CAST(data AS TEXT),'$.id','$.name','$.initial_revision')),
    FOREIGN KEY(workspace_id,initial_revision_id) REFERENCES revisions(workspace_id,revision_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE IF NOT EXISTS branches (
    branch_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    name TEXT NOT NULL,
    source_branch_id TEXT,
    base_revision_id TEXT NOT NULL,
    formal_head_revision_id TEXT NOT NULL,
    autosave_head_revision_id TEXT,
    working_root_object_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.working_root.id')) VIRTUAL NOT NULL,
    CHECK(json(CAST(data AS TEXT)) = json_remove(CAST(data AS TEXT),'$.id','$.workspace','$.name','$.source_branch','$.base_revision','$.formal_head','$.autosave_head')),
    UNIQUE(workspace_id,branch_id),
    FOREIGN KEY(workspace_id,formal_head_revision_id) REFERENCES revisions(workspace_id,revision_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS branches_workspace_source ON branches(workspace_id,source_branch_id);
CREATE TABLE IF NOT EXISTS collaborations (
    branch_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    CHECK(branch_id IS json_extract(CAST(data AS TEXT),'$.branch') AND workspace_id IS json_extract(CAST(data AS TEXT),'$.workspace'))
);
CREATE INDEX IF NOT EXISTS collaborations_workspace ON collaborations(workspace_id,branch_id);
CREATE TABLE IF NOT EXISTS workspace_selections (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    branch_id TEXT NOT NULL,
    FOREIGN KEY(workspace_id,branch_id) REFERENCES branches(workspace_id,branch_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE IF NOT EXISTS revisions (
    revision_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    branch_id TEXT,
    created_ns INTEGER NOT NULL,
    root_object_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.root.id')) VIRTUAL NOT NULL,
    operation_id TEXT NOT NULL,
    message TEXT,
    CHECK(json(CAST(data AS TEXT)) = json_remove(CAST(data AS TEXT),'$.id','$.workspace','$.branch','$.created_ns','$.operation','$.message','$.parents')),
    UNIQUE(workspace_id,revision_id)
);
CREATE INDEX IF NOT EXISTS revisions_branch_history ON revisions(workspace_id,branch_id,created_ns DESC,revision_id);
-- Only the child must be local. A retained remote history can refer to an unimported parent.
CREATE TABLE IF NOT EXISTS revision_parents (
    revision_id TEXT NOT NULL REFERENCES revisions(revision_id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    parent_order INTEGER NOT NULL CHECK(parent_order>=0),
    parent_revision_id TEXT NOT NULL,
    PRIMARY KEY(revision_id,parent_order)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS revision_children ON revision_parents(parent_revision_id);
CREATE VIEW IF NOT EXISTS workspace_records AS
    SELECT workspace_id, name, initial_revision_id,
        CAST(json_set(CAST(data AS TEXT),
        '$.id',workspace_id, '$.name',name, '$.initial_revision',initial_revision_id) AS BLOB) AS data
    FROM workspaces;
CREATE TRIGGER IF NOT EXISTS insert_workspace_record INSTEAD OF INSERT ON workspace_records BEGIN
    INSERT INTO workspaces(workspace_id, name, initial_revision_id, data)
        SELECT doc ->> '$.id', doc ->> '$.name', doc ->> '$.initial_revision',
            CAST(json_remove(doc,'$.id','$.name','$.initial_revision') AS BLOB)
        FROM (SELECT CAST(NEW.data AS TEXT) AS doc);
END;
CREATE TRIGGER IF NOT EXISTS update_workspace_record INSTEAD OF UPDATE OF data ON workspace_records BEGIN
    SELECT RAISE(ABORT,'workspace identity changed') WHERE OLD.workspace_id IS NOT json_extract(CAST(NEW.data AS TEXT),'$.id');
    UPDATE workspaces SET (name, initial_revision_id, data) = (
        SELECT doc ->> '$.name', doc ->> '$.initial_revision',
            CAST(json_remove(doc,'$.id','$.name','$.initial_revision') AS BLOB)
        FROM (SELECT CAST(NEW.data AS TEXT) AS doc)
    ) WHERE workspace_id=OLD.workspace_id;
END;
CREATE VIEW IF NOT EXISTS branch_records AS
    SELECT branch_id, workspace_id, name, source_branch_id, base_revision_id, formal_head_revision_id, autosave_head_revision_id,
        CAST(json_set(CAST(data AS TEXT),
        '$.id',branch_id, '$.workspace',workspace_id, '$.name',name,
        '$.source_branch',source_branch_id, '$.base_revision',base_revision_id, '$.formal_head',formal_head_revision_id,
        '$.autosave_head',autosave_head_revision_id) AS BLOB) AS data
    FROM branches;
CREATE TRIGGER IF NOT EXISTS insert_branch_record INSTEAD OF INSERT ON branch_records BEGIN
    INSERT INTO branches(branch_id, workspace_id, name, source_branch_id, base_revision_id, formal_head_revision_id, autosave_head_revision_id, data)
        SELECT doc ->> '$.id', doc ->> '$.workspace', doc ->> '$.name',
            doc ->> '$.source_branch', doc ->> '$.base_revision', doc ->> '$.formal_head',
            doc ->> '$.autosave_head',
            CAST(json_remove(doc,'$.id','$.workspace','$.name','$.source_branch','$.base_revision','$.formal_head','$.autosave_head') AS BLOB)
        FROM (SELECT CAST(NEW.data AS TEXT) AS doc);
END;
CREATE TRIGGER IF NOT EXISTS update_branch_record INSTEAD OF UPDATE OF data ON branch_records BEGIN
    SELECT RAISE(ABORT,'branch identity changed') WHERE OLD.branch_id IS NOT json_extract(CAST(NEW.data AS TEXT),'$.id') OR OLD.workspace_id IS NOT json_extract(CAST(NEW.data AS TEXT),'$.workspace');
    UPDATE branches SET (name, source_branch_id, base_revision_id, formal_head_revision_id, autosave_head_revision_id, data) = (
        SELECT doc ->> '$.name', doc ->> '$.source_branch', doc ->> '$.base_revision',
            doc ->> '$.formal_head', doc ->> '$.autosave_head',
            CAST(json_remove(doc,'$.id','$.workspace','$.name','$.source_branch','$.base_revision','$.formal_head','$.autosave_head') AS BLOB)
        FROM (SELECT CAST(NEW.data AS TEXT) AS doc)
    ) WHERE branch_id=OLD.branch_id;
END;
CREATE VIEW IF NOT EXISTS revision_records AS
    SELECT revision_id, workspace_id, branch_id, created_ns, operation_id, message,
        CAST(json_set(CAST(data AS TEXT),
        '$.id',revision_id, '$.workspace',workspace_id, '$.branch',branch_id,
        '$.created_ns',created_ns, '$.operation',operation_id, '$.message',message,
        '$.parents',json((SELECT json_group_array(parent_revision_id) FROM (
            SELECT parent_revision_id FROM revision_parents AS p
            WHERE p.revision_id = revisions.revision_id ORDER BY parent_order
        )))) AS BLOB) AS data
    FROM revisions;
CREATE TRIGGER IF NOT EXISTS insert_revision_record INSTEAD OF INSERT ON revision_records BEGIN
    INSERT INTO revisions(revision_id, workspace_id, branch_id, created_ns, operation_id, message, data)
        SELECT doc ->> '$.id', doc ->> '$.workspace', doc ->> '$.branch',
            doc ->> '$.created_ns', doc ->> '$.operation', doc ->> '$.message',
            CAST(json_remove(doc,'$.id','$.workspace','$.branch','$.created_ns','$.operation','$.message','$.parents') AS BLOB)
        FROM (SELECT CAST(NEW.data AS TEXT) AS doc);
    INSERT INTO revision_parents(revision_id,parent_order,parent_revision_id)
        SELECT json_extract(CAST(NEW.data AS TEXT),'$.id'),CAST(key AS INTEGER),value
        FROM json_each(CAST(NEW.data AS TEXT),'$.parents');
END;
CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    request_id TEXT NOT NULL,
    workspace_id TEXT,
    data BLOB NOT NULL,
    branch_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.branch')) VIRTUAL,
    UNIQUE(principal, request_id)
);
CREATE TABLE IF NOT EXISTS replicated_operations (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    data BLOB NOT NULL,
    branch_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.branch')) VIRTUAL
);
CREATE INDEX IF NOT EXISTS replicated_operations_workspace ON replicated_operations(workspace_id);
CREATE INDEX IF NOT EXISTS operations_merge_history ON operations(workspace_id)
    WHERE json_extract(CAST(data AS TEXT), '$.merge_history') IS NOT NULL;
CREATE INDEX IF NOT EXISTS operations_merge_actions ON operations(workspace_id)
    WHERE json_extract(CAST(data AS TEXT),'$.name') IN ('merge_prepare','merge_resolve','merge_apply')
       OR (json_extract(CAST(data AS TEXT),'$.name')='mount_release' AND json_extract(CAST(data AS TEXT),'$.merge_history') IS NOT NULL);
CREATE INDEX IF NOT EXISTS replicated_operations_merge_actions ON replicated_operations(workspace_id)
    WHERE json_extract(CAST(data AS TEXT),'$.name') IN ('merge_prepare','merge_resolve','merge_apply')
       OR (json_extract(CAST(data AS TEXT),'$.name')='mount_release' AND json_extract(CAST(data AS TEXT),'$.merge_history') IS NOT NULL);
CREATE TABLE IF NOT EXISTS merge_history_publications (
    operation TEXT PRIMARY KEY REFERENCES operations(id) DEFERRABLE INITIALLY DEFERRED,
    sequence INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS operations_branch_results ON operations(workspace_id)
    WHERE json_extract(CAST(data AS TEXT), '$.result.kind')='branch';
CREATE INDEX IF NOT EXISTS replicated_operations_branch_results ON replicated_operations(workspace_id)
    WHERE json_extract(CAST(data AS TEXT), '$.result.kind')='branch';
CREATE TABLE IF NOT EXISTS mounts (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    active INTEGER NOT NULL,
    writable INTEGER NOT NULL,
    data BLOB NOT NULL,
    workspace_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.workspace')) VIRTUAL NOT NULL,
    revision_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.revision')) VIRTUAL,
    FOREIGN KEY(workspace_id,branch_id) REFERENCES branches(workspace_id,branch_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE UNIQUE INDEX IF NOT EXISTS mounts_path ON mounts(path) WHERE active=1;
CREATE INDEX IF NOT EXISTS mounts_branch ON mounts(branch_id, id);
CREATE TABLE IF NOT EXISTS merges (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    target_branch_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.target.branch')) VIRTUAL NOT NULL,
    applied_revision_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.applied_revision')) VIRTUAL,
    FOREIGN KEY(workspace_id,target_branch_id) REFERENCES branches(workspace_id,branch_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE IF NOT EXISTS sync_jobs (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL,
    data BLOB NOT NULL,
    UNIQUE(branch_id, source_generation),
    FOREIGN KEY(workspace_id,branch_id) REFERENCES branches(workspace_id,branch_id) DEFERRABLE INITIALLY DEFERRED
);
-- Remote observations and published history can precede a locally materialized branch.
CREATE TABLE IF NOT EXISTS remote_refs (
    branch_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    name TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.metadata.name')) VIRTUAL,
    source_branch_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.metadata.source_branch')) VIRTUAL,
    formal_head_revision_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.formal_head')) VIRTUAL NOT NULL,
    CHECK(branch_id IS json_extract(CAST(data AS TEXT),'$.branch') AND workspace_id IS json_extract(CAST(data AS TEXT),'$.workspace'))
);
CREATE INDEX IF NOT EXISTS remote_refs_workspace ON remote_refs(workspace_id,branch_id);
CREATE TABLE IF NOT EXISTS cache_pins (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) DEFERRABLE INITIALLY DEFERRED,
    data BLOB NOT NULL,
    revision_id TEXT GENERATED ALWAYS AS (json_extract(CAST(data AS TEXT),'$.revision')) VIRTUAL NOT NULL
);
CREATE TABLE IF NOT EXISTS objects (
    workspace_id TEXT NOT NULL,
    hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    remote_confirmed INTEGER NOT NULL DEFAULT 0,
    last_access_ns INTEGER NOT NULL,
    PRIMARY KEY(workspace_id, hash)
);
CREATE TABLE IF NOT EXISTS node_deltas (
    branch_id TEXT NOT NULL REFERENCES branches(branch_id) DEFERRABLE INITIALLY DEFERRED,
    path TEXT NOT NULL,
    previous BLOB,
    PRIMARY KEY(branch_id, path)
);
CREATE TABLE IF NOT EXISTS branch_history (
    branch_id TEXT PRIMARY KEY,
    source_generation INTEGER NOT NULL,
    root BLOB NOT NULL
);
