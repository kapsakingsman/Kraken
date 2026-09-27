//! Performance budgets from `perf/budgets.toml`, and checking measurements against them.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max: Option<f64>,
    pub min: Option<f64>,
    /// `true` for frame timing, which only means something on a real GPU. On CI machines
    /// (software rendering) such budgets are reported but not enforced.
    #[serde(default)]
    pub real_gpu_only: bool,
}

/// Budgets by metric name, e.g. `app.scroll.missed_frames_pct`.
pub struct Budgets(BTreeMap<String, Budget>);

impl Budgets {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let table: toml::Table = text.parse()?;
        let mut budgets = BTreeMap::new();
        flatten(&table, "", &mut budgets)?;
        Ok(Budgets(budgets))
    }

    pub fn get(&self, metric: &str) -> Option<Budget> {
        self.0.get(metric).copied()
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

/// Tables whose values contain `max`/`min` are budgets; other tables are name prefixes.
fn flatten(table: &toml::Table, prefix: &str, out: &mut BTreeMap<String, Budget>) -> Result<()> {
    for (key, value) in table {
        let name = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let Some(inner) = value.as_table() else {
            bail!("{name}: expected a table like {{ max = 10 }}");
        };
        if inner.contains_key("max") || inner.contains_key("min") {
            let budget: Budget = value.clone().try_into().with_context(|| name.clone())?;
            out.insert(name, budget);
        } else {
            flatten(inner, &name, out)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    /// Measured and compared, but not enforced here (frame timing without a real GPU).
    Info,
    /// No budget for this metric.
    None,
    /// Over budget, but the run used a document the budgets were not made for.
    Over,
}

pub fn check(value: f64, budget: Option<Budget>, real_gpu: bool) -> Status {
    let Some(budget) = budget else {
        return Status::None;
    };
    let within =
        budget.max.is_none_or(|max| value <= max) && budget.min.is_none_or(|min| value >= min);
    match (within, budget.real_gpu_only && !real_gpu) {
        (_, true) => Status::Info,
        (true, false) => Status::Pass,
        (false, false) => Status::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        [engine]
        tile_p95_ms = { max = 15 }
        pages_per_s = { min = 30 }

        [app.scroll]
        missed_frames_pct = { max = 1.0, real_gpu_only = true }
    "#;

    #[test]
    fn nested_tables_become_dotted_names() {
        let budgets = Budgets::parse(EXAMPLE).unwrap();
        let names: Vec<&String> = budgets.names().collect();
        assert_eq!(
            names,
            [
                "app.scroll.missed_frames_pct",
                "engine.pages_per_s",
                "engine.tile_p95_ms"
            ]
        );
        assert_eq!(budgets.get("engine.tile_p95_ms").unwrap().max, Some(15.0));
    }

    #[test]
    fn checks_limits() {
        let budgets = Budgets::parse(EXAMPLE).unwrap();
        let tile = budgets.get("engine.tile_p95_ms");
        assert_eq!(check(14.0, tile, true), Status::Pass);
        assert_eq!(check(16.0, tile, true), Status::Fail);
        let rate = budgets.get("engine.pages_per_s");
        assert_eq!(check(20.0, rate, true), Status::Fail);
        assert_eq!(check(20.0, None, true), Status::None);
    }

    #[test]
    fn frame_budgets_are_informational_without_a_real_gpu() {
        let budgets = Budgets::parse(EXAMPLE).unwrap();
        let missed = budgets.get("app.scroll.missed_frames_pct");
        assert_eq!(check(30.0, missed, false), Status::Info);
        assert_eq!(check(30.0, missed, true), Status::Fail);
    }

    #[test]
    fn rejects_typos() {
        assert!(Budgets::parse("[engine]\ntile = { maxx = 3 }").is_err());
        assert!(Budgets::parse("[engine]\ntile = { max = 3, oops = 1 }").is_err());
    }
}
