//! Command-line parsing.
//!
//! ```text
//! strata-helper --pipe <name> --client-image <path> --client-pid <pid> [--parent-pid [<pid>]]
//! strata-helper --service --client-image <path>
//! strata-helper --install-service --client-image <path>
//! strata-helper --uninstall-service
//! ```
//!
//! Debug builds also accept `--image <file>` in on-demand mode: scans of
//! the volume id `strata-image` then read that NTFS image file, so the
//! request loop can be exercised without elevation. Release builds reject
//! the flag; an elevated helper never reads client-chosen files.
//!
//! SECURITY: every value is validated here (pipe name format, absolute
//! image path, non-zero PIDs); duplicates and unknown flags are errors.

use std::ffi::OsString;
use std::path::PathBuf;

use strata_ipc::security::is_valid_pipe_name;

/// On-demand launch parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnDemandArgs {
    /// Pipe to create (from `strata_ipc::security::session_pipe_name`).
    pub pipe: String,
    /// The only app image allowed to connect.
    pub client_image: PathBuf,
    /// The only process allowed to connect.
    pub client_pid: u32,
    /// Process whose exit ends the helper (`--parent-pid` alone means the
    /// client process).
    pub parent_pid: Option<u32>,
    /// Development image source (debug builds only).
    pub image: Option<PathBuf>,
}

/// What the process should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve one app session (launched elevated by the app).
    OnDemand(OnDemandArgs),
    /// Run under the Service Control Manager.
    Service {
        /// The trusted app image.
        client_image: PathBuf,
    },
    /// Install or reconfigure the service (elevated).
    InstallService {
        /// The trusted app image recorded in the service configuration.
        client_image: PathBuf,
    },
    /// Remove the service (elevated).
    UninstallService,
    /// Print usage.
    Help,
    /// Print the version.
    Version,
}

/// A command-line error, shown with the usage text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ArgError(pub String);

/// Usage text.
pub const USAGE: &str = "\
usage:
  strata-helper --pipe <name> --client-image <path> --client-pid <pid> [--parent-pid [<pid>]]
  strata-helper --service --client-image <path>
  strata-helper --install-service --client-image <path>
  strata-helper --uninstall-service
";

#[derive(Default)]
struct Raw {
    pipe: Option<String>,
    client_image: Option<PathBuf>,
    client_pid: Option<u32>,
    parent: Option<Option<u32>>,
    image: Option<PathBuf>,
    mode: Option<&'static str>,
}

fn set<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), ArgError> {
    if slot.is_some() {
        return Err(ArgError(format!("{flag} given more than once")));
    }
    *slot = Some(value);
    Ok(())
}

fn pid(value: &str, flag: &str) -> Result<u32, ArgError> {
    match value.parse::<u32>() {
        Ok(p) if p != 0 => Ok(p),
        _ => Err(ArgError(format!("{flag} needs a non-zero process id"))),
    }
}

/// Parses arguments (without the program name).
///
/// # Errors
///
/// Unknown, duplicated, missing or invalid arguments.
///
/// # Example
///
/// ```
/// use strata_helper::args::{parse_args, Command};
/// let cmd = parse_args([
///     "--pipe", r"\\.\pipe\strata-helper-S-1-5-21-1-2-3-1001-00ff",
///     "--client-image", r"C:\Program Files\Strata\strata.exe",
///     "--client-pid", "4242",
///     "--parent-pid",
/// ]).unwrap();
/// let Command::OnDemand(a) = cmd else { panic!() };
/// assert_eq!(a.parent_pid, Some(4242));
/// ```
pub fn parse_args<I, S>(args: I) -> Result<Command, ArgError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|a| {
            a.into()
                .into_string()
                .map_err(|_| ArgError("arguments must be valid Unicode".into()))
        })
        .collect::<Result<_, _>>()?;
    let mut raw = Raw::default();
    let mut i = 0;
    let value = |i: usize, flag: &str| -> Result<&String, ArgError> {
        args.get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| ArgError(format!("{flag} needs a value")))
    };
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" => return Ok(Command::Version),
            "--pipe" => {
                set(&mut raw.pipe, value(i, flag)?.clone(), flag)?;
                i += 1;
            }
            "--client-image" => {
                set(&mut raw.client_image, PathBuf::from(value(i, flag)?), flag)?;
                i += 1;
            }
            "--client-pid" => {
                set(&mut raw.client_pid, pid(value(i, flag)?, flag)?, flag)?;
                i += 1;
            }
            "--parent-pid" => {
                // NOTE: the value is optional; alone, the flag means "the
                // client process is my parent".
                let explicit = match args.get(i + 1) {
                    Some(v) if !v.starts_with("--") => {
                        i += 1;
                        Some(pid(v, flag)?)
                    }
                    _ => None,
                };
                set(&mut raw.parent, explicit, flag)?;
            }
            #[cfg(debug_assertions)]
            "--image" => {
                set(&mut raw.image, PathBuf::from(value(i, flag)?), flag)?;
                i += 1;
            }
            "--service" | "--install-service" | "--uninstall-service" => {
                let mode = match flag {
                    "--service" => "service",
                    "--install-service" => "install",
                    _ => "uninstall",
                };
                set(&mut raw.mode, mode, "the mode")?;
            }
            other => return Err(ArgError(format!("unknown argument {other}"))),
        }
        i += 1;
    }
    build(raw)
}

fn absolute_image(raw: &Raw) -> Result<PathBuf, ArgError> {
    let p = raw
        .client_image
        .clone()
        .ok_or_else(|| ArgError("--client-image is required".into()))?;
    if !p.is_absolute() {
        return Err(ArgError("--client-image must be an absolute path".into()));
    }
    Ok(p)
}

