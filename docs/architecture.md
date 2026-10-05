# Architecture

## Pipeline

```
repository ─► discovery ─► IaC adapter ─► normaliser ─► cost engine ─► FinOps rules ─► report
                                              │              ▲
                                              └─ price queries ─► pricing (cache, AWS Bulk API)
```

| Module | Responsibility | Must not know about |
| --- | --- | --- |
| `discovery` | Bounded walk; finds plan files and Terraform roots | pricing |
| `iac::terraform_plan`, `iac::terraform_hcl` | Produce `model::Input` | pricing, reporting |
| `aws` | Region resolution and derived attributes | how the IaC was written |
| `mapping` + `data/` | Which price components a resource type has | arithmetic |
| `pricing` | Answer `PriceQuery` with list prices; cache | IaC |
| `usage` | Resolve usage values and name their source | pricing |
| `estimate` | Quantities, tiers, units, deltas. Pure, no I/O | IaC tool, network |
| `finops` | Findings from resources and estimates | rendering |
| `report` | Format the `Report` | pricing arithmetic |
| `pipeline`, `cli` | Wiring, flags, exit codes | |

The cost engine is given resources and a map of resolved prices, so it is tested
without any network or filesystem.

## Decisions

**Unknown is a value, not zero.** A component outcome is `Priced` or `Unresolved`
with a reason; a resource is additionally `Unsupported` or `NoDirectCharge`. A
resource's `monthly` is `None` when nothing could be priced. Totals sum only priced
components and the report states how many resources are incomplete.

**Money is `rust_decimal::Decimal`.** Arithmetic keeps full precision and uses checked
operations; overflow yields `Unresolved`. Rounding to cents happens only in rendering.
JSON carries decimals as strings.

**Mappings are data.** A resource type is a YAML file listing components: price list
service, attribute filters with `${attr}` placeholders, expected unit, and a quantity
that is a product of `hours`, attributes, usage keys and constants. Values that need
logic (Fargate vCPU from the task definition, RDS engine names) are derived in
`aws.rs` as `_`-prefixed attributes. The engine has no per-resource code.

**Units are checked, not converted.** A mapping declares the unit it computes. If the
matched price uses a different unit the component is unresolved. `hours` may only
multiply a unit that contains hours.

**AWS prices come from the Bulk API.** It needs no credentials. The cost is file
size: the EC2 regional file is about 480 MB. It is streamed to disk and stream-parsed
with a serde visitor that keeps only matching SKUs and their on-demand terms, and
each query's answer is cached against the price list version. The Price List Query
API would avoid the download but needs credentials; it belongs with account-aware
pricing.

**Usage types are matched region-agnostically.** AWS prefixes usage types with a
region code in most regions (`EUW1-Request`) but not in us-east-1. A filter value
starting with `@` matches with or without the prefix. RDS and ElastiCache abbreviate
the instance size inside usage types (`db.m6g.xl`), so those mappings do not match on
the size there.

**Ambiguous matches are flagged.** If several SKUs with different prices satisfy a
query, the first by SKU id is used and the component is marked estimated.

**Deltas are like-for-like.** For a resource present on both sides, a component
counts toward the delta only if priced on both sides or present on just one.

**Plans beat static analysis.** A directory containing plan JSON is not also analysed
statically. Static analysis never guesses: an expression it cannot evaluate is
unknown, and an unknown `count` assumes one instance and says so.

**Sensitive values are removed at parse time,** using the plan's sensitivity masks,
so no later stage can leak them.

**Nothing is executed.** There is no subprocess code in this slice. Running
`terraform`, `terragrunt` or `cdk` on request is future work and needs its own
allowlist, timeout and output limits.

**A default usage profile is applied and shown.** With no usage at all, most
serverless and storage resources would be unpriced and the report of little use. The
`standard` profile is therefore the default, labelled `(default)`, with every value
listed under "Usage assumptions".

**Single crate.** A workspace is not needed until there is a second consumer.

## Known limits

- Reference detection in static analysis scans expression text rather than the syntax tree.
- Root-module detection uses a line scan for local `source =` values.
- Account-wide free tiers are applied per resource where AWS publishes them as tiers.
- Two root modules that declare the same address are both reported under that address.

## Not built yet

Terragrunt, CloudFormation, CDK, Azure, GCP, account-aware pricing, baseline files
and git-ref baselines, a data-driven rule engine, Markdown and SARIF output, `init`,
`auth`, `doctor` and `pricing` commands, cache size management, fuzzing, benchmarks,
release binaries, eCommerce and AI usage presets, Bedrock token pricing.
