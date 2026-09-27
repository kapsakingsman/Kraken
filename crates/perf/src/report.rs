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
            status: check(value, budget, real_gpu),
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
        let verdict = if self.failed() { "FAIL" } else { "PASS" };
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
