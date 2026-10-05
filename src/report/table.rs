//! Terminal report. Pure function of the report and render options, so it can be
//! snapshot-tested; colour codes are stripped by the output stream when not on a TTY.

use std::collections::BTreeMap;
use std::fmt::Write;

use anstyle::{AnsiColor, Style};
use rust_decimal::Decimal;

use super::{money, signed_money};
use crate::model::{
    Action, ChangeEstimate, ComponentEstimate, Confidence, Outcome, PriceKind, Report, ResourceEstimate,
    ResourceStatus, Severity,
};

const MAX_ROWS: usize = 15;
const LABEL_WIDTH: usize = 14;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub width: usize,
    pub unicode: bool,
    pub color: bool,
    pub verbose: bool,
}

struct Painter {
    options: Options,
    out: String,
}

pub fn render(report: &Report, options: Options) -> String {
    let mut painter = Painter {
        options,
        out: String::new(),
    };
    painter.summary(report);
    if report.has_baseline {
        painter.baseline(report);
    }
    painter.resources(report);
    painter.drivers(report);
    painter.findings(report);
    painter.assumptions(report);
    painter.unresolved(report);
    painter.unsupported(report);
    painter.list("Warnings", &report.warnings);
    painter.list("Errors", &report.errors);
    painter.footer(report);
    painter.out
}

/// Detailed derivation for the resources matching `address` (exact, or ignoring the instance key).
pub fn explain(report: &Report, address: &str, options: Options) -> Option<String> {
    let matches: Vec<&ChangeEstimate> = report
        .resources
        .iter()
        .filter(|change| change.address == address || crate::model::strip_index(&change.address) == address)
        .collect();
    if matches.is_empty() {
        return None;
    }

    let mut painter = Painter {
        options,
        out: String::new(),
    };
    for change in matches {
        painter.heading(&change.address);
        painter.pair("Type", &change.resource_type);
        painter.pair("Action", action_word(change.action));
        if let (Some(before), true) = (&change.before, report.has_baseline) {
            painter.side("Current state", before);
        }
        if let Some(after) = &change.after {
            painter.side(
                if report.has_baseline {
                    "Planned state"
                } else {
                    "Estimate"
                },
                after,
            );
        }
        if report.has_baseline {
            painter.pair("Monthly delta", &signed_money(change.delta));
        }
        painter.out.push('\n');
    }

    let findings: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| finding.resource == address || crate::model::strip_index(&finding.resource) == address)
        .collect();
    for finding in findings {
        painter.line(&format!(
            "{} {}  {}",
            severity_word(finding.severity),
            finding.rule_id,
            finding.title
        ));
        painter.wrapped(&finding.reason, 2);
        painter.wrapped(&format!("Recommendation: {}", finding.recommendation), 2);
    }
    painter.wrapped(&disclaimer(report), 0);
    Some(painter.out)
}

impl Painter {
    fn width(&self) -> usize {
        self.options.width.clamp(40, 100)
    }

    fn paint(&self, text: &str, style: Style) -> String {
        if !self.options.color {
            return text.to_string();
        }
        format!("{}{text}{}", style.render(), style.render_reset())
    }

