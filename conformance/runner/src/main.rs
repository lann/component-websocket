//! The conformance runner: loads the test registry (`tests.toml`), the
//! per-target facts (`targets.toml`), and every adapter result document
//! (`results/*.json`), classifies each cell, renders the markdown matrix,
//! and exits nonzero on any `FAIL`, `UNEXPECTED-PASS`, or undeclared skip.
//!
//! The runner asserts only that the mail arrived intact: adapters own the
//! running, the guest owns the assertions, and this binary owns the
//! bookkeeping — unregistered ids, undeclared targets, duplicate reports,
//! and divergence-without-artifact are its errors to raise.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{bail, ensure, Context as _, Result};
use serde::Deserialize;

// ----- registry (tests.toml) -------------------------------------------------

#[derive(Debug, Deserialize)]
struct Registry {
    #[serde(rename = "test")]
    tests: Vec<TestEntry>,
}

#[derive(Debug, Deserialize)]
struct TestEntry {
    id: String,
    tags: Vec<String>,
    #[allow(dead_code, reason = "for humans reading the registry")]
    description: String,
}

fn load_registry(text: &str) -> Result<Registry> {
    let registry: Registry = toml::from_str(text).context("parse tests.toml")?;
    let mut seen = BTreeSet::new();
    for test in &registry.tests {
        ensure!(seen.insert(&test.id), "duplicate test id {:?}", test.id);
    }
    Ok(registry)
}

// ----- target facts (targets.toml) --------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TargetFacts {
    #[serde(default)]
    unsupported: Vec<Unsupported>,
    #[serde(default, rename = "expected-fail")]
    expected_fail: Vec<ExpectedFail>,
}

#[derive(Debug, Deserialize)]
struct Unsupported {
    tag: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedFail {
    test: String,
    reason: String,
    tracking: String,
}

#[derive(Debug, Deserialize)]
struct TargetsFile {
    target: BTreeMap<String, TargetFacts>,
}

fn load_targets(text: &str, registry: &Registry) -> Result<BTreeMap<String, TargetFacts>> {
    let file: TargetsFile = toml::from_str(text).context("parse targets.toml")?;
    let known_tags: BTreeSet<&str> = registry
        .tests
        .iter()
        .flat_map(|t| t.tags.iter().map(String::as_str))
        .collect();
    let known_ids: BTreeSet<&str> = registry.tests.iter().map(|t| t.id.as_str()).collect();
    for (target, facts) in &file.target {
        for unsupported in &facts.unsupported {
            ensure!(
                known_tags.contains(unsupported.tag.as_str()),
                "target {target}: unsupported tag {:?} matches no registered tag",
                unsupported.tag
            );
            ensure!(
                !unsupported.reason.trim().is_empty(),
                "target {target}: unsupported tag {:?} needs a reason",
                unsupported.tag
            );
        }
        for expected in &facts.expected_fail {
            ensure!(
                known_ids.contains(expected.test.as_str()),
                "target {target}: expected-fail test {:?} is not registered",
                expected.test
            );
            ensure!(
                !expected.reason.trim().is_empty(),
                "target {target}: expected-fail {:?} needs a reason",
                expected.test
            );
            ensure!(
                !expected.tracking.trim().is_empty(),
                "target {target}: expected-fail {:?} needs a tracking issue",
                expected.test
            );
        }
    }
    Ok(file.target)
}

// ----- adapter reports ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AdapterReport {
    target: String,
    environment: String,
    /// The sha256 of the guest component the run executed; empty when the
    /// adapter could not determine it.
    #[serde(default)]
    guest: String,
    results: Vec<RawResult>,
}

#[derive(Debug, Deserialize)]
struct RawResult {
    test_id: String,
    status: RawStatus,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawStatus {
    Pass,
    Fail,
    Skip,
}

// ----- classification ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    Pass,
    Fail(String),
    SkipUnsupported(String),
    /// A skip the target facts do not justify: an undeclared divergence.
    UndeclaredSkip(String),
    ExpectedFail(String),
    UnexpectedPass(String),
    Missing,
    /// The whole row has no report (a declared target that did not run).
    NotRun,
}

