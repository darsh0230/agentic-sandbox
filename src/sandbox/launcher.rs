use std::process::Stdio;

use anyhow::{Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::master::supervisor::ExitStatus;
use crate::sandbox::{OutputChunk, OutputStream, SandboxLaunchConfig};

pub struct SandboxLauncher;

pub struct SandboxChild {
    child: Child,
    output_rx: mpsc::Receiver<OutputChunk>,
}

impl SandboxLauncher {
    pub fn spawn(config: SandboxLaunchConfig) -> Result<SandboxChild> {
        if config.command.is_empty() {
            bail!("command is empty");
        }

        let mut command = Command::new(&config.command[0]);
        command.args(&config.command[1..]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.env_clear();
        for (key, value) in &config.env {
            command.env(key, value);
        }

        let setup_error_path = config
            .root
            .join("state")
            .join(format!("{}.setup-error", config.run_id));
        let _ = std::fs::remove_file(&setup_error_path);
        configure_platform(&mut command, config, setup_error_path.clone())?;
        let mut child = command.spawn().map_err(|err| {
            if let Ok(details) = std::fs::read_to_string(&setup_error_path) {
                anyhow::anyhow!("{err}: {details}")
            } else {
                anyhow::anyhow!(err)
            }
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, output_rx) = mpsc::channel(64);
        if let Some(stdout) = stdout {
            spawn_reader(stdout, OutputStream::Stdout, tx.clone());
        }
        if let Some(stderr) = stderr {
            spawn_reader(stderr, OutputStream::Stderr, tx);
        }
        Ok(SandboxChild { child, output_rx })
    }
}

impl SandboxChild {
    pub fn pid(&self) -> i32 {
        self.child.id().unwrap_or_default() as i32
    }

    pub async fn next_output(&mut self) -> Option<OutputChunk> {
        self.output_rx.recv().await
    }

    pub async fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?.map(exit_status_from))
    }

    pub async fn wait(&mut self) -> Result<ExitStatus> {
        Ok(exit_status_from(self.child.wait().await?))
    }

    pub async fn kill(&mut self) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        self.child.kill().await.ok();
        Ok(())
    }
}

fn spawn_reader<R>(mut reader: R, stream: OutputStream, tx: mpsc::Sender<OutputChunk>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(OutputChunk {
                            stream,
                            data: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn exit_status_from(status: std::process::ExitStatus) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        ExitStatus {
            code: status.code(),
            signal: None,
        }
    }
}

#[cfg(target_os = "linux")]
fn configure_platform(
    command: &mut Command,
    config: SandboxLaunchConfig,
    setup_error_path: std::path::PathBuf,
) -> Result<()> {
    unsafe {
        command.pre_exec(move || {
            crate::sandbox::init::prepare_and_drop(&config).map_err(|err| {
                let message = format!("{err:#}");
                let _ = std::fs::write(&setup_error_path, &message);
                std::io::Error::new(std::io::ErrorKind::Other, message)
            })
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn configure_platform(
    command: &mut Command,
    config: SandboxLaunchConfig,
    _setup_error_path: std::path::PathBuf,
) -> Result<()> {
    command.current_dir(&config.slot.workspace_path);
    Ok(())
}
