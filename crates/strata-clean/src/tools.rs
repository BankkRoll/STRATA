//! Built-in Windows tool actions.
//!
//! Every action is first turned into a [`CommandSpec`]: the exact program,
//! arguments and a plain-language description the UI shows before the user
//! confirms. Only then can [`run`] execute it. Tools are launched by absolute
//! path under `{WINDIR}` so a planted `cleanmgr.exe` elsewhere on `PATH` is
//! never picked up.
//!
//! Hibernation is guidance only: Strata never runs `powercfg`. Uninstallers
//! always run with their own UI; silent flags are stripped and listed.
//! Emptying the Recycle Bin needs a [`Consent`] minted from a [`Prompt`]
//! that showed the current item count and size.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::known::{KnownFolder, KnownFolders};

use crate::consent::{Consent, ConsentError, EmptyRecycleBin, Prompt};
use crate::win::{process, vol};

/// Paths the tool launcher needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContext {
    /// `{WINDIR}`.
    pub windir: PathBuf,
}

impl ToolContext {
    /// Builds the context from resolved known folders.
    #[must_use]
    pub fn from_known(known: &KnownFolders) -> Option<Self> {
        known
            .machine
            .get(&KnownFolder::Windir)
            .map(|w| Self { windir: w.clone() })
    }

    fn system32(&self, exe: &str) -> String {
        self.windir.join("System32").join(exe).display().to_string()
    }
}

/// A built-in tool the UI can offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolAction {
    /// Windows Disk Cleanup, optionally for one drive.
    DiskCleanup {
        /// Drive letter.
        drive: Option<char>,
    },
    /// The Storage Sense settings page.
    StorageSenseSettings,
    /// `DISM /Online /Cleanup-Image /StartComponentCleanup` (elevated,
    /// output captured).
    DismComponentCleanup,
    /// System Protection (restore point and shadow copy limits).
    SystemProtection,
    /// How to remove `hiberfil.sys` (`powercfg /h off`), never executed.
    HibernationGuidance,
    /// An app's own uninstaller.
    Uninstall {
        /// App name shown to the user.
        app_name: String,
        /// `UninstallString` from the registry.
        uninstall_string: String,
    },
    /// `compact /compactos:query` (read-only status).
    CompactOsStatus,
}

/// How a command is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Launch {
    /// Started directly; output captured when [`CommandSpec::captures_output`].
    Process,
    /// Opened through the Shell (honours UAC manifests and URIs).
    ShellOpen,
    /// Needs elevation; runs through a UAC prompt with output captured.
    Elevated,
    /// Not runnable: the description tells the user what to do.
    GuidanceOnly,
}

/// The exact command and the text shown before running it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Short title.
    pub title: String,
    /// What it does, for the confirmation dialog.
    pub description: String,
    /// Program (absolute path or URI).
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// The exact command line shown to the user.
    pub command_line: String,
    /// How it starts.
    pub launch: Launch,
    /// Whether output is captured and shown.
    pub captures_output: bool,
    /// Silent-install flags removed from an uninstall string.
    pub removed_flags: Vec<String>,
}

/// Tool launch failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// Guidance-only actions are never executed.
    #[error("this action is guidance only and is never run automatically")]
    NotRunnable,
    /// The uninstall string could not be understood.
    #[error("the uninstall command is malformed: {0}")]
    BadUninstallString(String),
    /// The consent was stale or expired.
    #[error(transparent)]
    Consent(ConsentError),
    /// Launch failed.
    #[error("could not start the tool: {0}")]
    Launch(String),
}

