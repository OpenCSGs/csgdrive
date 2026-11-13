use crate::workspace_view::WorkspaceView;
use agentdrive_config::{ConfigStore, Settings, SettingsUpdate};
use agentdrive_model::*;
use agentdrive_ports::*;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

#[cfg(test)]
#[path = "merge_history_tests.rs"]
mod merge_history_tests;

pub(crate) fn api(services: Services) -> Router {
    Router::new()
        .merge(crate::collaboration::api())
        .route("/api/sandboxes", get(sandboxes))
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox/force-stop",
            post(sandbox_force_stop),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox/proxy",
            get(sandbox_proxy_config).put(sandbox_set_proxy_config),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox/delete",
            post(sandbox_delete),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox/desktop/{*path}",
            get(sandbox_desktop),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox/mcp-config",
            get(sandbox_mcp_config),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/sandbox",
            get(sandbox_status)
                .post(sandbox_create)
                .delete(sandbox_stop),
        )
        .route("/api/workspaces", get(workspaces))
        .route("/api/workspaces/{workspace}", get(workspace))
        .route("/api/workspaces/{workspace}/archive", get(archive))
        .route("/api/workspaces/{workspace}/merges", get(merge_history))
        .route(
            "/api/workspaces/{workspace}/merges/{record}",
            get(merge_detail),
        )
        .route(
            "/api/workspaces/{workspace}/merges/{record}/events",
            get(merge_events),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/history",
            get(branch_history),
        )
        .route(
            "/api/workspaces/{workspace}/relationships",
            get(relationships),
        )
        .route(
            "/api/workspaces/{workspace}/revisions/{revision}/changes",
            get(revision_changes),
        )
        .route(
            "/api/workspaces/{workspace}/revisions/{revision}/diff",
            get(revision_diff),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/files",
            get(branch_files),
        )
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/file",
            get(branch_file),
        )
        .route("/api/lifecycle", post(lifecycle))
        .route("/api/commands/{command}", post(management_command))
        .with_state(services)
}

async fn management_command(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(command): Path<String>,
    Json(arguments): Json<Value>,
) -> std::result::Result<Json<Value>, ApiError> {
    if !matches!(
        command.as_str(),
        "workspace_delete"
            | "workspace_discover"
            | "branch_create"
            | "branch_switch"
            | "mount_prepare"
            | "mount_save"
            | "mount_unmount"
            | "mount_release"
            | "operation_get"
            | "operation_by_request"
    ) {
        return Err(Error::new(ErrorCode::NotFound, "unknown management command").into());
    }
    Ok(Json(
        crate::McpServer::new(services, actor)
            .dispatch(&command, arguments)
            .await?,
    ))
}

pub(crate) fn settings_api(settings: Arc<ConfigStore>) -> Router {
    Router::new()
        .route("/api/settings", get(get_settings).put(update_settings))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(settings)
}

async fn get_settings(
    State(settings): State<Arc<ConfigStore>>,
) -> std::result::Result<Json<Settings>, ApiError> {
    Ok(Json(settings.settings()))
}

async fn update_settings(
    State(settings): State<Arc<ConfigStore>>,
    Json(update): Json<SettingsUpdate>,
) -> std::result::Result<Json<Settings>, ApiError> {
    let result = tokio::task::spawn_blocking(move || settings.update(update))
        .await
        .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
    Ok(Json(result))
}

