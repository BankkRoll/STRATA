//! `strata-cli` entry point. See [`strata_cli::run`].

fn main() {
    let code = strata_cli::run(
        std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::exit(code);
}
