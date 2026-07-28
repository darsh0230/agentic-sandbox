pub mod credentials;
pub mod fds;
pub mod init;
pub mod launcher;
pub mod mounts;
pub mod namespaces;
pub mod rootfs;
pub mod seccomp;

use std::path::PathBuf;
use std::str::FromStr;

use crate::master::allocator::Slot;
use crate::types::InjectedDir;

pub use launcher::{SandboxChild, SandboxLauncher};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationMode {
    #[default]
    Privileged,
    User,
}

impl std::fmt::Display for IsolationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Privileged => f.write_str("privileged"),
            Self::User => f.write_str("user"),
        }
    }
}

impl FromStr for IsolationMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "privileged" => Ok(Self::Privileged),
            "user" => Ok(Self::User),
            _ => Err("isolation must be one of: privileged, user".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SlotSpec {
    pub id: String,
    pub uid: u32,
    pub gid: u32,
    pub workspace_path: PathBuf,
}

impl From<&Slot> for SlotSpec {
    fn from(slot: &Slot) -> Self {
        Self {
            id: slot.id.clone(),
            uid: slot.uid,
            gid: slot.gid,
            workspace_path: slot.workspace_path.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxLaunchConfig {
    pub run_id: String,
    pub root: PathBuf,
    pub slot: SlotSpec,
    pub isolation_mode: IsolationMode,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub injected_readonly_dirs: Vec<InjectedDir>,
}

#[cfg(test)]
mod tests {
    use super::{IsolationMode, SlotSpec};
    use crate::master::allocator::Slot;

    #[test]
    fn parses_isolation_mode_values() {
        assert_eq!("privileged".parse(), Ok(IsolationMode::Privileged));
        assert_eq!("user".parse(), Ok(IsolationMode::User));
        for invalid in ["auto", "", "USER", " user", "user "] {
            assert_eq!(
                invalid.parse::<IsolationMode>().unwrap_err(),
                "isolation must be one of: privileged, user"
            );
        }
    }

    #[test]
    fn isolation_mode_default_and_display_are_stable() {
        assert_eq!(IsolationMode::default(), IsolationMode::Privileged);
        assert_eq!(IsolationMode::Privileged.to_string(), "privileged");
        assert_eq!(IsolationMode::User.to_string(), "user");
    }

    #[test]
    fn slot_spec_copies_allocator_slot_identity() {
        let slot = Slot {
            id: "slot-0042".into(),
            uid: 10_042,
            gid: 20_042,
            workspace_path: "/srv/workspaces/slot-0042".into(),
        };
        let spec = SlotSpec::from(&slot);
        assert_eq!(spec.id, slot.id);
        assert_eq!(spec.uid, slot.uid);
        assert_eq!(spec.gid, slot.gid);
        assert_eq!(spec.workspace_path, slot.workspace_path);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub data: Vec<u8>,
}
