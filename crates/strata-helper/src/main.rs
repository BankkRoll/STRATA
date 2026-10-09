//! `strata-helper.exe`: the elevated helper process (see the library docs).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    std::process::exit(strata_helper::run::main_with_args());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("strata-helper runs on Windows only");
    std::process::exit(1);
}
