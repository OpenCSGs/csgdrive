#[cfg(target_os = "linux")]
mod sandbox;
mod settings;
use agentdrive_config::{ConfigStore, initialize};
use agentdrive_engine::{Adapters, Engine, EngineConfig};
use agentdrive_local::LocalBackend;
use agentdrive_mcp::{GatewayConfig, router_with_routes};
use agentdrive_model::*;
use agentdrive_platform::{HostDirectories, NativeMountDriver, SystemClock, local_identity};
use agentdrive_ports::LocalStateStore;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(version, about = "AgentDrive workspace daemon")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Create private configuration in a new directory.
    Init {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7421")]
        listen: SocketAddr,
    },
    /// Serve local MCP clients and native filesystem mounts.
    Serve {
        #[arg(long)]
        config: PathBuf,
        /// Serve branch objects over S3 on this private address.
        #[arg(long)]
        drive_s3_listen: Option<SocketAddr>,
        /// Loopback port for CSI mount configuration.
        #[arg(long)]
        drive_s3_admin_port: Option<u16>,
    },
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentdrive=info,agentdrived=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run(Arguments::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run(arguments: Arguments) -> Result<()> {
    match arguments.command {
        Command::Init { directory, listen } => {
            let path = initialize(&directory, listen)?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Serve {
            config,
            drive_s3_listen,
            drive_s3_admin_port,
        } => {
            serve(
                ConfigStore::open(&config)?,
                drive_s3_listen,
                drive_s3_admin_port,
            )
            .await
        }
    }
}
async fn serve(
    settings: Arc<ConfigStore>,
    s3_listen: Option<SocketAddr>,
    s3_admin_port: Option<u16>,
) -> Result<()> {
    let config = settings.startup();
    let local = Arc::new(LocalBackend::open(&config.data_dir)?);
    settings::use_database_settings(&settings, &local)?;
    let driver = NativeMountDriver::new(config.mount_roots.clone())?;
    let (routes, _) = settings::storage_routes(&settings.live().load().values, None, &local)?;
    let engine = Engine::open_with_routes(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: driver.clone(),
            directories: HostDirectories::new(config.directory_roots.clone())?,
        },
        EngineConfig {
            live: settings.live(),
            identity: local_identity(),
        },
        routes,
    )
    .await?;
    settings.set_applier(Arc::new(settings::StorageSettings {
        engine: engine.clone(),
        metadata: local.as_ref().clone(),
        runtime: tokio::runtime::Handle::current(),
    }))?;
    let cancellation = CancellationToken::new();
    let shutdown = cancellation.clone();
    let signal = tokio::spawn(async move {
        shutdown_signal().await;
        shutdown.cancel();
    });
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let address = listener.local_addr()?;
    let services = engine.services.clone();
    #[cfg(target_os = "linux")]
    let mut services = services;
    #[cfg(target_os = "linux")]
    let mut sandbox_monitor = None;
    #[cfg(target_os = "linux")]
    if let Some(path) = std::env::var_os("AGENTDRIVE_SANDBOX_CONFIG") {
        let sandbox_config: sandbox::Config = serde_json::from_reader(std::fs::File::open(path)?)?;
        let mount_root = sandbox_config.mount_root.clone();
        let driver = sandbox::NativeSandbox::open(sandbox_config, address.port())?;
        let service = agentdrive_engine::SandboxService::new(
            engine.runtime.clone(),
            services.clone(),
            driver,
            mount_root,
        );
        services.sandbox = Some(service.clone());
        sandbox_monitor = Some(service);
    }
    let sandbox_service = services.sandbox.clone();
    let gateway = GatewayConfig {
        allowed_hosts: vec![address.to_string(), format!("localhost:{}", address.port())],
        settings: settings.clone(),
        cancellation: cancellation.clone(),
    };
    let (local_secret, local_port) = local.local_s3_listener()?;
    let listen = s3_listen.unwrap_or_else(|| ([127, 0, 0, 1], local_port).into());
    let credentials = match (
        std::env::var("DRIVE_S3_ACCESS_KEY").ok(),
        std::env::var("DRIVE_S3_SECRET_KEY").ok(),
    ) {
        (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
            (access, secret)
        }
        (None, None) if s3_listen.is_none() => ("local-drive".into(), local_secret),
        _ => {
            return Err(Error::invalid(
                "set both DRIVE_S3_ACCESS_KEY and DRIVE_S3_SECRET_KEY for the S3 listener",
            ));
        }
    };
    let store = Arc::new(
        drive_s3::Store::from_engine(
            engine.clone(),
            &config.data_dir.join("s3-sessions.sqlite3"),
            config.principal.clone(),
        )
        .await
        .map_err(|e| Error::new(ErrorCode::Io, e.to_string()))?,
    );
    let app = router_with_routes(services, gateway, drive_s3::dashboard_routes(store.clone()))?;
    let endpoints = drive_s3::Endpoints::bind(
        store,
        listen,
        s3_admin_port.unwrap_or(if s3_listen.is_some() { 19732 } else { 0 }),
        credentials.0.clone(),
        credentials.1.clone(),
    )
    .await?;
    let (endpoint, _admin_endpoint) = endpoints.local_addresses()?;
    if s3_listen.is_none() {
        local.remember_local_s3_port(endpoint.port())?;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        driver.configure_geesefs(
            format!("http://{endpoint}"),
            format!("http://{_admin_endpoint}"),
            credentials.0,
            credentials.1,
        )?;
    }
    // GeeseFS needs both listeners during startup restoration and unmount flushing.
    let s3_shutdown = CancellationToken::new();
    let s3_guard = s3_shutdown.clone().drop_guard();
    let mut s3 = tokio::spawn(endpoints.serve(s3_shutdown));
    engine.restore_native_mounts().await?;
    tracing::info!(endpoint = %format!("http://{address}/mcp"), location = %engine.runtime.location, "AgentDrive is ready");
    let mut workers = tokio::task::JoinSet::new();
    #[cfg(target_os = "linux")]
    if let Some(sandbox) = sandbox_monitor {
        let cancellation = cancellation.clone();
        workers.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    result = sandbox.reconcile_exited() => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "Sandbox process reconciliation failed");
                        }
                    }
                }
            }
        });
    }
    {
        let cancellation = cancellation.clone();
        workers.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_error = None;
            loop {
                tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                let settings = settings.clone();
                let result = tokio::task::spawn_blocking(move || settings.reload())
                    .await
                    .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))
                    .and_then(|result| result);
                match result {
                    Ok(changed) => {
                        if changed { tracing::info!("runtime configuration reloaded"); }
                        last_error = None;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if last_error.as_ref() != Some(&message) {
                            tracing::warn!(error = %message, "configuration reload failed; previous settings remain active");
                            last_error = Some(message);
                        }
                    }
                }
            }
        });
    }
    {
        let engine = engine.clone();
        let cancellation = cancellation.clone();
        workers.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = engine.refresh_requested_history() => {}
                }
            }
        });
    }
    for kind in ["autosave", "sync", "collaboration", "cache"] {
        let engine = engine.clone();
        let cancellation = cancellation.clone();
        workers.spawn(async move {
            let period = match kind {
                "autosave" | "collaboration" => 1,
                "sync" => 2,
                _ => 30,
            };
            let mut interval = tokio::time::interval(Duration::from_secs(period));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                let result = match kind {
                    "autosave" => engine.autosave_due().await.map(|_| ()),
                    "sync" => engine.sync_pending().await,
                    "collaboration" => engine.collaborate_due().await,
                    _ => engine.services.cache.collect().await.map(|_| ()),
                };
                if let Err(error) = result {
                    tracing::warn!(task = kind, %error, "background operation will be retried");
                }
            }
        });
    }
    let server_result = tokio::select! {
        result = axum::serve(listener, app)
            .with_graceful_shutdown(cancellation.clone().cancelled_owned())
            .into_future() => result,
        result = &mut s3 => result.map_err(std::io::Error::other)?,
    };
    signal.abort();
    cancellation.cancel();
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "background worker stopped unexpectedly");
        }
    }
    if let Some(sandbox) = sandbox_service {
        for mount in local.mounts().await? {
            if mount.state != MountState::Ready {
                continue;
            }
            if sandbox
                .status(&mount.principal, mount.workspace, mount.branch)
                .await?
                .is_some()
            {
                sandbox
                    .stop(&mount.principal, mount.workspace, mount.branch)
                    .await?;
            }
        }
    }
    #[cfg(windows)]
    loop {
        match engine.suspend_native_mounts().await {
            Err(error) if error.code == ErrorCode::MountBusy => {
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            result => {
                result?;
                break;
            }
        }
    }
    #[cfg(not(windows))]
    engine.suspend_native_mounts().await?;
    engine.flush_local().await?;
    drop(s3_guard);
    if !s3.is_finished() {
        s3.await.map_err(std::io::Error::other)??;
    }
    server_result.map_err(Into::into)
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