pub(crate) fn assets() -> Router {
    Router::new()
        .route("/sandboxes", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/sandboxes.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/sandboxes.js")) }))
        .route("/sandboxes.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/sandboxes.css")) }))
        .route("/sandbox.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/sandbox.js")) }))
        .route("/favicon.svg", get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], include_str!("../dashboard/favicon.svg")) }))
        .route("/", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/archive", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/settings", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/dashboard", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/mounts", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/branches", get(|| async { Html(include_str!("../dashboard/index.html")) }))
        .route("/management.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/management.js")) }))
        .route("/collaboration.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/collaboration.js")) }))
        .route("/commands.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/commands.js")) }))
        .route("/management.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/management.css")) }))
        .route("/workspace-actions.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/workspace-actions.js")) }))
        .route("/navigation.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/navigation.js")) }))
        .route("/navigation.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/navigation.css")) }))
        .route("/ui.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/ui.js")) }))
        .route("/ui.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/ui.css")) }))
        .route("/dashboard.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/dashboard.js")) }))
        .route("/history.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/history.js")) }))
        .route("/merges.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/merges.js")) }))
        .route("/merge-detail.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/merge-detail.js")) }))
        .route("/merge-presentation.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/merge-presentation.js")) }))
        .route("/merges.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/merges.css")) }))
        .route("/sync-status.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/sync-status.js")) }))
        .route("/workspace.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/workspace.js")) }))
        .route("/relationships.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/relationships.js")) }))
        .route("/relationship-detail.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/relationship-detail.js")) }))
        .route("/settings.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/settings.js")) }))
        .route("/revisions.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/revisions.js")) }))
        .route("/branch-files.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/branch-files.js")) }))
        .route("/changes.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/changes.js")) }))
        .route("/archive.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/archive.js")) }))
        .route("/file-browser.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/file-browser.js")) }))
        .route("/file-preview.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../dashboard/file-preview.js")) }))
        .route("/archive.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/archive.css")) }))
        .route("/dashboard.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../dashboard/dashboard.css")) }))
        .layer(axum::middleware::map_response(|mut response: Response| async move {
            response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'".parse().unwrap());
            response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
            response.headers_mut().insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
            response
        }))
}

pub(crate) struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::InvalidArgument => StatusCode::BAD_REQUEST,
            ErrorCode::Io | ErrorCode::Integrity => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        };
        (status, Json(json!({"error": self.0}))).into_response()
    }
}

#[derive(Default, Deserialize)]
struct CatalogQuery {
    #[serde(default)]
    discover: bool,
}

async fn workspaces(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Query(query): Query<CatalogQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    let listed = if query.discover {
        services.workspace.discover(&actor).await?
    } else {
        services.workspace.list(&actor).await?
    };
    Ok(Json(json!(listed)))
}

async fn merge_history(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(workspace): Path<WorkspaceId>,
    Query(filter): Query<MergeHistoryFilter>,
) -> std::result::Result<Json<MergeHistoryPage>, ApiError> {
    Ok(Json(
        services
            .merge_history
            .list(&actor, workspace, filter)
            .await?,
    ))
}

async fn merge_detail(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, record)): Path<(WorkspaceId, String)>,
) -> std::result::Result<Json<MergeHistoryDetail>, ApiError> {
    Ok(Json(
        services
            .merge_history
            .detail(&actor, workspace, &record)
            .await?,
    ))
}

async fn merge_events(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, record)): Path<(WorkspaceId, String)>,
    Query(query): Query<MergeEventsQuery>,
) -> std::result::Result<Json<MergeHistoryEvents>, ApiError> {
    Ok(Json(
        services
            .merge_history
            .events(&actor, workspace, &record, query)
            .await?,
    ))
}

