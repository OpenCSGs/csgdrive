# Drive S3

Expose a workspace branch as a path-style S3 bucket to CSI clients. The S3
gateway and the existing AgentDrive dashboard share one engine, so a completed
write is visible through the other mount and through the dashboard's file
browser. Clients with a directory or file cache may need to refresh or reopen
a file.

## Run the server

Build with Rust 1.96 or later. Give each server its own data directory.

```sh
cargo build --locked -p agentdrived
target/debug/agentdrived init --directory ./drive-server --listen 127.0.0.1:19743
export DRIVE_S3_ACCESS_KEY=drive-csi
export DRIVE_S3_SECRET_KEY="$(openssl rand -hex 32)"
target/debug/agentdrived serve --config ./drive-server/agentdrive.json \
    --drive-s3-listen 127.0.0.1:19741 --drive-s3-admin-port 19742
```

For Kubernetes clients, set `--drive-s3-listen` to the server's private IP
and configure the CSI secret with the same signing credentials. The dashboard
and the administration endpoint listen on loopback. Use the dashboard at
`http://127.0.0.1:19743/archive` to create the workspace and choose storage.
S3 and OpenCSG Codes are independent choices. XNet is optional for Codes
repositories. The workspace, branch tree, revisions and backend selection live
in SQLite. A personal daemon started without the S3 listener uses the same
database layout. The selected content backend receives file content.

Create the workspace and branch in the dashboard. In **Archive**, select the
branch and copy the `workspace` and `branch` UUIDs from the URL. Bind each
CSI volume to those IDs with a distinct bucket alias and static PV volume
handle. Set the CSI bucket to that alias and configure the path-style endpoint
and region. An unknown ID or a branch that belongs to another workspace
causes the bind to fail.

### Edit the same branch from two desktops

Set `WORKSPACE_ID` and `BRANCH_ID` to the UUIDs from the dashboard. These
examples use `jq` to encode the management requests.

```sh
manage() { curl -fsS --json "$1" http://127.0.0.1:19742/mounts; }
bind_volume() {
  manage "$(jq -nc --arg bucket "$1" --arg mode "$2" \
    --arg w "$WORKSPACE_ID" --arg b "$BRANCH_ID" \
    '{action:"bind_ids",bucket:$bucket,workspace_id:$w,branch_id:$b,write_mode:$mode}')"
}
bind_volume desktop-a serialized
bind_volume desktop-b serialized
```

Either mount can create, update or delete a file. The gateway reserves the destination branch throughout each publication,
including copies and completed multipart uploads. Independent branches can publish concurrently. A failed request releases its reservation. When both clients save the
same file, the last successful publication wins. A rename implemented as S3
copy followed by delete consists of two requests. Open the branch under
**Archive** and use **刷新文件** to read the latest contents.

### Assign one writer

Set `write_mode` to `exclusive` (the default) when binding each alias.
The first binding sets the mode for the branch. A second alias must specify
the same mode.

```sh
# Set BRANCH_ID to the existing branch for exclusive writes.
bind_volume owner-a exclusive
bind_volume owner-b exclusive
manage '{"action":"acquire","bucket":"owner-a"}'
manage '{"action":"handoff","from":"owner-a","to":"owner-b"}'
manage '{"action":"status","bucket":"owner-a"}'
```

Before handing off, stop new writes on the outgoing mount and check the results
of `fsync` and close. The outgoing alias remains readable. To grant its client
ownership again, create a fresh alias and remount it before the next handoff.
Retries from the previous mount keep their old alias and cannot publish.

## Storage and protocol

`meta.db` stores the engine metadata. `s3-sessions.sqlite3` stores CSI mount
bindings, write modes, holders and epochs. Each bound branch stores its workspace ID once. The dashboard and management API read current workspace and branch names from the engine. A per-branch file lock orders ownership changes with S3 publications.
Back up both databases while the service is stopped and retain the corresponding
file content (the local `objects/` directory for local workspaces or the selected
remote content backend). The server currently
supports one active engine per data directory.

Clients can probe a bucket, list objects (V1/V2), read byte ranges, create
directory markers, and write, copy or delete objects. The gateway supports up
to 64 MiB per uploaded object and eight unfinished multipart uploads. Uploads expire after one hour without a successful part, and the service releases abandoned parts from memory. Clients start a new upload after expiration or restart.

GET streams the requested byte range from one file version. Single and batch deletes check ETag and size under the branch reservation. Requests to delete a specific version or to match a deletion timestamp return an error. Source ETag and date conditions apply to both copy operations.

For standalone use, create a local workspace and branch with `agentdrived`, record their IDs, and stop that daemon. Run `drive-s3 --data-dir <daemon-data-directory>` (the default is `<init-directory>/state`) to serve that local workspace. Its loopback ports are 19731 (S3) and 19732 (admin). Use `agentdrived` when the workspace uses remote file storage.

## Verification

```sh
cargo test --locked -p drive-s3-store
cargo build --locked -p agentdrived
python3 tests/drive_s3_smoke.py --daemon target/debug/agentdrived
```

The HTTP test uses boto3. It verifies dashboard reads of S3 writes, both write
modes, ranges, copy, multipart completion, listing, and persistence after restart.
