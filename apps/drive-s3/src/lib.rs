//! S3 data and administration endpoints for a shared AgentDrive engine.
mod listing;
mod multipart;
mod s3;

use agentdrive_model::{BranchId, WorkspaceId};
use axum::error_handling::HandleError;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
pub use drive_s3_store::Store;
use drive_s3_store::{SessionStatus, WriteMode};
use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
use serde::Deserialize;
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum Command {
    Bind {
        bucket: String,
        workspace: String,
        branch: String,
        #[serde(default)]
        write_mode: WriteMode,
    },
    #[serde(rename = "bind_ids")]
    BindIds {
        bucket: String,
        workspace_id: WorkspaceId,
        branch_id: BranchId,
        #[serde(default)]
        write_mode: WriteMode,
    },
    Acquire {
        bucket: String,
    },
    Handoff {
        from: String,
        to: String,
    },
    Release {
        bucket: String,
    },
    Status {
        bucket: String,
    },
}

async fn manage(
    State(store): State<Arc<Store>>,
    Json(command): Json<Command>,
) -> Result<Json<SessionStatus>, (StatusCode, String)> {
    let result = match command {
        Command::Bind {
            bucket,
            workspace,
            branch,
            write_mode,
        } => {
            store
                .bind_with_mode(&bucket, &workspace, &branch, write_mode)
                .await
        }
        Command::BindIds {
            bucket,
            workspace_id,
            branch_id,
            write_mode,
        } => {
            store
                .bind_ids(&bucket, workspace_id, branch_id, write_mode)
                .await
        }
        Command::Acquire { bucket } => store.acquire(&bucket).await,
        Command::Handoff { from, to } => store.handoff(&from, &to).await,
        Command::Release { bucket } => store.release(&bucket).await,
        Command::Status { bucket } => store.status(&bucket).await,
    };
    result.map(Json).map_err(http_error)
}

fn http_error(e: drive_s3_store::Error) -> (StatusCode, String) {
    use drive_s3_store::ErrorKind;
    let status = match e.kind {
        ErrorKind::NoSuchBucket | ErrorKind::NoSuchKey => StatusCode::NOT_FOUND,
        ErrorKind::AccessDenied => StatusCode::FORBIDDEN,
        ErrorKind::Conflict => StatusCode::CONFLICT,
        ErrorKind::Invalid => StatusCode::BAD_REQUEST,
        ErrorKind::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

pub fn dashboard_routes(store: Arc<Store>) -> Router {
    Router::new()
        .route(
            "/api/workspaces/{workspace}/branches/{branch}/s3-mounts",
            get(branch_mounts),
        )
        .with_state(store)
}
async fn branch_mounts(
    State(store): State<Arc<Store>>,
    Path((workspace, branch)): Path<(WorkspaceId, BranchId)>,
) -> Result<Json<Vec<SessionStatus>>, (StatusCode, String)> {
    store
        .branch_mounts(workspace, branch)
        .await
        .map(Json)
        .map_err(http_error)
}

#[derive(Debug)]
pub struct Endpoints {
    s3_listener: TcpListener,
    admin_listener: TcpListener,
    api: Router,
    management: Router,
}

impl Endpoints {
    pub async fn bind(
        store: Arc<Store>,
        listen: SocketAddr,
        admin_port: u16,
        access_key: String,
        secret_key: String,
    ) -> io::Result<Self> {
        let s3_listener = TcpListener::bind(listen).await?;
        let admin_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, admin_port)).await?;
        let management = Router::new()
            .route("/mounts", post(manage))
            .with_state(store.clone());
        let mut builder = S3ServiceBuilder::new(s3::Gateway::new(store));
        builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
        let service = HandleError::new(builder.build(), |e: s3s::HttpError| async move {
            tracing::error!(%e, "S3 transport error");
            StatusCode::INTERNAL_SERVER_ERROR
        });
        Ok(Self {
            s3_listener,
            admin_listener,
            api: Router::new().fallback_service(service),
            management,
        })
    }

    pub async fn serve(self, cancellation: CancellationToken) -> io::Result<()> {
        tokio::try_join!(
            axum::serve(self.s3_listener, self.api)
                .with_graceful_shutdown(cancellation.clone().cancelled_owned())
                .into_future(),
            axum::serve(self.admin_listener, self.management)
                .with_graceful_shutdown(cancellation.cancelled_owned())
                .into_future(),
        )?;
        Ok(())
    }
}