fn quote(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '\t']) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(quote(program))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the command for `action`.
///
/// # Errors
///
/// [`ToolError::BadUninstallString`] for unparseable uninstall commands.
pub fn build(ctx: &ToolContext, action: &ToolAction) -> Result<CommandSpec, ToolError> {
    let spec =
        |title: &str, description: &str, program: String, args: Vec<String>, launch, capture| {
            CommandSpec {
                title: title.into(),
                description: description.into(),
                command_line: command_line(&program, &args),
                program,
                args,
                launch,
                captures_output: capture,
                removed_flags: Vec::new(),
            }
        };
    Ok(match action {
        ToolAction::DiskCleanup { drive } => spec(
            "Disk Cleanup",
            "Opens Windows Disk Cleanup, which lists system files it can remove safely. You choose what to delete there.",
            ctx.system32("cleanmgr.exe"),
            drive
                .filter(char::is_ascii_alphabetic)
                .map(|d| vec!["/d".into(), d.to_ascii_uppercase().to_string()])
                .unwrap_or_default(),
            Launch::ShellOpen,
            false,
        ),
        ToolAction::StorageSenseSettings => spec(
            "Storage Sense",
            "Opens the Storage Sense page in Settings, where Windows can clean temporary files automatically.",
            "ms-settings:storagesense".into(),
            Vec::new(),
            Launch::ShellOpen,
            false,
        ),
        ToolAction::DismComponentCleanup => spec(
            "Clean up Windows component store",
            "Runs DISM component cleanup as administrator. It removes superseded Windows Update components from WinSxS. It can take several minutes and cannot be undone; installed updates can no longer be uninstalled afterwards. Output is shown when it finishes.",
            ctx.system32("Dism.exe"),
            vec![
                "/Online".into(),
                "/Cleanup-Image".into(),
                "/StartComponentCleanup".into(),
            ],
            Launch::Elevated,
            true,
        ),
        ToolAction::SystemProtection => spec(
            "System Protection",
            "Opens System Protection, where you can limit or delete restore points (shadow copies) per drive.",
            ctx.system32("SystemPropertiesProtection.exe"),
            Vec::new(),
            Launch::ShellOpen,
            false,
        ),
        ToolAction::HibernationGuidance => spec(
            "Hibernation file",
            "hiberfil.sys holds memory for hibernation and Fast Startup, usually 40% of RAM. To remove it, open Terminal as administrator and run: powercfg /h off. This also turns off Fast Startup. Run powercfg /h on to bring it back. Strata never runs this for you.",
            "powercfg".into(),
            vec!["/h".into(), "off".into()],
            Launch::GuidanceOnly,
            false,
        ),
        ToolAction::CompactOsStatus => spec(
            "CompactOS status",
            "Shows whether Windows system files are compressed (CompactOS). This only reads the status.",
            ctx.system32("compact.exe"),
            vec!["/compactos:query".into()],
            Launch::Process,
            true,
        ),
        ToolAction::Uninstall {
            app_name,
            uninstall_string,
        } => {
            let parsed = parse_uninstall_string(ctx, uninstall_string)?;
            let mut s = spec(
                &format!("Uninstall {app_name}"),
                &format!(
                    "Starts {app_name}'s own uninstaller. Follow its prompts; Strata does not answer them for you."
                ),
                parsed.program,
                parsed.args,
                Launch::ShellOpen,
                false,
            );
            s.removed_flags = parsed.removed_flags;
            s
        }
    })
}

/// A parsed `UninstallString`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUninstall {
    /// Program to start.
    pub program: String,
    /// Arguments, silent flags removed.
    pub args: Vec<String>,
    /// Silent flags that were removed.
    pub removed_flags: Vec<String>,
}

const SILENT_FLAGS: &[&str] = &[
    "/s",
    "-s",
    "/silent",
    "-silent",
    "--silent",
    "/verysilent",
    "/quiet",
    "-quiet",
    "--quiet",
    "/q",
    "/qn",
    "/qn+",
    "/qb",
    "/qb-",
    "/qb!",
    "/qr",
    "/passive",
    "/suppressmsgboxes",
    "/norestart",
    "/forcerestart",
];

