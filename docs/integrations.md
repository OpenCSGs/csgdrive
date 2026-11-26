# Agent integrations and dashboard

[English](integrations.md) | [简体中文](integrations.zh-CN.md)

Start `agentdrived`; the local connection needs no token. Install GeeseFS with FUSE on Linux, GeeseFS with macFUSE on macOS, or WinFsp on Windows. Mount paths must be inside the daemon's `mount_roots`. The agent's execution environment must be able to access the mounted directory.

## MCP and mounted directories

Configure an existing agent with the stdio MCP command `agentdrive mcp`, or connect to the daemon's `/mcp` HTTP endpoint. MCP connections share the long-running daemon.

```sh
agentdrive mount --workspace "$WORKSPACE" "$AGENTDRIVE_DIRECTORY/mounts/project"
```

The corresponding MCP tool is `mount_prepare`, with these minimum arguments:

```json
{
  "request_id": "mount-project-1",
  "request": {
    "workspace": "<workspace-id>",
    "path": "/absolute/path/to/project"
  }
}
```

A `location_id` belongs to the daemon data directory and survives restarts. Repeated native mounts reuse the selected branch. To share a branch across remote sandboxes, give each CSI volume its own bucket alias on the same AgentDrive service. Each alias maps to the same workspace and branch IDs. The service dashboard reads that branch directly. [S3 mount setup](../apps/drive-s3/README.md)

Identical mount requests at the same path from the same principal return the existing binding, allowing multiple agents to share the directory. A different principal cannot reuse that binding, including one with `manage` permission. File edits and branch switches affect everyone using that directory. A branch can have one writable desktop directory and a separate mount for its local sandbox. Unmount the desktop directory before attaching it at a different path. Unmounting and agent disconnection preserve the selected branch. Explicit CLI or MCP branch creation provides separate working content.

MCP returns `result.binding`, including `id`, `branch`, `path`, and `generation`. Agents use the returned path for file operations. Connecting MCP does not change an agent's working directory or filesystem permissions. `mount_save` accepts `mount` and `expected_binding_generation` to save the mounted branch.

## Convert an existing directory

Use `workspace_adopt_directory` to mount an existing local directory at the same path. By default, it creates a workspace named after the directory, owned by the caller, with the original contents as the initial revision of `main`. Supply `request.workspace` to import into a new branch of an existing workspace; this requires `manage` permission. The path must be strictly below a configured `mount_roots` entry and within `directory_roots`; its parent must exist. Native mount support must be available. Existing mounts, including overlapping parent or child paths, must be released first.

Pause all file writes before calling this tool. Set `restart_agent: true` to acknowledge that the Agent and its terminals must restart after conversion. The tool does not terminate or launch those processes. Old working-directory references and file handles can still refer to the backup; do not perform further file operations through the running Agent after conversion.

```json
{
  "request_id": "adopt-project-1",
  "request": {
    "path": "/absolute/path/to/project",
    "restart_agent": true
  }
}
```

The tool imports the directory using the same validation as `workspace_import`, then moves the original directory to the sibling `.agentdrive-backup-<operation-id>/original` and mounts the imported branch at the original path. Unsupported files, including symbolic links, fail import before the directory is moved. When supplying a workspace ID, existing workspace content and the default branch selection are preserved; the new branch is explicitly mounted at `path`.

Success requires `phase: "complete"`, `mount_ready: true`, and no `error`. The operation returns the workspace ID in `workspace`. The result contains `branch`, `path`, `backup_path`, `binding`, and `restart_required: true`. Restart the Agent with the original path as its working directory. For later explicit mounting, use the returned branch ID. Keep the backup until the restarted Agent has verified its files; backup cleanup is manual.

