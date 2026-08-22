//! Converts runit service directories into apollo unit files.
//!
//! A runit service is a directory holding at least an executable `run`
//! script (the standard layout under e.g. Void Linux's `/etc/sv/`) —
//! `runsv` execs that script directly and restarts it whenever it exits.
//! Since the script already has its own shebang and already does whatever
//! privilege-dropping/env-loading/redirection it needs internally (via
//! `chpst`, `envdir`, `setuidgid`, ...), the generated unit just execs the
//! same script directly, the same way `runsv` does — nothing about that
//! internal logic needs to be understood or reimplemented here.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub struct Summary {
    /// Names of services successfully converted.
    pub imported: Vec<String>,
    /// (name, reason) for services that couldn't be converted at all.
    pub skipped: Vec<(String, String)>,
    /// (name, note) for services converted but with something runit-side
    /// that has no apollo equivalent yet — worth the user's attention.
    pub notes: Vec<(String, String)>,
}

/// Converts every service directory directly under `src` into a `*.toml`
/// unit file under `dest` (created if missing). `src` should be the
/// real, final location of the service directories — the generated units
/// point straight at each one's `run` script there; nothing is copied.
pub fn convert(src: &Path, dest: &Path, force: bool) -> Result<Summary> {
    fs::create_dir_all(dest)
        .with_context(|| format!("creating destination directory {}", dest.display()))?;

    let mut summary = Summary {
        imported: Vec::new(),
        skipped: Vec::new(),
        notes: Vec::new(),
    };

    let mut entries: Vec<_> = fs::read_dir(src)
        .with_context(|| format!("reading runit service directory {}", src.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("reading entries of {}", src.display()))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            summary.skipped.push((
                path.display().to_string(),
                "non-UTF-8 directory name".into(),
            ));
            continue;
        };
        let name = name.to_string();

        if let Some(reason) = convert_one(&path, &name, dest, force, &mut summary)? {
            summary.skipped.push((name, reason));
        } else {
            summary.imported.push(name);
        }
    }

    Ok(summary)
}

/// Returns `Ok(Some(reason))` if this service was skipped, `Ok(None)` if
/// it was imported (having already pushed any notes onto `summary`).
fn convert_one(
    path: &Path,
    name: &str,
    dest: &Path,
    force: bool,
    summary: &mut Summary,
) -> Result<Option<String>> {
    let run_path = path.join("run");
    if !run_path.is_file() {
        return Ok(Some("no 'run' file".into()));
    }
    let mode = fs::metadata(&run_path)
        .with_context(|| format!("statting {}", run_path.display()))?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        return Ok(Some("'run' file isn't executable".into()));
    }

    let dest_file = dest.join(format!("{name}.toml"));
    if dest_file.exists() && !force {
        return Ok(Some(format!(
            "{} already exists (use --force to overwrite)",
            dest_file.display()
        )));
    }

    let run_abs = std::path::absolute(&run_path)
        .with_context(|| format!("resolving absolute path for {}", run_path.display()))?;

    let mut note_bits = Vec::new();
    if path.join("down").exists() {
        note_bits.push(
            "had a 'down' file (started disabled under runit) — apollo has no equivalent \
             yet, so this unit WILL auto-start; remove the generated unit file if that's \
             not wanted"
                .to_string(),
        );
    }
    if path.join("log").is_dir() {
        note_bits.push(
            "had a 'log/' service — runit piped its output there; apollo has no log \
             capture yet (see README roadmap), so this unit's output now just inherits \
             apollod's own console instead"
                .to_string(),
        );
    }
    if path.join("finish").exists() {
        note_bits.push(
            "had a 'finish' script — runit ran this on exit for cleanup; apollo has no \
             equivalent hook, it was not migrated"
                .to_string(),
        );
    }
    for n in &note_bits {
        summary.notes.push((name.to_string(), n.clone()));
    }

    let mut toml = String::new();
    toml.push_str(&format!(
        "# Imported from the runit service '{name}' at {}.\n",
        path.display()
    ));
    for n in &note_bits {
        toml.push_str(&format!("# NOTE: {n}\n"));
    }
    toml.push_str(&format!("name = {}\n", toml_string(name)));
    // Exec the run script directly (no `sh -c` wrapper) — same as runsv
    // itself does, relying on the script's own shebang line.
    toml.push_str(&format!(
        "exec = [{}]\n",
        toml_string(&run_abs.display().to_string())
    ));
    // runsv restarts a service's run script whenever it exits, forever, by
    // default — runit has no native one-shot concept to map to apollo's
    // "no"/"on-failure", so "always" is the faithful default here; edit by
    // hand afterward for any service that's actually meant to run once.
    toml.push_str("restart = \"always\"\n");

    fs::write(&dest_file, toml).with_context(|| format!("writing {}", dest_file.display()))?;

    Ok(None)
}

/// Minimal TOML basic-string escaping — good enough for filesystem paths
/// and service names, not a general-purpose TOML writer.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