async fn workspace(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(workspace): Path<WorkspaceId>,
) -> std::result::Result<Json<WorkspaceView>, ApiError> {
    let remote_refresh = services
        .workspace
        .request_history_refresh(&actor, workspace)
        .await?;
    let status = services.workspace.status(&actor, workspace).await?;
    let mut ids = std::collections::BTreeSet::from([status.workspace.initial_revision]);
    for branch in &status.branches {
        ids.extend([branch.formal_head, branch.base_revision]);
        ids.extend(branch.autosave_head);
    }
    for branch in &status.remote_branches {
        ids.insert(branch.formal_head);
        ids.extend(branch.autosave_head);
        ids.extend(
            branch
                .metadata
                .as_ref()
                .map(|metadata| metadata.base_revision),
        );
    }
    let revisions = services
        .revisions
        .metadata(&actor, workspace, ids.into_iter().collect())
        .await?;
    let mut view = WorkspaceView::new(status, revisions);
    view.remote_error = remote_refresh.error.clone();
    view.remote_refresh = Some(remote_refresh);
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    search: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn branch_history(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Query(query): Query<HistoryQuery>,
) -> std::result::Result<Json<RevisionHistoryPage>, ApiError> {
    Ok(Json(
        services
            .revisions
            .history(
                &actor,
                RevisionHistoryQuery {
                    workspace,
                    branch,
                    search: query.search,
                    cursor: query.cursor,
                    limit: query.limit,
                },
            )
            .await?,
    ))
}

// File browsing only needs current workspace state, independent of history size.
async fn archive(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(workspace): Path<WorkspaceId>,
) -> std::result::Result<Json<WorkspaceStatus>, ApiError> {
    Ok(Json(services.workspace.status(&actor, workspace).await?))
}

async fn relationships(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(workspace): Path<WorkspaceId>,
    Query(filter): Query<RelationshipFilter>,
) -> std::result::Result<Json<WorkspaceRelationships>, ApiError> {
    Ok(Json(
        services
            .workspace
            .relationships(&actor, WorkspaceRelationshipsQuery { workspace, filter })
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchFilesQuery {
    path: WorkspacePath,
    after: Option<WorkspacePath>,
    version: Option<String>,
}

async fn branch_files(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Query(query): Query<BranchFilesQuery>,
) -> std::result::Result<Json<BranchFiles>, ApiError> {
    Ok(Json(
        services
            .branch_files
            .list(
                &actor,
                ListBranchFiles {
                    workspace,
                    branch,
                    path: query.path,
                    after: query.after,
                    version: query.version,
                },
            )
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchFileQuery {
    path: WorkspacePath,
    version: String,
    offset: u64,
    size: u32,
}

async fn branch_file(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Query(query): Query<BranchFileQuery>,
) -> std::result::Result<Response, ApiError> {
    let bytes = services
        .branch_files
        .read(
            &actor,
            ReadBranchFile {
                workspace,
                branch,
                path: query.path,
                version: query.version,
                offset: query.offset,
                size: query.size,
            },
        )
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangesQuery {
    before: Option<RevisionId>,
    cursor: Option<WorkspacePath>,
    limit: Option<usize>,
}

async fn revision_changes(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, revision)): Path<(WorkspaceId, RevisionId)>,
    Query(query): Query<ChangesQuery>,
) -> std::result::Result<Json<RevisionDifferences>, ApiError> {
    Ok(Json(
        services
            .revisions
            .diff(
                &actor,
                CompareRevisions {
                    workspace,
                    before: query.before,
                    after: revision,
                    cursor: query.cursor,
                    limit: query.limit,
                },
            )
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDiffQuery {
    before: Option<RevisionId>,
    path: WorkspacePath,
    previous_path: Option<WorkspacePath>,
}

async fn revision_diff(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, revision)): Path<(WorkspaceId, RevisionId)>,
    Query(query): Query<FileDiffQuery>,
) -> std::result::Result<Json<RevisionTextDiff>, ApiError> {
    Ok(Json(
        services
            .revisions
            .text_diff(
                &actor,
                CompareRevisionFile {
                    workspace,
                    before: query.before,
                    after: revision,
                    path: query.path,
                    previous_path: query.previous_path,
                },
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct LifecycleInput {
    request_id: String,
    #[serde(flatten)]
    event: LifecycleEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum LifecycleEvent {
    Attach {
        workspace: WorkspaceId,
        mount_path: PathBuf,
        branch: Option<BranchId>,
        #[serde(default)]
        automatic: bool,
    },
    Check {
        workspace: WorkspaceId,
        mount_path: PathBuf,
    },
    Save {
        mount: MountId,
        expected_binding_generation: u64,
    },
}

fn checked(record: OperationRecord) -> Result<OperationRecord> {
    match record.error.clone() {
        Some(error) => Err(error),
        None => Ok(record),
    }
}

async fn lifecycle(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Json(input): Json<LifecycleInput>,
) -> std::result::Result<Json<Value>, ApiError> {
    let context = RequestContext {
        principal: actor,
        request_id: input.request_id,
    };
    context.validate()?;
    let value = match input.event {
        LifecycleEvent::Attach {
            workspace,
            mount_path,
            branch,
            automatic,
        } => {
            let record = checked(
                services
                    .mounts
                    .prepare(
                        context,
                        PrepareMount {
                            workspace,
                            branch,
                            revision: None,
                            path: mount_path,
                            access: AccessMode::ReadWrite,
                            durability: Durability::Local,
                            automatic,
                        },
                    )
                    .await?,
            )?;
            let Some(CommandResult::Mount { binding }) = record.result else {
                return Err(Error::new(ErrorCode::MountBusy, "mount is not ready").into());
            };
            json!({"mount_path": binding.path, "binding": binding})
        }
        LifecycleEvent::Check {
            workspace,
            mount_path,
        } => {
            let status = services
                .workspace
                .status(&context.principal, workspace)
                .await?;
            let binding = status
                .mounts
                .into_iter()
                .find(|mount| {
                    mount.path == mount_path
                        && mount.state == MountState::Ready
                        && mount.access == AccessMode::ReadWrite
                })
                .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "mount is not ready"))?;
            let collaboration = services
                .collaboration
                .status(&context.principal, workspace)
                .await?
                .into_iter()
                .find(|status| status.mounts.contains(&binding.id));
            json!({"binding": binding, "collaboration": collaboration})
        }
        LifecycleEvent::Save {
            mount,
            expected_binding_generation,
        } => {
            json!(checked(
                services
                    .mounts
                    .save(
                        context,
                        SaveMount {
                            mount,
                            expected_binding_generation,
                            durability: Durability::Local,
                            wait_for_remote_ms: None,
                            message: None,
                        }
                    )
                    .await?
            )?)
        }
    };
    Ok(Json(value))
}

async fn sandboxes(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
) -> std::result::Result<Json<Value>, ApiError> {
    let workspaces = services.workspace.list(&actor).await?;
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let mut template = None;
    let mut templates = Vec::new();
    if let Some(runtime) = &services.sandbox {
        match runtime.templates(&actor).await {
            Ok(value) => templates = value,
            Err(error) => errors.push(error.message),
        }
        match runtime.template(&actor).await {
            Ok(value) => template = value,
            Err(error) => errors.push(format!("模板信息：{}", error.message)),
        }
        for workspace in workspaces {
            let status = match services.workspace.status(&actor, workspace.id).await {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{}：{}", workspace.name, error.message));
                    continue;
                }
            };
            for branch in status.branches {
                if branch.state == BranchState::Deleted {
                    continue;
                }
                match runtime.status(&actor, workspace.id, branch.id).await {
                    Ok(sandbox) => entries.push(json!({
                        "workspace":workspace.id,"workspace_name":workspace.name,
                        "branch":branch.id,"branch_name":branch.name,"sandbox":sandbox,
                        "can_start":branch.state == BranchState::Writable && branch.owner == status.location,
                    })),
                    Err(error) if error.code == ErrorCode::PermissionDenied => {},
                    Err(error) => errors.push(format!("{} / {}：{}", workspace.name, branch.name, error.message)),
                }
            }
        }
    }
    Ok(Json(
        json!({"enabled":services.sandbox.is_some(),"entries":entries,"template":template,"templates":templates,"errors":errors}),
    ))
}

async fn sandbox_mcp_config(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
) -> std::result::Result<Json<SandboxMcpConfig>, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox runtime is not configured"))?;
    Ok(Json(runtime.mcp_config(&actor, workspace, branch).await?))
}

async fn sandbox_status(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
) -> std::result::Result<Json<Value>, ApiError> {
    services.workspace.status(&actor, workspace).await?;
    match services.sandbox {
        Some(runtime) => Ok(Json(
            json!({"enabled":true,"sandbox":runtime.status(&actor, workspace, branch).await?}),
        )),
        None => Ok(Json(json!({"enabled":false,"sandbox":null}))),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxCreateRequest {
    template_id: Option<String>,
}

async fn sandbox_create(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    request: Option<Json<SandboxCreateRequest>>,
) -> std::result::Result<Json<SandboxStatus>, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox runtime is not configured"))?;
    Ok(Json(
        runtime
            .create_with_template(
                &actor,
                workspace,
                branch,
                request.and_then(|request| request.0.template_id),
            )
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxDeleteRequest {
    confirm_branch: BranchId,
}

async fn sandbox_delete(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Json(request): Json<SandboxDeleteRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    if request.confirm_branch != branch {
        return Err(Error::invalid("删除确认与目标分支不一致").into());
    }
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox runtime is not configured"))?;
    runtime.delete(&actor, workspace, branch).await?;
    Ok(Json(
        json!({"deleted":true,"workspace":workspace,"branch":branch}),
    ))
}

async fn sandbox_stop(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
) -> std::result::Result<Json<SandboxStatus>, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox runtime is not configured"))?;
    Ok(Json(runtime.stop(&actor, workspace, branch).await?))
}

async fn sandbox_force_stop(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Json(request): Json<SandboxDeleteRequest>,
) -> std::result::Result<Json<SandboxStatus>, ApiError> {
    if request.confirm_branch != branch {
        return Err(Error::invalid("强制停止确认与目标分支不一致").into());
    }
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox 尚未配置"))?;
    Ok(Json(runtime.force_stop(&actor, workspace, branch).await?))
}

async fn sandbox_desktop(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch, path)): Path<(WorkspaceId, BranchId, String)>,
    mut request: axum::extract::Request,
) -> std::result::Result<Response, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox runtime is not configured"))?;
    if path != "websockify" {
        let content = runtime
            .desktop_asset(&actor, workspace, branch, &path)
            .await?;
        let mime = match path.rsplit('.').next().unwrap_or("") {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "json" => "application/json",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        };
        return Ok((
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            content,
        )
            .into_response());
    }
    if !request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return Err(Error::invalid("Desktop connection requires WebSocket").into());
    }
    request.headers_mut().remove(header::COOKIE);
    request.headers_mut().remove(header::AUTHORIZATION);
    let stream = runtime.desktop(&actor, workspace, branch).await?;
    let client_upgrade = hyper::upgrade::on(&mut request);
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    *request.uri_mut() = format!("/{path}{query}")
        .parse()
        .map_err(|_| Error::invalid("Invalid desktop path"))?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|error| Error::new(ErrorCode::Unavailable, error.to_string()))?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|error| Error::new(ErrorCode::Unavailable, error.to_string()))?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let guest_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            if let (Ok(client), Ok(guest)) = tokio::join!(client_upgrade, guest_upgrade) {
                let _ = tokio::io::copy_bidirectional(
                    &mut hyper_util::rt::TokioIo::new(client),
                    &mut hyper_util::rt::TokioIo::new(guest),
                )
                .await;
            }
        });
    }
    Ok(response.map(axum::body::Body::new))
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod sandbox_tests;

async fn sandbox_proxy_config(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
) -> std::result::Result<Json<SandboxProxyConfig>, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox 尚未配置"))?;
    Ok(Json(runtime.proxy_config(&actor, workspace, branch).await?))
}
async fn sandbox_set_proxy_config(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
    Json(config): Json<SandboxProxyConfig>,
) -> std::result::Result<Json<Value>, ApiError> {
    let runtime = services
        .sandbox
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "Sandbox 尚未配置"))?;
    runtime
        .set_proxy_config(&actor, workspace, branch, config)
        .await?;
    Ok(Json(json!({"saved":true,"restart_required":true})))
}
