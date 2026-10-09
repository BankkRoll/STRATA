//! Running-app awareness (SPEC §15.5): "Close Chrome first".
//!
//! Browser and Electron caches are written continuously while their app
//! runs; deleting them underneath it fails or corrupts state. A path is
//! matched against a table of known cache owners, and the owners' processes
//! (plus anything Restart Manager says holds files there) are reported.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::canon::CanonicalPath;
use crate::locks::LockHolder;
use crate::win::process;

/// An app whose caches live under recognizable folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheOwner {
    /// Display name.
    pub app: &'static str,
    /// Executable names (case-insensitive).
    pub exe_names: &'static [&'static str],
    /// Folder sequences (uppercase) that mark its data; a trailing `*`
    /// matches a name prefix (package family names).
    pub markers: &'static [&'static [&'static str]],
}

/// Known cache owners.
pub const CACHE_OWNERS: &[CacheOwner] = &[
    CacheOwner {
        app: "Google Chrome",
        exe_names: &["chrome.exe"],
        markers: &[&["GOOGLE", "CHROME"]],
    },
    CacheOwner {
        app: "Microsoft Edge",
        exe_names: &["msedge.exe", "msedgewebview2.exe"],
        markers: &[&["MICROSOFT", "EDGE"]],
    },
    CacheOwner {
        app: "Brave",
        exe_names: &["brave.exe"],
        markers: &[&["BRAVESOFTWARE"]],
    },
    CacheOwner {
        app: "Opera",
        exe_names: &["opera.exe", "opera_gx.exe"],
        markers: &[&["OPERA SOFTWARE"]],
    },
    CacheOwner {
        app: "Vivaldi",
        exe_names: &["vivaldi.exe"],
        markers: &[&["VIVALDI", "USER DATA"]],
    },
    CacheOwner {
        app: "Arc",
        exe_names: &["arc.exe"],
        markers: &[&["PACKAGES", "THEBROWSERCOMPANY.ARC*"]],
    },
    CacheOwner {
        app: "Firefox",
        exe_names: &["firefox.exe"],
        markers: &[&["MOZILLA", "FIREFOX"]],
    },
    CacheOwner {
        app: "Discord",
        exe_names: &["discord.exe", "discordcanary.exe", "discordptb.exe"],
        markers: &[&["DISCORD"], &["DISCORDCANARY"], &["DISCORDPTB"]],
    },
    CacheOwner {
        app: "Slack",
        exe_names: &["slack.exe"],
        markers: &[
            &["ROAMING", "SLACK"],
            &["LOCAL", "SLACK"],
            &["PACKAGES", "91750D7E.SLACK*"],
        ],
    },
    CacheOwner {
        app: "Microsoft Teams",
        exe_names: &["ms-teams.exe", "teams.exe"],
        markers: &[&["MICROSOFT", "TEAMS"], &["PACKAGES", "MSTEAMS_*"]],
    },
    CacheOwner {
        app: "Visual Studio Code",
        exe_names: &["code.exe"],
        markers: &[&["ROAMING", "CODE"]],
    },
    CacheOwner {
        app: "Cursor",
        exe_names: &["cursor.exe"],
        markers: &[&["ROAMING", "CURSOR"]],
    },
    CacheOwner {
        app: "Spotify",
        exe_names: &["spotify.exe"],
        markers: &[&["SPOTIFY"], &["PACKAGES", "SPOTIFYAB.SPOTIFYMUSIC*"]],
    },
    CacheOwner {
        app: "Zoom",
        exe_names: &["zoom.exe"],
        markers: &[&["ROAMING", "ZOOM"]],
    },
    CacheOwner {
        app: "Notion",
        exe_names: &["notion.exe"],
        markers: &[&["ROAMING", "NOTION"]],
    },
    CacheOwner {
        app: "Figma",
        exe_names: &["figma.exe", "figma_agent.exe"],
        markers: &[&["ROAMING", "FIGMA"], &["LOCAL", "FIGMA"]],
    },
    CacheOwner {
        app: "Obsidian",
        exe_names: &["obsidian.exe"],
        markers: &[&["ROAMING", "OBSIDIAN"]],
    },
    CacheOwner {
        app: "Claude",
        exe_names: &["claude.exe"],
        markers: &[&["ROAMING", "CLAUDE"], &["LOCAL", "ANTHROPICCLAUDE"]],
    },
];

fn marker_matches(components: &[String], marker: &[&str]) -> bool {
    if marker.is_empty() || components.len() < marker.len() {
        return false;
    }
    components.windows(marker.len()).any(|w| {
        w.iter()
            .zip(marker)
            .all(|(c, m)| match m.strip_suffix('*') {
                Some(prefix) => c.starts_with(prefix),
                None => c == m,
            })
    })
}

/// Owners whose cache folders contain `path`.
#[must_use]
pub fn cache_owners(path: &Path) -> Vec<&'static CacheOwner> {
    let Ok(c) = CanonicalPath::parse(path) else {
        return Vec::new();
    };
    let comps: Vec<String> = c
        .components()
        .iter()
        .map(|n| String::from_utf16_lossy(n.folded()))
        .collect();
    CACHE_OWNERS
        .iter()
        .filter(|o| o.markers.iter().any(|m| marker_matches(&comps, m)))
        .collect()
}

