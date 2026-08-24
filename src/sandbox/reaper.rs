//! UID-scoped process reaping used as the slot cleanup backstop.
//!
//! Slot agents are allowed to call `setsid` and `setpgid`: agent runtimes (for
//! example ante) place every spawned command in its own session or process
//! group, which escapes the process group that group-based signaling targets.
//! Each slot's agent runs under a dedicated UID that cannot change (the child
//! sets `PR_SET_NO_NEW_PRIVS` and drops its capability bounding set before
//! exec), so the control plane can reap leftover slot processes by real UID
//! regardless of session or process-group membership.
//!
//! Signals are delivered through `pidfd_open` + `pidfd_send_signal`: the pidfd
//! pins the PID's identity, so a process can never be recycled between the
//! `/proc` ownership check and the signal. Without that pin, a scan-then-kill
//! loop has a PID-reuse race in which the control plane (which holds
//! `CAP_KILL`) could SIGKILL a different slot's freshly recycled process.

// Scanning and parsing helpers are production code only on Linux, but tests
// exercise them on every platform, so they stay compiled under `cfg(test)`
// on all hosts.

#[cfg(target_os = "linux")]
use tracing::{debug, warn};

#[cfg(target_os = "linux")]
const MAX_PASSES: usize = 5;
#[cfg(target_os = "linux")]
const PASS_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

#[cfg(target_os = "linux")]
pub fn reap_uid(uid: u32) {
    if uid == 0 {
        // Slot UIDs are always non-root (see master::allocator). Refuse to
        // sweep root-owned processes even if a caller is misconfigured.
        return;
    }
    reap_proc(std::path::Path::new("/proc"), uid);
}

#[cfg(not(target_os = "linux"))]
pub fn reap_uid(_uid: u32) {
    // UID reaping requires /proc and pidfd; development-host builds rely on
    // process-group signaling alone.
}

#[cfg(target_os = "linux")]
fn reap_proc(proc_root: &std::path::Path, uid: u32) {
    for pass in 0..MAX_PASSES {
        let signaled = sweep_and_signal(proc_root, uid);
        if signaled == 0 {
            return;
        }
        debug!(uid, signaled, pass, "reaped slot processes by uid");
        if pass + 1 < MAX_PASSES {
            std::thread::sleep(PASS_DELAY);
        }
    }
    warn!(
        uid,
        "uid sweep exhausted its passes; fork-happy slot descendants may have survived cleanup"
    );
}

/// Enumerates live, non-zombie processes in `proc_root` owned by real UID
/// `uid`. Pure scanning, no signaling: unit-tested on all platforms with
/// fixture directories.
#[cfg(any(test, target_os = "linux"))]
fn matching_pids(proc_root: &std::path::Path, uid: u32) -> Vec<i32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        if matches!(process_uid_state(&entry.path()), Some((owner, state)) if owner == uid && state != b'Z')
        {
            pids.push(pid);
        }
    }
    pids
}

/// SIGKILLs every process from a fresh `matching_pids` scan and returns how
/// many were signaled. Each candidate is pinned with a pidfd first and its
/// ownership is re-checked while pinned, so a PID recycled between the scan
/// and the signal can never redirect the SIGKILL onto an unrelated process.
#[cfg(target_os = "linux")]
fn sweep_and_signal(proc_root: &std::path::Path, uid: u32) -> usize {
    let mut signaled = 0;
    for pid in matching_pids(proc_root, uid) {
        let Some(pinned) = PinnedPid::open(pid) else {
            continue;
        };
        if matches!(
            process_uid_state(&proc_root.join(pid.to_string())),
            Some((owner, state)) if owner == uid && state != b'Z'
        ) {
            pinned.sigkill();
            signaled += 1;
        }
    }
    signaled
}

/// A pidfd pinning one process's identity. While the fd stays open the PID
/// cannot be recycled, and `pidfd_send_signal` can only ever target the
/// originally pinned process.
#[cfg(target_os = "linux")]
struct PinnedPid(std::os::fd::RawFd);