    fn approx(&self) -> &'static str {
        if self.options.unicode { "≈" } else { "~" }
    }

    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn heading(&mut self, title: &str) {
        let rule = if self.options.unicode { "─" } else { "-" };
        let title = self.paint(title, Style::new().bold());
        let rule = rule.repeat(self.width());
        let _ = writeln!(self.out, "{title}\n{rule}");
    }

    fn section(&mut self, title: &str) {
        self.out.push('\n');
        self.heading(title);
    }

    fn pair(&mut self, label: &str, value: &str) {
        let _ = writeln!(self.out, "{label:<LABEL_WIDTH$}{value}");
    }

    /// Word-wraps `text` to the terminal width with a hanging indent.
    fn wrapped(&mut self, text: &str, indent: usize) {
        let limit = self.width().saturating_sub(indent).max(20);
        let mut line = String::new();
        for word in text.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > limit {
                let _ = writeln!(self.out, "{:indent$}{line}", "");
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            let _ = writeln!(self.out, "{:indent$}{line}", "");
        }
    }

    /// One row: flexible left text, right-aligned value column.
    fn row(&mut self, left: &str, right: &str) {
        let available = self.width().saturating_sub(right.chars().count() + 2);
        let left = truncate(left, available, self.options.unicode);
        let padding = self
            .width()
            .saturating_sub(left.chars().count() + right.chars().count());
        let _ = writeln!(self.out, "{left}{:padding$}{right}", "");
    }

    fn summary(&mut self, report: &Report) {
        self.heading("Cloud Cost Preflight");
        self.pair("Project", &report.project);
        self.pair("Detected", &or_none(&report.iac));
        self.pair(
            "Cloud",
            &or_none(
                &report
                    .clouds
                    .iter()
                    .map(|cloud| cloud.to_uppercase())
                    .collect::<Vec<_>>(),
            ),
        );
        self.pair("Region", &or_none(&report.regions));

        let counts = &report.counts;
        let mut breakdown = vec![format!("{} priced", counts.priced)];
        for (count, label) in [
            (counts.partially_priced, "partially priced"),
            (counts.unresolved, "unresolved"),
            (counts.unsupported, "unsupported"),
            (counts.no_direct_charge, "no direct charge"),
        ] {
            if count > 0 {
                breakdown.push(format!("{count} {label}"));
            }
        }
        self.pair("Resources", &format!("{} ({})", counts.total, breakdown.join(", ")));

        let published = report
            .pricing
            .iter()
            .map(|snapshot| {
                snapshot
                    .publication_date
                    .get(..10)
                    .unwrap_or(&snapshot.publication_date)
            })
            .max();
        let pricing = match published {
            Some(date) => format!("{}, published up to {date}", report.pricing_source),
            None => report.pricing_source.clone(),
        };
        self.pair("Pricing", &pricing);
        if let Some(profile) = &report.usage_profile {
            self.pair("Usage", &format!("profile: {profile}"));
        }

        let estimate = format!(
            "{} {} / month   ({} {} / year)",
            self.approx(),
            money(report.costs.after),
            self.approx(),
            money(report.costs.annual_after)
        );
        let estimate = self.paint(&estimate, Style::new().bold());
        self.pair("Estimate", &estimate);

        let excluded = counts.partially_priced + counts.unresolved + counts.unsupported;
        if excluded > 0 {
            let note = format!(
                "Not a complete total: {excluded} resource(s) could not be fully priced and are not counted as $0."
            );
            self.wrapped(&note, LABEL_WIDTH);
        }
        if let Some(budget) = &report.budget {
            let percent = (budget.used_fraction * Decimal::ONE_HUNDRED).round_dp(0);
            let (word, color) = if budget.exceeded {
                ("EXCEEDED", AnsiColor::Red)
            } else {
                ("within budget", AnsiColor::Green)
            };
            let status = self.paint(word, Style::new().fg_color(Some(color.into())));
            self.pair(
                "Budget",
                &format!("{} / month, {percent}% used, {status}", money(budget.monthly)),
            );
        }
    }

    fn baseline(&mut self, report: &Report) {
        self.section("Change vs current state");
        let approx = self.approx();
        let costs = &report.costs;
        self.row("New monthly cost", &format!("{approx} {}", money(costs.after)));
        self.row("Previous monthly cost", &format!("{approx} {}", money(costs.before)));

        let delta = match costs.delta_percent {
            Some(percent) => format!("{} ({:+}%)", signed_money(costs.delta), percent.round_dp(1)),
            None => signed_money(costs.delta),
        };
        let color = if costs.delta > Decimal::ZERO {
            AnsiColor::Red
        } else {
            AnsiColor::Green
        };
        let style = Style::new().fg_color(Some(color.into()));
        // Pad before painting so escape codes do not disturb the alignment.
        let width = self.width();
        for (label, value) in [
            ("Monthly delta", delta),
            ("Annualized delta", signed_money(costs.annual_delta)),
        ] {
            let padding = width.saturating_sub(label.len() + value.chars().count());
            let value = self.paint(&value, style);
            let _ = writeln!(self.out, "{label}{:padding$}{value}", "");
        }
    }

    fn resources(&mut self, report: &Report) {
        let mut rows: Vec<(&ChangeEstimate, Decimal)> = report
            .resources
            .iter()
            .filter_map(|change| {
                let shown = change.after.as_ref().or(change.before.as_ref())?;
                let billable = !matches!(
                    shown.status,
                    ResourceStatus::Unsupported | ResourceStatus::NoDirectCharge
                );
                billable.then(|| (change, shown.monthly.unwrap_or_default()))
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.address.cmp(&b.0.address)));

        self.section("Cost by resource");
        let limit = if self.options.verbose { rows.len() } else { MAX_ROWS };
        for (change, _) in rows.iter().take(limit) {
            let marker = if report.has_baseline {
                action_marker(change.action)
            } else {
                ""
            };
            let value = match (&change.after, &change.before) {
                (Some(after), _) => status_value(after),
                (None, Some(before)) => match before.monthly {
                    Some(monthly) => format!("removed, avoids {}", money(monthly)),
                    None => "removed".to_string(),
                },
                (None, None) => String::new(),
            };
            self.row(&format!("{marker}{}", change.address), &value);
        }
        if rows.len() > limit {
            self.line(&format!(
                "... and {} more (use --verbose to list all)",
                rows.len() - limit
            ));
        }
        if report.has_baseline {
            self.line("+ create   ~ update   - delete   -+ replace");
        }
    }

    fn drivers(&mut self, report: &Report) {
        let mut by_service: BTreeMap<&str, Decimal> = BTreeMap::new();
        for change in &report.resources {
            if let Some(ResourceEstimate {
                service,
                monthly: Some(monthly),
                ..
            }) = &change.after
            {
                *by_service.entry(service.as_str()).or_default() += *monthly;
            }
        }
        let mut drivers: Vec<_> = by_service
            .into_iter()
            .filter(|(_, monthly)| !monthly.is_zero())
            .collect();
        if drivers.len() < 2 {
            return;
        }
        drivers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

        self.section("Top cost drivers");
        for (service, monthly) in drivers.into_iter().take(5) {
            self.row(service, &money(monthly));
        }
    }

    fn findings(&mut self, report: &Report) {
        if report.findings.is_empty() {
            return;
        }
        self.section("FinOps findings");
        for finding in &report.findings {
            let color = match finding.severity {
                Severity::High => AnsiColor::Red,
                Severity::Medium => AnsiColor::Yellow,
                Severity::Low => AnsiColor::Cyan,
            };
            let severity = self.paint(
                severity_word(finding.severity),
                Style::new().fg_color(Some(color.into())),
            );
            let _ = writeln!(self.out, "{severity}  {}", finding.title);
            let impact = match finding.estimated_impact {
                Some(impact) => format!(", {} {} / month", self.approx(), money(impact)),
                None => String::new(),
            };
            self.wrapped(&format!("{} ({}{impact})", finding.resource, finding.rule_id), 6);
        }
    }

    fn assumptions(&mut self, report: &Report) {
        if report.assumptions.is_empty() {
            return;
        }
        // Many resources usually share one profile value; show each distinct value once.
        let mut grouped: BTreeMap<(String, &str, &str, &str), usize> = BTreeMap::new();
        for assumption in &report.assumptions {
            let service = report
                .resources
                .iter()
                .find(|change| change.address == assumption.scope)
                .and_then(|change| change.after.as_ref().or(change.before.as_ref()))
                .map_or_else(|| assumption.scope.clone(), |estimate| estimate.service.clone());
            *grouped
                .entry((
                    service,
                    assumption.key.as_str(),
                    assumption.value.as_str(),
                    assumption.source.as_str(),
                ))
                .or_default() += 1;
        }

        self.section("Usage assumptions");
        for ((service, key, value, source), count) in grouped {
            let resources = if count > 1 { format!(" x{count}") } else { String::new() };
            let left = if service.is_empty() {
                key.to_string()
            } else {
                format!("{service} {key}{resources}")
            };
            self.row(&left, &format!("{}  [{source}]", thousands(value)));
        }
        self.wrapped(
            "These volumes are not visible in infrastructure code. Replace them with measured values under `usage:` in the config file.",
            0,
        );
    }

    fn unresolved(&mut self, report: &Report) {
        let mut any = false;
        for change in &report.resources {
            let Some(estimate) = &change.after else {
                continue;
            };
            let unresolved: Vec<&ComponentEstimate> = estimate
                .components
                .iter()
                .filter(|component| matches!(component.outcome, Outcome::Unresolved { .. }))
                .collect();
            if unresolved.is_empty() {
                continue;
            }
            if !any {
                self.section("Not priced (excluded from the estimate, not counted as $0)");
                any = true;
            }
            self.line(&change.address);
            for component in unresolved {
                if let Outcome::Unresolved { reason } = &component.outcome {
                    self.wrapped(&format!("{}: {reason}", component.name), 4);
                }
            }
        }
    }

    fn unsupported(&mut self, report: &Report) {
        if report.unsupported_resources.is_empty() {
            return;
        }
        self.section(&format!(
            "Unsupported resources: {}",
            report.unsupported_resources.len()
        ));
        let limit = if self.options.verbose { usize::MAX } else { MAX_ROWS };
        for skipped in report.unsupported_resources.iter().take(limit) {
            self.line(&skipped.address);
        }
        if report.unsupported_resources.len() > limit {
            self.line(&format!("... and {} more", report.unsupported_resources.len() - limit));
        }
        self.wrapped(
            "Reason: no pricing adapter is available for these resource types yet. Their cost is not in the estimate.",
            0,
        );
    }

    fn list(&mut self, title: &str, items: &[String]) {
        if items.is_empty() {
            return;
        }
        self.section(title);
        for item in items {
            self.wrapped(item, 0);
        }
    }

    fn footer(&mut self, report: &Report) {
        self.out.push('\n');
        let color = match report.confidence {
            Confidence::High => AnsiColor::Green,
            Confidence::Medium => AnsiColor::Yellow,
            Confidence::Low => AnsiColor::Red,
        };
        let confidence = self.paint(
            &format!("{:?}", report.confidence).to_uppercase(),
            Style::new().bold().fg_color(Some(color.into())),
        );
        let _ = writeln!(self.out, "Estimate confidence: {confidence}");
        self.wrapped(&disclaimer(report), 0);

        let top = report
            .resources
            .iter()
            .filter_map(|change| Some((change, change.after.as_ref()?.monthly?)))
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.address.cmp(&a.0.address)));
        if let Some((change, _)) = top {
            let _ = writeln!(self.out, "\nRun: cloudpreflight explain '{}'", change.address);
        }
    }

    fn side(&mut self, title: &str, estimate: &ResourceEstimate) {
        let _ = writeln!(self.out, "\n{title}: {}", status_value(estimate));
        if let Some(region) = &estimate.region {
            self.pair("  Region", region);
        }
        for component in &estimate.components {
            match &component.outcome {
                Outcome::Priced { monthly, kind } => {
                    self.row(
                        &format!("  {}", component.name),
                        &format!("{}  [{}]", money(*monthly), kind_word(*kind)),
                    );
                    if let Some(formula) = &component.formula {
                        self.wrapped(formula, 6);
                    }
                    if let (Some(sku), true) = (&component.sku, self.options.verbose) {
                        let description = component.price_description.as_deref().unwrap_or_default();
                        self.wrapped(&format!("SKU {sku}: {description}"), 6);
                    }
                }
                Outcome::Unresolved { reason } => {
                    self.row(&format!("  {}", component.name), "not priced");
                    self.wrapped(reason, 6);
                }
            }
            for note in &component.notes {
                self.wrapped(note, 6);
            }
        }
        for note in &estimate.notes {
            self.wrapped(&format!("Note: {note}"), 2);
        }
    }
}

