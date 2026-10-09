//! Opening Strata's own web pages in the default browser.
//!
//! The UI can only ask for URLs on an allow-list: the GitHub repository (and
//! anything under it) and the project website. "Report an issue" builds its
//! pre-filled issue URL here, from facts the backend knows, so the page can
//! never smuggle paths or other data into it.

use tauri::{AppHandle, Runtime};

use super::error::FeatureError;

/// The repository; the URL itself and anything under it may be opened.
const REPO: &str = "https://github.com/BankkRoll/STRATA";
/// The project website; anything under it may be opened.
const SITE: &str = "https://bankkroll.github.io/STRATA/";
/// Longest URL accepted; issue URLs with pre-filled fields stay far below it.
const MAX_URL_LEN: usize = 2048;

/// Whether `url` is one of Strata's own pages.
///
/// # Example
///
/// ```
/// use strata_app_lib::features::links::is_allowed_url;
/// assert!(is_allowed_url("https://github.com/BankkRoll/STRATA/issues"));
/// assert!(!is_allowed_url("https://github.com/BankkRoll/STRATA.evil.example"));
/// assert!(!is_allowed_url("file:///C:/Windows"));
/// ```
#[must_use]
pub fn is_allowed_url(url: &str) -> bool {
    if url.len() > MAX_URL_LEN
        || url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\'))
    {
        return false;
    }
    let under_repo = url
        .strip_prefix(REPO)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']));
    under_repo || url.starts_with(SITE)
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The Windows description used in issue reports, e.g. "Windows 11 26100, x64".
fn windows_description(build: u32) -> String {
    let name = if build >= 22000 {
        "Windows 11"
    } else if build > 0 {
        "Windows 10"
    } else {
        "Windows"
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        other => other,
    };
    format!("{name} {build}, {arch}")
}

/// Builds the pre-filled "new issue" URL for the bug report form.
///
/// Only the app version and the Windows build and architecture are included.
#[must_use]
pub fn issue_url(version: &str, windows_build: u32) -> String {
    format!(
        "{REPO}/issues/new?template=bug_report.yml&version={}&windows={}",
        encode_query_value(version),
        encode_query_value(&windows_description(windows_build)),
    )
}

#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), FeatureError> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, w};

    let target = HSTRING::from(url);
    // SAFETY: every pointer argument is a valid NUL-terminated wide string
    // that outlives the call; no window handle or directory is passed.
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL) };
    // NOTE: ShellExecuteW reports success as a pseudo-handle value above 32.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(FeatureError::internal(
            "Windows could not open the default browser.",
        ))
    }
}

#[cfg(not(windows))]
fn open_in_browser(_url: &str) -> Result<(), FeatureError> {
    Err(FeatureError::internal("Opening links needs Windows."))
}

/// Opens one of Strata's own pages in the default browser.
///
/// # Errors
///
/// Refuses any URL outside the repository and the website.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), FeatureError> {
    if !is_allowed_url(&url) {
        return Err(FeatureError::invalid(
            "Only Strata's GitHub and website pages can be opened.",
        ));
    }
    open_in_browser(&url)
}

/// Opens a pre-filled GitHub bug report in the default browser.
///
/// # Errors
///
/// Fails only when Windows cannot launch the browser.
#[tauri::command]
pub fn report_issue<R: Runtime>(app: AppHandle<R>) -> Result<(), FeatureError> {
    let url = issue_url(
        &app.package_info().version.to_string(),
        crate::backdrop::windows_build(),
    );
    open_in_browser(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_accepts_only_strata_pages() {
        for ok in [
            "https://github.com/BankkRoll/STRATA",
            "https://github.com/BankkRoll/STRATA/",
            "https://github.com/BankkRoll/STRATA/releases/latest",
            "https://github.com/BankkRoll/STRATA?tab=readme",
            "https://github.com/BankkRoll/STRATA#readme",
            "https://bankkroll.github.io/STRATA/",
            "https://bankkroll.github.io/STRATA/#benchmarks",
        ] {
            assert!(is_allowed_url(ok), "{ok}");
        }
        for bad in [
            "https://github.com/BankkRoll/STRATA.evil.example",
            "https://github.com/BankkRoll/STRATAX",
            "http://github.com/BankkRoll/STRATA",
            "https://github.com/someone-else/STRATA",
            "https://bankkroll.github.io/STRATAx/",
            "https://bankkroll.github.io/",
            "file:///C:/Windows/System32/cmd.exe",
            "https://github.com/BankkRoll/STRATA/\" & calc",
            "https://github.com/BankkRoll/STRATA/ x",
            "https://github.com/BankkRoll/STRATA\\..\\x",
            "",
        ] {
            assert!(!is_allowed_url(bad), "{bad}");
        }
        assert!(!is_allowed_url(&format!(
            "{REPO}/{}",
            "a".repeat(MAX_URL_LEN)
        )));
    }

    #[test]
    fn issue_url_is_encoded_and_allowed() {
        let url = issue_url("0.1.0", 26100);
        assert!(
            url.starts_with(
                "https://github.com/BankkRoll/STRATA/issues/new?template=bug_report.yml"
            )
        );
        assert!(url.contains("&version=0.1.0"));
        let expected_arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "ARM64",
            other => other,
        };
        assert!(url.contains(&format!(
            "&windows=Windows%2011%2026100%2C%20{expected_arch}"
        )));
        assert!(is_allowed_url(&url));
        assert!(!url.contains('\\'));
    }

    #[test]
    fn windows_names_follow_the_build() {
        assert!(windows_description(19045).starts_with("Windows 10 19045"));
        assert!(windows_description(22000).starts_with("Windows 11 22000"));
        assert!(windows_description(0).starts_with("Windows 0"));
    }

    #[test]
    fn encoding_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(encode_query_value("a-Z_9.~"), "a-Z_9.~");
        assert_eq!(encode_query_value("a b&c=d"), "a%20b%26c%3Dd");
    }
}
