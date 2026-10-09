//! Content sniffing: detect a file's real type from its first bytes.
//!
//! Used for files with no or misleading extensions, only when
//! the user opens the detail panel or in a background pass over large files.
//! Every check is bounds-checked; malformed or truncated input returns `None`
//! rather than panicking.
//!
//! Read [`HEAD_LEN`] bytes from the start of the file (ISO 9660 needs 32 KiB
//! plus a sector) and, for VHD detection of fixed disks, the last
//! [`TAIL_LEN`] bytes.

use serde::{Deserialize, Serialize};
use strata_core::Category;

/// Bytes to read from the start of a file for full detection.
pub const HEAD_LEN: usize = 0x8800;

/// Bytes to read from the end of a file (fixed VHD footer).
pub const TAIL_LEN: usize = 512;

/// A detected content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectedType {
    /// ZIP container (also docx/xlsx/jar/apk/...).
    Zip,
    /// 7-Zip archive.
    SevenZip,
    /// RAR 1.5–4.x archive.
    Rar4,
    /// RAR 5 archive.
    Rar5,
    /// Windows PE executable or DLL.
    Pe,
    /// PDF document.
    Pdf,
    /// ISO-BMFF video/audio (MP4, M4A, 3GP).
    Mp4,
    /// QuickTime movie.
    Mov,
    /// HEIF/HEIC image.
    Heic,
    /// Matroska video.
    Mkv,
    /// WebM video.
    Webm,
    /// AVI video (RIFF).
    Avi,
    /// WAV audio (RIFF).
    Wav,
    /// MP3 audio.
    Mp3,
    /// FLAC audio.
    Flac,
    /// Ogg container.
    Ogg,
    /// PNG image.
    Png,
    /// JPEG image.
    Jpeg,
    /// GIF image.
    Gif,
    /// WebP image (RIFF).
    Webp,
    /// SQLite 3 database.
    Sqlite,
    /// safetensors model weights.
    Safetensors,
    /// GGUF model weights.
    Gguf,
    /// Virtual Hard Disk (VHD).
    Vhd,
    /// Virtual Hard Disk v2 (VHDX).
    Vhdx,
    /// ISO 9660 / UDF disc image.
    Iso,
}

impl DetectedType {
    /// Every detectable type.
    pub const ALL: [Self; 26] = [
        Self::Zip,
        Self::SevenZip,
        Self::Rar4,
        Self::Rar5,
        Self::Pe,
        Self::Pdf,
        Self::Mp4,
        Self::Mov,
        Self::Heic,
        Self::Mkv,
        Self::Webm,
        Self::Avi,
        Self::Wav,
        Self::Mp3,
        Self::Flac,
        Self::Ogg,
        Self::Png,
        Self::Jpeg,
        Self::Gif,
        Self::Webp,
        Self::Sqlite,
        Self::Safetensors,
        Self::Gguf,
        Self::Vhd,
        Self::Vhdx,
        Self::Iso,
    ];

