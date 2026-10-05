//! The scan pipeline: discovery, IaC adapters, normalisation, pricing, cost engine,
//! FinOps rules, report. Each stage only sees the output of the one before it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;

use crate::config::Config;
use crate::discovery::{self, Discovery, Limits};
use crate::estimate::{self, Engine};
use crate::iac::{terraform_hcl, terraform_plan};
use crate::mapping::Mappings;
use crate::model::{
    Assumption, BudgetStatus, ChangeEstimate, Confidence, Counts, IacSource, Input, Outcome, PriceKind, Report,
    ResourceEstimate, ResourceStatus, Skipped,
};
use crate::pricing::aws_bulk::{AwsPricing, DEFAULT_BASE_URL, Progress, Resolved};
use crate::pricing::cache::Cache;
use crate::usage::Usage;
use crate::{aws, finops};

pub const SCHEMA_VERSION: &str = "1.0";
const DEFAULT_TTL_HOURS: u64 = 24 * 7;
const LOW_CONFIDENCE_SHARE: Decimal = Decimal::from_parts(25, 0, 0, false, 2);

pub struct ScanOptions {
    /// Directory to scan. Ignored when `plan_file` is set.
    pub path: PathBuf,
    pub plan_file: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub region: Option<String>,
    pub currency: Option<String>,
    pub usage_profile: Option<String>,
    pub offline: bool,
    pub no_cache: bool,
    pub budget: Option<Decimal>,
    pub progress: Progress,
}

pub async fn run(options: &ScanOptions) -> Result<Report> {
    let repo = match &options.plan_file {
        Some(plan) => plan
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
        None => options.path.as_path(),
    };
    let (config, _) = Config::load(options.config_path.as_deref(), repo)?;

    let currency = options
        .currency
        .clone()
        .or_else(|| config.defaults.currency.clone())
        .unwrap_or_else(|| "USD".to_string())
        .to_uppercase();
    if currency != "USD" {
        bail!(
            "currency {currency} is not supported. AWS public list prices are published in USD and no exchange rate is applied."
        );
    }

    let limits = limits(&config);
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let (project, mut inputs) = load_inputs(options, &limits, &mut warnings, &mut errors)?;

    let default_region = options.region.clone().or_else(|| config.aws.region.clone());
    let mut region_assumed = false;
    for input in &mut inputs {
        let fallback = default_region.as_deref().unwrap_or(aws::FALLBACK_REGION);
        region_assumed |= aws::normalise(input, fallback) && default_region.is_none();
        warnings.append(&mut input.warnings);
    }
    if region_assumed {
        warnings.push(format!(
            "No region found in the provider configuration; {} prices assumed. Pass --region to set it.",
            aws::FALLBACK_REGION
        ));
    }

    let mappings = Mappings::load()?;
    let profile = options.usage_profile.as_deref().or(config.usage_profile.as_deref());
    let usage = Usage::new(profile, config.usage.clone(), config.resource_usage())?;
    let engine = Engine {
        mappings: &mappings,
        usage: &usage,
        monthly_hours: config.defaults.monthly_hours.unwrap_or(Decimal::from(730)),
    };

    let changes: Vec<_> = inputs.iter().flat_map(|input| input.changes.iter()).collect();
    (options.progress)(format!("{} resources normalized", changes.len()));
    let queries = engine.queries(
        changes
            .iter()
            .flat_map(|change| change.before.iter().chain(change.after.iter())),
    );
    let resolved = if queries.is_empty() {
        Resolved::default()
    } else {
        let resolved = pricing(options, &config)?.resolve(queries).await;
        (options.progress)("AWS pricing resolved".to_string());
        resolved
    };
    warnings.extend(resolved.warnings);

    let mut assumptions = BTreeSet::new();
    if region_assumed {
        assumptions.insert(Assumption {
            scope: String::new(),
            key: "region".into(),
            value: aws::FALLBACK_REGION.into(),
            source: "default".into(),
        });
    }
    let estimates: Vec<ChangeEstimate> = changes
        .iter()
        .map(|change| {
            // Usage assumptions are reported for the planned state only.
            let before = change
                .before
                .as_ref()
                .map(|r| engine.estimate(r, &resolved.prices, &mut BTreeSet::new()));
            let after = change
                .after
                .as_ref()
                .map(|r| engine.estimate(r, &resolved.prices, &mut assumptions));
            estimate::change_estimate(change, before, after)
        })
        .collect();
    for change in estimate::partial_deltas(&estimates) {
        warnings.push(format!(
            "{}: the delta leaves out a component that could be priced on one side of the change only.",
            change.address
        ));
    }
    let assumptions: Vec<Assumption> = assumptions.into_iter().collect();

    let owned_changes: Vec<_> = changes.iter().map(|change| (*change).clone()).collect();
    let findings = finops::findings(&owned_changes, &estimates, &assumptions);

    let planned: Vec<&ResourceEstimate> = estimates.iter().filter_map(|change| change.after.as_ref()).collect();
    let count = |status: ResourceStatus| planned.iter().filter(|estimate| estimate.status == status).count();
    let counts = Counts {
        total: planned.len(),
        priced: count(ResourceStatus::Priced),
        partially_priced: count(ResourceStatus::PartiallyPriced),
        unresolved: count(ResourceStatus::Unresolved),
        unsupported: count(ResourceStatus::Unsupported),
        no_direct_charge: count(ResourceStatus::NoDirectCharge),
    };
    let unsupported_resources = planned
        .iter()
        .filter(|estimate| estimate.status == ResourceStatus::Unsupported)
        .map(|estimate| Skipped {
            address: estimate.address.clone(),
            resource_type: estimate.resource_type.clone(),
            reason: "no pricing adapter available yet".into(),
        })
        .collect();

    let has_baseline = inputs.iter().any(|input| input.source == IacSource::TerraformPlan);
    let costs = estimate::totals(&estimates);
    let budget = options.budget.or(config.budget.monthly).map(|monthly| BudgetStatus {
        monthly,
        used_fraction: costs.after / monthly,
        exceeded: costs.after > monthly,
    });
    let confidence = confidence(&counts, &planned, has_baseline, region_assumed);

    let unique = |values: Vec<String>| {
        values
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    };
    let sides = || {
        changes
            .iter()
            .flat_map(|change| change.after.iter().chain(change.before.iter()))
    };

    Ok(Report {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: SCHEMA_VERSION.to_string(),
        scan_time: scan_time(),
        project,
        iac: unique(inputs.iter().map(|input| input.source.label().to_string()).collect()),
        sources: inputs.iter().map(|input| input.origin.clone()).collect(),
        clouds: unique(sides().map(|resource| resource.provider.clone()).collect()),
        regions: unique(sides().filter_map(|resource| resource.region.clone()).collect()),
        currency,
        pricing_source: "AWS public list prices (on-demand)".to_string(),
        pricing: resolved.snapshots,
        monthly_hours: engine.monthly_hours,
        usage_profile: usage.profile_label(),
        has_baseline,
        counts,
        costs,
        resources: estimates,
        assumptions,
        findings,
        unsupported_resources,
        warnings,
        errors,
        confidence,
        budget,
    })
}

