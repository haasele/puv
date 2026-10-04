mod cli;
mod inspect;
mod ops;

use std::process::ExitCode;

use clap::Parser;
use cli::{Cli, inline_code};
use miette::Report;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    match dispatch() {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("{err:?}");
            ExitCode::from(1)
        }
    }
}

fn dispatch() -> miette::Result<i32> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let verbose = raw
        .iter()
        .filter(|arg| *arg == "-v" || *arg == "--verbose")
        .count() as u8;
    init_tracing(verbose);
    if raw
        .iter()
        .any(|arg| arg == "-c" || arg == "--code" || arg.starts_with("--code="))
    {
        let code = inline_code(&raw).unwrap_or_default();
        if code.is_empty() {
            return Err(Report::msg("puv -c requires a PHP snippet"));
        }
        return ops::execute_code(&code, verbose);
    }
    if let Some((script, args)) = cli::direct_script(&raw) {
        return ops::execute(
            vec![script].into_iter().chain(args).collect(),
            None,
            verbose,
            false,
        );
    }
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    ops::run(cli)
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
