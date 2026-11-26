# Drive S3

The local `agentdrived` process serves a path-style S3 API alongside its dashboard and MCP tools. Linux and macOS desktop mounts use GeeseFS. Local Sandbox guests access a host GeeseFS mount through virtio-fs. A signed S3 client can use this API directly.

## Connect an external S3 client

For desktop use, start `agentdrived serve --config <config>`. It opens a private loopback listener and persists the selected port and a generated signing secret in the local SQLite catalog. To expose a stable endpoint to a service, supply the listen address and credentials:

```sh
make -j8 build
target/debug/agentdrived init --directory ./drive --listen 127.0.0.1:19743
export DRIVE_S3_ACCESS_KEY=drive-client
export DRIVE_S3_SECRET_KEY="$(openssl rand -hex 32)"
target/debug/agentdrived serve --config ./drive/agentdrive.json \
    --drive-s3-listen 127.0.0.1:19741 --drive-s3-admin-port 19742
```

Use a private network address reachable by the external client for `--drive-s3-listen`. The management endpoint listens on loopback. The S3 credentials grant access to the bound aliases on this daemon. The desktop, S3 API and dashboard use the same local engine and SQLite catalog. A branch belongs to one workspace and inherits its content backend. Available destinations include local files, S3, Scion and OpenCSG Codes with optional XNet.

Create the workspace and branch in the dashboard. In **Archive**, select the
branch and copy the `workspace` and `branch` UUIDs from the URL. Bind each
CSI volume to those IDs with a distinct bucket alias and static PV volume
handle. Set the CSI bucket to that alias and configure the path-style endpoint
and region. An unknown ID or a branch that belongs to another workspace
causes the bind to fail.

### Access the same branch through two S3 clients

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

A direct S3 client can now read and write the branch through either alias:

```sh
AWS_ACCESS_KEY_ID="$DRIVE_S3_ACCESS_KEY" AWS_SECRET_ACCESS_KEY="$DRIVE_S3_SECRET_KEY" \
    aws --region us-east-1 --endpoint-url http://127.0.0.1:19741 s3 ls s3://desktop-a/
```

Omit `write_mode` to use serialized writes. Set `read_only: true` to bind a read-only alias. To expose a saved revision, also pass its UUID as `revision`. A revision alias stays read-only and keeps returning that revision as the branch changes.

### Assign one writer

Set `write_mode` to `exclusive` when binding each alias.
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

## Verification

```sh
cargo test --locked -p drive-s3-store
cargo build --locked -p agentdrived
python3 tests/drive_s3_smoke.py target/debug/agentdrived
```

The HTTP test uses boto3. It verifies dashboard reads of S3 writes, both write
modes, ranges, copy, multipart completion, listing, and persistence after restart.
