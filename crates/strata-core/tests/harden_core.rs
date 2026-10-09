//! Time and Unicode edge cases of the shared types: the suspicious-time
//! boundaries, conversions at the ends of every range, and names with
//! unpaired surrogates, dots and non-BMP characters.

use strata_core::{CloudState, EntryFlags, EpochSecs, FileTime, ReparseKind, Sizes, WideName};

const TICKS: u64 = 10_000_000;

#[test]
fn suspicious_time_boundaries_are_exact() {
    let now = FileTime::from_unix_secs(1_760_000_000);
    let y1990 = FileTime::from_unix_secs(631_152_000); // 1990-01-01T00:00:00Z
    assert!(!y1990.is_suspicious(now));
    assert!(FileTime(y1990.0 - 1).is_suspicious(now));
    let day_ahead = FileTime(now.0 + 86_400 * TICKS);
    assert!(!day_ahead.is_suspicious(now));
    assert!(FileTime(day_ahead.0 + 1).is_suspicious(now));
    // A clock near the end of the range saturates instead of wrapping.
    assert!(!FileTime(u64::MAX).is_suspicious(FileTime(u64::MAX - 5)));
    assert!(FileTime(0).is_suspicious(FileTime(u64::MAX)));
}

#[test]
fn conversions_saturate_at_both_ends() {
    assert_eq!(FileTime::from_unix_secs(i64::MIN), FileTime(0));
    assert_eq!(FileTime::from_unix_secs(-11_644_473_600), FileTime(0));
    assert_eq!(
        FileTime::from_unix_secs(-11_644_473_599),
        FileTime(TICKS),
        "one second after 1601"
    );
    assert_eq!(FileTime::from_unix_secs(i64::MAX), FileTime(u64::MAX));
    let _ = FileTime(u64::MAX).to_unix_secs();
    assert_eq!(FileTime(0).to_unix_secs(), -11_644_473_600);
    assert_eq!(EpochSecs::from_filetime(FileTime(0)), EpochSecs(0));
    assert_eq!(
        EpochSecs::from_filetime(FileTime(u64::MAX)),
        EpochSecs(u32::MAX)
    );
    let _ = EpochSecs(u32::MAX).to_filetime();
}

#[test]
fn sizes_never_overflow() {
    let s = Sizes {
        logical: u64::MAX,
        allocated: u64::MAX,
        ads_logical: u64::MAX,
        ads_allocated: 1,
        dir_overhead: 1,
        attr_overhead: 1,
    };
    assert_eq!(s.total_allocated(), u64::MAX);
    assert_eq!(s.total_logical(), u64::MAX);
}

#[test]
fn names_with_surrogates_dots_and_astral_characters() {
    let crab: Vec<u16> = "🦀".encode_utf16().collect();
    let cases: &[(Vec<u16>, Option<Vec<u16>>)] = &[
        (
            "a.txt".encode_utf16().collect(),
            Some(vec![0x74, 0x78, 0x74]),
        ),
        (".gitignore".encode_utf16().collect(), None),
        ("trailing.".encode_utf16().collect(), None),
        (".".encode_utf16().collect(), None),
        ("..".encode_utf16().collect(), None),
        ("...".encode_utf16().collect(), None),
        ("a.b.c".encode_utf16().collect(), Some(vec![0x63])),
        (vec![0x61, 0x2E, 0xD800], Some(vec![0xD800])),
        (vec![0xDC00, 0x2E, 0x62], Some(vec![0x62])),
        ([&[0x78, 0x2E][..], &crab].concat(), Some(crab.clone())),
        (Vec::new(), None),
    ];
    for (units, ext) in cases {
        let n = WideName::from_units(units.clone());
        assert_eq!(n.extension_units(), ext.as_deref(), "{units:x?}");
        assert_eq!(n.units(), &units[..]);
        assert_eq!(n.len(), units.len());
        let lossy = n.to_string_lossy();
        assert_eq!(
            n.has_unpaired_surrogate(),
            lossy.contains('\u{FFFD}') && !String::from_utf16_lossy(units).is_empty()
        );
    }
    // Lossless constructor round-trips every scalar, including astral ones.
    let s = "Ærøskøbing-شلום-🦀-\u{10FFFF}";
    let n = WideName::from_str_lossless(s);
    assert_eq!(n.to_string_lossy(), s);
    assert!(!n.has_unpaired_surrogate());
}

#[test]
fn cloud_state_from_real_onedrive_attributes() {
    use strata_core::win32::*;
    let online = FILE_ATTRIBUTE_UNPINNED
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_REPARSE_POINT
        | FILE_ATTRIBUTE_ARCHIVE;
    assert_eq!(CloudState::from_attributes(online), CloudState::OnlineOnly);
    assert_eq!(
        CloudState::from_attributes(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_ARCHIVE),
        CloudState::LocallyAvailable
    );
    assert_eq!(
        CloudState::from_attributes(FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_REPARSE_POINT),
        CloudState::AlwaysKeep
    );
    assert_eq!(ReparseKind::from_tag(0x9000_701A), ReparseKind::Cloud);
    assert!(EntryFlags::from_win32_attributes(online).contains(EntryFlags::OFFLINE));
}
