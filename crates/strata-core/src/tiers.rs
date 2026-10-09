use serde::{Deserialize, Serialize};

/// Which size every view aggregates and displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    /// Bytes actually used on disk ("what it actually costs").
    #[default]
    Allocated,
    /// Bytes files claim to be.
    Logical,
}

/// Cleanup safety tier, from least to most protected.
///
/// The derived ordering is by strictness, so `max()` picks the stricter tier
/// when rules tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    /// Regenerable, no user data.
    Safe,
    /// Likely junk; review recommended.
    Probably,
    /// User data or large re-downloads; extra confirmation required.
    Careful,
    /// Never deletable through Strata.
    Never,
}

impl Safety {
    /// Stable lowercase key (the serde name), e.g. `probably`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Probably => "probably",
            Self::Careful => "careful",
            Self::Never => "never",
        }
    }

    /// Parses [`Safety::as_str`] output.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Safe, Self::Probably, Self::Careful, Self::Never]
            .into_iter()
            .find(|s| s.as_str() == key)
    }
}

/// Top-level category, color-coded in every view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Category {
    /// Not classified.
    Unknown = 0,
    /// Operating system files.
    System = 1,
    /// Installed applications.
    Apps = 2,
    /// Games and launcher libraries.
    Games = 3,
    /// AI model weights and caches.
    AiModels = 4,
    /// Developer tooling, dependencies and build output.
    DevBuild = 5,
    /// Regenerable caches.
    Caches = 6,
    /// Temporary files.
    Temp = 7,
    /// Downloads folder content.
    Downloads = 8,
    /// Documents.
    Documents = 9,
    /// Photos, video and audio.
    Media = 10,
    /// Archives and disk images.
    Archives = 11,
    /// Cloud-files placeholders.
    Cloud = 12,
    /// Recycle Bin contents.
    RecycleBin = 13,
    /// NTFS metadata files.
    NtfsMetadata = 14,
}

impl Category {
    /// Every category, in display order.
    pub const ALL: [Self; 15] = [
        Self::System,
        Self::Apps,
        Self::Games,
        Self::AiModels,
        Self::DevBuild,
        Self::Caches,
        Self::Temp,
        Self::Downloads,
        Self::Documents,
        Self::Media,
        Self::Archives,
        Self::Cloud,
        Self::RecycleBin,
        Self::NtfsMetadata,
        Self::Unknown,
    ];

    /// Decodes the `u16` stored in the index; unknown values map to `Unknown`.
    #[must_use]
    pub fn from_u16(v: u16) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|c| *c as u16 == v)
            .unwrap_or(Self::Unknown)
    }

    /// Stable lowercase key (the serde name), e.g. `ai_models`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::System => "system",
            Self::Apps => "apps",
            Self::Games => "games",
            Self::AiModels => "ai_models",
            Self::DevBuild => "dev_build",
            Self::Caches => "caches",
            Self::Temp => "temp",
            Self::Downloads => "downloads",
            Self::Documents => "documents",
            Self::Media => "media",
            Self::Archives => "archives",
            Self::Cloud => "cloud",
            Self::RecycleBin => "recycle_bin",
            Self::NtfsMetadata => "ntfs_metadata",
        }
    }

    /// Parses [`Category::key`] output.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }

    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::System => "System",
            Self::Apps => "Apps",
            Self::Games => "Games",
            Self::AiModels => "AI models",
            Self::DevBuild => "Dev & build",
            Self::Caches => "Caches",
            Self::Temp => "Temp",
            Self::Downloads => "Downloads",
            Self::Documents => "Documents",
            Self::Media => "Media",
            Self::Archives => "Archives & disk images",
            Self::Cloud => "Cloud placeholders",
            Self::RecycleBin => "Recycle Bin",
            Self::NtfsMetadata => "NTFS metadata",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stricter_tier_wins_max() {
        assert_eq!(Safety::Safe.max(Safety::Careful), Safety::Careful);
        assert_eq!(Safety::Never.max(Safety::Probably), Safety::Never);
    }

    #[test]
    fn category_u16_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::from_u16(c as u16), c);
        }
        assert_eq!(Category::from_u16(999), Category::Unknown);
    }

    #[test]
    fn keys_match_serde_names() {
        for c in Category::ALL {
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.key())
            );
            assert_eq!(Category::from_key(c.key()), Some(c));
        }
        for s in [
            Safety::Safe,
            Safety::Probably,
            Safety::Careful,
            Safety::Never,
        ] {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
            assert_eq!(Safety::from_key(s.as_str()), Some(s));
        }
    }

    #[test]
    fn serde_names() {
        assert_eq!(
            serde_json::to_string(&Safety::Probably).unwrap(),
            "\"probably\""
        );
        assert_eq!(
            serde_json::to_string(&Category::AiModels).unwrap(),
            "\"ai_models\""
        );
        assert_eq!(
            serde_json::to_string(&SizeMode::Allocated).unwrap(),
            "\"allocated\""
        );
    }
}
