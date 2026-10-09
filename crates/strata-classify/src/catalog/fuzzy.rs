//! Fuzzy matching of folder names against app names and publishers.
//!
//! Names are normalized into tokens: lowercased, split on punctuation,
//! whitespace, digits-to-letters and camelCase boundaries, with corporate
//! suffixes and generic words dropped ("Inc", "LLC", "Corporation",
//! "Software", "x64", version numbers) and vendor aliases applied. Scores:
//! - 1.0: identical token strings (`obs-studio` vs `OBS Studio`).
//! - 0.85: one token set contains the other (`Docker` vs `Docker Desktop`).
//! - up to 0.95 × similarity: small edit distance on the joined tokens
//!   (`Telegram Desktp` vs `Telegram Desktop`).
//! - otherwise 0.8 × Jaccard similarity of the token sets.

/// Words that carry no identity in app or vendor names.
const NOISE: &[&str] = &[
    "inc",
    "llc",
    "ltd",
    "limited",
    "corp",
    "corporation",
    "co",
    "company",
    "gmbh",
    "ag",
    "sa",
    "srl",
    "bv",
    "pty",
    "plc",
    "kk",
    "the",
    "software",
    "technologies",
    "technology",
    "systems",
    "x64",
    "x86",
    "64",
    "32",
    "bit",
    "amd64",
    "arm64",
    "win",
    "win64",
    "win32",
    "windows",
    "edition",
    "version",
    "setup",
    "installer",
    "for",
    "and",
    "of",
    "by",
    "app",
    "application",
];

/// Folder or publisher spellings mapped to a canonical vendor token.
const ALIASES: &[(&str, &str)] = &[
    ("bravesoftware", "brave"),
    ("msft", "microsoft"),
    ("ms", "microsoft"),
    ("nvidiacorporation", "nvidia"),
    ("advancedmicrodevices", "amd"),
    ("valve", "steam"),
    ("valvecorporation", "steam"),
    ("epicgames", "epic"),
    ("electronicarts", "ea"),
    ("ubisoftentertainment", "ubisoft"),
    ("blizzardentertainment", "blizzard"),
    ("anthropicpbc", "anthropic"),
    ("anthropicclaude", "claude"),
    ("googlellc", "google"),
    ("mozillacorporation", "mozilla"),
    ("operanorway", "opera"),
    ("operasoftware", "opera"),
    ("vivaldetechnologies", "vivaldi"),
    ("thebrowsercompany", "arc"),
    ("browsercompany", "arc"),
    ("slacktechnologies", "slack"),
    ("spotifyab", "spotify"),
    ("zoomvideocommunications", "zoom"),
    ("obsproject", "obs"),
];

