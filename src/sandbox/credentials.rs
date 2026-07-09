#[cfg(target_os = "linux")]
use anyhow::Result;

#[cfg(target_os = "linux")]
pub fn drop_to_agent(uid: u32, gid: u32) -> Result<()> {
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::setresgid(gid, gid, gid) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::setresuid(uid, uid, uid) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_no_new_privs() -> Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn drop_capabilities_best_effort() {
    for cap in 0..64 {
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
    }
}
