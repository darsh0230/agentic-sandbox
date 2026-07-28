use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use tokio::sync::oneshot;

pub const DEFAULT_MAX_WAITING_PER_SLOT: usize = 10;

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: String,
    pub uid: u32,
    pub gid: u32,
    pub workspace_path: PathBuf,
}

struct AllocatorState {
    available: VecDeque<Slot>,
    waiters: VecDeque<Waiter>,
    next_waiter_id: u64,
}

struct Waiter {
    id: u64,
    tx: oneshot::Sender<SlotLease>,
}

pub struct SlotAllocator {
    state: Mutex<AllocatorState>,
    max_waiting: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStatus {
    pub position: usize,
    pub max_waiting: usize,
}

pub enum SlotAllocation {
    Ready(SlotLease),
    Waiting(SlotWaiter),
}

pub struct SlotWaiter {
    allocator: Arc<SlotAllocator>,
    id: u64,
    status: QueueStatus,
    rx: oneshot::Receiver<SlotLease>,
    queued: bool,
}

pub struct SlotLease {
    allocator: Arc<SlotAllocator>,
    slot: Option<Slot>,
}

impl SlotAllocator {
    pub fn new(workspace_root: PathBuf, slot_count: u16) -> Result<Self> {
        let max_waiting = usize::from(slot_count)
            .checked_mul(DEFAULT_MAX_WAITING_PER_SLOT)
            .expect("validated slot count cannot overflow usize");
        Self::with_max_waiting(workspace_root, slot_count, max_waiting)
    }

    pub fn with_max_waiting(
        workspace_root: PathBuf,
        slot_count: u16,
        max_waiting: usize,
    ) -> Result<Self> {
        Ok(Self {
            state: Mutex::new(AllocatorState {
                available: VecDeque::from(Self::planned_slots(&workspace_root, slot_count)?),
                waiters: VecDeque::new(),
                next_waiter_id: 1,
            }),
            max_waiting,
        })
    }

    pub fn planned_slots(workspace_root: &Path, slot_count: u16) -> Result<Vec<Slot>> {
        if slot_count == 0 || slot_count > 4096 {
            bail!("slot count must be between 1 and 4096");
        }
        let mut slots = Vec::new();
        for idx in 1..=slot_count {
            let id = format!("slot-{idx:04}");
            let uid = 10_000 + u32::from(idx);
            slots.push(Slot {
                id: id.clone(),
                uid,
                gid: uid,
                workspace_path: workspace_root.join(id),
            });
        }
        Ok(slots)
    }

    pub fn begin_allocate(self: &Arc<Self>) -> Result<SlotAllocation> {
        let mut state = self.lock_state();
        if let Some(slot) = state.available.pop_front() {
            return Ok(SlotAllocation::Ready(SlotLease::new(
                Arc::clone(self),
                slot,
            )));
        }

        if state.waiters.len() >= self.max_waiting {
            bail!(
                "sandbox wait queue is full ({} waiting; limit {})",
                state.waiters.len(),
                self.max_waiting
            );
        }

        let id = state.next_waiter_id;
        state.next_waiter_id = state
            .next_waiter_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("sandbox wait queue identifier space exhausted"))?;
        let (tx, rx) = oneshot::channel();
        state.waiters.push_back(Waiter { id, tx });
        let status = QueueStatus {
            position: state.waiters.len(),
            max_waiting: self.max_waiting,
        };

        Ok(SlotAllocation::Waiting(SlotWaiter {
            allocator: Arc::clone(self),
            id,
            status,
            rx,
            queued: true,
        }))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AllocatorState> {
        // Keep the allocator usable if an unrelated task panics while holding this
        // short-lived bookkeeping lock.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cancel_waiter(&self, id: u64) {
        let mut state = self.lock_state();
        if let Some(index) = state.waiters.iter().position(|waiter| waiter.id == id) {
            state.waiters.remove(index);
        }
    }

    fn release_slot(self: &Arc<Self>, mut slot: Slot) {
        let mut state = self.lock_state();
        while let Some(waiter) = state.waiters.pop_front() {
            let lease = SlotLease::new(Arc::clone(self), slot);
            match waiter.tx.send(lease) {
                Ok(()) => return,
                Err(mut rejected_lease) => {
                    // The waiter was canceled after it was dequeued. Recover the
                    // slot and offer it to the next request without losing capacity.
                    slot = rejected_lease
                        .slot
                        .take()
                        .expect("newly created slot lease must contain a slot");
                }
            }
        }
        state.available.push_back(slot);
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let state = self.lock_state();
        (state.available.len(), state.waiters.len())
    }
}

impl SlotWaiter {
    pub fn status(&self) -> QueueStatus {
        self.status
    }

