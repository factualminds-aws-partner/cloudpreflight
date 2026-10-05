//! Command-line interface: argument parsing, output selection and exit codes.

use std::fmt::Write as _;
use std::io::{ErrorKind, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rust_decimal::Decimal;

use crate::config::Config;
use crate::discovery;
use crate::iac::{terraform_hcl, terraform_plan};
use crate::mapping::Mappings;
use crate::model::Report;
use crate::pipeline::{self, ScanOptions};
use crate::report::{self, table};
use crate::usage;

/// Appends one line to a `String` buffer.
macro_rules! say {
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

/// Exit codes are part of the interface; scripts may rely on them.
pub mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    // 2 is used by the argument parser for usage errors.
    pub const BUDGET_EXCEEDED: u8 = 3;
    pub const STRICT: u8 = 4;
    pub const INTERRUPTED: u8 = 130;
}

#[derive(Parser)]
#[command(
    name = "cloudpreflight",
    version,
    about = "See the cost before you ship the infrastructure."
)]
#[command(after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 budget exceeded, 4 --strict violation.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    #[command(flatten)]
    pub global: Global,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scan a repository and estimate its monthly cloud cost
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Estimate a Terraform plan JSON file (`terraform show -json plan > plan.json`)
    Estimate { plan: PathBuf },
    /// Show how the estimate for one resource was derived
    Explain {
        /// Resource address, for example `aws_db_instance.main`
        address: String,
        /// Directory to scan
        #[arg(long, default_value = ".", conflicts_with = "plan")]
        path: PathBuf,
        /// Terraform plan JSON file to read instead of scanning
        #[arg(long)]
        plan: Option<PathBuf>,
    },
    /// Check that the IaC and configuration can be read, without pricing anything
    Validate {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List supported clouds, resource types and usage profiles
    Providers,
}

#[derive(Args)]
pub struct Global {
    /// Config file (default: .factualminds-cost.yaml in the scanned directory)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Region to assume when the IaC does not name one
    #[arg(long, global = true)]
    pub region: Option<String>,
    /// Report currency (only USD is available)
    #[arg(long, global = true)]
    pub currency: Option<String>,
    /// Usage profile: light, standard or high
    #[arg(long, global = true, value_name = "NAME")]
    pub usage_profile: Option<String>,
    #[arg(long, global = true, value_enum, default_value_t = Format::Table)]
    pub format: Format,
    /// Use cached prices only; make no network requests
    #[arg(long, global = true, conflicts_with = "no_cache")]
    pub offline: bool,
    /// Fetch fresh prices instead of using cached ones
    #[arg(long, global = true)]
    pub no_cache: bool,
    /// Exit with code 3 when the monthly estimate exceeds this amount
    #[arg(long, global = true, value_name = "USD")]
    pub budget: Option<Decimal>,
    /// Exit with code 4 when any resource is unsupported or not fully priced
    #[arg(long, global = true)]
    pub strict: bool,
    /// List every resource and show informational logs
    #[arg(long, short, global = true)]
    pub verbose: bool,
    /// Print only the report
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Show debug logs
    #[arg(long, global = true)]
    pub debug: bool,
    /// Disable colours (also honoured: the NO_COLOR environment variable)
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

pub async fn run(cli: Cli) -> Result<u8> {
    let global = &cli.global;
    if let Some(budget) = global.budget
        && budget <= Decimal::ZERO
    {
        bail!("--budget must be greater than zero");
    }

    match &cli.command {
        Command::Scan { path } => {
            let report = pipeline::run(&scan_options(global, path.clone(), None)).await?;
            print_report(&report, global)?;
            Ok(exit_code(&report, global.strict))
        }
        Command::Estimate { plan } => {
            let report = pipeline::run(&scan_options(global, PathBuf::from("."), Some(plan.clone()))).await?;
            print_report(&report, global)?;
            Ok(exit_code(&report, global.strict))
        }
        Command::Explain { address, path, plan } => {
            let report = pipeline::run(&scan_options(global, path.clone(), plan.clone())).await?;
            explain(&report, address, global)
        }
        Command::Validate { path } => validate(path, global),
        Command::Providers => providers(),
    }
}

fn scan_options(global: &Global, path: PathBuf, plan_file: Option<PathBuf>) -> ScanOptions {
    // Progress goes to stderr, and only when a person is watching a table report.
    let show = std::io::stderr().is_terminal() && !global.quiet && global.format == Format::Table;
    let tick = if unicode() { "✓" } else { "ok" };
    ScanOptions {
        path,
        plan_file,
        config_path: global.config.clone(),
        region: global.region.clone(),
        currency: global.currency.clone(),
        usage_profile: global.usage_profile.clone(),
        offline: global.offline,
        no_cache: global.no_cache,
        budget: global.budget,
        progress: Arc::new(move |message| {
            if show {
                anstream::eprintln!("{tick} {message}");
            }
        }),
    }
}

fn render_options(global: &Global) -> table::Options {
    let width = terminal_size::terminal_size()
        .map(|(width, _)| usize::from(width.0))
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(80);
    table::Options {
        width,
        unicode: unicode(),
        color: !global.no_color,
        verbose: global.verbose,
    }
}

/// Unicode output unless the locale or terminal says it cannot be shown.
fn unicode() -> bool {
    if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
        return false;
    }
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    match locale {
        Some(locale) => {
            let locale = locale.to_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        }
        None => true,
    }
}

