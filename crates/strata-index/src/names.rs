//! Name storage: one byte buffer, no per-name allocation, no per-entry offset.
//!
//! Names are WTF-8, each prefixed by its LEB128 byte length. After a build
//! (or compaction) the names of the *base* entries are laid out in `EntryId`
//! order, so an entry's name is found from a sampled offset (one `u32` per
//! [`SAMPLE`] entries) plus at most `SAMPLE - 1` length-prefix skips. That
//! costs 0.25 bytes per entry instead of the 6 a `(offset, len)` pair would,
//! and it makes search a single sequential pass over the buffer.
//!
//! Names written after the build (live creates and renames) are appended to
//! the buffer and recorded in a small override map; the stale base copy stays
//! in place (it is still needed to skip over during sequential decoding) and
//! is counted as garbage until the next compaction.

use hashbrown::HashMap;

/// Base entries per sampled offset.
pub(crate) const SAMPLE: usize = 16;

/// The name buffer. See the module docs for the layout.
#[derive(Debug, Clone, Default)]
pub(crate) struct NameStore {
    pub(crate) buf: Vec<u8>,
    /// Offset of the length prefix of base entry `k * SAMPLE`.
    pub(crate) samples: Vec<u32>,
    /// Number of entries laid out in id order.
    pub(crate) base_len: u32,
    /// Overrides: names of non-base entries and renamed base entries.
    pub(crate) moved: HashMap<u32, u32>,
    /// One bit per base entry: set when the entry's name lives in `moved`.
    pub(crate) moved_bits: Vec<u64>,
    /// Bytes no longer referenced by any entry.
    pub(crate) garbage: u64,
}

impl NameStore {
    /// Lays out `names` (in id order) as the base region.
    pub(crate) fn from_base<'a>(names: impl ExactSizeIterator<Item = &'a [u8]>) -> Self {
        let n = names.len();
        let mut s = Self {
            buf: Vec::new(),
            samples: Vec::with_capacity(n.div_ceil(SAMPLE)),
            base_len: n as u32,
            moved: HashMap::new(),
            moved_bits: vec![0; n.div_ceil(64)],
            garbage: 0,
        };
        for (i, name) in names.enumerate() {
            if i % SAMPLE == 0 {
                s.samples.push(s.buf.len() as u32);
            }
            write_prefixed(&mut s.buf, name);
        }
        s
    }

    /// Name of entry `id` (WTF-8). Entries with no name return `&[]`.
    #[inline]
    pub(crate) fn get(&self, id: u32) -> &[u8] {
        if id < self.base_len && !self.is_moved_base(id) {
            let mut off = self.samples[id as usize / SAMPLE] as usize;
            for _ in 0..(id as usize % SAMPLE) {
                off = skip(&self.buf, off);
            }
            read_at(&self.buf, off)
        } else {
            match self.moved.get(&id) {
                Some(&off) => read_at(&self.buf, off as usize),
                None => &[],
            }
        }
    }

    /// Replaces (or sets) the name of entry `id`.
    pub(crate) fn set(&mut self, id: u32, name: &[u8]) {
        let old_len = {
            let old = self.get(id);
            if old == name {
                return;
            }
            old.len()
        };
        self.garbage += old_len as u64;
        let off = self.buf.len() as u32;
        write_prefixed(&mut self.buf, name);
        self.moved.insert(id, off);
        if id < self.base_len {
            self.moved_bits[id as usize / 64] |= 1 << (id % 64);
        }
    }

    /// Forgets the name of a removed entry.
    pub(crate) fn remove(&mut self, id: u32) {
        if let Some(off) = self.moved.remove(&id) {
            self.garbage += read_at(&self.buf, off as usize).len() as u64;
        } else if id < self.base_len {
            self.garbage += self.get(id).len() as u64;
        }
        if id < self.base_len {
            // The base copy stays (sequential decoding skips over it), so
            // mark it moved to make `get` consult the (now empty) override.
            self.moved_bits[id as usize / 64] |= 1 << (id % 64);
        }
    }

    #[inline]
    pub(crate) fn is_moved_base(&self, id: u32) -> bool {
        self.moved_bits[id as usize / 64] & (1 << (id % 64)) != 0
    }

    /// Calls `f(id, name)` for base entries in `start..end` whose name is
    /// still the base copy, decoding sequentially. `start` must be a
    /// multiple of [`SAMPLE`].
    #[inline]
    pub(crate) fn for_each_base(&self, start: u32, end: u32, mut f: impl FnMut(u32, &[u8])) {
        debug_assert_eq!(start as usize % SAMPLE, 0);
        let end = end.min(self.base_len);
        if start >= end {
            return;
        }
        let mut off = self.samples[start as usize / SAMPLE] as usize;
        for id in start..end {
            let (len, hdr) = read_len(&self.buf, off);
            let name = &self.buf[off + hdr..off + hdr + len];
            off += hdr + len;
            if self.is_moved_base(id) {
                continue;
            }
            f(id, name);
        }
    }

    /// Overridden names: `(id, name)` for every entry whose name lives
    /// outside the base layout.
    pub(crate) fn moved_iter(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        self.moved
            .iter()
            .map(|(&id, &off)| (id, read_at(&self.buf, off as usize)))
    }

    /// Heap bytes held, split into (name bytes, bookkeeping bytes).
    pub(crate) fn heap_bytes(&self) -> (u64, u64) {
        let names = self.buf.capacity() as u64;
        let book = (self.samples.capacity() * 4) as u64
            + (self.moved_bits.capacity() * 8) as u64
            + crate::mem::map_bytes(self.moved.capacity(), 8);
        (names, book)
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
        self.samples.shrink_to_fit();
        self.moved_bits.shrink_to_fit();
        self.moved.shrink_to_fit();
    }
}

