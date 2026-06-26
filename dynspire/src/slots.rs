pub const MAX_IN_SLOTS: usize = 16;

pub struct SlotWriter {
    inline: [u64; MAX_IN_SLOTS],
    inline_len: usize,
    heap: Option<Vec<u64>>,
}

impl SlotWriter {
    pub fn new() -> Self {
        Self { inline: [0; MAX_IN_SLOTS], inline_len: 0, heap: None }
    }

    pub fn write_u64(&mut self, val: u64) {
        if let Some(h) = &mut self.heap {
            h.push(val);
        } else if self.inline_len < MAX_IN_SLOTS {
            self.inline[self.inline_len] = val;
            self.inline_len += 1;
        } else {
            let mut h = self.inline.to_vec();
            h.push(val);
            self.heap = Some(h);
        }
    }

    pub fn as_slice(&self) -> &[u64] {
        match &self.heap {
            Some(h) => h,
            None => &self.inline[..self.inline_len],
        }
    }

    pub fn len(&self) -> usize {
        match &self.heap {
            Some(h) => h.len(),
            None => self.inline_len,
        }
    }
}

impl Default for SlotWriter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SlotReader<'a> {
    slots: &'a [u64],
    pos: usize,
}

impl<'a> SlotReader<'a> {
    pub fn new(slots: &'a [u64]) -> Self {
        Self { slots, pos: 0 }
    }

    pub fn read_u64(&mut self) -> u64 {
        let val = self.slots[self.pos];
        self.pos += 1;
        val
    }
}

pub const MAX_OUT_SLOTS: usize = 8;
