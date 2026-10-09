//! Interned, case-folded file extensions (`ext_id` column).

use hashbrown::HashMap;

use crate::fold;

/// Extension id for "no extension".
pub(crate) const EXT_NONE: u16 = 0;
/// Extension id shared by every extension after the table filled up.
pub(crate) const EXT_OVERFLOW: u16 = u16::MAX;

/// Extension interner. Id 0 is "no extension"; ids are dense and stable.
#[derive(Debug, Clone)]
pub(crate) struct ExtTable {
    pub(crate) names: Vec<Box<[u8]>>,
    map: HashMap<Box<[u8]>, u16>,
}

impl Default for ExtTable {
    fn default() -> Self {
        Self {
            names: vec![Box::from(&b""[..])],
            map: HashMap::new(),
        }
    }
}

impl ExtTable {
    /// Rebuilds the table from its id-ordered names (cache load).
    pub(crate) fn from_names(names: Vec<Box<[u8]>>) -> Self {
        let map = names
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, n)| (n.clone(), i as u16))
            .collect();
        Self { names, map }
    }

    /// Interns the extension of WTF-8 `name` (already case-folded or not).
    pub(crate) fn intern_name(&mut self, name: &[u8]) -> u16 {
        match extension_of(name) {
            Some(ext) => self.intern(&fold::fold(ext)),
            None => EXT_NONE,
        }
    }

    /// Interns a folded extension.
    pub(crate) fn intern(&mut self, folded: &[u8]) -> u16 {
        if let Some(&id) = self.map.get(folded) {
            return id;
        }
        if self.names.len() >= EXT_OVERFLOW as usize {
            return EXT_OVERFLOW;
        }
        let id = self.names.len() as u16;
        let b: Box<[u8]> = folded.into();
        self.names.push(b.clone());
        self.map.insert(b, id);
        id
    }

    /// Id of an already-interned folded extension.
    pub(crate) fn lookup(&self, folded: &[u8]) -> Option<u16> {
        self.map.get(folded).copied()
    }

    /// Folded extension bytes for `id` (empty for none or overflow).
    pub(crate) fn name(&self, id: u16) -> &[u8] {
        self.names.get(id as usize).map_or(&[], |b| b)
    }

    pub(crate) fn heap_bytes(&self) -> u64 {
        let strings: usize = self.names.iter().map(|n| n.len() * 2 + 16).sum();
        strings as u64 + crate::mem::map_bytes(self.map.capacity(), 18)
    }
}

/// Bytes after the last `.` of a WTF-8 name. A leading dot (`.gitignore`) or a
/// trailing dot is not an extension, matching `WideName::extension_units`.
pub(crate) fn extension_of(name: &[u8]) -> Option<&[u8]> {
    let dot = memchr::memrchr(b'.', name)?;
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_case_insensitively() {
        let mut t = ExtTable::default();
        let a = t.intern_name(b"movie.MP4");
        let b = t.intern_name(b"clip.mp4");
        assert_eq!(a, b);
        assert_ne!(a, EXT_NONE);
        assert_eq!(t.name(a), b"mp4");
        assert_eq!(t.intern_name(b".gitignore"), EXT_NONE);
        assert_eq!(t.intern_name(b"trailing."), EXT_NONE);
    }
}
