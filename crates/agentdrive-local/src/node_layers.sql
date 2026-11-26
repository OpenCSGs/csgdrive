CREATE TABLE IF NOT EXISTS node_layers (
    id INTEGER PRIMARY KEY,
    parent INTEGER REFERENCES node_layers(id),
    parent_version INTEGER,
    version INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS branch_node_views (
    branch TEXT PRIMARY KEY REFERENCES branches(id),
    layer INTEGER NOT NULL REFERENCES node_layers(id),
    inodes INTEGER CHECK(inodes >= 0),
    bytes INTEGER CHECK(bytes IS NULL OR (typeof(bytes)='integer' AND bytes>=0))
);
CREATE TABLE IF NOT EXISTS saved_node_views (
    workspace TEXT NOT NULL,
    hash TEXT NOT NULL,
    root BLOB NOT NULL,
    layer INTEGER NOT NULL REFERENCES node_layers(id),
    version INTEGER NOT NULL,
    inodes INTEGER CHECK(inodes >= 0),
    bytes INTEGER CHECK(bytes IS NULL OR (typeof(bytes)='integer' AND bytes>=0)),
    PRIMARY KEY(workspace, hash)
);
CREATE INDEX IF NOT EXISTS saved_node_versions ON saved_node_views(layer,version);
CREATE TABLE IF NOT EXISTS node_versions (
    layer INTEGER NOT NULL REFERENCES node_layers(id),
    path TEXT NOT NULL,
    since INTEGER NOT NULL,
    until INTEGER,
    parent TEXT,
    inode INTEGER,
    data BLOB,
    PRIMARY KEY(layer,path,since)
);
CREATE INDEX IF NOT EXISTS versioned_children ON node_versions(layer,parent,path);
CREATE INDEX IF NOT EXISTS versioned_inodes ON node_versions(layer,inode);
CREATE UNIQUE INDEX IF NOT EXISTS baseline_inodes ON node_versions(layer,inode) WHERE since=0 AND inode IS NOT NULL;
CREATE INDEX IF NOT EXISTS node_layer_parents ON node_layers(parent,parent_version);