    pub async fn wait(mut self) -> Result<SlotLease> {
        let lease = (&mut self.rx)
            .await
            .map_err(|_| anyhow::anyhow!("sandbox slot wait was canceled"))?;
        self.queued = false;
        Ok(lease)
    }
}

impl Drop for SlotWaiter {
    fn drop(&mut self) {
        if self.queued {
            self.allocator.cancel_waiter(self.id);
        }
    }
}

impl SlotLease {
    fn new(allocator: Arc<SlotAllocator>, slot: Slot) -> Self {
        Self {
            allocator,
            slot: Some(slot),
        }
    }

    pub fn slot(&self) -> &Slot {
        self.slot
            .as_ref()
            .expect("live slot lease must contain a slot")
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.allocator.release_slot(slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{SlotAllocation, SlotAllocator};

    fn allocator(max_waiting: usize) -> Arc<SlotAllocator> {
        Arc::new(SlotAllocator::with_max_waiting("/tmp/workspaces".into(), 1, max_waiting).unwrap())
    }

    fn ready(allocation: SlotAllocation) -> super::SlotLease {
        match allocation {
            SlotAllocation::Ready(lease) => lease,
            SlotAllocation::Waiting(_) => panic!("expected an immediately available slot"),
        }
    }

    fn waiting(allocation: SlotAllocation) -> super::SlotWaiter {
        match allocation {
            SlotAllocation::Ready(_) => panic!("expected a queued slot request"),
            SlotAllocation::Waiting(waiter) => waiter,
        }
    }

    #[tokio::test]
    async fn waits_in_fifo_order_and_reuses_the_slot() {
        let allocator = allocator(2);
        let active = ready(allocator.begin_allocate().unwrap());
        let first = waiting(allocator.begin_allocate().unwrap());
        let second = waiting(allocator.begin_allocate().unwrap());

        assert_eq!(first.status().position, 1);
        assert_eq!(second.status().position, 2);
        assert_eq!(allocator.counts(), (0, 2));

        drop(active);
        let first_lease = first.wait().await.unwrap();
        assert_eq!(allocator.counts(), (0, 1));
        drop(first_lease);
        let second_lease = second.wait().await.unwrap();
        assert_eq!(second_lease.slot().id, "slot-0001");
        drop(second_lease);
        assert_eq!(allocator.counts(), (1, 0));
    }

    #[test]
    fn rejects_requests_beyond_the_waiting_limit() {
        let allocator = allocator(2);
        let _active = ready(allocator.begin_allocate().unwrap());
        let _first = waiting(allocator.begin_allocate().unwrap());
        let _second = waiting(allocator.begin_allocate().unwrap());

        let err = match allocator.begin_allocate() {
            Ok(_) => panic!("queue should be full"),
            Err(err) => err,
        };
        assert_eq!(
            err.to_string(),
            "sandbox wait queue is full (2 waiting; limit 2)"
        );
    }

    #[test]
    fn canceled_waiter_immediately_frees_queue_capacity() {
        let allocator = allocator(1);
        let _active = ready(allocator.begin_allocate().unwrap());
        let canceled = waiting(allocator.begin_allocate().unwrap());
        assert_eq!(allocator.counts(), (0, 1));

        drop(canceled);
        assert_eq!(allocator.counts(), (0, 0));
        let replacement = waiting(allocator.begin_allocate().unwrap());
        assert_eq!(replacement.status().position, 1);
    }

    #[tokio::test]
    async fn cancellation_after_assignment_hands_slot_to_next_waiter() {
        let allocator = allocator(2);
        let active = ready(allocator.begin_allocate().unwrap());
        let first = waiting(allocator.begin_allocate().unwrap());
        let second = waiting(allocator.begin_allocate().unwrap());

        drop(active);
        drop(first);

        let lease = second.wait().await.unwrap();
        assert_eq!(lease.slot().id, "slot-0001");
        drop(lease);
        assert_eq!(allocator.counts(), (1, 0));
    }

    #[test]
    fn default_waiting_limit_is_ten_per_slot() {
        let allocator = SlotAllocator::new("/tmp/workspaces".into(), 3).unwrap();
        assert_eq!(allocator.max_waiting, 30);
    }

    #[test]
    fn planned_slots_validate_boundaries_and_assign_stable_identities() {
        for count in [0, 4097, u16::MAX] {
            let error =
                SlotAllocator::planned_slots("/srv/workspaces".as_ref(), count).unwrap_err();
            assert_eq!(error.to_string(), "slot count must be between 1 and 4096");
        }

        let one = SlotAllocator::planned_slots("/srv/workspaces".as_ref(), 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "slot-0001");
        assert_eq!(one[0].uid, 10_001);
        assert_eq!(one[0].gid, 10_001);
        assert_eq!(
            one[0].workspace_path,
            std::path::Path::new("/srv/workspaces/slot-0001")
        );

        let maximum = SlotAllocator::planned_slots("/srv/workspaces".as_ref(), 4096).unwrap();
        assert_eq!(maximum.len(), 4096);
        let last = maximum.last().unwrap();
        assert_eq!(last.id, "slot-4096");
        assert_eq!(last.uid, 14_096);
        assert_eq!(last.gid, 14_096);
        assert_eq!(
            last.workspace_path,
            std::path::Path::new("/srv/workspaces/slot-4096")
        );
    }

    #[test]
    fn multiple_slots_are_allocated_in_order_and_returned_independently() {
        let allocator =
            Arc::new(SlotAllocator::with_max_waiting("/tmp/workspaces".into(), 3, 0).unwrap());
        let first = ready(allocator.begin_allocate().unwrap());
        let second = ready(allocator.begin_allocate().unwrap());
        let third = ready(allocator.begin_allocate().unwrap());

        assert_eq!(first.slot().id, "slot-0001");
        assert_eq!(second.slot().id, "slot-0002");
        assert_eq!(third.slot().id, "slot-0003");
        assert_eq!(allocator.counts(), (0, 0));

        drop(second);
        let reused = ready(allocator.begin_allocate().unwrap());
        assert_eq!(reused.slot().id, "slot-0002");

        drop(first);
        drop(third);
        drop(reused);
        assert_eq!(allocator.counts(), (3, 0));
    }

    #[test]
    fn zero_waiting_capacity_still_allows_immediate_allocations() {
        let allocator = allocator(0);
        let active = ready(allocator.begin_allocate().unwrap());
        assert_eq!(active.slot().id, "slot-0001");

        let error = match allocator.begin_allocate() {
            Ok(_) => panic!("a request should not be queued when the limit is zero"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "sandbox wait queue is full (0 waiting; limit 0)"
        );

        drop(active);
        assert_eq!(allocator.counts(), (1, 0));
    }

    #[tokio::test]
    async fn canceling_a_middle_waiter_preserves_fifo_order_and_queue_capacity() {
        let allocator = allocator(3);
        let active = ready(allocator.begin_allocate().unwrap());
        let first = waiting(allocator.begin_allocate().unwrap());
        let middle = waiting(allocator.begin_allocate().unwrap());
        let third = waiting(allocator.begin_allocate().unwrap());

        drop(middle);
        assert_eq!(allocator.counts(), (0, 2));
        let replacement = waiting(allocator.begin_allocate().unwrap());
        assert_eq!(replacement.status().position, 3);

        drop(active);
        let first_lease = first.wait().await.unwrap();
        drop(first_lease);
        let third_lease = third.wait().await.unwrap();
        drop(third_lease);
        let replacement_lease = replacement.wait().await.unwrap();
        drop(replacement_lease);
        assert_eq!(allocator.counts(), (1, 0));
    }

    #[test]
    fn exhausted_waiter_identifier_space_does_not_enqueue_a_partial_waiter() {
        let allocator = allocator(1);
        let _active = ready(allocator.begin_allocate().unwrap());
        allocator.lock_state().next_waiter_id = u64::MAX;

        let error = match allocator.begin_allocate() {
            Ok(_) => panic!("identifier exhaustion should fail allocation"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "sandbox wait queue identifier space exhausted"
        );
        assert_eq!(allocator.counts(), (0, 0));
    }

    #[test]
    fn allocator_recovers_after_its_bookkeeping_mutex_is_poisoned() {
        let allocator = allocator(0);
        let poisoner = Arc::clone(&allocator);
        let result = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("poison allocator bookkeeping lock");
        })
        .join();
        assert!(result.is_err());

        let lease = ready(allocator.begin_allocate().unwrap());
        assert_eq!(lease.slot().id, "slot-0001");
        drop(lease);
        assert_eq!(allocator.counts(), (1, 0));
    }
}
