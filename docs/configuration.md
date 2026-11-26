# Configuration and workspace storage

[English](configuration.md) | [简体中文](configuration.zh-CN.md)

## Start the daemon

```sh
agentdrived init --directory /srv/agentdrive
agentdrived serve --config /srv/agentdrive/agentdrive.json
```

Open the dashboard at `http://127.0.0.1:7421/`. The CLI and MCP clients use the same process and workspace database. To also serve S3 clients such as a Kubernetes CSI driver, start the daemon with a reachable S3 listener and set its signing credentials in the service environment.

```sh
agentdrived serve --config /srv/agentdrive/agentdrive.json \
  --drive-s3-listen 10.0.0.10:19731 --drive-s3-admin-port 19732
```

`DRIVE_S3_ACCESS_KEY` and `DRIVE_S3_SECRET_KEY` identify clients of this endpoint. Configure the CSI driver with these credentials. The separate credentials for storing file contents in an upstream bucket belong to the daemon.

## Startup and runtime settings

`agentdrive.json` supplies the startup parameters. `init` creates the file and the directories in the table. Paths must be absolute. Each database directory has one active daemon.

| Startup field | Meaning |
| --- | --- |
| `data_dir` | SQLite database, file data/cache, and staging directory. Default `<directory>/state`. |
| `listen` | Dashboard and MCP loopback address. Default `127.0.0.1:7421`. |
| `mount_roots` | Permitted local mount roots. Default `<directory>/mounts`. |
| `directory_roots` | Permitted import/export roots. Default `<directory>/exchange`. |
| `principal` | Identity used by the daemon's clients. Default `owner`. |

Startup changes take effect after restart. A new database initializes its cache, collaboration and HTTP origin settings from the file. Subsequent runtime changes use SQLite. Open **Settings** to view the current values. Workspace storage is selected in **Create workspace**.

- `GET /api/settings` returns `startup`, `runtime`, `revision` and the config file path. It redacts credentials.
- To update runtime settings, send the complete `runtime` object and its current `revision` to `PUT /api/settings`. A stale revision returns 409. On success, subsequent requests and restarts use the new values.
- `allowed_origins` accepts exact HTTP(S) origins. `cache.max_bytes` defaults to 2 GiB and limits the local cache of immutable objects. The collector checks every 30 seconds. Reachable objects needed locally and files with active readers remain available.
- `collaboration.enabled` defaults to true. It controls automatic coordination for native writable mounts. S3 mount sessions use the branch writer recorded by the S3 management API.

## Select a workspace backend

Create the workspace in the dashboard or send `POST /api/workspaces` with `request_id`, `name`, and an optional `storage` object. Retry an interrupted request with the same `request_id`. Omitting `storage` selects local file storage. Each workspace keeps its original storage destination. **Settings → Storage** displays it. The settings API supports credential rotation for the same destination.

The database records the workspace, branches, file paths and versions. It maps each file to its immutable content identifier. S3 and OpenCSG provide the bytes for that identifier. A backup needs a consistent SQLite backup, the local storage directory for local-only files and pending changes, and the configured upstream content. Keep the database directory private because saved backend credentials are stored there.

### S3 and OSS

Create the destination bucket before selecting it. The following example is a request body for the workspace creation endpoint.

```json
{
  "request_id": "create-shared",
  "name": "shared",
  "storage": {
    "backend": "s3",
    "bucket": "drive-content",
    "prefix": "agentdrive/v1",
    "region": "us-east-1",
    "endpoint": "http://127.0.0.1:9000",
    "allow_http": true
  }
}
```

| Storage field | Meaning |
| --- | --- |
| `bucket` | Required existing bucket. |
| `prefix` | Defaults to `agentdrive/v1`. File objects use `<prefix>/<workspace UUID>/objects/<content ID>`. |
| `region` | Signing region, defaults to `us-east-1`. |
| `endpoint` | API origin without the bucket name. Path-style requests append the bucket. Without a configured endpoint, the daemon consults `AWS_ENDPOINT_URL_S3`, `AWS_ENDPOINT_URL`, then `AWS_ENDPOINT`, then the AWS regional endpoint. |
| `allow_http` | Defaults to false. Set true for an HTTP test endpoint. |
| `credentials` | Optional `access_key_id`, `secret_access_key`, and `session_token`. Omit to use the service environment or its IAM role. |

Set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and, for temporary credentials, `AWS_SESSION_TOKEN` in the daemon's service environment. A configured credential takes precedence. Environment credentials are read when the client is created. The adapter also supports web identity, container roles and instance metadata. It uses strongly consistent GET/HEAD, conditional creation, and multipart upload above 8 MiB. Filesystem paths and branch names are resolved by AgentDrive from SQLite.

### OpenCSG Codes and optional XNet

Create a Codes repository with an initialized Git branch. In the creation request, use this `storage` object or enter the same values in the dashboard.

```json
{
  "backend": "opencsg",
  "endpoint": "https://hub.example.org",
  "repository": "team/drive-content",
  "branch": "main",
  "prefix": "agentdrive/v1",
  "token": "<repository-token>",
  "xnet": false
}
```

The token must permit reading and writing repository files. The endpoint is an HTTP(S) origin. HTTP requires `allow_http: true`. The Git storage branch defaults to `main` and the prefix to `agentdrive/v1`. Each AgentDrive workspace branch keeps its identity and history in SQLite. Regular files are represented by SHA-256-verified chunks of up to 4 MiB in the repository. The chunk list and its order are in the server database.

To use XNet, enable it on the Codes repository, select the XNet checkbox at workspace creation, and install the transfer dependency on the daemon host.

```sh
python3 -m venv /opt/agentdrive-xnet
/opt/agentdrive-xnet/bin/python -m pip install 'hf-xet==1.1.10'
export AGENTDRIVE_XNET_PYTHON=/opt/agentdrive-xnet/bin/python
```

Start the daemon with this environment. Codes classifies each nonempty file with its preupload API. For an LFS file the daemon uploads the bytes through XNet and writes a standard LFS pointer to the repository. Regular files follow the chunk path. AgentDrive verifies the downloaded size and content identifier.

## Share one branch across machines

Point every dashboard, CLI and MCP client at the same daemon. For CSI mounts, bind a different bucket alias per mount to the same workspace and branch on that daemon. Choose `write_mode: "serialized"` for a shared editing session. The service serializes object writes; a later successful write to the same object replaces the previous content. Reopen the file on the other mount or refresh the dashboard file browser. For explicit ownership, choose `exclusive`, flush pending writes and use the management endpoint to hand off the branch. A retired mount needs a new session before regaining exclusive write access.

An independent daemon using a different database has its own workspace catalog even if it uses the same S3 bucket or Git repository. Restore the database and its content when moving the service. See the [architecture guide](architecture.md) and [S3 gateway guide](../apps/drive-s3/README.md).
