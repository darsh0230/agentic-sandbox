/// Stub: closes nothing yet. tokio/std process setup already gives the child
/// only requested stdio plus close-on-exec descriptors; the roadmap tracks
/// replacing this with a `close_range` allowlist. The launcher logs the gap on
/// the host side because this runs in the `pre_exec` fork context, where
/// tracing is not safe.
#[cfg(target_os = "linux")]
pub fn close_unneeded_fds() -> anyhow::Result<()> {
    Ok(())
}