WinFsp creates its own directory mount point. On Windows, AgentDrive removes the validated empty placeholder immediately before starting the driver, then restores it if startup fails. Existing files and reparse points are rejected, and the original imported files remain in the backup. The successful mount uses the same original path. See the [WinFsp directory mount implementation](https://github.com/winfsp/winfsp/blob/v2.1/src/dll/mount.c#L439).

If mounting fails after the move, the original files remain at `backup_path`; the original path may be empty. Pause file access and retry with the same request ID and identical arguments, including after daemon restart. A completed retry reuses the imported branch. Inspect `operation_get` or `operation_by_request` after a lost response. If import itself failed, correct the source and use a new request ID. Do not start another conversion on an already moved path.

## Saving mounted files

Agents can read and write the mounted directory through their ordinary file tools. Flush changed files before calling `mount_save` with the current mount ID and generation. A successful operation reports `local_saved`. Inspect `remote_confirmed` when requesting remote durability.

## Branches and dashboard

`agentdrive branch create` and MCP `branch_create` explicitly create branches while preserving the selection. `agentdrive branch switch` and MCP `branch_switch` save the previous branch and update this location's workspace selection. CLI and MCP complete the switch in one call. On Windows, retry the same request after closing any file or directory that WinFsp reports as busy. Other errors preserve their specific cause.

Existing databases retain their branches and selections. With no selection, one writable local branch is reused automatically; multiple writable local branches require an explicit `branch_switch`.

Run `agentdrive dashboard --open` or open the daemon URL directly. The left panel lists all workspaces and supports searching and collapsing. Selecting a workspace opens its archive and the adjacent menu provides Archive, Branches & Versions, Mount management and Branch management. The root URL opens the first workspace archive; explicit workspace and file links remain supported. Global settings are at the bottom left. Pages refresh every 2 seconds.

The **Merge records** tab shows merges performed through MCP, including preparation, candidate edits, application, failures and mount recovery. Filter by branch, source, target, location, status or time, and load further pages as needed. Each record retains its source revision and target revision at preparation time. Its detail view provides an action timeline and file comparisons against the original target, source, base or previous candidate. Applied merge revisions link back to their record from version history.

Local saving, remote confirmation and mount readiness have separate indicators. Clients of the service can review the same records and candidate content; their mount indicators describe the recorded operation. Older records identify incomplete history. A resolved file conflict does not establish that the article or business decision has been reviewed. These views use read-only `GET /api/workspaces/{workspace}/merges`, `GET /api/workspaces/{workspace}/merges/{record}` and `GET /api/workspaces/{workspace}/merges/{record}/events` endpoints, with paginated records and events and existing branch read permissions.

Read-only endpoints are `GET /api/workspaces` and `GET /api/workspaces/{workspace}`. The latter returns `status`, `revisions`, `summary` and `origins`. `summary` contains `branch_count`, `mount_count`, `ready_mount_count` and `mounted_branch_count`. `status.mounts` contains unreleased bindings sorted by path. `origins` identifies each branch's source branch and revision, revision availability, and whether it is the workspace root. `selected_branch` is this location's default selection for the workspace. Global runtime settings are available at `/settings`; see [configuration](configuration.md).


The cards show active branches, local mount bindings, distinct branches used by ready mounts, and remote synchronization for the selected workspace. Released bindings are excluded; preparing, paused and recovery-required bindings remain visible with their state. The branch graph connects each branch to its exact creation revision using `source_branch` and `base_revision`; source revisions can be commits, autosaves or checkpoints. Click a source revision to inspect it. **Default** marks the local selection for requests that omit a branch; mounted badges count ready directories.

**Mount management** (`/mounts`) selects a branch, local directory and access mode to create a mount. Existing bindings support save and unmount actions. A branch permits one local writable binding. Paths must be within the daemon's configured `mount_roots`; operations apply to the machine running that daemon.

The **Branch and mount relationships** panel on this page shows branch ownership, local paths and principals, access modes, mount states and creation origins. Search branch names, local paths, principals or partial IDs, and filter by branch, owner location or local mounting status. Advanced filters include exact state, mounting location, full mount ID and released bindings. Filters persist in the URL; reset returns to all branches. Pages contain at most 50 rows. **Load more** continues the query, while refresh retains filters and starts from the first page. Record details show the observed storage version/time, and creation details show the captured content root. Remote mounts, selections and machine availability remain unknown.

**Branch management** (`/branches`) creates a named branch from selected source content. Local branches can use `source_branch` to capture current working content, including uncommitted changes. Use `source` to fork a saved revision in the workspace. These two parameters are mutually exclusive. The list shows all known branches and their origins, with switch and mount links for locally writable branches. Switching updates the local selection and any mounted directory using the previous selected branch.

The pages call existing MCP services through `POST /api/commands/{command}`. Allowed commands are `branch_create`, `branch_switch`, `mount_prepare`, `mount_save`, `mount_unmount`, `mount_release`, `operation_get` and `operation_by_request`, with the same argument shapes as the MCP tools. The daemon identity and binding generation checks apply. Progress separately reports `local_saved`, `remote_confirmed` and `mount_ready`. Each action retains its `request_id` across reloads and interrupted responses so the browser can query or retry the original request. The page displays recovery controls for operations that request a mount release.

The branch badge distinguishes unsaved changes (`mutation_seq != saved_seq`), automatically saved but uncommitted changes (matching sequences with `dirty`), and committed content (matching sequences without `dirty`). Saved content has a durable local checkpoint. The tooltip identifies checkpoints awaiting an autosave revision; remote confirmation appears in the synchronization card. Stopped writers have a separate indicator.

Version history lists only revisions created on the selected branch. Select the source branch to browse its history. A branch with no revisions of its own shows an empty history.

Double-click a history entry, or focus it and press Enter or Space, to open **Version details**. The dialog shows version metadata above a paginated changed-file list and automatically displays the first entry’s details and text differences on the right. Source revisions in the branch graph open the same dialog. Loading more entries preserves the current selection. Automatic refresh updates the current-commit marker in an open dialog while preserving the selected file and loaded differences. Close the dialog with its close button, Escape, or a click outside it. Comparison always uses the parent revision, or the first parent for a merge. Initial revisions compare against an empty tree. The previous version is determined by parent relationships. Exact-content moves with a unique source and destination are marked as renames. Ambiguous moves and moves with edited content appear as additions/deletions. Added and deleted empty files are explicitly labeled as empty (0 bytes); unchanged content from a rename or attribute change is labeled separately. Binary and oversized files show metadata. Browsing does not change mounts or the selected working branch.

Revision endpoints are `GET /api/workspaces/{workspace}/revisions/{revision}/changes` and `GET /api/workspaces/{workspace}/revisions/{revision}/diff`; both use the local daemon identity and require workspace read access. `changes` accepts `before`, `cursor`, `limit`; `diff` accepts `before`, `path`, `previous_path`. Omitted `before` selects an empty base. See [MCP parameters](mcp.md) for response fields.

Double-click a branch, or focus it and press Enter or Space, to open **Branch files**. Browse directories on the left, including hidden files; the first ordinary file is previewed automatically on the right. Reads include current unsaved and uncommitted changes and also work for unmounted local branches. Text is displayed in 64 KiB chunks with previous/next controls. Binary and non-UTF-8 bytes use hexadecimal previews. Directories contain up to 200 entries per page, with a load-more control and parent/root navigation.

If the branch changes during browsing, refresh the file dialog to read its latest contents. Browsing does not change mounts, create revisions or trigger checkpoints. `GET /api/workspaces/{workspace}/branches/{branch}/files` accepts `path`, optional `after` and `version`, returning `entries`, `next` and `version`. Subsequent directory pages and file reads use the same version; a changed branch returns HTTP 409. `GET /api/workspaces/{workspace}/branches/{branch}/file` accepts `path`, `version`, `offset` and `size`, returning raw bytes with a per-read limit of 1–262144 bytes. Both endpoints enforce workspace membership and branch read permission, and disable response caching.

## Archive

The default dashboard landing page is `/archive`, above Branches & Versions in the navigation. Select a project and branch to read its current files without changing the mounted branch. The page uses `GET /api/workspaces/{workspace}/archive` for current workspace status without loading revision history, and shares `BranchFilesApi` directory and file reads with the branch dialog. Filtering applies to loaded entries in the current directory; hidden files can be revealed. Markdown uses safe document text with a source toggle. Project, branch and directory are preserved in the URL.

Remote confirmation requires a known remote reference with matching owner, authority epoch, generation, durable sequence and working root, plus locally saved changes. An empty queue alone does not establish remote saving. Status reflects the last known confirmation. To read the latest changes from a CSI client, reopen the file in the other sandbox or click **刷新文件** in the dashboard.

## Workspace discovery and deletion

The dashboard lists workspaces from the service database. Select a workspace to view its branches, or use `workspace_discover` through the same service’s MCP endpoint.

Each workspace row provides a deletion button and a confirmation dialog requiring its name. `workspace_delete` uses the standard request envelope with `request: {"workspace":"<workspace-id>"}`. It requires workspace management permission, released local mounts, and completed local saving/publication. The service records the deletion in SQLite while the publication barrier is idle. Retry an uncertain or rejected request with the same request ID after resolving the reported condition.

Deletion applies to all clients of that service. A deleted workspace disappears from the catalog and subsequent reads and writes are rejected. The service retains the deletion record, stored objects and Git history.
