use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use agentic_sandbox::api;
use agentic_sandbox::master::Master;
use agentic_sandbox::proxy::config::{DEFAULT_PROXY_CA_CERT_PATH, DEFAULT_PROXY_CA_KEY_PATH};
use agentic_sandbox::proxy::{ProxyConfig, ProxyHandle, load_global_config};
use agentic_sandbox::sandbox::IsolationMode;
use agentic_sandbox::shutdown::{
    BeginShutdown, DEFAULT_SHUTDOWN_GRACE_MS, ShutdownCoordinator, ShutdownSignal,
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "agentic-sandbox",
    version,
    about = "Single-container agentic sandbox service"
)]
struct Cli {
    /// Time allowed for active slots to handle SIGINT/SIGTERM before SIGKILL.
    #[arg(
        long,
        global = true,
        env = "AGENTIC_SANDBOX_SHUTDOWN_GRACE_MS",
        default_value_t = DEFAULT_SHUTDOWN_GRACE_MS
    )]
    shutdown_grace_ms: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_ROOT",
            default_value = "/var/lib/agentic-sandbox"
        )]
        root: PathBuf,
        #[arg(long, default_value_t = 8)]
        slots: u16,
        /// Maximum requests allowed to wait for a slot (default: 10 × --slots).
        #[arg(long, env = "AGENTIC_SANDBOX_MAX_WAITING")]
        max_waiting: Option<usize>,
        #[arg(long, default_value = "/var/lib/agentic-sandbox/state/master.sock")]
        socket: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, env = "AGENTIC_SANDBOX_HTTP_ADDR")]
        http: Option<SocketAddr>,
        #[arg(long, env = "AGENTIC_SANDBOX_HTTP_TOKEN")]
        http_token: Option<String>,
        #[arg(long, env = "AGENTIC_SANDBOX_ISOLATION", default_value_t = IsolationMode::Privileged)]
        isolation: IsolationMode,
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_PROXY_LISTEN",
            default_value = "127.0.0.1:18080"
        )]
        proxy_listen: SocketAddr,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CONFIG")]
        proxy_config: Option<PathBuf>,
        #[arg(long, env = "AGENTIC_SANDBOX_DISABLE_PROXY", default_value_t = false)]
        disable_proxy: bool,
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_ENABLE_PROXY_MITM",
            default_value_t = false
        )]
        enable_proxy_mitm: bool,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CA_CERT", default_value = DEFAULT_PROXY_CA_CERT_PATH)]
        proxy_ca_cert: PathBuf,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CA_KEY", default_value = DEFAULT_PROXY_CA_KEY_PATH)]
        proxy_ca_key: PathBuf,
    },
    RunJson {
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_ROOT",
            default_value = "/var/lib/agentic-sandbox"
        )]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long, env = "AGENTIC_SANDBOX_ISOLATION", default_value_t = IsolationMode::Privileged)]
        isolation: IsolationMode,
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_PROXY_LISTEN",
            default_value = "127.0.0.1:18080"
        )]
        proxy_listen: SocketAddr,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CONFIG")]
        proxy_config: Option<PathBuf>,
        #[arg(long, env = "AGENTIC_SANDBOX_DISABLE_PROXY", default_value_t = false)]
        disable_proxy: bool,
        #[arg(
            long,
            env = "AGENTIC_SANDBOX_ENABLE_PROXY_MITM",
            default_value_t = false
        )]
        enable_proxy_mitm: bool,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CA_CERT", default_value = DEFAULT_PROXY_CA_CERT_PATH)]
        proxy_ca_cert: PathBuf,
        #[arg(long, env = "AGENTIC_SANDBOX_PROXY_CA_KEY", default_value = DEFAULT_PROXY_CA_KEY_PATH)]
        proxy_ca_key: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let shutdown = ShutdownCoordinator::new(Duration::from_millis(cli.shutdown_grace_ms));

    match cli.command {
        Command::Serve {
            root,
            slots,
            max_waiting,
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
            let max_waiting = max_waiting.unwrap_or_else(|| {
                usize::from(slots)
                    .checked_mul(agentic_sandbox::master::allocator::DEFAULT_MAX_WAITING_PER_SLOT)
                    .expect("validated slot count cannot overflow usize")
            });
            let master = Master::new_with_max_waiting_and_shutdown(
                root,
                slots,
                max_waiting,
                isolation,
                proxy,
                shutdown.clone(),
            )
            .await?;
            if let Some(addr) = http {
                let http_master = master.clone();
                supervise_signals(
                    async move {
                        tokio::try_join!(
                            api::serve_unix(socket, master),
                            api::http::serve_http(
                                api::http::HttpConfig {
                                    addr,
                                    token: http_token,
                                },
                                http_master,
                            ),
                        )?;
                        Ok(())
                    },
                    shutdown,
                )
                .await
            } else {
                supervise_signals(api::serve_unix(socket, master), shutdown).await
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
            let master = Master::new_with_max_waiting_and_shutdown(
                root,
                1,
                agentic_sandbox::master::allocator::DEFAULT_MAX_WAITING_PER_SLOT,
                isolation,
                proxy,
                shutdown.clone(),
            )
            .await?;
            let result = supervise_signals(master.run_agent(req, None), shutdown).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

async fn supervise_signals<F, T>(operation: F, shutdown: ShutdownCoordinator) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::pin!(operation);

    loop {
        tokio::select! {
            result = &mut operation => return result,
            Some(()) = sigint.recv() => request_shutdown(&shutdown, ShutdownSignal::Interrupt),
            Some(()) = sigterm.recv() => request_shutdown(&shutdown, ShutdownSignal::Terminate),
        }
    }
}

fn request_shutdown(shutdown: &ShutdownCoordinator, signal: ShutdownSignal) {
    match shutdown.begin(signal) {
        BeginShutdown::Started { deadline } => {
            info!(
                signal = signal.name(),
                "shutdown requested; draining APIs and signaling active sandboxes"
            );
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                if shutdown.force() {
                    warn!(
                        signal = signal.name(),
                        "shutdown grace period expired; forcing active sandboxes to stop"
                    );
                }
            });
        }
        BeginShutdown::AlreadyStarted => {
            if shutdown.force() {
                warn!(
                    signal = signal.name(),
                    "additional shutdown signal received; forcing active sandboxes to stop"
                );
            }
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
            agentic_sandbox::proxy::mitm::MitmConfig::load_or_create(ca_cert_path, ca_key_path)
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
        .unwrap_or_else(|_| "agentic_sandbox=info,info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_cli_defaults_are_stable() {
        let cli = Cli::try_parse_from(["agentic-sandbox", "serve"]).unwrap();
        assert_eq!(cli.shutdown_grace_ms, DEFAULT_SHUTDOWN_GRACE_MS);
        let Command::Serve { slots, socket, .. } = cli.command else {
            panic!("expected serve command");
        };

        assert_eq!(slots, 8);
        assert_eq!(
            socket,
            PathBuf::from("/var/lib/agentic-sandbox/state/master.sock")
        );
    }

    #[test]
    fn serve_cli_accepts_queue_http_and_isolation_overrides() {
        let cli = Cli::try_parse_from([
            "agentic-sandbox",
            "serve",
            "--slots",
            "3",
            "--max-waiting",
            "0",
            "--http",
            "127.0.0.1:9000",
            "--http-token",
            "secret",
            "--isolation",
            "user",
            "--disable-proxy",
            "--shutdown-grace-ms",
            "45000",
        ])
        .unwrap();
        assert_eq!(cli.shutdown_grace_ms, 45_000);
        let Command::Serve {
            slots,
            max_waiting,
            http,
            http_token,
            isolation,
            disable_proxy,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };

        assert_eq!(slots, 3);
        assert_eq!(max_waiting, Some(0));
        assert_eq!(http, Some("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(http_token.as_deref(), Some("secret"));
        assert_eq!(isolation, IsolationMode::User);
        assert!(disable_proxy);
    }

    #[test]
    fn cli_rejects_invalid_values_and_missing_run_json_request() {
        assert!(
            Cli::try_parse_from(["agentic-sandbox", "serve", "--slots", "not-a-number"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["agentic-sandbox", "serve", "--isolation", "automatic"]).is_err()
        );
        assert!(Cli::try_parse_from(["agentic-sandbox", "run-json"]).is_err());
    }

    #[tokio::test]
    async fn disabling_proxy_short_circuits_config_and_mitm_loading() {
        let result = start_proxy(
            "127.0.0.1:0".parse().unwrap(),
            Some(PathBuf::from("/definitely/missing/proxy.json")),
            true,
            true,
            PathBuf::from("/definitely/missing/ca.pem"),
            PathBuf::from("/definitely/missing/ca-key.pem"),
        )
        .await
        .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn shutdown_request_escalates_at_the_configured_deadline() {
        let shutdown = ShutdownCoordinator::new(Duration::from_millis(5));
        let mut receiver = shutdown.subscribe();
        request_shutdown(&shutdown, ShutdownSignal::Terminate);

        let phase = tokio::time::timeout(
            Duration::from_secs(1),
            agentic_sandbox::shutdown::wait_for_force(&mut receiver),
        )
        .await
        .expect("shutdown deadline did not force escalation");
        assert_eq!(
            phase,
            agentic_sandbox::shutdown::ShutdownPhase::Forced {
                signal: ShutdownSignal::Terminate,
            }
        );
    }

    #[tokio::test]
    async fn second_shutdown_signal_forces_immediate_escalation() {
        let shutdown = ShutdownCoordinator::new(Duration::from_secs(60));
        request_shutdown(&shutdown, ShutdownSignal::Interrupt);
        request_shutdown(&shutdown, ShutdownSignal::Terminate);

        assert_eq!(
            shutdown.phase(),
            agentic_sandbox::shutdown::ShutdownPhase::Forced {
                signal: ShutdownSignal::Interrupt,
            }
        );
    }
}