fn is_silent_flag(a: &str) -> bool {
    SILENT_FLAGS.contains(&a.to_ascii_lowercase().as_str())
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                any = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

fn find_guid(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let g = s.get(start..start + 38)?;
    let ok = g.ends_with('}')
        && g[1..37].char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    ok.then_some(g)
}

/// Parses an `UninstallString`: quoted or unquoted program paths (including
/// unquoted paths with spaces), `MsiExec.exe /I{GUID}` or `/X{GUID}`
/// (normalized to `/X{GUID}` with the full UI), and silent flags (removed).
///
/// # Errors
///
/// [`ToolError::BadUninstallString`] when empty or an MSI string has no
/// product code.
///
/// # Example
///
/// ```
/// use strata_clean::tools::{ToolContext, parse_uninstall_string};
/// let ctx = ToolContext { windir: r"C:\Windows".into() };
/// let p = parse_uninstall_string(&ctx, "MsiExec.exe /I{11111111-2222-3333-4444-555555555555} /qn").unwrap();
/// assert_eq!(p.program, r"C:\Windows\System32\msiexec.exe");
/// assert_eq!(p.args, ["/X{11111111-2222-3333-4444-555555555555}"]);
/// assert_eq!(p.removed_flags, ["/qn"]);
/// ```
pub fn parse_uninstall_string(ctx: &ToolContext, s: &str) -> Result<ParsedUninstall, ToolError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ToolError::BadUninstallString("empty".into()));
    }
    let (program, rest) = if let Some(stripped) = s.strip_prefix('"') {
        let end = stripped
            .find('"')
            .ok_or_else(|| ToolError::BadUninstallString("unterminated quote".into()))?;
        (stripped[..end].to_string(), &stripped[end + 1..])
    } else {
        let lower = s.to_ascii_lowercase();
        // Unquoted paths with spaces: the program ends at the first `.exe`
        // followed by whitespace or the end.
        let exe_end = lower
            .match_indices(".exe")
            .map(|(i, _)| i + 4)
            .find(|&e| e == s.len() || s[e..].starts_with(char::is_whitespace));
        match exe_end {
            Some(e) => (s[..e].to_string(), &s[e..]),
            None => {
                let e = s.find(char::is_whitespace).unwrap_or(s.len());
                (s[..e].to_string(), &s[e..])
            }
        }
    };
    let args = split_args(rest);
    let base = Path::new(&program)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if base == "msiexec" || base == "msiexec.exe" {
        let guid = find_guid(rest)
            .ok_or_else(|| ToolError::BadUninstallString("no MSI product code".into()))?;
        let removed_flags = args.iter().filter(|a| is_silent_flag(a)).cloned().collect();
        return Ok(ParsedUninstall {
            program: ctx.system32("msiexec.exe"),
            args: vec![format!("/X{guid}")],
            removed_flags,
        });
    }
    let (removed_flags, args): (Vec<String>, Vec<String>) =
        args.into_iter().partition(|a| is_silent_flag(a));
    Ok(ParsedUninstall {
        program,
        args,
        removed_flags,
    })
}

/// Result of running a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Exit code, when waited for.
    pub exit_code: Option<i64>,
    /// Captured output (stdout then stderr), when captured.
    pub output: Option<String>,
}