    /// Rule-pack name (`match.magic`), e.g. `gguf`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "seven_zip",
            Self::Rar4 => "rar4",
            Self::Rar5 => "rar5",
            Self::Pe => "pe",
            Self::Pdf => "pdf",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Heic => "heic",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
            Self::Avi => "avi",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Sqlite => "sqlite",
            Self::Safetensors => "safetensors",
            Self::Gguf => "gguf",
            Self::Vhd => "vhd",
            Self::Vhdx => "vhdx",
            Self::Iso => "iso",
        }
    }

    /// Parses a rule-pack name. `rar` and `7z` are accepted aliases.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "7z" => Some(Self::SevenZip),
            "rar" => Some(Self::Rar5),
            _ => Self::ALL.into_iter().find(|t| t.name() == s),
        }
    }

    /// Human-readable label, e.g. "GGUF model".
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP archive",
            Self::SevenZip => "7-Zip archive",
            Self::Rar4 | Self::Rar5 => "RAR archive",
            Self::Pe => "Windows executable",
            Self::Pdf => "PDF document",
            Self::Mp4 => "MP4 media",
            Self::Mov => "QuickTime movie",
            Self::Heic => "HEIC image",
            Self::Mkv => "Matroska video",
            Self::Webm => "WebM video",
            Self::Avi => "AVI video",
            Self::Wav => "WAV audio",
            Self::Mp3 => "MP3 audio",
            Self::Flac => "FLAC audio",
            Self::Ogg => "Ogg media",
            Self::Png => "PNG image",
            Self::Jpeg => "JPEG image",
            Self::Gif => "GIF image",
            Self::Webp => "WebP image",
            Self::Sqlite => "SQLite database",
            Self::Safetensors => "safetensors model",
            Self::Gguf => "GGUF model",
            Self::Vhd => "VHD disk image",
            Self::Vhdx => "VHDX disk image",
            Self::Iso => "ISO disc image",
        }
    }

    /// Category the content belongs to.
    #[must_use]
    pub fn category(self) -> Category {
        match self {
            Self::Zip
            | Self::SevenZip
            | Self::Rar4
            | Self::Rar5
            | Self::Vhd
            | Self::Vhdx
            | Self::Iso => Category::Archives,
            Self::Pe => Category::Apps,
            Self::Pdf => Category::Documents,
            Self::Sqlite => Category::Unknown,
            Self::Safetensors | Self::Gguf => Category::AiModels,
            _ => Category::Media,
        }
    }

    /// Extensions (lowercase, no dot) that legitimately carry this content.
    #[must_use]
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Zip => &[
                "zip",
                "docx",
                "xlsx",
                "pptx",
                "odt",
                "ods",
                "odp",
                "jar",
                "apk",
                "aab",
                "ipa",
                "nupkg",
                "vsix",
                "epub",
                "whl",
                "xpi",
                "crx",
                "appx",
                "msix",
                "appxbundle",
                "msixbundle",
                "kmz",
                "3mf",
                "cbz",
                "xps",
                "oxps",
                "sketch",
                "fig",
                "pbix",
                "mcpack",
                "mcworld",
                "zipx",
            ],
            Self::SevenZip => &["7z"],
            Self::Rar4 | Self::Rar5 => &["rar", "cbr"],
            Self::Pe => &[
                "exe", "dll", "sys", "scr", "ocx", "cpl", "efi", "mui", "node", "pyd", "com",
                "drv", "winmd", "ax", "tlb", "mun",
            ],
            Self::Pdf => &["pdf", "ai"],
            Self::Mp4 => &["mp4", "m4v", "m4a", "m4b", "3gp", "3g2", "mp4v", "f4v"],
            Self::Mov => &["mov", "qt"],
            Self::Heic => &["heic", "heif", "avif"],
            Self::Mkv => &["mkv", "mka", "mks", "mk3d"],
            Self::Webm => &["webm"],
            Self::Avi => &["avi"],
            Self::Wav => &["wav"],
            Self::Mp3 => &["mp3"],
            Self::Flac => &["flac"],
            Self::Ogg => &["ogg", "oga", "ogv", "opus", "spx"],
            Self::Png => &["png", "apng"],
            Self::Jpeg => &["jpg", "jpeg", "jpe", "jfif"],
            Self::Gif => &["gif"],
            Self::Webp => &["webp"],
            Self::Sqlite => &["db", "sqlite", "sqlite3", "db3", "sdb", "s3db"],
            Self::Safetensors => &["safetensors"],
            Self::Gguf => &["gguf"],
            Self::Vhd => &["vhd", "avhd"],
            Self::Vhdx => &["vhdx", "avhdx"],
            Self::Iso => &["iso", "img", "udf"],
        }
    }
}

fn at<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off.checked_add(N)?)?.try_into().ok()
}

fn u32_le(b: &[u8], off: usize) -> Option<u32> {
    at::<4>(b, off).map(u32::from_le_bytes)
}

fn u64_le(b: &[u8], off: usize) -> Option<u64> {
    at::<8>(b, off).map(u64::from_le_bytes)
}

fn starts(b: &[u8], magic: &[u8]) -> bool {
    b.starts_with(magic)
}

fn sniff_pe(b: &[u8]) -> Option<DetectedType> {
    if !starts(b, b"MZ") {
        return None;
    }
    let off = usize::try_from(u32_le(b, 0x3C)?).ok()?;
    (at::<4>(b, off)? == *b"PE\0\0").then_some(DetectedType::Pe)
}

fn sniff_ftyp(b: &[u8]) -> Option<DetectedType> {
    if at::<4>(b, 4)? != *b"ftyp" {
        return None;
    }
    let brand = at::<4>(b, 8)?;
    Some(match &brand {
        b"qt  " => DetectedType::Mov,
        b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" | b"avif" => {
            DetectedType::Heic
        }
        _ => DetectedType::Mp4,
    })
}

