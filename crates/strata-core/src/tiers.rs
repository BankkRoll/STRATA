use serde::{Deserialize, Serialize};

/// Which size every view aggregates and displays. See SPEC §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    /// Bytes actually used on disk ("what it actually costs").
    #[default]
    Allocated,
    /// Bytes files claim to be.
    Logical,
}

/// Cleanup safety tier, from least to most protected. See SPEC §15.1.
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

/// Top-level category, color-coded in every view. See SPEC §12.5.
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
