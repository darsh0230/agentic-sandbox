use std::process::Stdio;

use anyhow::{Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::master::supervisor::ExitStatus;
use crate::sandbox::{OutputChunk, OutputStream, SandboxLaunchConfig};
use crate::shutdown::ShutdownSignal;

pub struct SandboxLauncher;

pub struct SandboxChild {
    child: Child,
    output_rx: mpsc::Receiver<OutputChunk>,
    pid: i32,
    process_tree_signaled: bool,
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
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("spawned child did not expose a process id"))?
            as i32;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, output_rx) = mpsc::channel(64);
        if let Some(stdout) = stdout {
            spawn_reader(stdout, OutputStream::Stdout, tx.clone());
        }
        if let Some(stderr) = stderr {
            spawn_reader(stderr, OutputStream::Stderr, tx);
        }
        Ok(SandboxChild {
            child,
            output_rx,
            pid,
            process_tree_signaled: false,
        })
    }
}

impl SandboxChild {
    pub fn pid(&self) -> i32 {
        self.pid
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
        let process_group_result = self.signal_kill();
        self.child.kill().await.ok();
        process_group_result
    }

    pub fn send_signal(&mut self, signal: ShutdownSignal) -> Result<()> {
        self.signal_process_group(signal.as_raw())
    }

    #[cfg(unix)]
    fn signal_process_group(&self, signal: i32) -> Result<()> {
        if self.pid <= 0 {
            bail!("sandbox child has an invalid process group id");
        }
        if unsafe { libc::kill(-self.pid, signal) } == 0 {
            return Ok(());
        }

        let group_error = std::io::Error::last_os_error();
        if group_error.raw_os_error() != Some(libc::ESRCH) {
            return Err(group_error.into());
        }

        // Non-Linux development hosts and a child that exits during pre-exec
        // can briefly lack the expected process group. Preserve direct-child
        // delivery without treating an already-exited child as an error.
        if unsafe { libc::kill(self.pid, signal) } == 0 {
            return Ok(());
        }
        let child_error = std::io::Error::last_os_error();
        if child_error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(child_error.into())
        }
    }

    #[cfg(not(unix))]
    fn signal_process_group(&self, _signal: i32) -> Result<()> {
        bail!("graceful sandbox signaling requires a Unix host")
    }

    fn signal_kill(&mut self) -> Result<()> {
        if self.process_tree_signaled {
            return Ok(());
        }
        self.process_tree_signaled = true;
        // The direct child may already have been reaped, at which point
        // tokio::process::Child::id returns None. Its original process group
        // can still contain live descendants, so retain the spawn PID and
        // always target that group exactly once.
        let result = self.signal_process_group(libc::SIGKILL);
        self.child.start_kill().ok();
        result
    }
}

