#[cfg(target_os = "linux")]
#[test]
fn seccomp_module_is_linked_on_linux() {
    let _install: fn() -> anyhow::Result<()> =
        pi_sandbox_master::sandbox::seccomp::install_agent_seccomp;
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_seccomp_is_not_available_on_non_linux() {
    let target = std::env::consts::OS;
    assert_ne!(target, "linux");
}