fn sniff_riff(b: &[u8]) -> Option<DetectedType> {
    if !starts(b, b"RIFF") {
        return None;
    }
    match &at::<4>(b, 8)? {
        b"AVI " => Some(DetectedType::Avi),
        b"WAVE" => Some(DetectedType::Wav),
        b"WEBP" => Some(DetectedType::Webp),
        _ => None,
    }
}

fn sniff_ebml(b: &[u8]) -> Option<DetectedType> {
    if !starts(b, &[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }
    let window = &b[..b.len().min(64)];
    let is_webm = window.windows(4).any(|w| w == b"webm");
    Some(if is_webm {
        DetectedType::Webm
    } else {
        DetectedType::Mkv
    })
}

fn sniff_mp3(b: &[u8]) -> Option<DetectedType> {
    if starts(b, b"ID3") && b.len() >= 10 && b[3] < 0x10 {
        return Some(DetectedType::Mp3);
    }
    let h = at::<3>(b, 0)?;
    let sync = h[0] == 0xFF && h[1] & 0xE0 == 0xE0;
    let version = (h[1] >> 3) & 0x3;
    let layer = (h[1] >> 1) & 0x3;
    let bitrate = h[2] >> 4;
    let rate = (h[2] >> 2) & 0x3;
    // Layer 0 is reserved (and is what AAC ADTS uses), version 1 is reserved,
    // bitrate 0xF and sample-rate 3 are invalid; rejecting them keeps random
    // 0xFF-prefixed data from passing as MP3.
    (sync && version != 1 && layer != 0 && bitrate != 0xF && bitrate != 0 && rate != 3)
        .then_some(DetectedType::Mp3)
}

fn sniff_safetensors(b: &[u8]) -> Option<DetectedType> {
    let len = u64_le(b, 0)?;
    // Header JSON is at most 100 MB by the format's own limit.
    if !(2..=100_000_000).contains(&len) {
        return None;
    }
    let first = *b.get(8)?;
    let next = b.get(9).copied();
    (first == b'{' && next.is_none_or(|c| c == b'"' || c == b'}' || c.is_ascii_whitespace()))
        .then_some(DetectedType::Safetensors)
}

fn sniff_gguf(b: &[u8]) -> Option<DetectedType> {
    if !starts(b, b"GGUF") {
        return None;
    }
    let v = u32_le(b, 4)?;
    (1..=16).contains(&v).then_some(DetectedType::Gguf)
}

fn sniff_iso(b: &[u8]) -> Option<DetectedType> {
    let id = at::<5>(b, 0x8001)?;
    matches!(&id, b"CD001" | b"BEA01" | b"NSR02" | b"NSR03").then_some(DetectedType::Iso)
}

fn sniff_pdf(b: &[u8]) -> Option<DetectedType> {
    // The PDF header may be preceded by junk within the first 1 KiB.
    let window = &b[..b.len().min(1024)];
    window
        .windows(5)
        .any(|w| w == b"%PDF-")
        .then_some(DetectedType::Pdf)
}

/// Detects a file's type from its first bytes.
///
/// Pass at least [`HEAD_LEN`] bytes for ISO detection; fewer bytes still
/// detect every other format. Fixed-size VHDs only carry their signature at
/// the end of the file; use [`sniff_with_tail`] for those.
///
/// # Example
///
/// ```
/// use strata_classify::sniff::{sniff, DetectedType};
/// let mut gguf = b"GGUF".to_vec();
/// gguf.extend_from_slice(&3u32.to_le_bytes());
/// assert_eq!(sniff(&gguf), Some(DetectedType::Gguf));
/// assert_eq!(sniff(b"hello"), None);
/// ```
#[must_use]
pub fn sniff(head: &[u8]) -> Option<DetectedType> {
    if starts(head, b"PK\x03\x04") || starts(head, b"PK\x05\x06") || starts(head, b"PK\x07\x08") {
        return Some(DetectedType::Zip);
    }
    if starts(head, &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return Some(DetectedType::SevenZip);
    }
    if starts(head, b"Rar!\x1A\x07\x01\x00") {
        return Some(DetectedType::Rar5);
    }
    if starts(head, b"Rar!\x1A\x07\x00") {
        return Some(DetectedType::Rar4);
    }
    if starts(head, b"vhdxfile") {
        return Some(DetectedType::Vhdx);
    }
    if starts(head, b"conectix") {
        // Dynamic and differencing VHDs keep a footer copy at offset 0.
        return Some(DetectedType::Vhd);
    }
    if starts(head, b"SQLite format 3\0") {
        return Some(DetectedType::Sqlite);
    }
    if starts(head, &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(DetectedType::Png);
    }
    if starts(head, &[0xFF, 0xD8, 0xFF]) {
        return Some(DetectedType::Jpeg);
    }
    if starts(head, b"GIF87a") || starts(head, b"GIF89a") {
        return Some(DetectedType::Gif);
    }
    if starts(head, b"fLaC") {
        return Some(DetectedType::Flac);
    }
    if starts(head, b"OggS") {
        return Some(DetectedType::Ogg);
    }
    sniff_gguf(head)
        .or_else(|| sniff_pe(head))
        .or_else(|| sniff_riff(head))
        .or_else(|| sniff_ftyp(head))
        .or_else(|| sniff_ebml(head))
        .or_else(|| sniff_pdf(head))
        .or_else(|| sniff_iso(head))
        .or_else(|| sniff_safetensors(head))
        .or_else(|| sniff_mp3(head))
}

/// Like [`sniff`], also checking the file's last bytes for a fixed-VHD
/// footer (`conectix` at the start of the final 512-byte sector).
///
/// # Example
///
/// ```
/// use strata_classify::sniff::{sniff_with_tail, DetectedType};
/// let mut tail = vec![0u8; 512];
/// tail[..8].copy_from_slice(b"conectix");
/// assert_eq!(sniff_with_tail(&[0u8; 64], Some(&tail)), Some(DetectedType::Vhd));
/// ```
#[must_use]
pub fn sniff_with_tail(head: &[u8], tail: Option<&[u8]>) -> Option<DetectedType> {
    sniff(head).or_else(|| {
        let tail = tail?;
        // Pre-2004 Virtual PC wrote a 511-byte footer; accept both layouts.
        let footer_at = |len: usize| tail.len().checked_sub(len).and_then(|o| at::<8>(tail, o));
        (footer_at(512) == Some(*b"conectix") || footer_at(511) == Some(*b"conectix"))
            .then_some(DetectedType::Vhd)
    })
}

/// A file whose content disagrees with its extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// The extension the name claims (lowercase), or `None` if it has none.
    pub claimed: Option<String>,
    /// What the content actually is.
    pub detected: DetectedType,
}