/// Why an app is mentioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningReason {
    /// The path is that app's cache and the app is running.
    OwnsCache,
    /// Restart Manager reports the app holding files there.
    HoldsFiles,
}

/// "Close X first."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningAppWarning {
    /// App name.
    pub app: String,
    /// Process ids.
    pub pids: Vec<u32>,
    /// Why.
    pub reason: WarningReason,
}

impl RunningAppWarning {
    /// Sentence for the UI.
    #[must_use]
    pub fn message(&self) -> String {
        format!("Close {} first; it is using these files.", self.app)
    }
}

/// Matches owners and lock holders against a process list. Pure.
#[must_use]
pub fn match_running(
    owners: &[&CacheOwner],
    processes: &[(u32, String)],
    holders: &[LockHolder],
) -> Vec<RunningAppWarning> {
    let mut out = Vec::new();
    for o in owners {
        let pids: Vec<u32> = processes
            .iter()
            .filter(|(_, exe)| o.exe_names.iter().any(|n| n.eq_ignore_ascii_case(exe)))
            .map(|(pid, _)| *pid)
            .collect();
        if !pids.is_empty() {
            out.push(RunningAppWarning {
                app: o.app.to_string(),
                pids,
                reason: WarningReason::OwnsCache,
            });
        }
    }
    let mut by_app: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for h in holders {
        if out.iter().any(|w| w.pids.contains(&h.pid)) {
            continue;
        }
        by_app.entry(h.app_name.clone()).or_default().push(h.pid);
    }
    out.extend(by_app.into_iter().map(|(app, pids)| RunningAppWarning {
        app,
        pids,
        reason: WarningReason::HoldsFiles,
    }));
    out
}

/// Running apps the user should close before cleaning `path`.
///
/// # Errors
///
/// Fails when processes cannot be listed.
pub fn running_app_warnings(
    path: &Path,
    holders: &[LockHolder],
) -> io::Result<Vec<RunningAppWarning>> {
    let owners = cache_owners(path);
    let processes: Vec<(u32, String)> = if owners.is_empty() {
        Vec::new()
    } else {
        process::processes()?
            .into_iter()
            .map(|p| (p.pid, p.exe_name))
            .collect()
    };
    Ok(match_running(&owners, &processes, holders))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locks::AppKind;

    fn names(p: &str) -> Vec<&'static str> {
        cache_owners(Path::new(p)).iter().map(|o| o.app).collect()
    }

    #[test]
    fn recognizes_cache_folders() {
        assert_eq!(
            names(r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache\Cache_Data"),
            ["Google Chrome"]
        );
        assert_eq!(
            names(r"C:\Users\me\AppData\Local\Microsoft\Edge\User Data\Default\Code Cache"),
            ["Microsoft Edge"]
        );
        assert_eq!(
            names(r"C:\Users\me\AppData\Roaming\discord\Cache"),
            ["Discord"]
        );
        assert_eq!(
            names(r"C:\Users\me\AppData\Roaming\Code\GPUCache"),
            ["Visual Studio Code"]
        );
        assert_eq!(
            names(r"C:\Users\me\AppData\Local\Packages\MSTeams_8wekyb3d8bbwe\LocalCache"),
            ["Microsoft Teams"]
        );
        assert_eq!(
            names(r"C:\Users\me\AppData\Roaming\Mozilla\Firefox\Profiles\x"),
            ["Firefox"]
        );
        assert!(names(r"D:\src\code\target").is_empty());
        assert!(names(r"C:\Users\me\Documents\chrome-notes.txt").is_empty());
    }

    #[test]
    fn running_owner_and_lock_holders_are_reported() {
        let owners = cache_owners(Path::new(r"C:\Users\me\AppData\Roaming\Slack\Cache"));
        let procs = vec![
            (10, "slack.exe".to_string()),
            (11, "Slack.exe".to_string()),
            (12, "explorer.exe".to_string()),
        ];
        let holder = LockHolder {
            pid: 99,
            start_time: 1,
            app_name: "Antivirus".into(),
            exe_path: None,
            service: None,
            kind: AppKind::Service,
            restartable: false,
        };
        let w = match_running(&owners, &procs, std::slice::from_ref(&holder));
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].app, "Slack");
        assert_eq!(w[0].pids, [10, 11]);
        assert_eq!(w[0].reason, WarningReason::OwnsCache);
        assert_eq!(w[1].reason, WarningReason::HoldsFiles);
        assert!(w[0].message().contains("Close Slack first"));
    }

    #[test]
    fn nothing_running_means_no_warning() {
        let owners = cache_owners(Path::new(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data",
        ));
        assert!(match_running(&owners, &[(1, "notepad.exe".into())], &[]).is_empty());
    }

    #[test]
    fn live_process_list_works() {
        // This test process is running, so a fake owner keyed on our own exe
        // name must be found.
        let me = std::env::current_exe().unwrap();
        let exe = me.file_name().unwrap().to_string_lossy().into_owned();
        let procs: Vec<(u32, String)> = process::processes()
            .unwrap()
            .into_iter()
            .map(|p| (p.pid, p.exe_name))
            .collect();
        assert!(
            procs
                .iter()
                .any(|(pid, e)| *pid == std::process::id() && e.eq_ignore_ascii_case(&exe))
        );
    }
}
