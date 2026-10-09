//! WTF-8: the lossless byte encoding the index uses for names.
//!
//! WTF-8 is UTF-8 generalized to code points in the surrogate range, so any
//! sequence of UTF-16 code units (including unpaired surrogates, which NTFS
//! allows) round-trips exactly. Valid surrogate pairs are always combined
//! into one 4-byte sequence, which keeps the encoding canonical and
//! byte-identical to UTF-8 for every well-formed name.
//!
//! Bytes are the right unit for the name buffer: `memchr::memmem` searches
//! bytes, ASCII-heavy names take half the space of UTF-16, and the bytes of a
//! well-formed name are valid `str` data.

/// Appends the WTF-8 encoding of `units` to `out`.
pub(crate) fn encode_into(units: &[u16], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if u < 0x80 {
            out.push(u as u8);
            i += 1;
            continue;
        }
        let cp = if is_lead(u) && i + 1 < units.len() && is_trail(units[i + 1]) {
            let cp =
                0x1_0000 + ((u32::from(u) - 0xD800) << 10) + (u32::from(units[i + 1]) - 0xDC00);
            i += 2;
            cp
        } else {
            i += 1;
            u32::from(u)
        };
        push_code_point(cp, out);
    }
}

/// Encodes `units` as WTF-8.
pub(crate) fn encode(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    encode_into(units, &mut out);
    out
}

/// Decodes WTF-8 back to UTF-16 code units.
///
/// Input produced by [`encode`] round-trips exactly. Malformed bytes (only
/// possible from a corrupted cache file that slipped past checksums) decode
/// to U+FFFD instead of panicking.
pub(crate) fn decode(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match next_code_point(bytes, i) {
            Some((cp, len)) => {
                i += len;
                if cp >= 0x1_0000 {
                    let v = cp - 0x1_0000;
                    out.push(0xD800 | (v >> 10) as u16);
                    out.push(0xDC00 | (v & 0x3FF) as u16);
                } else {
                    out.push(cp as u16);
                }
            }
            None => {
                out.push(0xFFFD);
                i += 1;
            }
        }
    }
    out
}

/// Display form of WTF-8 bytes; unpaired surrogates become U+FFFD.
pub(crate) fn to_string_lossy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf16_lossy(&decode(bytes)),
    }
}

/// Decodes the code point starting at `bytes[i]`, returning it and its byte
/// length. Surrogate code points are returned as-is (that is the point of
/// WTF-8). Returns `None` for malformed or truncated sequences.
#[inline]
pub(crate) fn next_code_point(bytes: &[u8], i: usize) -> Option<(u32, usize)> {
    let b0 = *bytes.get(i)?;
    let (len, init) = match b0 {
        0x00..=0x7F => return Some((u32::from(b0), 1)),
        0xC2..=0xDF => (2, u32::from(b0 & 0x1F)),
        0xE0..=0xEF => (3, u32::from(b0 & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(b0 & 0x07)),
        _ => return None,
    };
    let tail = bytes.get(i + 1..i + len)?;
    let mut cp = init;
    for &b in tail {
        if b & 0xC0 != 0x80 {
            return None;
        }
        cp = (cp << 6) | u32::from(b & 0x3F);
    }
    let min = match len {
        2 => 0x80,
        3 => 0x800,
        _ => 0x1_0000,
    };
    if cp < min || cp > 0x10_FFFF {
        return None;
    }
    Some((cp, len))
}

#[inline]
fn is_lead(u: u16) -> bool {
    (0xD800..0xDC00).contains(&u)
}

#[inline]
fn is_trail(u: u16) -> bool {
    (0xDC00..0xE000).contains(&u)
}

#[inline]
fn push_code_point(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.extend_from_slice(&[0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else if cp < 0x1_0000 {
        out.extend_from_slice(&[
            0xE0 | (cp >> 12) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]);
    } else {
        out.extend_from_slice(&[
            0xF0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3F) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn well_formed_names_are_plain_utf8() {
        let s = "café 🎉 日本";
        let units: Vec<u16> = s.encode_utf16().collect();
        assert_eq!(encode(&units), s.as_bytes());
        assert_eq!(to_string_lossy(&encode(&units)), s);
    }

    #[test]
    fn unpaired_surrogates_round_trip() {
        for units in [
            vec![0xD800],
            vec![0xDC00],
            vec![0x61, 0xDC00, 0xD800, 0x62],
            vec![0xD800, 0xD800, 0xDC00],
        ] {
            let enc = encode(&units);
            assert_eq!(decode(&enc), units);
        }
        assert_eq!(to_string_lossy(&encode(&[0x61, 0xD800])), "a\u{FFFD}");
    }

    #[test]
    fn malformed_input_does_not_panic() {
        assert_eq!(decode(&[0xFF, b'a']), vec![0xFFFD, 0x61]);
        assert_eq!(decode(&[0xE0, 0x80]), vec![0xFFFD, 0xFFFD]);
    }

    proptest! {
        #[test]
        fn any_units_round_trip(units in proptest::collection::vec(any::<u16>(), 0..64)) {
            prop_assert_eq!(decode(&encode(&units)), units);
        }
    }
}
