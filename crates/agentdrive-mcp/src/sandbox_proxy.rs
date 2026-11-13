use crate::gateway::GatewayConfig;
use agentdrive_model::{BranchId, ErrorCode, WorkspaceId};
use agentdrive_ports::Services;
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use std::sync::Arc;

fn starting_page(html: bool) -> Response {
    if !html {
        return (StatusCode::SERVICE_UNAVAILABLE, "助手正在启动，请稍后重试").into_response();
    }
    (StatusCode::SERVICE_UNAVAILABLE, [(header::CACHE_CONTROL, "no-store")], Html(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"refresh\" content=\"3\"><title>CSGClaw 正在启动</title><style>body{font:16px system-ui;background:#f6f8f7;color:#203c34;display:grid;place-items:center;min-height:90vh}main{padding:40px;background:white;border:1px solid #dfe8e3;border-radius:16px;max-width:460px}h1{font-size:24px}p{line-height:1.8;color:#60756b}a{color:#28745a}</style><main><h1>CSGClaw 正在启动</h1><p>Ubuntu 已经启动，正在准备助手服务。页面会自动重试。</p><a href=\"\">立即重试</a></main></html>"
    )).into_response()
}

// 每个实例的助手使用独立 localhost 来源，页面资源与 WebSocket 保持原路径。
pub(crate) async fn service_origin(
    State((services, config)): State<(Services, Arc<GatewayConfig>)>,
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    let html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let Some((hostname, port)) = host.rsplit_once(':') else {
        return next.run(request).await;
    };
    let Some(identity) = hostname.strip_suffix(".localhost") else {
        return next.run(request).await;
    };
    let parts: Vec<_> = identity.split('.').collect();
    if parts.len() != 3 {
        return (StatusCode::FORBIDDEN, "助手地址无效").into_response();
    }
    if !config
        .allowed_hosts
        .iter()
        .any(|value| value == &format!("127.0.0.1:{port}") || value == &format!("localhost:{port}"))
    {
        return (StatusCode::FORBIDDEN, "助手端口无效").into_response();
    }
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && origin.to_str().ok() != Some(format!("http://{host}").as_str())
    {
        return (StatusCode::FORBIDDEN, "助手来源无效").into_response();
    }
    let (Ok(workspace), Ok(branch)) = (
        parts[1].parse::<WorkspaceId>(),
        parts[2].parse::<BranchId>(),
    ) else {
        return (StatusCode::BAD_REQUEST, "助手分支无效").into_response();
    };
    let Some(runtime) = services.sandbox else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Sandbox 尚未配置").into_response();
    };
    let stream = match runtime
        .service(
            &config.settings.startup().principal,
            workspace,
            branch,
            parts[0],
        )
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            return match error.code {
                ErrorCode::PermissionDenied => {
                    (StatusCode::FORBIDDEN, error.message).into_response()
                }
                ErrorCode::NotFound => (StatusCode::NOT_FOUND, error.message).into_response(),
                _ => starting_page(html),
            };
        }
    };
    request.headers_mut().remove("forwarded");
    request.headers_mut().remove("x-forwarded-for");
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await {
            Ok(value) => value,
            Err(_) => return starting_page(html),
        };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(_) => return starting_page(html),
    };
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
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response.map(axum::body::Body::new)
}
