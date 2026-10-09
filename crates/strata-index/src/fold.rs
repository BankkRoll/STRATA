//! Case folding and natural ordering for WTF-8 names.
//!
//! Folding is lowercase mapping: an ASCII fast path for the overwhelmingly
//! common case, and `char::to_lowercase` for everything else. Surrogate code
//! points (unpaired surrogates in NTFS names) have no case and are copied
//! through byte-for-byte, so folding never loses or invents bytes for them.

use std::cmp::Ordering;

use crate::wtf8;

/// Writes the case-folded form of WTF-8 `name` into `out` (cleared first).
#[inline]
pub(crate) fn fold_into(name: &[u8], out: &mut Vec<u8>) {
    out.clear();
    if name.is_ascii() {
        out.extend(name.iter().map(u8::to_ascii_lowercase));
        return;
    }
    fold_unicode(name, out);
}

/// Case-folded copy of WTF-8 `name`.
pub(crate) fn fold(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    fold_into(name, &mut out);
    out
}

#[cold]
fn fold_unicode(name: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    let mut buf = [0u8; 4];
    while i < name.len() {
        match wtf8::next_code_point(name, i) {
            Some((cp, len)) => {
                match char::from_u32(cp) {
                    Some(c) if c.is_ascii() => out.push(c.to_ascii_lowercase() as u8),
                    Some(c) => {
                        for l in c.to_lowercase() {
                            out.extend_from_slice(l.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    None => out.extend_from_slice(&name[i..i + len]),
                }
                i += len;
            }
            None => {
                out.push(name[i]);
                i += 1;
            }
        }
    }
}

/// Natural, case-insensitive comparison of two pre-folded names: runs of
/// ASCII digits compare by numeric value (`file2` < `file10`), everything
/// else bytewise. Ties on numeric value fall back to the shorter digit run
/// (`a01` vs `a1`), then to plain bytes, so the order is total.
pub(crate) fn natural_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (ca, cb) = (a[i], b[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (ra, rb) = (trim_zeros(&a[si..i]), trim_zeros(&b[sj..j]));
            let ord = ra.len().cmp(&rb.len()).then_with(|| ra.cmp(rb));
            if ord != Ordering::Equal {
                return ord;
            }
            let ord = (i - si).cmp(&(j - sj));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            if ca != cb {
                return ca.cmp(&cb);
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let n = d.iter().take_while(|&&c| c == b'0').count();
    &d[n..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_unicode_fold() {
        assert_eq!(fold(b"ReadMe.TXT"), b"readme.txt");
        assert_eq!(fold("ÄÖÜ.Doc".as_bytes()), "äöü.doc".as_bytes());
        assert_eq!(fold("ΣΑΣ".as_bytes()), "σασ".as_bytes());
    }

    #[test]
    fn surrogates_pass_through() {
        let name = wtf8::encode(&[u16::from(b'A'), 0xD800]);
        let f = fold(&name);
        assert_eq!(f[0], b'a');
        assert_eq!(&f[1..], &name[1..]);
    }

    #[test]
    fn natural_order() {
        let mut v: Vec<&[u8]> = vec![b"file10", b"file2", b"file1", b"file02", b"a", b"file"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            vec![&b"a"[..], b"file", b"file1", b"file2", b"file02", b"file10"]
        );
    }
}