fn disclaimer(report: &Report) -> String {
    format!(
        "Based on {}. This is an estimate, not a bill: discounts, commitments, free tiers, taxes, support and data transfer are not included unless shown.",
        report.pricing_source
    )
}

fn status_value(estimate: &ResourceEstimate) -> String {
    match (estimate.status, estimate.monthly) {
        (ResourceStatus::Priced, Some(monthly)) => money(monthly),
        (ResourceStatus::PartiallyPriced, Some(monthly)) => format!("{} + unpriced parts", money(monthly)),
        (ResourceStatus::NoDirectCharge, _) => "no direct charge".to_string(),
        (ResourceStatus::Unsupported, _) => "unsupported".to_string(),
        _ => "not priced".to_string(),
    }
}

fn action_marker(action: Action) -> &'static str {
    match action {
        Action::Create => "+  ",
        Action::Update => "~  ",
        Action::Delete => "-  ",
        Action::Replace => "-+ ",
        Action::NoOp => "   ",
    }
}

fn action_word(action: Action) -> &'static str {
    match action {
        Action::Create => "create",
        Action::Update => "update",
        Action::Delete => "delete",
        Action::Replace => "replace",
        Action::NoOp => "no change",
    }
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "HIGH",
        Severity::Medium => "MED ",
        Severity::Low => "LOW ",
    }
}

