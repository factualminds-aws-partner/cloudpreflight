# cloudpreflight

[![CI](https://github.com/factualminds-aws-partner/cloudpreflight/actions/workflows/ci.yml/badge.svg)](https://github.com/factualminds-aws-partner/cloudpreflight/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](Cargo.toml)

**See the cost before you ship the infrastructure.**

`cloudpreflight` reads Terraform, works out what it will create, and estimates the
monthly cost from public AWS list prices. It runs locally, needs no account or cloud
credentials, and never executes your infrastructure code.

```
$ cloudpreflight scan fixtures/aws-ecommerce-plan

Cloud Cost Preflight
────────────────────────────────────────────────────────────────────────────────
Project       aws-ecommerce-plan
Detected      Terraform (plan JSON)
Cloud         AWS
Region        us-east-1
Resources     14 (9 priced, 2 partially priced, 1 unsupported, 2 no direct charge)
Pricing       AWS public list prices (on-demand), published up to 2026-10-03
Usage         profile: standard (default)
Estimate      ≈ $1,165.44 / month
              ≈ $13,985.22 / year
              Not a complete total: 3 resource(s) could not be fully priced and
              are not counted as $0.

Change vs current state
────────────────────────────────────────────────────────────────────────────────
New monthly cost                                                     ≈ $1,165.44
Previous monthly cost                                                  ≈ $405.82
Monthly delta                                                 +$759.61 (+187.2%)
Annualized delta                                                      +$9,115.37

FinOps findings
────────────────────────────────────────────────────────────────────────────────
HIGH  RDS Multi-AZ adds significant fixed monthly cost
      aws_db_instance.orders (RDS-MULTIAZ-COST, ≈ $255.14 / month)
MED   NAT Gateway traffic assumptions are undefined
      aws_nat_gateway.main (NAT-TRAFFIC-UNDEFINED)
```

## Status and scope

This is the first slice of a larger design. What works today:

| Area | Supported now | Not yet |
| --- | --- | --- |
| IaC | Terraform plan JSON, Terraform static analysis (`.tf`, `.tf.json`) | Terragrunt, CloudFormation, CDK (detected and reported, not analysed) |
| Cloud | AWS public on-demand list prices | Azure, GCP, account-specific pricing |
| Output | Terminal report, versioned JSON | Markdown, SARIF |
| Commands | `scan`, `estimate`, `explain`, `validate`, `providers` | `init`, `auth`, `pricing`, `doctor` |

Priced AWS resource types: `aws_instance`, `aws_ebs_volume`, `aws_db_instance`
(PostgreSQL, MySQL, MariaDB), `aws_lb`/`aws_alb`, `aws_nat_gateway`, `aws_ecs_service`
(Fargate), `aws_lambda_function`, `aws_s3_bucket`, `aws_cloudfront_distribution`,
`aws_dynamodb_table`, `aws_elasticache_cluster`, `aws_elasticache_replication_group`.
Run `cloudpreflight providers` for the current list. Anything else is reported as
unsupported. It is never shown as `$0`.

## Install

Requires Rust 1.89 or newer.

```
cargo install --git https://github.com/factualminds-aws-partner/cloudpreflight --locked
```

Or from a clone: `cargo install --path .`

## Quick start

```
cloudpreflight scan .                      # scan a repository
cloudpreflight estimate plan.json          # one Terraform plan
cloudpreflight explain aws_db_instance.main
cloudpreflight scan . --format json        # for scripts and CI
cloudpreflight scan . --budget 2500 --strict
```

For the most accurate result, give it a plan:

```
terraform plan -out tfplan
terraform show -json tfplan > tfplan.json
cloudpreflight scan .
```

`scan` picks up plan JSON files (any `*.json` whose name contains `plan`, verified
by content) and prefers them over static analysis of the same directory.
`cloudpreflight` does not run Terraform for you.

## How the estimate is built

1. **Discover.** A bounded, read-only walk finds plan files and Terraform root modules.
2. **Read the IaC.** A plan gives exact before and after values. Static analysis
   resolves variable defaults, `terraform.tfvars`, locals, literal `count` and
   `for_each`, and local modules. Whatever it cannot resolve is marked unknown.
3. **Price.** Each resource maps to one or more price components (instance hours,
   storage, requests). Prices come from the AWS Price List Bulk API.
4. **Apply usage.** Traffic and data volumes are not in infrastructure code, so they
   come from a usage profile or your config file, and are always listed in the report.
5. **Report.** Totals, like-for-like deltas, findings, and everything that could not
   be priced.

Every priced component carries one of three labels:

- **exact list price**: every input came from the IaC and exactly one SKU matched.
- **estimated**: a stated assumption was needed (for example, Linux for EC2, since
  the operating system comes from the AMI).
- **usage assumed**: the quantity comes from a usage profile or config value.

A component that cannot be priced is **not priced**, with the reason. It is left out
of the total and the report says the total is incomplete.

Monthly hours default to 730 and apply only to hourly prices. A mapping that tries to
multiply a monthly price by hours is refused.

### Deltas

With a plan, each change is priced before and after. The delta is like-for-like: if a
component can be priced on only one side of a change, it is left out of the delta and
a warning names the resource. Deleted resources show the monthly cost they avoid.

## Usage profiles and configuration

Built-in profiles: `light`, `standard` (default), `high`. They are illustrative
numbers, not measurements. See them with `cloudpreflight providers` and in
[`data/usage/`](data/usage).

Put measured values in `.factualminds-cost.yaml` at the repository root:

```yaml
version: 1

defaults:
  monthly_hours: 730

aws:
  region: us-east-1        # used only when the IaC names no region

usage_profile: standard

usage:
  aws:
    s3:
      storage_gb: 500
      requests_monthly: 10000000
    lambda:
      invocations_monthly: 5000000
      average_duration_ms: 180
    nat_gateway:
      data_processed_gb: 800

resource:
  "aws_s3_bucket.catalog":
    usage:
      storage_gb: 1000

budget:
  monthly: 2500

cache:
  ttl_hours: 168
```

Precedence for a usage value: per-resource override, then `usage:`, then the profile.
A value that is not a non-negative number stops the run; it does not fall back.

Usage keys by service: `s3` (`storage_gb`, `requests_monthly`,
`write_requests_monthly`), `lambda` (`invocations_monthly`, `average_duration_ms`),
`lb` (`lcu_average`), `nat_gateway` (`data_processed_gb`), `cloudfront`
(`data_transfer_out_gb`, `https_requests_monthly`), `dynamodb` (`storage_gb`,
`read_request_units_monthly`, `write_request_units_monthly`).

If no repository config exists, `~/.config/cloudpreflight/config.yaml` (or the
platform equivalent) is used. `--config` names a file explicitly.

## CI

Exit codes are stable:

| Code | Meaning |
| --- | --- |
| 0 | Completed |
| 1 | Error (unreadable input, invalid config, or a file in the scan failed to parse) |
| 2 | Invalid command-line usage |
| 3 | Monthly estimate exceeds the budget |
| 4 | `--strict` and something was unsupported or not fully priced |

GitHub Actions:

```yaml
jobs:
  cost:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install --git https://github.com/factualminds-aws-partner/cloudpreflight --locked
      - uses: actions/cache@v4
        with:
          path: ~/.cache/cloudpreflight
          key: cloudpreflight-prices-${{ github.run_id }}
          restore-keys: cloudpreflight-prices-
      - run: cloudpreflight scan . --format json --budget 2500 > cost.json
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: cost-estimate
          path: cost.json
```

GitLab CI:

```yaml
cost:
  image: rust:latest
  cache:
    paths: [.cloudpreflight-cache]
  variables:
    CLOUDPREFLIGHT_CACHE_DIR: $CI_PROJECT_DIR/.cloudpreflight-cache
  script:
    - cargo install --git https://github.com/factualminds-aws-partner/cloudpreflight --locked
    - cloudpreflight scan . --format json --budget 2500 > cost.json
  artifacts:
    when: always
    paths: [cost.json]
```

### JSON output

`--format json` prints one object with a `schema_version` (currently `1.0`). It
includes `tool_version`, `scan_time`, `project`, `iac`, `sources`, `clouds`,
`regions`, `pricing_source`, `pricing` (price list version and publication date per
service and region), `counts`, `costs`, `resources` (with per-component quantity,
SKU, formula and status), `assumptions`, `findings`, `unsupported_resources`,
`warnings`, `errors`, `confidence` and `budget`.

Money values are decimal strings, not floats. Identical IaC, config, usage and price
list versions produce identical output; set `SOURCE_DATE_EPOCH` to fix `scan_time`.
If two runs differ, compare `pricing[].version` to see whether AWS published new prices.

## Pricing data and cache

Prices are downloaded from
`https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/<service>/current/<region>/index.json`
and cached under the user cache directory (`CLOUDPREFLIGHT_CACHE_DIR` overrides it).
No prices are compiled into the binary.

- The cache is refreshed after 7 days (`cache.ttl_hours`), using a conditional
  request so an unchanged file is not downloaded again.
- `--offline` uses the cache only. A price that is not cached is reported as not priced.
- `--no-cache` fetches fresh price lists.
- Interrupted downloads resume. A corrupt cache entry is removed and fetched again.

**The first scan that needs EC2, EBS or NAT Gateway prices in a region downloads a
price file of roughly 480 MB**, because AWS publishes those services as one file and
offers no filtered endpoint without credentials. That run takes as long as the
download; later runs take about a second. Other services are between 7 KB and 27 MB.

## FinOps findings

| Rule | Severity | Trigger |
| --- | --- | --- |
| `RDS-MULTIAZ-COST` | High | `multi_az = true` on an RDS instance. Impact is half the instance's estimate. |
| `NAT-TRAFFIC-UNDEFINED` | Medium | A NAT Gateway whose traffic volume is not set in the config file. |
| `S3-NO-LIFECYCLE` | Low | A bucket with no lifecycle configuration in the scanned infrastructure. |
| `EBS-GP2-VOLUME` | Low | An EBS volume of type gp2. |

Findings describe what the configuration shows and what to review. They do not claim
a resource is wasteful; that needs runtime evidence the IaC does not contain.

## Accuracy and limitations

- This is an estimate from public list prices. It is not a bill. Discounts, Savings
  Plans, Reserved Instances, credits, taxes and support are not included.
- Free allowances appear only where AWS publishes them as a zero-priced tier
  (DynamoDB), and are then applied per resource although they are account-wide. The
  Lambda free tier is not applied.
- Not modelled: data transfer between services and to the internet (except
  CloudFront), backups and snapshots, CloudWatch, EC2 operating systems other than
  Linux, RDS engines with licences, Aurora, ECS on EC2 capacity, DynamoDB indexes.
- CloudFront is priced at United States edge rates.
- Static analysis does not evaluate remote modules, data sources or most Terraform
  functions. Use a plan for anything that matters.
- Only USD is available.

## Security and privacy

- No subprocesses. `cloudpreflight` never runs `terraform`, `terragrunt`, `cdk` or a shell.
- It does not modify the repository, Terraform state or any cloud resource.
- Values Terraform marks sensitive are dropped while the plan is parsed, before
  anything can print or log them.
- Repository traversal does not follow symlinks and is bounded in depth, file count
  and file size (`limits:` in the config). Local modules outside the scanned
  directory are skipped.
- Region and service names are validated before they are used in a URL or a file path.
- The only network requests are HTTPS GETs for AWS price lists. No source code, plan
  content or credentials are sent anywhere. With `--offline` there are none.
- No credentials are read or stored.

## Development

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo nextest run        # or: cargo test
cargo deny check
```

Tests use trimmed copies of real AWS price lists served by a local mock server; no
test needs the network or a cloud account. Snapshots are managed with `cargo insta`.

### Adding a resource type

1. Add `data/aws/resources/<type>.yaml` describing its price components, and one
   `include_str!` line in `src/mapping.rs`. See `aws_nat_gateway.yaml` for a small example.
2. If a filter needs a value that is not a plain attribute, derive it in `src/aws.rs`.
3. Add the resource to a fixture and the SKUs it needs to `tests/fixtures/pricing/`.

If a resource type has no charge of its own, add it to `data/aws/no_charge.yaml`.
The design is described in [docs/architecture.md](docs/architecture.md).

## License

Apache-2.0
