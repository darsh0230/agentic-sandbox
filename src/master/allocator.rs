use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Result, bail};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: String,
    pub uid: u32,
    pub gid: u32,
    pub workspace_path: PathBuf,
}

pub struct SlotAllocator {
    available: Mutex<VecDeque<Slot>>,
}

impl SlotAllocator {
    pub fn new(workspace_root: PathBuf, slot_count: u16) -> Result<Self> {
        if slot_count == 0 || slot_count > 4096 {
            bail!("slot count must be between 1 and 4096");
        }
        let mut slots = VecDeque::new();
        for idx in 1..=slot_count {
            let id = format!("slot-{idx:04}");
            let uid = 10_000 + u32::from(idx);
            slots.push_back(Slot {
                id: id.clone(),
                uid,
                gid: uid,
                workspace_path: workspace_root.join(id),
            });
        }
        Ok(Self {
            available: Mutex::new(slots),
        })
    }

    pub async fn allocate(&self) -> Result<Slot> {
        let mut slots = self.available.lock().await;
        slots
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no sandbox slots available"))
    }

    pub async fn release(&self, slot: Slot) {
        self.available.lock().await.push_back(slot);
    }
}
