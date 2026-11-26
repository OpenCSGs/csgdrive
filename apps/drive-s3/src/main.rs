use clap::Parser;
use drive_s3::{Endpoints, Store};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Args {
    /// Directory containing the AgentDrive workspace and SQLite state.
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long, default_value = "127.0.0.1:19731")]
    listen: SocketAddr,
    /// The management endpoint always listens on loopback.
    #[arg(long, default_value_t = 19732)]
    admin_port: u16,
    #[arg(long, env = "DRIVE_S3_ACCESS_KEY", default_value = "drive-demo")]
    access_key: String,
    #[arg(long, env = "DRIVE_S3_SECRET_KEY", default_value = "drive-demo-secret")]
    secret_key: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let store = Arc::new(Store::open(&args.data_dir).await?);
    Endpoints::bind(
        store,
        args.listen,
        args.admin_port,
        args.access_key,
        args.secret_key,
    )
    .await?
    .serve(CancellationToken::new())
    .await?;
    Ok(())
}
