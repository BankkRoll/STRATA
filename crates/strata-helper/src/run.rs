//! Process entry points used by `main`.

use std::sync::Arc;

use strata_ipc::pipe::ServerConfig;
use strata_ipc::protocol::Capabilities;
use strata_win::process::{drop_privileges, is_elevated};

use crate::args::{Command, OnDemandArgs, USAGE, parse_args};
use crate::diag::diag;
use crate::ops::Shared;
use crate::privs::KEPT_PRIVILEGES;
use crate::server::{ExitReason, HelperConfig, serve};
use crate::source::Volumes;
use crate::verify::{LaunchedClientVerifier, trust_policy};
use crate::watch::ProcessWatch;

/// Exit code: success.
pub const EXIT_OK: i32 = 0;
/// Exit code: failure while running.
pub const EXIT_FAILURE: i32 = 1;
/// Exit code: the launching app exited first.
pub const EXIT_PARENT_GONE: i32 = 3;
/// Exit code: bad command line.
pub const EXIT_USAGE: i32 = 64;

/// What this helper advertises in `Welcome`.
#[must_use]
pub fn capabilities(elevated: bool, has_image: bool) -> Capabilities {
    Capabilities {
        mft_scan: elevated || has_image,
        usn_journal: elevated,
        read_records: elevated || has_image,
        privileged_delete: true,
    }
}

/// Parses `std::env::args_os` and runs the selected mode. Returns the
/// process exit code.
#[must_use]
pub fn main_with_args() -> i32 {
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("strata-helper: {e}\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    match command {
        Command::Help => {
            print!("{USAGE}");
            EXIT_OK
        }
        Command::Version => {
            println!("strata-helper {}", env!("CARGO_PKG_VERSION"));
            EXIT_OK
        }
        Command::OnDemand(args) => harden().map_or(EXIT_FAILURE, |()| on_demand(args)),
        Command::Service { client_image } => harden().map_or(EXIT_FAILURE, |()| {
            match crate::service::run_service(client_image) {
                Ok(()) => EXIT_OK,
                Err(e) => {
                    diag!("{e}");
                    EXIT_FAILURE
                }
            }
        }),
        Command::InstallService { client_image } => report(crate::service::install(&client_image)),
        Command::UninstallService => report(crate::service::uninstall()),
    }
}

fn report(r: Result<(), crate::service::ServiceError>) -> i32 {
    match r {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("strata-helper: {e}");
            EXIT_FAILURE
        }
    }
}

/// Removes every token privilege the helper never uses (SPEC §4).
fn harden() -> Result<(), ()> {
    match drop_privileges(KEPT_PRIVILEGES) {
        Ok(_) => Ok(()),
        Err(e) => {
            diag!("cannot drop privileges: {e}");
            Err(())
        }
    }
}

/// On-demand mode: serve the launching app until it disconnects, goes idle,
/// sends `Shutdown`, or exits.
fn on_demand(args: OnDemandArgs) -> i32 {
    let parent = match args.parent_pid.map(ProcessWatch::open).transpose() {
        Ok(p) => p,
        Err(e) => {
            // NOTE: the parent already exited between launch and here.
            diag!("cannot watch the parent process: {e}");
            return EXIT_PARENT_GONE;
        }
    };
    let user = match strata_win::process::current_user() {
        Ok(u) => u,
        Err(e) => {
            diag!("cannot read the token user: {e}");
            return EXIT_FAILURE;
        }
    };
    let policy = match trust_policy(&args.client_image) {
        Ok(p) => p,
        Err(e) => {
            diag!("cannot build the trust policy: {e}");
            return EXIT_FAILURE;
        }
    };
    let elevated = is_elevated().unwrap_or(false);
    let volumes = image_volumes(args.image);
    let verifier = Arc::new(LaunchedClientVerifier::new(
        Arc::new(policy),
        args.client_pid,
    ));
    let mut server = ServerConfig::new(args.pipe, user.sid, verifier);
    server.elevated = elevated;
    server.capabilities = capabilities(elevated, volumes.has_image());
    let mut config = HelperConfig::on_demand(server);
    config.parent = parent;
    let shared = Shared::new(volumes);
    match serve(config, &shared) {
        Ok(ExitReason::ParentExited) => EXIT_PARENT_GONE,
        Ok(_) => EXIT_OK,
        Err(e) => {
            diag!("{e}");
            EXIT_FAILURE
        }
    }
}

#[cfg(debug_assertions)]
fn image_volumes(image: Option<std::path::PathBuf>) -> Volumes {
    image.map_or_else(Volumes::raw, Volumes::with_image)
}

#[cfg(not(debug_assertions))]
fn image_volumes(_image: Option<std::path::PathBuf>) -> Volumes {
    Volumes::raw()
}