/// Appends `name` with its LEB128 length prefix.
pub(crate) fn write_prefixed(buf: &mut Vec<u8>, name: &[u8]) {
    let mut len = name.len();
    loop {
        let b = (len & 0x7F) as u8;
        len >>= 7;
        if len == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
    buf.extend_from_slice(name);
}

/// Reads the LEB128 length at `off`: `(length, prefix bytes)`.
#[inline]
pub(crate) fn read_len(buf: &[u8], off: usize) -> (usize, usize) {
    let b0 = buf[off];
    if b0 < 0x80 {
        return (b0 as usize, 1);
    }
    let mut len = (b0 & 0x7F) as usize;
    let mut shift = 7;
    let mut i = off + 1;
    loop {
        let b = buf[i];
        len |= ((b & 0x7F) as usize) << shift;
        i += 1;
        if b < 0x80 {
            return (len, i - off);
        }
        shift += 7;
    }
}

#[inline]
fn read_at(buf: &[u8], off: usize) -> &[u8] {
    let (len, hdr) = read_len(buf, off);
    &buf[off + hdr..off + hdr + len]
}

#[inline]
fn skip(buf: &[u8], off: usize) -> usize {
    let (len, hdr) = read_len(buf, off);
    off + hdr + len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(names: &[&str]) -> NameStore {
        NameStore::from_base(names.iter().map(|s| s.as_bytes()))
    }

    #[test]
    fn sampled_lookup_matches_input() {
        let names: Vec<String> = (0..100).map(|i| format!("name-{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let s = store(&refs);
        for (i, n) in names.iter().enumerate() {
            assert_eq!(s.get(i as u32), n.as_bytes());
        }
    }

    #[test]
    fn long_names_use_multibyte_prefix() {
        let long = "x".repeat(700);
        let s = store(&["a", &long, "b"]);
        assert_eq!(s.get(1), long.as_bytes());
        assert_eq!(s.get(2), b"b");
    }

    #[test]
    fn overrides_and_removal() {
        let mut s = store(&["a", "b", "c"]);
        s.set(1, b"renamed");
        s.set(5, b"delta");
        assert_eq!(s.get(1), b"renamed");
        assert_eq!(s.get(5), b"delta");
        let mut seen = Vec::new();
        s.for_each_base(0, 3, |id, n| seen.push((id, n.to_vec())));
        assert_eq!(seen, vec![(0, b"a".to_vec()), (2, b"c".to_vec())]);
        s.remove(0);
        assert_eq!(s.get(0), b"");
        assert!(s.garbage >= 2);
    }
}
