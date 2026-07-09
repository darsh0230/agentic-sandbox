use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pi_sandbox_master::api;
use pi_sandbox_master::master::Master;
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
    },
    RunJson {
        #[arg(long, env = "PI_SANDBOX_ROOT", default_value = "/var/lib/pi-sandbox")]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
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
        } => {
            if let Some(path) = config {
                info!(config = %path.display(), "config file argument accepted for packaging compatibility");
            }
            let master = Master::new(root, slots).await?;
            api::serve_unix(socket, master).await
        }
        Command::RunJson { root, request } => {
            let body = tokio::fs::read(&request).await?;
            let req = serde_json::from_slice(&body)?;
            let master = Master::new(root, 1).await?;
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
