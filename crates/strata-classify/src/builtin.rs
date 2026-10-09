//! Built-in rule packs, embedded at compile time from the repository's
//! `rules/` folder so the binary never reads the repo at runtime.

/// `(file name, TOML text)` of every built-in pack, in load order.
pub const BUILTIN_PACKS: &[(&str, &str)] = &[
    ("windows.toml", include_str!("../../../rules/windows.toml")),
    (
        "userdata.toml",
        include_str!("../../../rules/userdata.toml"),
    ),
    (
        "browsers.toml",
        include_str!("../../../rules/browsers.toml"),
    ),
    ("apps.toml", include_str!("../../../rules/apps.toml")),
    ("ai.toml", include_str!("../../../rules/ai.toml")),
    ("claude.toml", include_str!("../../../rules/claude.toml")),
    ("dev.toml", include_str!("../../../rules/dev.toml")),
    ("games.toml", include_str!("../../../rules/games.toml")),
    ("media.toml", include_str!("../../../rules/media.toml")),
    (
        "downloads.toml",
        include_str!("../../../rules/downloads.toml"),
    ),
    ("generic.toml", include_str!("../../../rules/generic.toml")),
];
