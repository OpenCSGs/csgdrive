CREATE TABLE IF NOT EXISTS metadata_objects (
    workspace TEXT NOT NULL, hash TEXT NOT NULL, data BLOB NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace, hash)
);
CREATE TABLE IF NOT EXISTS published_refs (
    target TEXT NOT NULL, workspace TEXT NOT NULL, branch TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0), data BLOB NOT NULL,
    PRIMARY KEY (target, workspace, branch)
);
CREATE TABLE IF NOT EXISTS server_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1), runtime BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_storage (
    workspace TEXT PRIMARY KEY, config BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS file_placements (
    workspace TEXT NOT NULL, hash TEXT NOT NULL, placement BLOB NOT NULL,
    format TEXT GENERATED ALWAYS AS (json_extract(CAST(placement AS TEXT), '$.format')) VIRTUAL
        NOT NULL CHECK (format IN ('git_chunks', 'git_lfs')),
    PRIMARY KEY (workspace, hash)
);
CREATE TABLE IF NOT EXISTS object_verification (
    target TEXT NOT NULL, workspace TEXT NOT NULL, hash TEXT NOT NULL,
    size INTEGER NOT NULL, etag TEXT, version TEXT, hashes BLOB NOT NULL,
    PRIMARY KEY (target, workspace, hash)
);