fn build(raw: Raw) -> Result<Command, ArgError> {
    let only_image = |raw: &Raw| {
        raw.pipe.is_none()
            && raw.client_pid.is_none()
            && raw.parent.is_none()
            && raw.image.is_none()
    };
    match raw.mode {
        Some("service") | Some("install") => {
            if !only_image(&raw) {
                return Err(ArgError("service modes take only --client-image".into()));
            }
            let client_image = absolute_image(&raw)?;
            Ok(if raw.mode == Some("service") {
                Command::Service { client_image }
            } else {
                Command::InstallService { client_image }
            })
        }
        Some(_) => {
            if !only_image(&raw) || raw.client_image.is_some() {
                return Err(ArgError(
                    "--uninstall-service takes no other arguments".into(),
                ));
            }
            Ok(Command::UninstallService)
        }
        None => {
            let pipe = raw
                .pipe
                .clone()
                .ok_or_else(|| ArgError("--pipe is required".into()))?;
            if !is_valid_pipe_name(&pipe) {
                return Err(ArgError("--pipe is not a Strata helper pipe name".into()));
            }
            let client_image = absolute_image(&raw)?;
            let client_pid = raw
                .client_pid
                .ok_or_else(|| ArgError("--client-pid is required".into()))?;
            Ok(Command::OnDemand(OnDemandArgs {
                pipe,
                client_image,
                client_pid,
                parent_pid: raw.parent.map(|p| p.unwrap_or(client_pid)),
                image: raw.image,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIPE: &str = r"\\.\pipe\strata-helper-S-1-5-21-1-2-3-1001-0123456789abcdef";
    const APP: &str = r"C:\Program Files\Strata\strata.exe";

    fn on_demand(extra: &[&str]) -> Result<Command, ArgError> {
        let mut a = vec!["--pipe", PIPE, "--client-image", APP, "--client-pid", "77"];
        a.extend_from_slice(extra);
        parse_args(a)
    }

    #[test]
    fn on_demand_forms() {
        let Command::OnDemand(a) = on_demand(&[]).unwrap() else {
            panic!()
        };
        assert_eq!(a.pipe, PIPE);
        assert_eq!(a.client_image, PathBuf::from(APP));
        assert_eq!((a.client_pid, a.parent_pid, a.image), (77, None, None));
        let Command::OnDemand(a) = on_demand(&["--parent-pid"]).unwrap() else {
            panic!()
        };
        assert_eq!(a.parent_pid, Some(77));
        let Command::OnDemand(a) = on_demand(&["--parent-pid", "88"]).unwrap() else {
            panic!()
        };
        assert_eq!(a.parent_pid, Some(88));
        let Command::OnDemand(a) = parse_args([
            "--parent-pid",
            "--pipe",
            PIPE,
            "--client-image",
            APP,
            "--client-pid",
            "5",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(a.parent_pid, Some(5));
    }

    #[test]
    fn on_demand_rejections() {
        assert!(parse_args::<_, &str>([]).is_err());
        assert!(parse_args(["--pipe", PIPE]).is_err());
        assert!(on_demand(&["--client-pid", "1"]).is_err(), "duplicate");
        assert!(on_demand(&["--bogus"]).is_err());
        assert!(on_demand(&["--parent-pid", "0"]).is_err());
        assert!(on_demand(&["--parent-pid", "x"]).is_err());
        assert!(on_demand(&["--service"]).is_err());
        assert!(
            parse_args([
                "--pipe",
                r"\\.\pipe\other",
                "--client-image",
                APP,
                "--client-pid",
                "1"
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--pipe",
                PIPE,
                "--client-image",
                "strata.exe",
                "--client-pid",
                "1"
            ])
            .is_err()
        );
        assert!(parse_args(["--pipe", PIPE, "--client-image", APP, "--client-pid", "0"]).is_err());
        assert!(parse_args(["--pipe", PIPE, "--client-image", APP, "--client-pid"]).is_err());
        assert!(
            parse_args(["--pipe", "--client-image", APP, "--client-pid", "1"]).is_err(),
            "a flag is not a value"
        );
    }

    #[test]
    fn image_flag_exists_only_in_debug_builds() {
        let r = on_demand(&["--image", r"D:\img\ntfs.img"]);
        if cfg!(debug_assertions) {
            let Command::OnDemand(a) = r.unwrap() else {
                panic!()
            };
            assert_eq!(a.image, Some(PathBuf::from(r"D:\img\ntfs.img")));
        } else {
            assert!(r.is_err());
        }
    }

    #[test]
    fn service_forms() {
        assert_eq!(
            parse_args(["--service", "--client-image", APP]).unwrap(),
            Command::Service {
                client_image: APP.into()
            }
        );
        assert_eq!(
            parse_args(["--install-service", "--client-image", APP]).unwrap(),
            Command::InstallService {
                client_image: APP.into()
            }
        );
        assert_eq!(
            parse_args(["--uninstall-service"]).unwrap(),
            Command::UninstallService
        );
        assert!(parse_args(["--service"]).is_err());
        assert!(parse_args(["--install-service"]).is_err());
        assert!(parse_args(["--uninstall-service", "--client-image", APP]).is_err());
        assert!(parse_args(["--service", "--install-service", "--client-image", APP]).is_err());
        assert!(parse_args(["--service", "--client-image", APP, "--client-pid", "1"]).is_err());
        assert_eq!(parse_args(["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_args(["--version"]).unwrap(), Command::Version);
    }
}
