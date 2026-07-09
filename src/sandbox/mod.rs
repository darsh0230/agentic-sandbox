pub mod credentials;
pub mod fds;
pub mod init;
pub mod launcher;
pub mod mounts;
pub mod namespaces;
pub mod rootfs;
pub mod seccomp;

use std::path::PathBuf;

use crate::master::allocator::Slot;
use crate::types::InjectedDir;

pub use launcher::{SandboxChild, SandboxLauncher};

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
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub injected_readonly_dirs: Vec<InjectedDir>,
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