impl Drop for SandboxChild {
    fn drop(&mut self) {
        // A canceled or panicking execution task must not leave a process using
        // a slot that is about to be returned to the allocator.
        self.signal_kill().ok();
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
    if config.isolation_mode == crate::sandbox::IsolationMode::User {
        command.current_dir(&config.slot.workspace_path);
    }
    unsafe {
        command.pre_exec(move || {
            let result = match config.isolation_mode {
                crate::sandbox::IsolationMode::Privileged => {
                    crate::sandbox::init::prepare_and_drop(&config)
                }
                crate::sandbox::IsolationMode::User => prepare_user_mode(&config),
            };
            result.map_err(|err| {
                let message = format!("{err:#}");
                let _ = std::fs::write(&setup_error_path, &message);
                std::io::Error::other(message)
            })
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_user_mode(config: &SandboxLaunchConfig) -> Result<()> {
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(anyhow::anyhow!(
            "create child process group: {}",
            std::io::Error::last_os_error()
        ));
    }
    crate::sandbox::credentials::drop_to_agent(config.slot.uid, config.slot.gid)
        .map_err(|err| anyhow::anyhow!("drop to slot uid/gid: {err}"))?;
    crate::sandbox::credentials::drop_capabilities_best_effort();
    crate::sandbox::credentials::set_no_new_privs()
        .map_err(|err| anyhow::anyhow!("set no_new_privs: {err}"))?;
    crate::sandbox::seccomp::install_agent_seccomp()
        .map_err(|err| anyhow::anyhow!("install agent seccomp: {err}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn configure_platform(
    command: &mut Command,
    config: SandboxLaunchConfig,
    _setup_error_path: std::path::PathBuf,
) -> Result<()> {
    command.current_dir(&config.slot.workspace_path);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{IsolationMode, SlotSpec};
    use tokio::io::AsyncWriteExt;

    fn config(command: Vec<String>) -> SandboxLaunchConfig {
        SandboxLaunchConfig {
            run_id: "test-run".into(),
            root: "/tmp/agentic-sandbox-test".into(),
            slot: SlotSpec {
                id: "slot-test".into(),
                uid: 0,
                gid: 0,
                workspace_path: "/tmp".into(),
            },
            isolation_mode: IsolationMode::User,
            command,
            env: vec![],
            injected_readonly_dirs: vec![],
        }
    }

    #[cfg(unix)]
    fn stopped_process_group(script: &str) -> SandboxChild {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap() as i32;
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
            pid
        );
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(unsafe { libc::kill(-pid, libc::SIGCONT) }, 0);
        let (_, output_rx) = mpsc::channel(1);
        SandboxChild {
            child,
            output_rx,
            pid,
            process_tree_signaled: false,
        }
    }

    #[test]
    fn rejects_an_empty_command_before_platform_setup() {
        let error = SandboxLauncher::spawn(config(Vec::new()))
            .err()
            .expect("empty command must fail");
        assert_eq!(error.to_string(), "command is empty");
    }

    #[tokio::test]
    async fn output_reader_preserves_large_binary_streams() {
        let (reader, mut writer) = tokio::io::duplex(32 * 1024);
        let (tx, mut rx) = mpsc::channel(8);
        spawn_reader(reader, OutputStream::Stdout, tx);
        let expected = (0..20_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();

        writer.write_all(&expected).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut observed = Vec::new();
        while let Some(chunk) = rx.recv().await {
            assert!(matches!(chunk.stream, OutputStream::Stdout));
            observed.extend(chunk.data);
        }
        assert_eq!(observed, expected);
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_preserves_exit_codes_and_signals() {
        use std::os::unix::process::ExitStatusExt;

        let exited = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .status()
            .unwrap();
        let exited = exit_status_from(exited);
        assert_eq!(exited.code, Some(7));
        assert_eq!(exited.signal, None);

        let signaled = exit_status_from(std::process::ExitStatus::from_raw(libc::SIGKILL));
        assert_eq!(signaled.code, None);
        assert_eq!(signaled.signal, Some(libc::SIGKILL));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwards_the_requested_signal_to_the_child_process_group() {
        let mut child =
            stopped_process_group("trap 'exit 71' INT; kill -STOP $$; while :; do sleep 1; done");
        child.send_signal(ShutdownSignal::Interrupt).unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .expect("child did not handle SIGINT")
            .unwrap();
        child.process_tree_signaled = true;
        assert_eq!(exit.code, Some(71));
        assert_eq!(exit.signal, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_kill_escalates_after_a_ignored_graceful_signal() {
        let mut child =
            stopped_process_group("trap '' TERM; kill -STOP $$; while :; do sleep 1; done");
        child.send_signal(ShutdownSignal::Terminate).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(child.try_wait().await.unwrap().is_none());

        child.kill().await.unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .expect("SIGKILL escalation did not reap the child")
            .unwrap();
        assert_eq!(exit.code, None);
        assert_eq!(exit.signal, Some(libc::SIGKILL));
    }
}
