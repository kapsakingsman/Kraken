//! Collected measurements, and the JSON and Markdown reports.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::budgets::{Budget, Budgets, Status, check};

#[derive(Clone, Debug, Serialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    pub unit: &'static str,
    pub budget: Option<Budget>,
    pub status: Status,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub metrics: Vec<Metric>,
    /// Problems that stopped a scenario from producing numbers.
    pub errors: Vec<String>,
    pub notes: Vec<String>,
    /// Budgets are shown but not enforced: the run used the caller's own document, and
    /// the budgets are set for the generated fixtures.
    pub advisory: bool,
}

impl Report {
    pub fn add(
        &mut self,
        budgets: &Budgets,
        real_gpu: bool,
        name: &str,
        value: f64,
        unit: &'static str,
    ) {
        let budget = budgets.get(name);
        self.metrics.push(Metric {
            name: name.to_owned(),
            value,
            unit,
            budget,
            status: match check(value, budget, real_gpu) {
                Status::Fail if self.advisory => Status::Over,
                status => status,
            },
        });
    }

    pub fn failed(&self) -> bool {
        !self.errors.is_empty() || self.metrics.iter().any(|m| m.status == Status::Fail)
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("report.json"), serde_json::to_string_pretty(self)?)?;
        std::fs::write(dir.join("report.md"), self.markdown())?;
        Ok(())
    }

    pub fn markdown(&self) -> String {
        let mut md = String::from("# Performance report\n\n");
        let verdict = match (self.failed(), self.advisory) {
            (true, _) => "FAIL",
            // Nothing failed, but the budgets were not enforced either.
            (false, true) => "PASS (advisory: budgets shown, not enforced)",
            (false, false) => "PASS",
        };
        let _ = writeln!(md, "**Result: {verdict}**\n");
        md.push_str("| Metric | Value | Budget | Status |\n|---|---:|---|---|\n");
        for m in &self.metrics {
            let budget = match m.budget {
                Some(Budget { max: Some(max), .. }) => format!("≤ {max} {}", m.unit),
                Some(Budget { min: Some(min), .. }) => format!("≥ {min} {}", m.unit),
                _ => String::new(),
            };
            let status = match m.status {
                Status::Pass => "pass",
                Status::Fail => "**FAIL**",
                Status::Info => "info (needs real GPU)",
                Status::Over => "over (not enforced for --pdf)",
                Status::None => "",
            };
            let _ = writeln!(
                md,
                "| {} | {:.2} {} | {budget} | {status} |",
                m.name, m.value, m.unit
            );
        }
        for (title, list) in [("Errors", &self.errors), ("Notes", &self.notes)] {
            if !list.is_empty() {
                let _ = writeln!(md, "\n## {title}\n");
                for item in list {
                    let _ = writeln!(md, "- {item}");
                }
            }
        }
        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_budget_fails_unless_the_run_is_advisory() {
        let budgets = Budgets::parse("[app.x]\nms = { max = 10 }\n").unwrap();
        let mut report = Report::default();
        report.add(&budgets, true, "app.x.ms", 20.0, "ms");
        assert!(report.failed());

        let mut advisory = Report {
            advisory: true,
            ..Report::default()
        };
        advisory.add(&budgets, true, "app.x.ms", 20.0, "ms");
        assert_eq!(advisory.metrics[0].status, Status::Over);
        assert!(!advisory.failed());
        assert!(advisory.markdown().contains("over (not enforced"));
        assert!(advisory.markdown().contains("Result: PASS (advisory"));
    }
}