pub fn limits(config: &Config) -> Limits {
    let defaults = Limits::default();
    Limits {
        max_depth: config.limits.max_depth.unwrap_or(defaults.max_depth),
        max_files: config.limits.max_files.unwrap_or(defaults.max_files),
        max_file_bytes: config.limits.max_file_mb.map_or(defaults.max_file_bytes, |megabytes| {
            megabytes.saturating_mul(1024 * 1024)
        }),
    }
}

/// Reads every IaC input. A broken file in a repository scan is recorded as an error
/// and the scan continues; an explicitly named plan file that cannot be read is fatal.
fn load_inputs(
    options: &ScanOptions,
    limits: &Limits,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Result<(String, Vec<Input>)> {
    if let Some(plan) = &options.plan_file {
        let text = discovery::read_limited(plan, limits.max_file_bytes)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("cannot read {}", plan.display()))?;
        let input = terraform_plan::parse(&text, &plan.display().to_string())
            .with_context(|| format!("{} could not be used as a Terraform plan", plan.display()))?;
        let project = plan
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().to_string());
        (options.progress)("Terraform plan loaded".to_string());
        return Ok((project, vec![input]));
    }

    let Discovery {
        root,
        project,
        plans,
        terraform_roots,
        not_yet_supported,
        warnings: found,
    } = discovery::discover(&options.path, limits)?;
    warnings.extend(found);
    for technology in &not_yet_supported {
        warnings.push(format!("{technology} files were detected but are not analysed yet."));
    }
    (options.progress)("Repository scanned".to_string());

    let relative = |path: &Path| {
        let shown = path.strip_prefix(&root).unwrap_or(path).display().to_string();
        if shown.is_empty() { ".".to_string() } else { shown }
    };
    let mut inputs = Vec::new();
    let mut planned_dirs = BTreeSet::new();

    for plan in &plans {
        let parsed = discovery::read_limited(plan, limits.max_file_bytes)
            .map_err(anyhow::Error::msg)
            .and_then(|text| terraform_plan::parse(&text, &relative(plan)));
        match parsed {
            Ok(input) => {
                planned_dirs.extend(plan.parent().map(Path::to_path_buf));
                inputs.push(input);
            }
            Err(error) => errors.push(format!("{}: {error:#}. The scan continued without it.", relative(plan))),
        }
    }

    // A plan is the higher-fidelity view of its directory, so that directory is not also read statically.
    let hcl_options = terraform_hcl::Options {
        scan_root: &root,
        max_file_bytes: limits.max_file_bytes,
    };
    for dir in terraform_roots.iter().filter(|dir| !planned_dirs.contains(*dir)) {
        match terraform_hcl::parse_root(dir, &relative(dir), &hcl_options) {
            Ok(input) => inputs.push(input),
            Err(error) => errors.push(format!("{}: {error:#}. The scan continued without it.", relative(dir))),
        }
    }

    if inputs.is_empty() && errors.is_empty() {
        warnings.push(format!(
            "No Terraform configuration or plan JSON found in {}. Nothing was estimated.",
            options.path.display()
        ));
    }
    Ok((project, inputs))
}