fn print_report(report: &Report, global: &Global) -> Result<()> {
    match global.format {
        Format::Json => emit(&format!("{}\n", report::json(report)?))?,
        Format::Table => {
            if std::io::stderr().is_terminal() && !global.quiet {
                anstream::eprintln!();
            }
            emit(&table::render(report, render_options(global)))?;
        }
    }
    Ok(())
}

/// Writes to stdout, stripping colour when it is not a terminal. A closed pipe
/// (`cloudpreflight scan | head`) is a normal way to stop reading, not an error.
fn emit(text: &str) -> Result<()> {
    let mut stdout = anstream::stdout().lock();
    match stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() != ErrorKind::BrokenPipe => Err(error).context("cannot write to stdout"),
        _ => Ok(()),
    }
}

fn exit_code(report: &Report, strict: bool) -> u8 {
    let counts = &report.counts;
    if !report.errors.is_empty() {
        return exit::ERROR;
    }
    if report.budget.as_ref().is_some_and(|budget| budget.exceeded) {
        return exit::BUDGET_EXCEEDED;
    }
    if strict && counts.unsupported + counts.unresolved + counts.partially_priced > 0 {
        return exit::STRICT;
    }
    exit::OK
}

fn explain(report: &Report, address: &str, global: &Global) -> Result<u8> {
    if global.format == Format::Json {
        let matches: Vec<_> = report
            .resources
            .iter()
            .filter(|change| change.address == address || crate::model::strip_index(&change.address) == address)
            .collect();
        if matches.is_empty() {
            bail!("no resource with address `{address}` was found");
        }
        emit(&format!("{}\n", serde_json::to_string_pretty(&matches)?))?;
        return Ok(exit::OK);
    }

    let Some(text) = table::explain(report, address, render_options(global)) else {
        let mut known: Vec<&str> = report.resources.iter().map(|change| change.address.as_str()).collect();
        known.truncate(10);
        bail!(
            "no resource with address `{address}` was found. Addresses in this scan include: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    emit(&text)?;
    Ok(exit::OK)
}

fn validate(path: &Path, global: &Global) -> Result<u8> {
    let (config, config_path) = Config::load(global.config.as_deref(), path)?;
    let limits = pipeline::limits(&config);
    let discovery = discovery::discover(path, &limits)?;
    let relative = |file: &Path| {
        file.strip_prefix(&discovery.root)
            .unwrap_or(file)
            .display()
            .to_string()
            .replace('\\', "/")
    };

    let mut out = String::new();
    match config_path {
        Some(config_path) => say!(out, "ok       config {}", config_path.display()),
        None => say!(out, "-        no config file (defaults apply)"),
    }

    let mut failures = 0;
    for plan in &discovery.plans {
        let parsed = discovery::read_limited(plan, limits.max_file_bytes)
            .map_err(anyhow::Error::msg)
            .and_then(|text| terraform_plan::parse(&text, ""));
        match parsed {
            Ok(input) => say!(
                out,
                "ok       plan {} ({} resource changes)",
                relative(plan),
                input.changes.len()
            ),
            Err(error) => {
                failures += 1;
                say!(out, "invalid  plan {}: {error:#}", relative(plan));
            }
        }
    }
    for root in &discovery.terraform_roots {
        let shown = match relative(root) {
            shown if shown.is_empty() => ".".to_string(),
            shown => shown,
        };
        match terraform_hcl::load_dir(root, limits.max_file_bytes) {
            Ok(blocks) => {
                let resources = blocks
                    .iter()
                    .filter(|block| block.identifier.as_str() == "resource")
                    .count();
                say!(out, "ok       terraform {shown} ({resources} resource blocks)");
            }
            Err(error) => {
                failures += 1;
                say!(out, "invalid  terraform {shown}: {error:#}");
            }
        }
    }
    for technology in &discovery.not_yet_supported {
        say!(out, "skipped  {technology} detected, not analysed yet");
    }
    for warning in &discovery.warnings {
        say!(out, "warning  {warning}");
    }
    if discovery.plans.is_empty() && discovery.terraform_roots.is_empty() {
        say!(out, "-        no Terraform configuration or plan JSON found");
    }

    emit(&out)?;
    Ok(if failures > 0 { exit::ERROR } else { exit::OK })
}

fn providers() -> Result<u8> {
    let mappings = Mappings::load().context("embedded mappings failed to load")?;
    let mut out = String::new();
    say!(
        out,
        "AWS    public list prices (on-demand) from the AWS Price List Bulk API"
    );
    say!(out, "Azure  not available yet");
    say!(out, "GCP    not available yet");

    say!(out, "\nPriced AWS resource types:");
    for (resource_type, service) in mappings.supported_types() {
        say!(out, "  {resource_type:<36}{service}");
    }
    say!(
        out,
        "\n{} further AWS resource types are recognised as having no direct charge.",
        mappings.no_charge_count()
    );
    say!(
        out,
        "Everything else is reported as unsupported and is never counted as $0."
    );

    say!(out, "\nUsage profiles:");
    for profile in usage::builtin_profiles()? {
        say!(out, "  {:<10}{}", profile.name, profile.description);
    }
    emit(&out)?;
    Ok(exit::OK)
}