impl Mismatch {
    /// UI text, e.g. `Detected: GGUF model (claims .bin)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.claimed {
            Some(ext) => format!("Detected: {} (claims .{ext})", self.detected.label()),
            None => format!("Detected: {} (no extension)", self.detected.label()),
        }
    }
}

/// Compares a file name's extension with sniffed content.
///
/// Returns `None` when the extension is one that legitimately carries the
/// detected content (e.g. `.docx` for ZIP).
///
/// # Example
///
/// ```
/// use strata_classify::sniff::{mismatch, DetectedType};
/// let m = mismatch("llama.bin", DetectedType::Gguf).unwrap();
/// assert_eq!(m.describe(), "Detected: GGUF model (claims .bin)");
/// assert!(mismatch("report.docx", DetectedType::Zip).is_none());
/// ```
#[must_use]
pub fn mismatch(file_name: &str, detected: DetectedType) -> Option<Mismatch> {
    let ext = crate::fold::extension(file_name).map(str::to_ascii_lowercase);
    match ext {
        Some(e) if detected.extensions().contains(&e.as_str()) => None,
        claimed => Some(Mismatch { claimed, detected }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_pad(prefix: &[u8], len: usize) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.resize(len.max(prefix.len()), 0);
        v
    }

    #[test]
    fn detects_every_type() {
        let mut pe = with_pad(b"MZ", 0x100);
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");

        let mut iso = vec![0u8; 0x8010];
        iso[0x8001..0x8006].copy_from_slice(b"CD001");

        let mut st = 64u64.to_le_bytes().to_vec();
        st.extend_from_slice(br#"{"__metadata__":{}}"#);

        let mut gguf = b"GGUF".to_vec();
        gguf.extend_from_slice(&3u32.to_le_bytes());

        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x88];
        mkv.extend_from_slice(b"matroska");
        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x84];
        webm.extend_from_slice(b"webm");

        let cases: Vec<(Vec<u8>, DetectedType)> = vec![
            (b"PK\x03\x04rest".to_vec(), DetectedType::Zip),
            (b"PK\x05\x06".to_vec(), DetectedType::Zip),
            (
                vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4],
                DetectedType::SevenZip,
            ),
            (b"Rar!\x1A\x07\x00".to_vec(), DetectedType::Rar4),
            (b"Rar!\x1A\x07\x01\x00".to_vec(), DetectedType::Rar5),
            (pe, DetectedType::Pe),
            (b"%PDF-1.7\n".to_vec(), DetectedType::Pdf),
            (b"\xEF\xBB\xBF%PDF-1.4".to_vec(), DetectedType::Pdf),
            (b"\0\0\0\x18ftypmp42".to_vec(), DetectedType::Mp4),
            (b"\0\0\0\x14ftypqt  ".to_vec(), DetectedType::Mov),
            (b"\0\0\0\x18ftypheic".to_vec(), DetectedType::Heic),
            (mkv, DetectedType::Mkv),
            (webm, DetectedType::Webm),
            (b"RIFF\0\0\0\0AVI LIST".to_vec(), DetectedType::Avi),
            (b"RIFF\0\0\0\0WAVEfmt ".to_vec(), DetectedType::Wav),
            (b"RIFF\0\0\0\0WEBPVP8 ".to_vec(), DetectedType::Webp),
            (b"ID3\x04\0\0\0\0\0\0".to_vec(), DetectedType::Mp3),
            (vec![0xFF, 0xFB, 0x90, 0x64], DetectedType::Mp3),
            (b"fLaC\0\0\0\x22".to_vec(), DetectedType::Flac),
            (b"OggS\0\x02".to_vec(), DetectedType::Ogg),
            (
                vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                DetectedType::Png,
            ),
            (vec![0xFF, 0xD8, 0xFF, 0xE0], DetectedType::Jpeg),
            (b"GIF89a".to_vec(), DetectedType::Gif),
            (b"SQLite format 3\0".to_vec(), DetectedType::Sqlite),
            (st, DetectedType::Safetensors),
            (gguf, DetectedType::Gguf),
            (b"conectix\0\0\0\x02".to_vec(), DetectedType::Vhd),
            (b"vhdxfile".to_vec(), DetectedType::Vhdx),
            (iso, DetectedType::Iso),
        ];
        let mut seen = std::collections::HashSet::new();
        for (bytes, want) in cases {
            assert_eq!(sniff(&bytes), Some(want), "{want:?}");
            seen.insert(want);
        }
        for t in DetectedType::ALL {
            assert!(seen.contains(&t), "no test for {t:?}");
            assert_eq!(DetectedType::from_name(t.name()), Some(t));
        }
    }

    #[test]
    fn rejects_near_misses() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"M"), None);
        // MZ with a PE offset pointing past the buffer or at garbage.
        let mut mz = with_pad(b"MZ", 0x40);
        mz[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(sniff(&mz), None);
        // AAC ADTS sync (layer 0) is not MP3.
        assert_eq!(sniff(&[0xFF, 0xF1, 0x50, 0x80]), None);
        // Bad bitrate index.
        assert_eq!(sniff(&[0xFF, 0xFB, 0xF0, 0x00]), None);
        // GGUF with absurd version.
        let mut g = b"GGUF".to_vec();
        g.extend_from_slice(&999u32.to_le_bytes());
        assert_eq!(sniff(&g), None);
        // safetensors-like length but not JSON.
        let mut s = 16u64.to_le_bytes().to_vec();
        s.extend_from_slice(b"xx");
        assert_eq!(sniff(&s), None);
        // Huge declared header.
        let mut s = u64::MAX.to_le_bytes().to_vec();
        s.push(b'{');
        assert_eq!(sniff(&s), None);
        // RIFF with unknown form type.
        assert_eq!(sniff(b"RIFF\0\0\0\0CDXA"), None);
        // ISO buffer too short.
        assert_eq!(sniff(&[0u8; 0x8003]), None);
        // VHD tail too short.
        assert_eq!(sniff_with_tail(b"x", Some(b"conec")), None);
    }

    #[test]
    fn mismatch_helper() {
        assert_eq!(
            mismatch("model.BIN", DetectedType::Gguf)
                .unwrap()
                .describe(),
            "Detected: GGUF model (claims .bin)"
        );
        assert_eq!(
            mismatch("Cookies", DetectedType::Sqlite)
                .unwrap()
                .describe(),
            "Detected: SQLite database (no extension)"
        );
        assert!(mismatch("x.GGUF", DetectedType::Gguf).is_none());
        assert!(mismatch("pkg.nupkg", DetectedType::Zip).is_none());
    }

    proptest::proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..0x9000)) {
            let _ = sniff(&bytes);
            let _ = sniff_with_tail(&bytes, Some(&bytes));
        }
    }
}
