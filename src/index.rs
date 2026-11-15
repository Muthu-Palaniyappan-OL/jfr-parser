//! Open-addressing hash index from constant-pool id to byte offset.
//!
//! Constant pools are never decoded eagerly. Loading a chunk only records
//! *where* each pool entry lives (`id -> offset`); entries are decoded on
//! demand straight from the mapped file. For a pool of N entries this costs
//! 12 bytes per slot (8-byte key, 4-byte offset) at a load factor of at most
//! one half, instead of a heap object per entry.
//!
//! Offsets use 32 bits when the chunk is smaller than 4 GiB and 64 bits
//! otherwise, so chunk size is not limited.

const EMPTY32: u32 = u32::MAX;
const EMPTY64: u64 = u64::MAX;

#[derive(Debug)]
enum Vals {
    Narrow(Vec<u32>),
    Wide(Vec<u64>),
}

/// Maps ids to offsets. Inserting an existing id replaces its offset.
#[derive(Debug)]
pub struct IdIndex {
    keys: Vec<u64>,
    vals: Vals,
    len: usize,
    mask: usize,
    wide: bool,
}

#[inline(always)]
fn hash(k: u64) -> usize {
    (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize
}

impl IdIndex {
    /// An empty index that allocates nothing until the first insert.
    pub fn new(wide: bool) -> IdIndex {
        IdIndex {
            keys: Vec::new(),
            vals: if wide { Vals::Wide(Vec::new()) } else { Vals::Narrow(Vec::new()) },
            len: 0,
            mask: 0,
            wide,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Makes room for `additional` more entries without rehashing.
    pub fn reserve(&mut self, additional: usize) {
        let want = (self.len + additional).saturating_mul(2).max(8).next_power_of_two();
        if want > self.keys.len() {
            self.rehash(want);
        }
    }

    fn rehash(&mut self, cap: usize) {
        let old_keys = std::mem::take(&mut self.keys);
        let old_vals = std::mem::replace(
            &mut self.vals,
            if self.wide { Vals::Wide(vec![EMPTY64; cap]) } else { Vals::Narrow(vec![EMPTY32; cap]) },
        );
        self.keys = vec![0; cap];
        self.mask = cap - 1;
        match old_vals {
            Vals::Narrow(v) => {
                for (k, o) in old_keys.into_iter().zip(v) {
                    if o != EMPTY32 {
                        self.put_raw(k, o as u64);
                    }
                }
            }
            Vals::Wide(v) => {
                for (k, o) in old_keys.into_iter().zip(v) {
                    if o != EMPTY64 {
                        self.put_raw(k, o);
                    }
                }
            }
        }
    }

    /// Insert without growth checks. Returns true when a new key was added.
    #[inline]
    fn put_raw(&mut self, key: u64, off: u64) -> bool {
        let mut i = hash(key) & self.mask;
        loop {
            match &mut self.vals {
                Vals::Narrow(v) => {
                    if v[i] == EMPTY32 {
                        self.keys[i] = key;
                        v[i] = off as u32;
                        return true;
                    }
                    if self.keys[i] == key {
                        v[i] = off as u32;
                        return false;
                    }
                }
                Vals::Wide(v) => {
                    if v[i] == EMPTY64 {
                        self.keys[i] = key;
                        v[i] = off;
                        return true;
                    }
                    if self.keys[i] == key {
                        v[i] = off;
                        return false;
                    }
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Records that entry `key` starts at `offset`.
    ///
    /// The caller guarantees `offset` is below the chunk length, which
    /// selects 32-bit storage only when it fits.
    #[inline]
    pub fn insert(&mut self, key: u64, offset: usize) {
        if (self.len + 1) * 2 > self.keys.len() {
            let cap = (self.keys.len() * 2).max(8);
            self.rehash(cap);
        }
        if self.put_raw(key, offset as u64) {
            self.len += 1;
        }
    }

    /// Offset of the entry with this id.
    #[inline]
    pub fn get(&self, key: u64) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mut i = hash(key) & self.mask;
        loop {
            match &self.vals {
                Vals::Narrow(v) => {
                    let o = v[i];
                    if o == EMPTY32 {
                        return None;
                    }
                    if self.keys[i] == key {
                        return Some(o as usize);
                    }
                }
                Vals::Wide(v) => {
                    let o = v[i];
                    if o == EMPTY64 {
                        return None;
                    }
                    if self.keys[i] == key {
                        return Some(o as usize);
                    }
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Iterates over `(id, offset)` in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        (0..self.keys.len()).filter_map(move |i| {
            let o = match &self.vals {
                Vals::Narrow(v) => (v[i] != EMPTY32).then(|| v[i] as usize),
                Vals::Wide(v) => (v[i] != EMPTY64).then(|| v[i] as usize),
            }?;
            Some((self.keys[i], o))
        })
    }

    /// Approximate heap bytes used.
    pub fn heap_bytes(&self) -> usize {
        self.keys.len() * 8 + self.keys.len() * if self.wide { 8 } else { 4 }
    }
}
