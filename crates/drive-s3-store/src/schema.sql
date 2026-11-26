CREATE TABLE drive_branches(
    branch_id TEXT PRIMARY KEY NOT NULL, workspace_id TEXT NOT NULL, epoch INTEGER NOT NULL DEFAULT 0,
    serialized INTEGER NOT NULL DEFAULT 0 CHECK(serialized IN (0,1)),
    holder TEXT UNIQUE REFERENCES drive_sessions(bucket));
CREATE TABLE drive_sessions(
    bucket TEXT PRIMARY KEY NOT NULL, branch_id TEXT NOT NULL REFERENCES drive_branches(branch_id),
    retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1)), successor TEXT REFERENCES drive_sessions(bucket),
    revision TEXT, read_only INTEGER NOT NULL DEFAULT 0 CHECK(read_only IN (0,1)));
CREATE INDEX drive_session_buckets ON drive_sessions(branch_id,bucket);
