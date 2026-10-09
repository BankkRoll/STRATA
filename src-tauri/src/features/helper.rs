//! Helper service mode from settings (SPEC §4, §19 "Helper").
//!
//! `helper_service_install` / `helper_service_uninstall` start
//! `strata-helper.exe --install-service` / `--uninstall-service` through UAC
//! (`strata_win::process::launch_elevated`), wait for it, and record the
//! mode in settings. `helper_service_status` reports whether the helper
//! binary is present and the service's state from the Service Control
//! Manager (`sc.exe query`, read-only).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use strata_store::HelperMode;
use strata_win::process::{LaunchError, launch_elevated};
use tauri::{AppHandle, Runtime};

use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// Helper executable name, installed next to the app.
pub const HELPER_EXE: &str = "strata-helper.exe";

/// Service name the helper registers (agreed with the helper track).
pub const HELPER_SERVICE_NAME: &str = "StrataHelper";

/// How long to wait for the elevated helper to finish (after the UAC
/// prompt was answered).
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);

/// `CREATE_NO_WINDOW`: keeps `sc.exe` from flashing a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `ERROR_SERVICE_DOES_NOT_EXIST`, returned by `sc query` as its exit code.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

/// Install or uninstall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// `--install-service`.
    Install,
    /// `--uninstall-service`.
    Uninstall,
}

impl ServiceAction {
    /// The helper flag.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Install => "--install-service",
            Self::Uninstall => "--uninstall-service",
        }
    }
}

/// The exact elevated command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperCommand {
    /// Helper path.
    pub exe: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Command line as shown to the user.
    pub command_line: String,
}

/// The helper command for `action`, with the helper next to `app_dir`.
#[must_use]
pub fn helper_command(app_dir: &Path, action: ServiceAction) -> HelperCommand {
    let exe = app_dir.join(HELPER_EXE);
    let args = vec![action.flag().to_string()];
    let command_line = format!(
        "{} {}",
        strata_win::process::quote_arg(exe.as_os_str()).to_string_lossy(),
        action.flag()
    );
    HelperCommand {
        exe,
        args,
        command_line,
    }
}

fn app_dir() -> FeatureResult<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .ok_or_else(|| FeatureError::internal("cannot locate the app folder"))
}

/// Service state from the SCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Not installed.
    NotInstalled,
    /// Installed, stopped (it starts on demand).
    Stopped,
    /// Starting.
    StartPending,
    /// Stopping.
    StopPending,
    /// Running.
    Running,
    /// Paused or another transitional state.
    Other,
    /// The SCM could not be asked.
    Unknown,
}

/// Parses `sc query` output and exit code. The `STATE` line carries a
/// numeric code that is the same in every Windows language.
#[must_use]
pub fn parse_sc_query(exit_code: Option<i32>, stdout: &str) -> ServiceState {
    if exit_code == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
        return ServiceState::NotInstalled;
    }
    if exit_code != Some(0) {
        return ServiceState::Unknown;
    }
    let code = stdout.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        if key.trim() != "STATE" {
            return None;
        }
        value.split_whitespace().next()?.parse::<u32>().ok()
    });
    match code {
        Some(1) => ServiceState::Stopped,
        Some(2) => ServiceState::StartPending,
        Some(3) => ServiceState::StopPending,
        Some(4) => ServiceState::Running,
        Some(_) => ServiceState::Other,
        None => ServiceState::Unknown,
    }
}

fn query_service() -> ServiceState {
    let sc = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("sc.exe");
    match std::process::Command::new(sc)
        .args(["query", HELPER_SERVICE_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) => parse_sc_query(out.status.code(), &String::from_utf8_lossy(&out.stdout)),
        Err(_) => ServiceState::Unknown,
    }
}

/// Response of the helper service commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperServiceStatus {
    /// Whether `strata-helper.exe` is installed next to the app.
    pub helper_present: bool,
    /// Service state.
    pub service: ServiceState,
    /// Mode recorded in settings.
    pub mode: HelperMode,
    /// Exit code of the install/uninstall run, when one just ran.
    pub exit_code: Option<u32>,
}

fn status<R: Runtime>(
    app: &AppHandle<R>,
    exit_code: Option<u32>,
) -> FeatureResult<HelperServiceStatus> {
    let mode = super::store::handle(app)?.load_settings()?.helper.mode;
    Ok(HelperServiceStatus {
        helper_present: app_dir()?.join(HELPER_EXE).is_file(),
        service: query_service(),
        mode,
        exit_code,
    })
}