/// Runs a command the user confirmed.
///
/// # Errors
///
/// [`ToolError::NotRunnable`] for guidance, [`ToolError::Launch`] otherwise.
pub fn run(spec: &CommandSpec) -> Result<ToolOutput, ToolError> {
    let launch_err = |e: std::io::Error| ToolError::Launch(e.to_string());
    match spec.launch {
        Launch::GuidanceOnly => Err(ToolError::NotRunnable),
        Launch::ShellOpen => {
            let params = (!spec.args.is_empty()).then(|| {
                spec.args
                    .iter()
                    .map(|a| quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            process::shell_execute(Some("open"), &spec.program, params.as_deref(), false)
                .map_err(launch_err)?;
            Ok(ToolOutput {
                exit_code: None,
                output: None,
            })
        }
        Launch::Process => {
            let out = std::process::Command::new(&spec.program)
                .args(&spec.args)
                .output()
                .map_err(launch_err)?;
            Ok(ToolOutput {
                exit_code: out.status.code().map(i64::from),
                output: Some(format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )),
            })
        }
        Launch::Elevated if process::is_elevated() => run(&CommandSpec {
            launch: Launch::Process,
            ..spec.clone()
        }),
        Launch::Elevated => run_elevated_captured(spec),
    }
}

fn run_elevated_captured(spec: &CommandSpec) -> Result<ToolOutput, ToolError> {
    // An unelevated process cannot read an elevated child's pipes, so the
    // elevated `cmd` redirects into a temp file we read afterwards.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let log =
        std::env::temp_dir().join(format!("strata-tool-{}-{nanos:x}.log", std::process::id()));
    let inner = format!(
        "\"{} > {} 2>&1\"",
        command_line(&spec.program, &spec.args),
        quote(&log.display().to_string())
    );
    let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
        .join("cmd.exe");
    let code = process::shell_execute(
        Some("runas"),
        &cmd.display().to_string(),
        Some(&format!("/d /c {inner}")),
        true,
    )
    .map_err(|e| ToolError::Launch(e.to_string()))?;
    let output = std::fs::read(&log)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(&log);
    Ok(ToolOutput {
        exit_code: code.map(i64::from),
        output,
    })
}

/// Builds the consent prompt for emptying the Recycle Bin of `root` (a
/// drive root such as `C:\`), or of every drive when `None`, with the
/// current item count and size.
///
/// # Errors
///
/// Fails when the Recycle Bin cannot be queried.
pub fn empty_recycle_bin_prompt(root: Option<&str>) -> std::io::Result<Prompt<EmptyRecycleBin>> {
    let (bytes, items) = vol::query_recycle_bin_any(root)?;
    Ok(Prompt::new(EmptyRecycleBin {
        root: root.map(str::to_string),
        items,
        bytes,
    }))
}

/// Whether the bin still holds what the user was shown.
///
/// # Errors
///
/// [`ConsentError::Stale`] when the contents changed since the prompt.
pub fn check_empty_is_fresh(
    action: &EmptyRecycleBin,
    items: u64,
    bytes: u64,
) -> Result<(), ConsentError> {
    if action.items == items && action.bytes == bytes {
        Ok(())
    } else {
        Err(ConsentError::Stale)
    }
}

/// Empties the Recycle Bin the user confirmed (`SHEmptyRecycleBinW`).
///
/// # Errors
///
/// A consent error when expired or stale, or a launch error.
pub fn empty_recycle_bin(consent: Consent<EmptyRecycleBin>) -> Result<(), ToolError> {
    let action = consent.redeem().map_err(ToolError::Consent)?;
    let (bytes, items) = vol::query_recycle_bin_any(action.root.as_deref())
        .map_err(|e| ToolError::Launch(e.to_string()))?;
    check_empty_is_fresh(&action, items, bytes).map_err(ToolError::Consent)?;
    vol::empty_recycle_bin(action.root.as_deref()).map_err(|e| ToolError::Launch(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            windir: r"C:\Windows".into(),
        }
    }

    #[test]
    fn builtin_commands_use_absolute_system_paths() {
        let c = build(&ctx(), &ToolAction::DiskCleanup { drive: Some('d') }).unwrap();
        assert_eq!(c.command_line, r"C:\Windows\System32\cleanmgr.exe /d D");
        let c = build(&ctx(), &ToolAction::DiskCleanup { drive: Some('1') }).unwrap();
        assert!(c.args.is_empty());
        let c = build(&ctx(), &ToolAction::DismComponentCleanup).unwrap();
        assert_eq!(
            c.command_line,
            r"C:\Windows\System32\Dism.exe /Online /Cleanup-Image /StartComponentCleanup"
        );
        assert_eq!(c.launch, Launch::Elevated);
        assert!(c.captures_output);
        let c = build(&ctx(), &ToolAction::SystemProtection).unwrap();
        assert_eq!(
            c.program,
            r"C:\Windows\System32\SystemPropertiesProtection.exe"
        );
        let c = build(&ctx(), &ToolAction::StorageSenseSettings).unwrap();
        assert_eq!(
            (c.program.as_str(), c.launch),
            ("ms-settings:storagesense", Launch::ShellOpen)
        );
        let c = build(&ctx(), &ToolAction::CompactOsStatus).unwrap();
        assert_eq!(
            c.command_line,
            r"C:\Windows\System32\compact.exe /compactos:query"
        );
    }

    #[test]
    fn hibernation_is_guidance_only() {
        let c = build(&ctx(), &ToolAction::HibernationGuidance).unwrap();
        assert_eq!(c.launch, Launch::GuidanceOnly);
        assert!(c.description.contains("powercfg /h off"));
        assert_eq!(run(&c), Err(ToolError::NotRunnable));
    }

    #[test]
    fn uninstall_strings() {
        let p = |s: &str| parse_uninstall_string(&ctx(), s).unwrap();
        let q = p(r#""C:\Program Files\App\unins000.exe" /SILENT"#);
        assert_eq!(q.program, r"C:\Program Files\App\unins000.exe");
        assert!(q.args.is_empty());
        assert_eq!(q.removed_flags, ["/SILENT"]);

        let q = p(r"C:\Program Files\Some App\uninstall.exe /S --keep-data");
        assert_eq!(q.program, r"C:\Program Files\Some App\uninstall.exe");
        assert_eq!(q.args, ["--keep-data"]);
        assert_eq!(q.removed_flags, ["/S"]);

        let q = p(r#""C:\Users\me\AppData\Local\Discord\Update.exe" --uninstall"#);
        assert_eq!(q.args, ["--uninstall"]);

        let q = p("MsiExec.exe /X{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}");
        assert_eq!(q.args, ["/X{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}"]);
        let q = p("msiexec /i{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee} /quiet /norestart");
        assert_eq!(q.program, r"C:\Windows\System32\msiexec.exe");
        assert_eq!(q.args, ["/X{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"]);
        assert_eq!(q.removed_flags, ["/quiet", "/norestart"]);

        let q = p(r#"C:\Windows\system32\rundll32.exe "C:\Program Files\X\x.dll",Uninstall"#);
        assert_eq!(q.program, r"C:\Windows\system32\rundll32.exe");
        assert_eq!(q.args, [r"C:\Program Files\X\x.dll,Uninstall"]);

        assert!(parse_uninstall_string(&ctx(), "  ").is_err());
        assert!(parse_uninstall_string(&ctx(), "MsiExec.exe /X").is_err());
        assert!(parse_uninstall_string(&ctx(), r#""C:\x.exe"#).is_err());
    }

    #[test]
    fn uninstall_spec_never_contains_silent_flags() {
        let c = build(
            &ctx(),
            &ToolAction::Uninstall {
                app_name: "App".into(),
                uninstall_string: r#""C:\A\u.exe" /VERYSILENT /SUPPRESSMSGBOXES /qn"#.into(),
            },
        )
        .unwrap();
        assert!(c.args.is_empty());
        assert_eq!(c.removed_flags.len(), 3);
        assert_eq!(c.command_line, r"C:\A\u.exe");
        assert_eq!(c.launch, Launch::ShellOpen);
    }

    #[test]
    fn empty_bin_guard_requires_unchanged_contents() {
        let a = EmptyRecycleBin {
            root: Some(r"C:\".into()),
            items: 3,
            bytes: 100,
        };
        assert!(check_empty_is_fresh(&a, 3, 100).is_ok());
        assert_eq!(check_empty_is_fresh(&a, 4, 100), Err(ConsentError::Stale));
        assert_eq!(check_empty_is_fresh(&a, 3, 101), Err(ConsentError::Stale));
    }

    #[test]
    fn empty_bin_prompt_reads_current_stats() {
        let p = empty_recycle_bin_prompt(Some(r"C:\")).unwrap();
        assert!(p.text().contains("Permanently delete"));
        // Dropped without confirming: SHEmptyRecycleBinW is never called.
    }

    #[test]
    fn compact_os_status_runs_read_only() {
        let c = build(&ctx(), &ToolAction::CompactOsStatus).unwrap();
        let out = run(&c).unwrap();
        assert!(out.exit_code.is_some());
        assert!(!out.output.unwrap_or_default().trim().is_empty());
    }
}
