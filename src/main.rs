//! cloudpreflight: IaC preflight for cloud cost.

mod aws;
mod cli;
mod config;
mod discovery;
mod estimate;
mod finops;
mod iac;
mod mapping;
mod model;
mod pipeline;
mod pricing;
mod report;
mod usage;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();

    // RUST_LOG wins; otherwise the flags pick the level. Logs go to stderr so that
    // stdout stays clean for the report.
    let level = match (cli.global.debug, cli.global.verbose, cli.global.quiet) {
        (true, _, _) => "debug",
        (_, true, _) => "info",
        (_, _, true) => "error",
        _ => "warn",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(format!("cloudpreflight={level}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    // Dropping the run future on Ctrl-C cancels in-flight downloads and removes their temp files.
    let outcome = tokio::select! {
        outcome = cli::run(cli) => outcome,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Cancelled.");
            return ExitCode::from(cli::exit::INTERRUPTED);
        }
    };

    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            anstream::eprintln!("error: {error:#}");
            ExitCode::from(cli::exit::ERROR)
        }
    }
}