fn split_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if let Some(p) = prev {
            let boundary = (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() && c.is_ascii_digit())
                || (p.is_ascii_digit() && c.is_alphabetic());
            if boundary {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// Normalizes a name into identity tokens.
///
/// # Example
///
/// ```
/// use strata_classify::catalog::fuzzy::tokens;
/// assert_eq!(tokens("BraveSoftware"), vec!["brave"]);
/// assert_eq!(tokens("NVIDIA Corporation"), vec!["nvidia"]);
/// assert_eq!(tokens("obs-studio 30.1.2 (64-bit)"), vec!["obs", "studio"]);
/// ```
#[must_use]
pub fn tokens(name: &str) -> Vec<String> {
    let spaced = split_camel(name);
    let mut raw: Vec<String> = spaced
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    let alias_of = |toks: &[String]| {
        let joined = toks.concat();
        ALIASES
            .iter()
            .find(|(a, _)| *a == joined)
            .map(|(_, c)| vec![(*c).to_string()])
    };
    // Whole-name aliases, before and after dropping noise words
    // ("NVIDIA Corporation", "Epic Games, Inc.").
    if let Some(v) = alias_of(&raw) {
        return v;
    }
    raw.retain(|t| {
        !NOISE.contains(&t.as_str())
            && !t.chars().all(|c| c.is_ascii_digit())
            && t.chars().count() > 1
    });
    if let Some(v) = alias_of(&raw) {
        return v;
    }
    for t in &mut raw {
        if let Some((_, canon)) = ALIASES.iter().find(|(a, _)| *a == t.as_str()) {
            *t = (*canon).to_string();
        }
    }
    raw.dedup();
    raw
}

/// Levenshtein distance, aborting early once `limit` is exceeded.
#[must_use]
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > limit {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= limit).then_some(d)
}

/// Similarity of two token lists in `[0, 1]`.
///
/// # Example
///
/// ```
/// use strata_classify::catalog::fuzzy::{score, tokens};
/// let s = |a: &str, b: &str| score(&tokens(a), &tokens(b));
/// assert_eq!(s("obs-studio", "OBS Studio"), 1.0);
/// assert!(s("Docker", "Docker Desktop") >= 0.85);
/// assert!(s("Telegram Desktp", "Telegram Desktop") > 0.8);
/// assert!(s("Steam", "Slack") < 0.75);
/// ```
#[must_use]
pub fn score(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let ja = a.concat();
    let jb = b.concat();
    if ja == jb {
        return 1.0;
    }
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let contained = small.iter().all(|t| large.contains(t));
    // Single short tokens ("go", "qt") are too ambiguous for containment.
    if contained && small.iter().map(String::len).sum::<usize>() >= 3 {
        return 0.85;
    }
    let max_len = ja.chars().count().max(jb.chars().count());
    if max_len >= 5 {
        let limit = (max_len / 6).max(1);
        if let Some(d) = edit_distance(&ja, &jb, limit) {
            #[allow(clippy::cast_precision_loss)]
            let sim = 1.0 - d as f32 / max_len as f32;
            return 0.95 * sim;
        }
    }
    let inter = a.iter().filter(|t| b.contains(t)).count();
    let union = a.len() + b.len() - inter;
    #[allow(clippy::cast_precision_loss)]
    let j = inter as f32 / union as f32;
    0.8 * j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &str, b: &str) -> f32 {
        score(&tokens(a), &tokens(b))
    }

    #[test]
    fn fixtures() {
        // (folder, app/publisher, minimum score)
        let positives = [
            ("Discord", "Discord", 1.0),
            ("BraveSoftware", "Brave Software Inc", 1.0),
            ("NVIDIA Corporation", "NVIDIA Corporation", 1.0),
            ("GitHubDesktop", "GitHub Desktop", 1.0),
            ("obs-studio", "OBS Studio", 1.0),
            ("Telegram Desktop", "Telegram Desktop 5.2", 1.0),
            ("Epic Games", "Epic Games, Inc.", 1.0),
            ("Docker", "Docker Desktop", 0.85),
            ("Zoom", "Zoom Workplace (64-bit)", 0.85),
            ("Mozilla", "Mozilla Firefox (x64 en-US)", 0.85),
            ("MongoDBCompass", "MongoDB Compass", 1.0),
            ("Ledger Live", "Ledger Live 2.80", 1.0),
            ("Postmann", "Postman", 0.75),
            ("AnthropicClaude", "Claude", 1.0),
        ];
        for (a, b, min) in positives {
            assert!(s(a, b) >= min, "{a} vs {b}: {}", s(a, b));
        }
        let negatives = [
            ("Steam", "Slack"),
            ("Google", "Git"),
            ("go", "Google Chrome"),
            ("Microsoft", "Discord"),
            ("Temp", "Telegram Desktop"),
            ("Code", "Codota"),
        ];
        for (a, b) in negatives {
            assert!(s(a, b) < 0.75, "{a} vs {b}: {}", s(a, b));
        }
    }

    #[test]
    fn edit_distance_bounds() {
        assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
        assert_eq!(edit_distance("kitten", "sitting", 2), None);
        assert_eq!(edit_distance("", "abc", 5), Some(3));
        assert_eq!(edit_distance("abc", "abc", 0), Some(0));
    }

    #[test]
    fn tokenization() {
        assert_eq!(tokens("Microsoft Corporation"), vec!["microsoft"]);
        assert_eq!(tokens("7-Zip 24.08 (x64)"), vec!["zip"]);
        assert_eq!(tokens("JetBrains s.r.o."), vec!["jet", "brains"]);
        assert_eq!(tokens("JetBrains"), vec!["jet", "brains"]);
        assert!(tokens("").is_empty());
    }
}
