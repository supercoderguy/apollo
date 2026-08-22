//! Unit file loading and start-order resolution.

use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    OnFailure,
}

/// A parsed `*.toml` unit file, e.g.:
///
/// ```toml
/// name = "sshd"
/// exec = ["/usr/sbin/sshd", "-D"]
/// restart = "on-failure"
/// after = ["network.target"]
/// working-dir = "/etc/sv/sshd"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct UnitConfig {
    pub name: String,
    pub exec: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory to `chdir` into before `exec`. Unset by default
    /// (inherits apollod's own cwd, as always) — mainly needed for units
    /// whose command relies on relative paths, e.g. a runit `run` script
    /// referencing `./env` or `./auto`: `runsv` always chdirs into the
    /// service's own directory before running it, apollod doesn't do
    /// anything like that on its own, so `apollo-import runit` sets this
    /// explicitly to match.
    #[serde(default, rename = "working-dir")]
    pub working_dir: Option<PathBuf>,
}

/// Load every `*.toml` file in `dir` as a unit definition. Returns units
/// sorted by name for a deterministic starting point (actual start order
/// comes from [`resolve_start_order`]).
pub fn load_units(dir: &Path) -> anyhow::Result<Vec<UnitConfig>> {
    let mut units = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let unit: UnitConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        units.push(unit);
    }
    units.sort_by(|a, b| a.name.cmp(&b.name));

    let mut seen = HashSet::new();
    for u in &units {
        if !seen.insert(u.name.as_str()) {
            bail!("duplicate unit name '{}' across service files", u.name);
        }
    }
    Ok(units)
}

/// Topologically sort units by their `after` dependencies, so a unit is
/// only started once everything it lists in `after` has been started.
///
/// Names in `after` that don't match a loaded unit (e.g. `network.target`,
/// which apollo doesn't model yet) are silently ignored — they're treated
/// as already satisfied. This is a deliberate simplification for now.
pub fn resolve_start_order(units: &[UnitConfig]) -> anyhow::Result<Vec<String>> {
    let names: HashSet<&str> = units.iter().map(|u| u.name.as_str()).collect();

    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for u in units {
        in_degree.entry(u.name.as_str()).or_insert(0);
        for dep in &u.after {
            if !names.contains(dep.as_str()) {
                continue;
            }
            *in_degree.entry(u.name.as_str()).or_insert(0) += 1;
            dependents.entry(dep.as_str()).or_default().push(&u.name);
        }
    }

    let mut queue: VecDeque<&str> = units
        .iter()
        .map(|u| u.name.as_str())
        .filter(|n| in_degree.get(n).copied().unwrap_or(0) == 0)
        .collect();

    let mut order = Vec::with_capacity(units.len());
    while let Some(n) = queue.pop_front() {
        order.push(n.to_string());
        if let Some(deps) = dependents.get(n) {
            for &d in deps {
                let entry = in_degree
                    .get_mut(d)
                    .expect("dependent was registered above");
                *entry -= 1;
                if *entry == 0 {
                    queue.push_back(d);
                }
            }
        }
    }

    if order.len() != units.len() {
        bail!("dependency cycle detected among service units (check `after` fields)");
    }
    Ok(order)
}
