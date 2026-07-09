#[cfg(target_os = "linux")]
pub fn close_unneeded_fds() -> anyhow::Result<()> {
    // tokio/std process setup already gives the child only requested stdio plus
    // close-on-exec descriptors. Keep this hook for a future close_range allowlist.
    Ok(())
}