/// Whether the helper binary is installed (for `app_capabilities`).
#[must_use]
pub fn helper_present() -> bool {
    app_dir().is_ok_and(|d| d.join(HELPER_EXE).is_file())
}

fn run<R: Runtime>(
    app: &AppHandle<R>,
    action: ServiceAction,
) -> FeatureResult<HelperServiceStatus> {
    let cmd = helper_command(&app_dir()?, action);
    if !cmd.exe.is_file() {
        return Err(FeatureError::new(
            ErrorKind::HelperMissing,
            format!("{HELPER_EXE} is not installed next to Strata; reinstall Strata to add it"),
        ));
    }
    let child = launch_elevated(&cmd.exe, &cmd.args).map_err(|e| match e {
        LaunchError::Declined => FeatureError::new(
            ErrorKind::HelperDeclined,
            "Administrator permission was not given; nothing changed",
        ),
        LaunchError::Failed(w) => {
            FeatureError::with_detail(ErrorKind::HelperFailed, w.to_string(), &w)
        }
    })?;
    let code = child
        .wait(HELPER_TIMEOUT)
        .map_err(|e| FeatureError::new(ErrorKind::HelperFailed, e.to_string()))?
        .ok_or_else(|| {
            FeatureError::new(ErrorKind::HelperFailed, "the helper did not finish in time")
        })?;
    if code != 0 {
        return Err(FeatureError::new(
            ErrorKind::HelperFailed,
            format!("{} failed with exit code {code}", cmd.command_line),
        ));
    }
    let store = super::store::handle(app)?;
    let mut s = store.load_settings()?;
    s.helper.mode = match action {
        ServiceAction::Install => HelperMode::Service,
        ServiceAction::Uninstall => HelperMode::OnDemand,
    };
    let (saved, fx) = super::settings::save(&store, &s)?;
    super::settings::announce(app, &saved, &fx);
    status(app, Some(code))
}

/// Installs the helper as a Windows service (UAC prompt).
#[tauri::command]
pub async fn helper_service_install<R: Runtime>(
    app: AppHandle<R>,
) -> FeatureResult<HelperServiceStatus> {
    blocking(move || run(&app, ServiceAction::Install)).await
}

/// Removes the helper service (UAC prompt).
#[tauri::command]
pub async fn helper_service_uninstall<R: Runtime>(
    app: AppHandle<R>,
) -> FeatureResult<HelperServiceStatus> {
    blocking(move || run(&app, ServiceAction::Uninstall)).await
}

/// Helper presence, service state and the recorded mode.
#[tauri::command]
pub async fn helper_service_status<R: Runtime>(
    app: AppHandle<R>,
) -> FeatureResult<HelperServiceStatus> {
    blocking(move || status(&app, None)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_use_the_helper_next_to_the_app() {
        let dir = Path::new(r"C:\Program Files\Strata");
        let c = helper_command(dir, ServiceAction::Install);
        assert_eq!(
            c.exe,
            Path::new(r"C:\Program Files\Strata\strata-helper.exe")
        );
        assert_eq!(c.args, ["--install-service"]);
        assert_eq!(
            c.command_line,
            r#""C:\Program Files\Strata\strata-helper.exe" --install-service"#
        );
        let u = helper_command(dir, ServiceAction::Uninstall);
        assert_eq!(u.args, ["--uninstall-service"]);
    }

    #[test]
    fn sc_query_parsing() {
        let running = "SERVICE_NAME: StrataHelper\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        STATE              : 4  RUNNING\r\n";
        assert_eq!(parse_sc_query(Some(0), running), ServiceState::Running);
        let stopped = running.replace("4  RUNNING", "1  STOPPED");
        assert_eq!(parse_sc_query(Some(0), &stopped), ServiceState::Stopped);
        assert_eq!(parse_sc_query(Some(1060), ""), ServiceState::NotInstalled);
        assert_eq!(parse_sc_query(Some(5), ""), ServiceState::Unknown);
        assert_eq!(parse_sc_query(Some(0), "garbage"), ServiceState::Unknown);
    }

    #[test]
    fn querying_a_missing_service_is_read_only_and_typed() {
        // The helper service is not installed on development machines;
        // either answer is fine, the query must simply not fail.
        assert_ne!(query_service(), ServiceState::Unknown);
    }
}
