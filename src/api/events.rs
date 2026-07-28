use std::collections::VecDeque;
use std::time::Duration;

pub(crate) const RUN_STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct TailBuffer {
    bytes: VecDeque<u8>,
    max: usize,
}

impl TailBuffer {
    pub fn new(max: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(max.min(8192)),
            max,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.max == 0 {
            return;
        }
        if chunk.len() >= self.max {
            self.bytes.clear();
            self.bytes.extend(
                chunk[chunk.len().saturating_sub(self.max)..]
                    .iter()
                    .copied(),
            );
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.max);
        self.bytes.drain(..overflow);
        self.bytes.extend(chunk.iter().copied());
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::TailBuffer;

    #[test]
    fn zero_capacity_discards_all_chunks() {
        let mut tail = TailBuffer::new(0);
        tail.push(b"ignored");
        tail.push(&[]);
        assert!(tail.into_vec().is_empty());
    }

    #[test]
    fn preserves_data_at_or_below_capacity() {
        let mut tail = TailBuffer::new(6);
        tail.push(b"abc");
        tail.push(b"def");
        tail.push(b"");
        assert_eq!(tail.into_vec(), b"abcdef");
    }

    #[test]
    fn overflow_across_chunks_keeps_only_the_newest_bytes() {
        let mut tail = TailBuffer::new(5);
        tail.push(b"abc");
        tail.push(b"def");
        assert_eq!(tail.into_vec(), b"bcdef");
    }

    #[test]
    fn chunk_larger_than_capacity_replaces_the_previous_tail() {
        let mut tail = TailBuffer::new(4);
        tail.push(b"old");
        tail.push(b"0123456789");
        assert_eq!(tail.into_vec(), b"6789");
    }

    #[test]
    fn repeated_overflow_preserves_binary_bytes_in_order() {
        let mut tail = TailBuffer::new(3);
        tail.push(&[0, 255]);
        tail.push(&[1, 2]);
        tail.push(&[3]);
        assert_eq!(tail.clone().into_vec(), vec![1, 2, 3]);
        assert_eq!(tail.into_vec(), vec![1, 2, 3]);
    }
}
