use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pi_sandbox_master::api;
use pi_sandbox_master::master::Master;
use pi_sandbox_master::proxy::config::{DEFAULT_PROXY_CA_CERT_PATH, DEFAULT_PROXY_CA_KEY_PATH};
use pi_sandbox_master::proxy::{ProxyConfig, ProxyHandle, load_global_config};
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
        #[arg(
            long,
            env = "PI_SANDBOX_PROXY_LISTEN",
            default_value = "127.0.0.1:18080"
        )]
        proxy_listen: SocketAddr,
        #[arg(long, env = "PI_SANDBOX_PROXY_CONFIG")]
        proxy_config: Option<PathBuf>,
        #[arg(long, env = "PI_SANDBOX_DISABLE_PROXY", default_value_t = false)]
        disable_proxy: bool,
        #[arg(long, env = "PI_SANDBOX_ENABLE_PROXY_MITM", default_value_t = false)]
        enable_proxy_mitm: bool,
        #[arg(long, env = "PI_SANDBOX_PROXY_CA_CERT", default_value = DEFAULT_PROXY_CA_CERT_PATH)]
        proxy_ca_cert: PathBuf,
        #[arg(long, env = "PI_SANDBOX_PROXY_CA_KEY", default_value = DEFAULT_PROXY_CA_KEY_PATH)]
        proxy_ca_key: PathBuf,
    },
    RunJson {
        #[arg(long, env = "PI_SANDBOX_ROOT", default_value = "/var/lib/pi-sandbox")]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long, env = "PI_SANDBOX_ISOLATION", default_value_t = IsolationMode::Privileged)]
        isolation: IsolationMode,
        #[arg(
            long,
            env = "PI_SANDBOX_PROXY_LISTEN",
            default_value = "127.0.0.1:18080"
        )]
        proxy_listen: SocketAddr,
        #[arg(long, env = "PI_SANDBOX_PROXY_CONFIG")]
        proxy_config: Option<PathBuf>,
        #[arg(long, env = "PI_SANDBOX_DISABLE_PROXY", default_value_t = false)]
        disable_proxy: bool,
        #[arg(long, env = "PI_SANDBOX_ENABLE_PROXY_MITM", default_value_t = false)]
        enable_proxy_mitm: bool,
        #[arg(long, env = "PI_SANDBOX_PROXY_CA_CERT", default_value = DEFAULT_PROXY_CA_CERT_PATH)]
        proxy_ca_cert: PathBuf,
        #[arg(long, env = "PI_SANDBOX_PROXY_CA_KEY", default_value = DEFAULT_PROXY_CA_KEY_PATH)]
        proxy_ca_key: PathBuf,
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
            proxy_listen,
            proxy_config,
            disable_proxy,
            enable_proxy_mitm,
            proxy_ca_cert,
            proxy_ca_key,
        } => {
            if let Some(path) = config {
                info!(config = %path.display(), "config file argument accepted for packaging compatibility");
            }
            let proxy = start_proxy(
                proxy_listen,
                proxy_config,
                disable_proxy,
                enable_proxy_mitm,
                proxy_ca_cert,
                proxy_ca_key,
            )
            .await?;
            let master = Master::new(root, slots, isolation, proxy).await?;
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
            proxy_listen,
            proxy_config,
            disable_proxy,
            enable_proxy_mitm,
            proxy_ca_cert,
            proxy_ca_key,
        } => {
            let body = tokio::fs::read(&request).await?;
            let req = serde_json::from_slice(&body)?;
            let proxy = start_proxy(
                proxy_listen,
                proxy_config,
                disable_proxy,
                enable_proxy_mitm,
                proxy_ca_cert,
                proxy_ca_key,
            )
            .await?;
            let master = Master::new(root, 1, isolation, proxy).await?;
            let result = master.run_agent(req, None).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

async fn start_proxy(
    listen_addr: SocketAddr,
    config_path: Option<PathBuf>,
    disable_proxy: bool,
    enable_mitm: bool,
    ca_cert_path: PathBuf,
    ca_key_path: PathBuf,
) -> Result<Option<ProxyHandle>> {
    if disable_proxy {
        info!("proxy disabled");
        return Ok(None);
    }
    let mut config = load_global_config(config_path, listen_addr).await?;
    if enable_mitm {
        config.mitm = Some(
            pi_sandbox_master::proxy::mitm::MitmConfig::load_or_create(ca_cert_path, ca_key_path)
                .await?,
        );
    }
    let config = ProxyConfig {
        listen_addr,
        global_rules: config.global_rules,
        mitm: config.mitm,
    };
    Ok(Some(ProxyHandle::start(config).await?))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pi_sandbox_master=info,info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
