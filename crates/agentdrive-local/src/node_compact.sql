-- Rebuildable views can be dropped when no live branch or revision points at the root.
CREATE TEMP TABLE IF NOT EXISTS retained_node_roots(workspace_id TEXT, hash TEXT, PRIMARY KEY(workspace_id,hash));
DELETE FROM retained_node_roots;
INSERT OR IGNORE INTO retained_node_roots SELECT workspace_id,working_root_object_id FROM branches;
INSERT OR IGNORE INTO retained_node_roots SELECT workspace_id,root_object_id FROM revisions;
DELETE FROM saved_node_views AS s WHERE NOT EXISTS (
    SELECT 1 FROM retained_node_roots r WHERE r.workspace_id=s.workspace_id AND r.hash=s.hash
) AND (s.inodes IS NOT NULL OR NOT EXISTS (
    SELECT 1 FROM node_layers l WHERE l.parent=s.layer AND l.parent_version=s.version
));

CREATE TEMP TABLE IF NOT EXISTS retained_node_layers(id INTEGER,version INTEGER,PRIMARY KEY(id,version));
DELETE FROM retained_node_layers;
WITH RECURSIVE roots(id,version) AS (
    SELECT layer,version FROM saved_node_views
    UNION SELECT l.id,l.version FROM branch_node_views b JOIN node_layers l ON l.id=b.layer
    UNION SELECT l.parent,l.parent_version FROM roots JOIN node_layers l ON l.id=roots.id WHERE l.parent IS NOT NULL
) INSERT INTO retained_node_layers SELECT id,version FROM roots;
DELETE FROM node_versions AS n WHERE NOT EXISTS (
    SELECT 1 FROM retained_node_layers k WHERE k.id=n.layer AND k.version>=n.since AND (n.until IS NULL OR k.version<n.until)
);
DELETE FROM node_layers WHERE id NOT IN (SELECT id FROM retained_node_layers);
DELETE FROM retained_node_roots;
DELETE FROM retained_node_layers;
