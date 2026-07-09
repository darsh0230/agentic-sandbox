use std::collections::VecDeque;

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
        for byte in chunk {
            if self.bytes.len() == self.max {
                self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }
}