fn pricing(options: &ScanOptions, config: &Config) -> Result<Arc<AwsPricing>> {
    let cache_dir = std::env::var_os("CLOUDPREFLIGHT_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| config.cache.dir.clone())
        .or_else(|| dirs::cache_dir().map(|dir| dir.join("cloudpreflight")))
        .context("no cache directory available. Set CLOUDPREFLIGHT_CACHE_DIR.")?;
    let base_url = std::env::var("CLOUDPREFLIGHT_AWS_PRICING_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());

    let client = reqwest::Client::builder()
        .user_agent(concat!("cloudpreflight/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("cannot initialise the HTTP client")?;

    Ok(Arc::new(AwsPricing {
        client,
        base_url,
        cache: Cache::new(cache_dir),
        offline: options.offline,
        no_cache: options.no_cache,
        ttl_secs: config.cache.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS).saturating_mul(3600),
        progress: Arc::clone(&options.progress),
    }))
}

fn confidence(counts: &Counts, planned: &[&ResourceEstimate], from_plan: bool, region_assumed: bool) -> Confidence {
    let billable = counts.total - counts.no_direct_charge;
    let incomplete = counts.partially_priced + counts.unresolved + counts.unsupported;
    if billable == 0 || counts.priced + counts.partially_priced == 0 {
        return Confidence::Low;
    }
    if Decimal::from(incomplete) / Decimal::from(billable) > LOW_CONFIDENCE_SHARE {
        return Confidence::Low;
    }

    let soft = planned.iter().any(|estimate| {
        estimate
            .components
            .iter()
            .any(|component| matches!(component.outcome, Outcome::Priced { kind, .. } if kind != PriceKind::Exact))
    });
    if incomplete > 0 || soft || !from_plan || region_assumed {
        return Confidence::Medium;
    }
    Confidence::High
}

/// RFC 3339 UTC timestamp. Honours `SOURCE_DATE_EPOCH` so output can be reproduced.
fn scan_time() -> String {
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(crate::pricing::cache::now);
    rfc3339(seconds)
}

fn rfc3339(epoch_seconds: u64) -> String {
    let days = (epoch_seconds / 86_400) as i64;
    let second_of_day = epoch_seconds % 86_400;

    // Civil-from-days (Howard Hinnant's algorithm), valid for the whole u64 range we accept.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_158_399), "2026-10-04T23:59:59Z");
    }

    fn counts(priced: usize, unresolved: usize, unsupported: usize) -> Counts {
        Counts {
            total: priced + unresolved + unsupported,
            priced,
            partially_priced: 0,
            unresolved,
            unsupported,
            no_direct_charge: 0,
        }
    }

    #[test]
    fn confidence_levels() {
        assert_eq!(confidence(&counts(10, 0, 0), &[], true, false), Confidence::High);
        assert_eq!(confidence(&counts(10, 0, 0), &[], false, false), Confidence::Medium);
        assert_eq!(confidence(&counts(10, 0, 0), &[], true, true), Confidence::Medium);
        assert_eq!(confidence(&counts(9, 1, 0), &[], true, false), Confidence::Medium);
        assert_eq!(confidence(&counts(6, 2, 2), &[], true, false), Confidence::Low);
        assert_eq!(confidence(&counts(0, 0, 0), &[], true, false), Confidence::Low);
    }
}
