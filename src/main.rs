use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pi_sandbox_master::api;
use pi_sandbox_master::master::Master;
use pi_sandbox_master::sandbox::IsolationMode;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "pi-sandbox-master",
    version,
    about = "Single-container Pi agent sandbox master"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, env = "PI_SANDBOX_ROOT", default_value = "/var/lib/pi-sandbox")]
        root: PathBuf,
        #[arg(long, default_value_t = 8)]
        slots: u16,
        #[arg(long, default_value = "/var/lib/pi-sandbox/state/master.sock")]
        socket: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, env = "PI_SANDBOX_HTTP_ADDR")]
        http: Option<SocketAddr>,
        #[arg(long, env = "PI_SANDBOX_HTTP_TOKEN")]
        http_token: Option<String>,
        #[arg(long, env = "PI_SANDBOX_ISOLATION", default_value_t = IsolationMode::Privileged)]
        isolation: IsolationMode,
    },
    RunJson {
        #[arg(long, env = "PI_SANDBOX_ROOT", default_value = "/var/lib/pi-sandbox")]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long, env = "PI_SANDBOX_ISOLATION", default_value_t = IsolationMode::Privileged)]
        isolation: IsolationMode,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            root,
            slots,
            socket,
            config,
            http,
            http_token,
            isolation,
        } => {
            if let Some(path) = config {
                info!(config = %path.display(), "config file argument accepted for packaging compatibility");
            }
            let master = Master::new(root, slots, isolation).await?;
            if let Some(addr) = http {
                let http_master = master.clone();
                tokio::select! {
                    result = api::serve_unix(socket, master) => result,
                    result = api::http::serve_http(api::http::HttpConfig { addr, token: http_token }, http_master) => result,
                }
            } else {
                api::serve_unix(socket, master).await
            }
        }
        Command::RunJson {
            root,
            request,
            isolation,
        } => {
            let body = tokio::fs::read(&request).await?;
            let req = serde_json::from_slice(&body)?;
            let master = Master::new(root, 1, isolation).await?;
            let result = master.run_agent(req, None).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pi_sandbox_master=info,info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