fn kind_word(kind: PriceKind) -> &'static str {
    match kind {
        PriceKind::Exact => "exact list price",
        PriceKind::Estimated => "estimated",
        PriceKind::UsageAssumed => "usage assumed",
    }
}

fn or_none(items: &[String]) -> String {
    if items.is_empty() {
        return "none".to_string();
    }
    items.join(", ")
}

/// Shortens from the middle so both the module path and the resource name stay visible.
fn truncate(text: &str, max: usize, unicode: bool) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    let ellipsis = if unicode { "…" } else { "..." };
    let keep = max.saturating_sub(ellipsis.chars().count());
    let head = keep / 2;
    let tail = keep - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(length - tail).collect();
    format!("{start}{ellipsis}{end}")
}

fn thousands(value: &str) -> String {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (value, None),
    };
    if !whole.chars().all(|c| c.is_ascii_digit()) {
        return value.to_string();
    }
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match fraction {
        Some(fraction) => format!("{grouped}.{fraction}"),
        None => grouped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_both_ends_and_respects_the_limit() {
        let address = "module.platform.module.database.aws_db_instance.primary[0]";
        let short = truncate(address, 30, true);
        assert_eq!(short.chars().count(), 30);
        assert!(
            short.starts_with("module.platfor") && short.ends_with("primary[0]"),
            "{short}"
        );
        assert_eq!(truncate("short", 30, true), "short");
        assert!(truncate(address, 20, false).contains("..."));
    }

    #[test]
    fn thousands_groups_only_plain_numbers() {
        assert_eq!(thousands("10000000"), "10,000,000");
        assert_eq!(thousands("1234.5"), "1,234.5");
        assert_eq!(thousands("abc"), "abc");
    }
}