impl Cell {
    fn label(&self) -> &'static str {
        match self {
            Cell::Pass => "PASS",
            Cell::Fail(_) => "FAIL",
            Cell::SkipUnsupported(_) => "SKIP-UNSUPPORTED",
            Cell::UndeclaredSkip(_) => "UNDECLARED-SKIP",
            Cell::ExpectedFail(_) => "XFAIL",
            Cell::UnexpectedPass(_) => "UNEXPECTED-PASS",
            Cell::Missing => "MISSING",
            Cell::NotRun => "—",
        }
    }

    /// Whether this cell fails the run.
    fn is_error(&self) -> bool {
        matches!(
            self,
            Cell::Fail(_) | Cell::UndeclaredSkip(_) | Cell::UnexpectedPass(_)
        )
    }
}

#[derive(Debug)]
struct Row {
    target: String,
    environment: String,
    cells: Vec<(String, Cell)>,
}

fn classify(
    registry: &Registry,
    targets: &BTreeMap<String, TargetFacts>,
    reports: &[AdapterReport],
) -> Result<Vec<Row>> {
    // Transport validation first. Mixed provenance is never classifiable:
    // two reports from different guest builds are not one matrix.
    let stamps: BTreeSet<&str> = reports
        .iter()
        .map(|r| r.guest.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    ensure!(
        stamps.len() <= 1,
        "results were produced from different guest builds ({} distinct stamps); \
         re-run the full suite",
        stamps.len()
    );
    let mut seen_rows = BTreeSet::new();
    for report in reports {
        ensure!(
            targets.contains_key(&report.target),
            "results for undeclared target {:?} (declare it in targets.toml)",
            report.target
        );
        ensure!(
            seen_rows.insert((report.target.clone(), report.environment.clone())),
            "duplicate results for target {:?} environment {:?}",
            report.target,
            report.environment
        );
        let mut seen_ids = BTreeSet::new();
        for result in &report.results {
            ensure!(
                registry.tests.iter().any(|t| t.id == result.test_id),
                "target {:?} reports unregistered test id {:?}",
                report.target,
                result.test_id
            );
            ensure!(
                seen_ids.insert(&result.test_id),
                "target {:?} reports test {:?} twice",
                report.target,
                result.test_id
            );
        }
    }

    let mut rows = Vec::new();
    for (target, facts) in targets {
        let target_reports: Vec<&AdapterReport> =
            reports.iter().filter(|r| &r.target == target).collect();
        if target_reports.is_empty() {
            // Planning-only row: the target is declared but did not report.
            rows.push(Row {
                target: target.clone(),
                environment: String::new(),
                cells: registry
                    .tests
                    .iter()
                    .map(|t| (t.id.clone(), Cell::NotRun))
                    .collect(),
            });
            continue;
        }
        for report in target_reports {
            let by_id: BTreeMap<&str, &RawResult> = report
                .results
                .iter()
                .map(|r| (r.test_id.as_str(), r))
                .collect();
            let cells = registry
                .tests
                .iter()
                .map(|test| {
                    let expected_fail = facts.expected_fail.iter().find(|e| e.test == test.id);
                    let cell = match by_id.get(test.id.as_str()) {
                        None => Cell::Missing,
                        Some(result) => {
                            let detail = result.detail.clone().unwrap_or_default();
                            match result.status {
                                RawStatus::Pass => match expected_fail {
                                    Some(expected) => Cell::UnexpectedPass(format!(
                                        "declared expected-fail ({}) but passed; \
                                         remove the declaration ({})",
                                        expected.reason, expected.tracking
                                    )),
                                    None => Cell::Pass,
                                },
                                RawStatus::Fail => match expected_fail {
                                    Some(expected) => Cell::ExpectedFail(format!(
                                        "{} ({})",
                                        expected.reason, expected.tracking
                                    )),
                                    None => Cell::Fail(detail),
                                },
                                RawStatus::Skip => {
                                    let justified = facts
                                        .unsupported
                                        .iter()
                                        .find(|u| test.tags.contains(&u.tag));
                                    match justified {
                                        Some(unsupported) => {
                                            Cell::SkipUnsupported(unsupported.reason.clone())
                                        }
                                        None => Cell::UndeclaredSkip(detail),
                                    }
                                }
                            }
                        }
                    };
                    (test.id.clone(), cell)
                })
                .collect();
            rows.push(Row {
                target: target.clone(),
                environment: report.environment.clone(),
                cells,
            });
        }
    }
    Ok(rows)
}

// ----- rendering ----------------------------------------------------------------

fn render_matrix(registry: &Registry, rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str("# Conformance matrix\n\n");
    out.push_str("Legend: PASS · FAIL · SKIP-UNSUPPORTED (declared in targets.toml) · ");
    out.push_str("XFAIL (declared expected-fail) · UNEXPECTED-PASS (stale declaration) · ");
    out.push_str("UNDECLARED-SKIP (divergence without artifact) · MISSING (no result) · ");
    out.push_str("— (target did not report)\n\n");
    out.push_str("| target | environment |");
    for test in &registry.tests {
        out.push_str(&format!(" {} |", test.id));
    }
    out.push('\n');
    out.push_str("| --- | --- |");
    for _ in &registry.tests {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in rows {
        out.push_str(&format!("| {} | {} |", row.target, row.environment));
        for (_, cell) in &row.cells {
            out.push_str(&format!(" {} |", cell.label()));
        }
        out.push('\n');
    }

    let mut details = String::new();
    for row in rows {
        for (id, cell) in &row.cells {
            let entry = match cell {
                Cell::Fail(detail) => Some(("FAIL", detail.clone())),
                Cell::UndeclaredSkip(detail) => Some(("UNDECLARED-SKIP", detail.clone())),
                Cell::UnexpectedPass(detail) => Some(("UNEXPECTED-PASS", detail.clone())),
                Cell::ExpectedFail(detail) => Some(("XFAIL", detail.clone())),
                Cell::SkipUnsupported(detail) => Some(("SKIP-UNSUPPORTED", detail.clone())),
                _ => None,
            };
            if let Some((label, detail)) = entry {
                details.push_str(&format!(
                    "- `{}` / `{}` ({}): {} — {}\n",
                    row.target,
                    id,
                    row.environment,
                    label,
                    if detail.is_empty() {
                        "(no detail)"
                    } else {
                        &detail
                    }
                ));
            }
        }
    }
    if !details.is_empty() {
        out.push_str("\n## Details\n\n");
        out.push_str(&details);
    }
    out
}

// ----- main ---------------------------------------------------------------------

struct Cli {
    tests: PathBuf,
    targets: PathBuf,
    results: PathBuf,
    matrix_out: Option<PathBuf>,
    /// Promote incompleteness (missing cells, targets that did not report,
    /// unstamped reports) from warnings to errors: the gate for the full
    /// `all`/CI runs, where a partial matrix means something upstream
    /// silently dropped work.
    require_complete: bool,
}

fn parse_cli() -> Result<Cli> {
    let mut tests = PathBuf::from("conformance/tests.toml");
    let mut targets = PathBuf::from("conformance/targets.toml");
    let mut results = PathBuf::from("conformance/results");
    let mut matrix_out = None;
    let mut require_complete = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--tests" => tests = PathBuf::from(value("--tests")?),
            "--targets" => targets = PathBuf::from(value("--targets")?),
            "--results" => results = PathBuf::from(value("--results")?),
            "--matrix-out" => matrix_out = Some(PathBuf::from(value("--matrix-out")?)),
            "--require-complete" => require_complete = true,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Cli {
        tests,
        targets,
        results,
        matrix_out,
        require_complete,
    })
}

fn main() -> Result<()> {
    let cli = parse_cli()?;
    let registry = load_registry(
        &std::fs::read_to_string(&cli.tests)
            .with_context(|| format!("read {}", cli.tests.display()))?,
    )?;
    let targets = load_targets(
        &std::fs::read_to_string(&cli.targets)
            .with_context(|| format!("read {}", cli.targets.display()))?,
        &registry,
    )?;

    let mut reports = Vec::new();
    for entry in std::fs::read_dir(&cli.results)
        .with_context(|| format!("read results dir {}", cli.results.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = std::fs::read_to_string(&path)?;
            let report: AdapterReport =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            reports.push(report);
        }
    }
    ensure!(
        !reports.is_empty(),
        "no result documents found in {}",
        cli.results.display()
    );

    let rows = classify(&registry, &targets, &reports)?;
    let matrix = render_matrix(&registry, &rows);
    match &cli.matrix_out {
        Some(path) => {
            std::fs::write(path, &matrix)?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{matrix}"),
    }

    let mut errors = 0usize;
    // Incompleteness — a declared target that did not report, a reported
    // target missing cells, or an unstamped report — is an error under
    // `--require-complete` (the full-run/CI gate) and a warning otherwise
    // (partial `--only` iteration).
    let mut incomplete: Vec<String> = Vec::new();
    for report in reports.iter().filter(|r| r.guest.is_empty()) {
        incomplete.push(format!(
            "target {:?} report carries no guest stamp",
            report.target
        ));
    }
    for row in &rows {
        if row.environment.is_empty() {
            incomplete.push(format!(
                "target {:?} declared but did not report",
                row.target
            ));
        }
        for (id, cell) in &row.cells {
            if matches!(cell, Cell::Missing) {
                incomplete.push(format!(
                    "target {:?} reported no result for {:?}",
                    row.target, id
                ));
            }
        }
    }
    for message in &incomplete {
        if cli.require_complete {
            errors += 1;
            eprintln!("error: {message}");
        } else {
            eprintln!("warning: {message}");
        }
    }
    for row in &rows {
        for (id, cell) in &row.cells {
            if cell.is_error() {
                errors += 1;
                eprintln!("error: {} / {}: {}", row.target, id, cell.label());
            }
        }
    }
    ensure!(errors == 0, "{errors} failing cell(s)");
    eprintln!("conformance: all rows clean");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        load_registry(
            r#"
            [[test]]
            id = "a"
            tags = ["x"]
            description = "a"
            [[test]]
            id = "b"
            tags = ["y"]
            description = "b"
            "#,
        )
        .unwrap()
    }

    fn report(target: &str, results: &[(&str, RawStatus)]) -> AdapterReport {
        report_stamped(target, "stamp-a", results)
    }

    fn report_stamped(target: &str, guest: &str, results: &[(&str, RawStatus)]) -> AdapterReport {
        AdapterReport {
            target: target.into(),
            environment: "loopback".into(),
            guest: guest.into(),
            results: results
                .iter()
                .map(|(id, status)| RawResult {
                    test_id: (*id).into(),
                    status: *status,
                    detail: None,
                })
                .collect(),
        }
    }

    #[test]
    fn mixed_guest_stamps_are_rejected() {
        let registry = registry();
        let targets = load_targets("[target.t]\n[target.u]", &registry).unwrap();
        let err = classify(
            &registry,
            &targets,
            &[
                report_stamped(
                    "t",
                    "stamp-a",
                    &[("a", RawStatus::Pass), ("b", RawStatus::Pass)],
                ),
                report_stamped(
                    "u",
                    "stamp-b",
                    &[("a", RawStatus::Pass), ("b", RawStatus::Pass)],
                ),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("different guest builds"));
    }

    #[test]
    fn unstamped_reports_classify_when_alone() {
        // A stampless report (an adapter that could not determine the
        // build) still classifies; --require-complete rejects it at the
        // completeness stage instead.
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report_stamped(
                "t",
                "",
                &[("a", RawStatus::Pass), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn checked_in_registry_and_targets_load() {
        let registry = load_registry(include_str!("../../tests.toml")).unwrap();
        load_targets(include_str!("../../targets.toml"), &registry).unwrap();
        assert!(registry.tests.len() >= 30);
    }

    #[test]
    fn pass_and_fail_classify() {
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Pass), ("b", RawStatus::Fail)],
            )],
        )
        .unwrap();
        assert_eq!(rows[0].cells[0].1, Cell::Pass);
        assert!(matches!(rows[0].cells[1].1, Cell::Fail(_)));
    }

    #[test]
    fn undeclared_skip_is_an_error() {
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Skip), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert!(matches!(rows[0].cells[0].1, Cell::UndeclaredSkip(_)));
        assert!(rows[0].cells[0].1.is_error());
    }

    #[test]
    fn declared_skip_is_unsupported() {
        let registry = registry();
        let targets = load_targets(
            r#"
            [target.t]
            [[target.t.unsupported]]
            tag = "x"
            reason = "platform cannot"
            "#,
            &registry,
        )
        .unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Skip), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert!(matches!(rows[0].cells[0].1, Cell::SkipUnsupported(_)));
        assert!(!rows[0].cells[0].1.is_error());
    }

    #[test]
    fn expected_fail_and_unexpected_pass() {
        let registry = registry();
        let targets = load_targets(
            r#"
            [target.t]
            [[target.t.expected-fail]]
            test = "a"
            reason = "known"
            tracking = "https://example.test/1"
            "#,
            &registry,
        )
        .unwrap();
        let failing = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Fail), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert!(matches!(failing[0].cells[0].1, Cell::ExpectedFail(_)));
        assert!(!failing[0].cells[0].1.is_error());
        let passing = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Pass), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert!(matches!(passing[0].cells[0].1, Cell::UnexpectedPass(_)));
        assert!(passing[0].cells[0].1.is_error());
    }

    #[test]
    fn undeclared_target_is_rejected() {
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let err = classify(
            &registry,
            &targets,
            &[report("nope", &[("a", RawStatus::Pass)])],
        )
        .unwrap_err();
        assert!(err.to_string().contains("undeclared target"));
    }

    #[test]
    fn unregistered_id_is_rejected() {
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let err = classify(
            &registry,
            &targets,
            &[report("t", &[("zzz", RawStatus::Pass)])],
        )
        .unwrap_err();
        assert!(err.to_string().contains("unregistered test id"));
    }

    #[test]
    fn missing_report_is_a_planning_row() {
        let registry = registry();
        let targets = load_targets("[target.t]\n[target.u]", &registry).unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report(
                "t",
                &[("a", RawStatus::Pass), ("b", RawStatus::Pass)],
            )],
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        let planning = rows.iter().find(|r| r.target == "u").unwrap();
        assert!(planning.cells.iter().all(|(_, c)| *c == Cell::NotRun));
    }

    #[test]
    fn missing_cell_is_flagged_not_fatal() {
        let registry = registry();
        let targets = load_targets("[target.t]", &registry).unwrap();
        let rows = classify(
            &registry,
            &targets,
            &[report("t", &[("a", RawStatus::Pass)])],
        )
        .unwrap();
        assert_eq!(rows[0].cells[1].1, Cell::Missing);
        assert!(!rows[0].cells[1].1.is_error());
    }

    #[test]
    fn facts_validation_catches_typos() {
        let registry = registry();
        assert!(load_targets(
            r#"
            [target.t]
            [[target.t.unsupported]]
            tag = "nope"
            reason = "r"
            "#,
            &registry
        )
        .is_err());
        assert!(load_targets(
            r#"
            [target.t]
            [[target.t.expected-fail]]
            test = "a"
            reason = "r"
            tracking = ""
            "#,
            &registry
        )
        .is_err());
    }
}
