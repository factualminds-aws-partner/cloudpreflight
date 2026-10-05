//! End-to-end tests of the compiled binary. Prices come from trimmed copies of real AWS
//! price lists served by a local mock server, so no test touches the network or needs
//! cloud credentials.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rust_decimal::Decimal;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PRICE_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pricing");
const PLAN_FIXTURE: &str = "fixtures/aws-ecommerce-plan";
const HCL_FIXTURE: &str = "fixtures/aws-ecommerce";

struct Sandbox {
    server: MockServer,
    home: tempfile::TempDir,
}

impl Sandbox {
    /// Serves every fixture price list at the path the AWS Bulk API uses.
    async fn new() -> Self {
        let server = MockServer::start().await;
        for service in fs::read_dir(PRICE_FIXTURES).unwrap() {
            let service = service.unwrap().path();
            for region in fs::read_dir(&service).unwrap() {
                let region = region.unwrap().path();
                let url = format!(
                    "/offers/v1.0/aws/{}/current/{}/index.json",
                    service.file_name().unwrap().to_string_lossy(),
                    region.file_stem().unwrap().to_string_lossy()
                );
                Mock::given(method("GET"))
                    .and(path(url))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(fs::read(&region).unwrap()))
                    .mount(&server)
                    .await;
            }
        }
        Self {
            server,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cloudpreflight"));
        command
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .env_clear()
            // Windows cannot open sockets or find its temp directory without these.
            .envs(
                [
                    "SYSTEMROOT",
                    "SystemRoot",
                    "TEMP",
                    "TMP",
                    "USERPROFILE",
                    "APPDATA",
                    "LOCALAPPDATA",
                ]
                .into_iter()
                .filter_map(|name| Some((name, std::env::var_os(name)?))),
            )
            .env("HOME", self.home.path())
            .env("CLOUDPREFLIGHT_CACHE_DIR", self.home.path().join("cache"))
            .env("CLOUDPREFLIGHT_AWS_PRICING_URL", self.server.uri())
            .env("SOURCE_DATE_EPOCH", "1791158400")
            .env("LANG", "en_US.UTF-8")
            .env("COLUMNS", "80");
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        Run::from(self.command(args).output().unwrap())
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
}

fn write(root: &Path, name: &str, content: &str) {
    fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
    fs::write(root.join(name), content).unwrap();
}

fn decimal(value: &Value) -> Decimal {
    value.as_str().unwrap().parse().unwrap()
}

/// A minimal plan that creates one resource.
fn plan_with(resource_type: &str, after: Value, region: &str) -> String {
    serde_json::json!({
        "format_version": "1.2",
        "resource_changes": [{
            "address": format!("{resource_type}.x"), "mode": "managed", "type": resource_type, "name": "x",
            "change": {"actions": ["create"], "before": null, "after": after, "after_unknown": {}}
        }],
        "configuration": {"provider_config": {"aws": {"name": "aws", "expressions": {"region": {"constant_value": region}}}}}
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_scan_report() {
    let sandbox = Sandbox::new().await;
    let run = sandbox.run(&["scan", PLAN_FIXTURE]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.stderr, "",
        "nothing is logged by default when stderr is not a terminal"
    );
    insta::assert_snapshot!(run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn static_scan_report() {
    let sandbox = Sandbox::new().await;
    let run = sandbox.run(&["scan", HCL_FIXTURE]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    insta::assert_snapshot!(run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn narrow_terminal_and_ascii_fallback() {
    let sandbox = Sandbox::new().await;
    let output = sandbox
        .command(&["scan", PLAN_FIXTURE])
        .env("COLUMNS", "48")
        .env("LANG", "C")
        .output()
        .unwrap();
    let run = Run::from(output);

    assert!(run.stdout.is_ascii(), "non-ASCII output under LANG=C");
    assert!(
        run.stdout.lines().all(|line| line.chars().count() <= 48),
        "a line exceeds 48 columns"
    );
    insta::assert_snapshot!(run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_colour_codes_reach_a_pipe_and_no_color_flag_is_accepted() {
    let sandbox = Sandbox::new().await;
    let piped = sandbox.run(&["scan", PLAN_FIXTURE]);
    let flagged = sandbox.run(&["scan", PLAN_FIXTURE, "--no-color"]);
    assert!(!piped.stdout.contains('\u{1b}'));
    assert_eq!(piped.stdout, flagged.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_is_versioned_complete_deterministic_and_free_of_secrets() {
    let sandbox = Sandbox::new().await;
    let plan = format!("{PLAN_FIXTURE}/tfplan.json");
    let first = sandbox.run(&["estimate", &plan, "--format", "json"]);
    let second = sandbox.run(&["estimate", &plan, "--format", "json"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(
        first.stdout, second.stdout,
        "identical inputs must give identical output"
    );

    let report: Value = serde_json::from_str(&first.stdout).unwrap();
    for field in [
        "tool_version",
        "schema_version",
        "scan_time",
        "project",
        "clouds",
        "iac",
        "pricing_source",
        "pricing",
        "resources",
        "costs",
        "assumptions",
        "findings",
        "unsupported_resources",
        "errors",
    ] {
        assert!(report.get(field).is_some(), "missing `{field}`");
    }
    assert_eq!(report["schema_version"], "1.0");
    assert_eq!(report["scan_time"], "2026-10-05T00:00:00Z");
    assert_eq!(report["has_baseline"], true);
    assert!(
        report["pricing"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["version"].is_string() && p["publication_date"].is_string())
    );

    // Money is serialised as decimal strings, never as binary floats.
    assert!(report["costs"]["before"].is_string());
    assert_eq!(
        report["unsupported_resources"][0]["address"],
        "aws_cloudwatch_log_group.web"
    );
    // The plan holds a database password marked sensitive.
    assert!(!first.stdout.contains("example-only-not-a-real-secret"));

    let stable = first.stdout.replace(env!("CARGO_PKG_VERSION"), "[version]");
    insta::assert_snapshot!(stable);
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_shows_the_formula_and_rejects_unknown_addresses() {
    let sandbox = Sandbox::new().await;
    let run = sandbox.run(&["explain", "aws_ecs_service.web", "--path", PLAN_FIXTURE, "--verbose"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    insta::assert_snapshot!(run.stdout);

    // An instance key may be omitted.
    let workers = sandbox.run(&["explain", "module.workers.aws_instance.worker", "--path", PLAN_FIXTURE]);
    assert!(workers.stdout.contains("worker[0]") && workers.stdout.contains("worker[1]"));

    let missing = sandbox.run(&["explain", "aws_instance.nope", "--path", PLAN_FIXTURE]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("no resource with address `aws_instance.nope`"),
        "{}",
        missing.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_codes_are_deterministic() {
    let sandbox = Sandbox::new().await;

    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--budget", "5000"]).code, 0);
    let over = sandbox.run(&["scan", PLAN_FIXTURE, "--budget", "100"]);
    assert_eq!(over.code, 3);
    assert!(over.stdout.contains("EXCEEDED"));

    // The fixture has an unsupported log group and two partly priced instances.
    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--strict"]).code, 4);
    // The budget in the fixture's config file applies without a flag.
    assert_eq!(sandbox.run(&["scan", HCL_FIXTURE]).code, 0);
    assert_eq!(sandbox.run(&["scan", HCL_FIXTURE, "--budget", "1"]).code, 3);

    assert_eq!(sandbox.run(&["scan", "does/not/exist"]).code, 1);
    assert_eq!(sandbox.run(&["scan", "--no-such-flag"]).code, 2);
    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--budget=-5"]).code, 1);
    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--currency", "EUR"]).code, 1);
    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--usage-profile", "nope"]).code, 1);
    assert_eq!(sandbox.run(&["scan", PLAN_FIXTURE, "--offline", "--no-cache"]).code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_repository_is_not_an_error() {
    let sandbox = Sandbox::new().await;
    let empty = tempfile::tempdir().unwrap();
    let run = sandbox.run(&["scan", empty.path().to_str().unwrap()]);
    assert_eq!(run.code, 0);
    assert!(
        run.stdout.contains("No Terraform configuration or plan JSON found"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Estimate confidence: LOW"));
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_uses_the_cache_and_reports_misses_without_failing() {
    let sandbox = Sandbox::new().await;

    let cold = sandbox.run(&["scan", PLAN_FIXTURE, "--offline"]);
    assert_eq!(cold.code, 0);
    assert!(
        cold.stdout.contains("offline and no cached AmazonRDS price list"),
        "{}",
        cold.stdout
    );
    assert!(cold.stdout.contains("≈ $0.00 / month") && cold.stdout.contains("Not a complete total"));

    let online = sandbox.run(&["scan", PLAN_FIXTURE]);
    let warm = sandbox.run(&["scan", PLAN_FIXTURE, "--offline"]);
    assert_eq!(online.stdout, warm.stdout);
    assert_eq!(
        sandbox.server.received_requests().await.unwrap().len(),
        9,
        "one request per price list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_profile_changes_only_usage_based_costs() {
    let sandbox = Sandbox::new().await;
    let plan = format!("{PLAN_FIXTURE}/tfplan.json");
    let cost = |profile: &str, address: &str| {
        let run = sandbox.run(&["estimate", &plan, "--format", "json", "--usage-profile", profile]);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let resource = report["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["address"] == address)
            .unwrap();
        resource["after"]["monthly"].as_str().unwrap().to_string()
    };

    assert_eq!(
        cost("light", "aws_db_instance.orders"),
        cost("high", "aws_db_instance.orders")
    );
    assert_ne!(
        cost("light", "aws_s3_bucket.catalog"),
        cost("high", "aws_s3_bucket.catalog")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_sku_and_missing_region_are_unresolved_with_reasons() {
    let sandbox = Sandbox::new().await;
    let dir = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "sku.json",
        &plan_with(
            "aws_instance",
            serde_json::json!({"instance_type": "tX.large"}),
            "us-east-1",
        ),
    );
    let sku = sandbox.run(&["estimate", dir.path().join("sku.json").to_str().unwrap()]);
    assert_eq!(sku.code, 0);
    assert!(
        sku.stdout.contains("no AmazonEC2 list price in us-east-1 matches"),
        "{}",
        sku.stdout
    );
    assert!(sku.stdout.contains("instanceType=tX.large"));

    write(
        dir.path(),
        "region.json",
        &plan_with("aws_nat_gateway", serde_json::json!({}), "xx-fake-1"),
    );
    let region = sandbox.run(&["estimate", dir.path().join("region.json").to_str().unwrap(), "--strict"]);
    assert_eq!(region.code, 4);
    assert!(region.stdout.contains("Check the region name"), "{}", region.stdout);

    // A region name that is not a plain identifier never reaches the filesystem or the URL.
    write(
        dir.path(),
        "evil.json",
        &plan_with("aws_nat_gateway", serde_json::json!({}), "../../etc"),
    );
    let evil = sandbox.run(&["estimate", dir.path().join("evil.json").to_str().unwrap()]);
    assert!(
        evil.stdout.contains("is not a valid provider, service or region name"),
        "{}",
        evil.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_and_extreme_usage() {
    let sandbox = Sandbox::new().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "main.tf",
        "resource \"aws_s3_bucket\" \"b\" {\n  bucket = \"b\"\n}\n",
    );
    let scan = |storage: &str| {
        write(
            dir.path(),
            ".factualminds-cost.yaml",
            &format!(
                "aws: {{region: us-east-1}}\nusage:\n  aws:\n    s3: {{storage_gb: {storage}, requests_monthly: 0, write_requests_monthly: 0}}\n"
            ),
        );
        sandbox.run(&["scan", dir.path().to_str().unwrap(), "--format", "json"])
    };
    let report = |run: &Run| serde_json::from_str::<Value>(&run.stdout).unwrap();

    // Zero usage is a known zero: priced, and distinct from "unresolved".
    let zero = report(&scan("0"));
    assert_eq!(zero["resources"][0]["after"]["status"], "priced");
    assert_eq!(decimal(&zero["costs"]["after"]), Decimal::ZERO);

    // 600 TB crosses both S3 tier boundaries: 51,200 at 0.023, 460,800 at 0.022, 88,000 at 0.021.
    let large = report(&scan("600000"));
    assert_eq!(decimal(&large["costs"]["after"]), Decimal::new(131_632, 1));

    // Near the top of the representable range the arithmetic still holds.
    let huge = report(&scan("70000000000000000000000000000"));
    assert_eq!(huge["resources"][0]["after"]["status"], "priced");
    assert!(decimal(&huge["costs"]["after"]) > Decimal::new(1, 0) * Decimal::from(10u64.pow(18)));

    // Beyond it, the run stops with a reason instead of wrapping or guessing.
    let absurd = scan("1e40");
    assert_eq!(absurd.code, 1);
    assert!(absurd.stderr.contains("usage.aws.s3.storage_gb"), "{}", absurd.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_root_is_reported_and_the_rest_of_the_repository_is_still_scanned() {
    let sandbox = Sandbox::new().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "good/main.tf",
        "provider \"aws\" {\n  region = \"us-east-1\"\n}\nresource \"aws_nat_gateway\" \"n\" {}\n",
    );
    write(
        dir.path(),
        "bad/main.tf",
        "resource \"aws_instance\" \"x\" {\n  instance_type = \n",
    );
    write(
        dir.path(),
        "bad-plan/tfplan.json",
        "{\"format_version\": \"9.0\", \"resource_changes\": []}",
    );

    let run = sandbox.run(&["scan", dir.path().to_str().unwrap()]);

    assert_eq!(run.code, 1, "errors make the exit code non-zero");
    assert!(run.stdout.contains("good") || run.stdout.contains("aws_nat_gateway.n"));
    assert!(run.stdout.contains("aws_nat_gateway.n"), "{}", run.stdout);
    assert!(
        run.stdout.contains("bad: ") && run.stdout.contains("not valid HCL"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("plan format version 9.0 is not supported"),
        "{}",
        run.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_repository_uses_plans_where_present_and_static_analysis_elsewhere() {
    let sandbox = Sandbox::new().await;
    let run = sandbox.run(&["scan", "fixtures", "--format", "json"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();

    assert_eq!(
        report["iac"],
        serde_json::json!(["Terraform (plan JSON)", "Terraform (static analysis)"])
    );
    assert_eq!(
        report["sources"],
        serde_json::json!(["aws-ecommerce-plan/tfplan.json", "aws-ecommerce"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_and_providers_need_no_pricing() {
    let sandbox = Sandbox::new().await;

    let validate = sandbox.run(&["validate", "fixtures"]);
    assert_eq!(validate.code, 0, "{}", validate.stderr);
    insta::assert_snapshot!(validate.stdout);

    let providers = sandbox.run(&["providers"]);
    assert!(providers.stdout.contains("aws_db_instance") && providers.stdout.contains("standard"));
    assert!(sandbox.server.received_requests().await.unwrap().is_empty());

    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "main.tf", "resource \"aws_instance\" {");
    write(dir.path(), ".factualminds-cost.yaml", "version: 1\n");
    let broken = sandbox.run(&["validate", dir.path().to_str().unwrap()]);
    assert_eq!(broken.code, 1);
    assert!(broken.stdout.contains("invalid  terraform"));

    write(dir.path(), ".factualminds-cost.yaml", "defaults: [oops");
    let bad_config = sandbox.run(&["validate", dir.path().to_str().unwrap()]);
    assert_eq!(bad_config.code, 1);
    assert!(
        bad_config.stderr.contains("invalid config file"),
        "{}",
        bad_config.stderr
    );
}