#[cfg(target_os = "linux")]
impl PinnedPid {
    fn open(pid: i32) -> Option<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        (fd >= 0).then_some(Self(fd))
    }

    fn sigkill(&self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0,
                libc::SIGKILL,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PinnedPid {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Reads the real UID and one-letter state (`R`, `S`, `Z`, ...) of the
/// process described by a `/proc/<pid>` directory.
#[cfg(any(test, target_os = "linux"))]
fn process_uid_state(process_dir: &std::path::Path) -> Option<(u32, u8)> {
    let status = std::fs::read_to_string(process_dir.join("status")).ok()?;
    let mut real_uid = None;
    let mut state = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            real_uid = value.split_whitespace().next()?.parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("State:") {
            state = value.trim_start().bytes().next();
        }
        if real_uid.is_some() && state.is_some() {
            break;
        }
    }
    Some((real_uid?, state?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fake_process(root: &Path, name: &str, status: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), status).unwrap();
    }

    #[test]
    fn matching_pids_selects_live_processes_of_the_exact_uid() {
        let root = tempfile::tempdir().unwrap();
        // Implausible PIDs keep the fixtures free of real-kernel collisions.
        fake_process(
            root.path(),
            "4000001",
            "Name:\tsleep\nState:\tS (sleeping)\nUid:\t10001\t10001\t10001\t10001\n",
        );
        fake_process(
            root.path(),
            "4000002",
            "Name:\tsleep\nState:\tZ (zombie)\nUid:\t10001\t10001\t10001\t10001\n",
        );
        fake_process(
            root.path(),
            "4000003",
            "Name:\tsleep\nState:\tS (sleeping)\nUid:\t10002\t10002\t10002\t10002\n",
        );
        fs::create_dir_all(root.path().join("self")).unwrap();

        assert_eq!(matching_pids(root.path(), 10_001), vec![4_000_001]);
        assert!(matching_pids(root.path(), 10_003).is_empty());
        assert!(matching_pids(root.path().join("missing").as_path(), 10_001).is_empty());
    }

    #[test]
    fn matching_pids_returns_every_live_match_and_handles_empty_dirs() {
        let root = tempfile::tempdir().unwrap();
        assert!(matching_pids(root.path(), 10_001).is_empty());
        for (name, uid) in [
            ("4000010", 10_001),
            ("4000011", 10_001),
            ("4000012", 10_003),
        ] {
            fake_process(
                root.path(),
                name,
                &format!("State:\tD (disk sleep)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            );
        }
        let mut pids = matching_pids(root.path(), 10_001);
        pids.sort_unstable();
        assert_eq!(pids, vec![4_000_010, 4_000_011]);
    }

    #[test]
    fn process_uid_state_requires_both_fields_and_parses_the_real_uid() {
        let root = tempfile::tempdir().unwrap();
        fake_process(
            root.path(),
            "4000004",
            "Uid:\t0\t10001\t10001\t10001\nState:\tR (running)\n",
        );
        assert_eq!(
            process_uid_state(&root.path().join("4000004")),
            Some((0, b'R'))
        );
        fake_process(root.path(), "4000005", "State:\tS (sleeping)\n");
        assert_eq!(process_uid_state(&root.path().join("4000005")), None);
        assert_eq!(process_uid_state(&root.path().join("4000006")), None);
    }

    #[test]
    fn reap_uid_never_sweeps_root_owned_processes() {
        reap_uid(0);
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;

        #[test]
        fn pinned_pid_rejects_missing_processes_and_accepts_live_ones() {
            assert!(PinnedPid::open(4_000_000).is_none());
            let own = PinnedPid::open(std::process::id() as i32);
            assert!(own.is_some());
            drop(own);
            assert!(PinnedPid::open(std::process::id() as i32).is_some());
        }

        #[test]
        fn reap_proc_over_unsignalable_fixtures_returns_after_one_pass() {
            // pidfd_open fails for fixture PIDs, so nothing is "signaled" and
            // the loop must not burn its retry budget sleeping.
            let root = tempfile::tempdir().unwrap();
            fake_process(
                root.path(),
                "4000020",
                "State:\tS (sleeping)\nUid:\t10001\t10001\t10001\t10001\n",
            );
            let started = std::time::Instant::now();
            reap_proc(root.path(), 10_001);
            assert!(started.elapsed() < PASS_DELAY);
        }

        #[test]
        fn reap_uid_kills_a_reattributed_cross_session_process_and_spares_neighbors() {
            // Running the sweep for real requires a dedicated throwaway UID,
            // which needs privileges to establish. Development hosts and
            // unprivileged CI skip; the Docker E2E environment exercises this.
            if unsafe { libc::geteuid() } != 0 {
                eprintln!("skipping: requires root to reattribute a child to a scratch uid");
                return;
            }
            const VICTIM_UID: u32 = 424_242;
            const NEIGHBOR_UID: u32 = VICTIM_UID + 1;
            let spawn_as = |uid: u32, seconds: &str| {
                let mut command = std::process::Command::new("/bin/sleep");
                command.arg(seconds);
                unsafe {
                    use std::os::unix::process::CommandExt;
                    command.pre_exec(move || {
                        if libc::setsid() == -1 && libc::setpgid(0, 0) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setresgid(uid, uid, uid) != 0
                            || libc::setresuid(uid, uid, uid) != 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                command.spawn().expect("spawn scratch-uid sleeper")
            };

            let mut victim = spawn_as(VICTIM_UID, "60");
            let mut neighbor = spawn_as(NEIGHBOR_UID, "60");
            // The pre_exec attribution races the sweep below: wait until the
            // kernel reports the scratch UIDs before reaping.
            let proc_root = Path::new("/proc");
            let attributed = (0..40).any(|_| {
                let victim_ready = matches!(
                    process_uid_state(&proc_root.join(victim.id().to_string())),
                    Some((owner, _)) if owner == VICTIM_UID
                );
                let neighbor_ready = matches!(
                    process_uid_state(&proc_root.join(neighbor.id().to_string())),
                    Some((owner, _)) if owner == NEIGHBOR_UID
                );
                if victim_ready && neighbor_ready {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                false
            });
            assert!(attributed, "scratch processes never took their uids");
            reap_uid(VICTIM_UID);

            let victim_dead = (0..60).any(|_| {
                if victim.try_wait().unwrap().is_some() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                false
            });
            assert!(
                victim_dead,
                "cross-session process owned by the slot uid survived the sweep"
            );
            assert!(
                neighbor.try_wait().unwrap().is_none(),
                "sweep leaked onto a neighboring uid"
            );
            unsafe {
                libc::kill(neighbor.id() as i32, libc::SIGKILL);
            }
            let _ = neighbor.wait();
        }
    }
}
