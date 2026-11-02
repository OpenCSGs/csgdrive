use crate::McpServer;
use agentdrive_config::ConfigStore;
use agentdrive_model::{Error, Result};
use agentdrive_ports::Services;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

type HttpService = StreamableHttpService<McpServer, LocalSessionManager>;

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub allowed_hosts: Vec<String>,
    pub settings: Arc<ConfigStore>,
    pub cancellation: CancellationToken,
}

pub fn router(services: Services, config: GatewayConfig) -> Result<Router> {
    if config.allowed_hosts.is_empty() {
        return Err(Error::invalid("MCP requires an explicit host allowlist"));
    }
    let server = McpServer::new(
        services.clone(),
        config.settings.startup().principal.clone(),
    );
    let transport = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts.clone())
            // Origin 由共享 HTTP 中间件按当前配置检查。
            .disable_allowed_origins()
            .with_max_request_body_bytes(8 * 1024 * 1024)
            .with_json_response(true)
            .with_cancellation_token(config.cancellation.clone()),
    );
    let api = crate::web::api(services)
        .merge(crate::web::settings_api(config.settings.clone()))
        .merge(
            Router::new()
                .route("/mcp", any(handle))
                .with_state(Arc::new(transport)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::new(config),
            local_request,
        ));
    Ok(api.merge(crate::web::assets()))
}

async fn handle(State(transport): State<Arc<HttpService>>, request: Request) -> Response {
    transport.handle(request).await.map(Body::new)
}

async fn local_request(
    State(config): State<Arc<GatewayConfig>>,
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !config.allowed_hosts.iter().any(|allowed| allowed == host) {
        return (StatusCode::FORBIDDEN, "host is not allowed").into_response();
    }
    let snapshot = config.settings.live().load();
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if origin != format!("http://{host}")
            && !snapshot
                .values
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin)
        {
            return (StatusCode::FORBIDDEN, "origin is not allowed").into_response();
        }
    }
    request
        .extensions_mut()
        .insert(config.settings.startup().principal.clone());
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
