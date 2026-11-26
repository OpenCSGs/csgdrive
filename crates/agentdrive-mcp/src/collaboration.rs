use crate::web::ApiError;
use agentdrive_model::*;
use agentdrive_ports::*;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    routing::{get, put},
};

pub(crate) fn api() -> Router<Services> {
    Router::new()
        .route("/api/workspaces/{workspace}/collaboration", get(status))
        .route("/api/collaboration", put(configure))
}

async fn status(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Path(workspace): Path<WorkspaceId>,
) -> std::result::Result<Json<Vec<CollaborationStatus>>, ApiError> {
    Ok(Json(
        services.collaboration.status(&actor, workspace).await?,
    ))
}

async fn configure(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Json(request): Json<ConfigureCollaboration>,
) -> std::result::Result<Json<CollaborationStatus>, ApiError> {
    Ok(Json(
        services.collaboration.configure(&actor, request).await?,
    ))
}
