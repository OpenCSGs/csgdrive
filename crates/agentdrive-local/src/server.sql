CREATE TABLE IF NOT EXISTS metadata_objects (
    workspace_id TEXT NOT NULL, hash TEXT NOT NULL, data BLOB NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, hash),
    FOREIGN KEY (workspace_id, hash) REFERENCES objects(workspace_id, hash) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE IF NOT EXISTS published_refs (
    target TEXT NOT NULL, workspace_id TEXT NOT NULL, branch_id TEXT CHECK(branch_id IS NULL OR length(branch_id)>0),
    version INTEGER NOT NULL CHECK (version > 0), data BLOB NOT NULL,
    UNIQUE (target, workspace_id, branch_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS published_workspace_ref ON published_refs(target, workspace_id) WHERE branch_id IS NULL;
CREATE TABLE IF NOT EXISTS server_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1), runtime BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_storage (
    workspace_id TEXT PRIMARY KEY, config BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS codes_file_placements (
    workspace_id TEXT NOT NULL, hash TEXT NOT NULL, placement BLOB NOT NULL,
    format TEXT GENERATED ALWAYS AS (json_extract(CAST(placement AS TEXT), '$.format')) VIRTUAL
        NOT NULL CHECK (format IN ('git_chunks', 'git_lfs')),
    git_revision TEXT GENERATED ALWAYS AS (json_extract(CAST(placement AS TEXT), '$.revision')) VIRTUAL NOT NULL,
    CHECK(hash IS json_extract(CAST(placement AS TEXT), '$.object.id')),
    PRIMARY KEY (workspace_id, hash)
);
CREATE TABLE IF NOT EXISTS object_verification (
    target TEXT NOT NULL, workspace_id TEXT NOT NULL, hash TEXT NOT NULL,
    size INTEGER NOT NULL, etag TEXT, version TEXT, hashes BLOB NOT NULL,
    PRIMARY KEY (target, workspace_id, hash)
);

CREATE TABLE IF NOT EXISTS local_s3_listener (
    id INTEGER PRIMARY KEY CHECK(id=1),
    secret TEXT NOT NULL DEFAULT (hex(randomblob(32))),
    port INTEGER NOT NULL DEFAULT 0 CHECK(port BETWEEN 0 AND 65535)
);
