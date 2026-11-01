use crate::web::ApiError;
use agentdrive_model::*;
use agentdrive_ports::*;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    routing::{get, post, put},
};
use serde::Deserialize;

pub(crate) fn api() -> Router<Services> {
    Router::new()
        .route("/api/workspaces/{workspace}/collaboration", get(status))
        .route("/api/collaboration", put(configure))
        .route("/api/collaboration/events", post(execution_event))
        .route("/api/collaboration/codex", post(codex_event))
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

async fn execution_event(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Json(request): Json<CollaborationEvent>,
) -> std::result::Result<Json<CollaborationStatus>, ApiError> {
    Ok(Json(
        services
            .collaboration
            .execution_event(&actor, request)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexEvent {
    mount: MountId,
    source: String,
    #[serde(default)]
    jsonrpc: Option<String>,
    method: String,
    params: CodexParams,
}

#[derive(Debug, Deserialize)]
struct CodexParams {
    #[serde(rename = "threadId")]
    thread_id: String,
    turn: CodexTurn,
}

#[derive(Debug, Deserialize)]
struct CodexTurn {
    id: String,
    status: Option<String>,
}

impl CodexEvent {
    fn into_event(self) -> Result<CollaborationEvent> {
        if self
            .jsonrpc
            .as_deref()
            .is_some_and(|version| version != "2.0")
        {
            return Err(Error::invalid("unsupported JSON-RPC version"));
        }
        let phase = match (self.method.as_str(), self.params.turn.status.as_deref()) {
            ("turn/started", None | Some("inProgress")) => ExecutionPhase::Started,
            ("turn/completed", Some("completed")) => ExecutionPhase::Completed,
            ("turn/completed", Some("interrupted")) => ExecutionPhase::Interrupted,
            ("turn/completed", Some("failed")) => ExecutionPhase::Failed,
            _ => return Err(Error::invalid("unsupported Codex turn event or status")),
        };
        Ok(CollaborationEvent {
            mount: self.mount,
            source: self.source,
            thread_id: self.params.thread_id,
            turn_id: self.params.turn.id,
            phase,
        })
    }
}

async fn codex_event(
    State(services): State<Services>,
    Extension(actor): Extension<Principal>,
    Json(request): Json<CodexEvent>,
) -> std::result::Result<Json<CollaborationStatus>, ApiError> {
    Ok(Json(
        services
            .collaboration
            .execution_event(&actor, request.into_event()?)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_notifications_preserve_execution_identity_and_outcome() {
        let mount = MountId::new();
        for (method, status, expected) in [
            ("turn/started", "inProgress", ExecutionPhase::Started),
            ("turn/completed", "completed", ExecutionPhase::Completed),
            ("turn/completed", "interrupted", ExecutionPhase::Interrupted),
            ("turn/completed", "failed", ExecutionPhase::Failed),
        ] {
            let input: CodexEvent = serde_json::from_value(json!({
                "mount":mount, "source":"codex-local", "jsonrpc":"2.0", "method":method,
                "params":{"threadId":"thread-1", "turn":{"id":"turn-1", "status":status, "items":[]}},
            })).unwrap();
            let event = input.into_event().unwrap();
            assert_eq!(event.mount, mount);
            assert_eq!(event.source, "codex-local");
            assert_eq!(event.thread_id, "thread-1");
            assert_eq!(event.turn_id, "turn-1");
            assert_eq!(event.phase, expected);
        }
    }

    #[test]
    fn codex_adapter_rejects_non_terminal_completions_and_arbitrary_unlock_fields() {
        let input = json!({"mount":MountId::new(),"source":"codex-local","method":"turn/completed",
            "params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress"}}});
        assert!(
            serde_json::from_value::<CodexEvent>(input.clone())
                .unwrap()
                .into_event()
                .is_err()
        );
        let mut forged = input;
        forged["authority_epoch"] = json!(100);
        assert!(serde_json::from_value::<CodexEvent>(forged).is_err());
    }
}
