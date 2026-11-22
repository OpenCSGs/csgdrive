use crate::{server::encode_operation, web::ApiError};
use agentdrive_config::{ConfigStore, RemoteConfig};
use agentdrive_model::*;
use agentdrive_ports::{CreateWorkspace, RequestContext, Services, WorkspaceInitializer};
use axum::{Extension, Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn api(settings: Arc<ConfigStore>, services: Services) -> Router {
    Router::new()
        .route("/api/workspaces", post(create))
        .with_state((settings, services))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    request_id: String,
    name: String,
    storage: Option<RemoteConfig>,
}

struct Binding {
    settings: Arc<ConfigStore>,
    remote: RemoteConfig,
    target: String,
}

#[async_trait::async_trait]
impl WorkspaceInitializer for Binding {
    fn storage_target(&self) -> Option<&str> {
        Some(&self.target)
    }

    async fn prepare(&self, id: WorkspaceId) -> Result<()> {
        let settings = self.settings.clone();
        let remote = self.remote.clone();
        tokio::task::spawn_blocking(move || settings.bind_new_workspace(id, Some(remote)))
            .await
            .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?
    }

    async fn discard(&self, id: WorkspaceId) -> Result<()> {
        let settings = self.settings.clone();
        let remote = self.remote.clone();
        tokio::task::spawn_blocking(move || settings.discard_new_binding(id, &Some(remote)))
            .await
            .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?
    }
}

async fn create(
    State((settings, services)): State<(Arc<ConfigStore>, Services)>,
    Extension(principal): Extension<Principal>,
    Json(input): Json<Create>,
) -> std::result::Result<Json<Value>, ApiError> {
    let context = RequestContext {
        principal,
        request_id: input.request_id,
    };
    context.validate()?;
    let binding = input
        .storage
        .map(|remote| -> Result<Binding> {
            remote.validate()?;
            if !remote.enabled() {
                return Err(Error::invalid(
                    "select local storage by omitting the remote profile",
                ));
            }
            let target = remote.storage_target()?;
            Ok(Binding {
                settings,
                remote,
                target,
            })
        })
        .transpose()?;
    let request = CreateWorkspace {
        name: input.name,
        grants: Vec::new(),
        max_file_bytes: None,
        max_working_bytes: None,
        max_inodes: None,
    };
    let record = services
        .workspace
        .create_configured(
            context,
            request,
            binding.as_ref().map(|b| b as &dyn WorkspaceInitializer),
        )
        .await?;
    Ok(Json(encode_operation(record)?))
}
