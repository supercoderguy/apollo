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
    /// Paths of pre-existing `*.toml` files removed from `dest` up front
    /// because `clean` was set.
    pub cleaned: Vec<String>,
}

/// Converts every service directory directly under `src` into a `*.toml`
/// unit file under `dest` (created if missing). `src` should be the
/// real, final location of the service directories — the generated units
/// point straight at each one's `run` script there; nothing is copied.
///
/// A service with a `down` file is skipped by default rather than
/// converted — see [`convert_one`] for why — unless `include_down` is set.
///
/// If `clean` is set, every existing `*.toml` file directly under `dest`
/// is removed *before* conversion starts — otherwise a service dropped
/// from this run (e.g. one now correctly skipped for having a `down`
/// file that an earlier, less careful import converted anyway) leaves
/// its old generated unit behind, still there and still auto-starting,
/// same as any other unit apollod finds in its config directory. `clean`
/// has no way to tell an apollo-import-generated file apart from one
/// placed in `dest` by hand — it removes every `*.toml` there.
pub fn convert(
    src: &Path,
    dest: &Path,
    force: bool,
    include_down: bool,
    clean: bool,
) -> Result<Summary> {
    fs::create_dir_all(dest)
        .with_context(|| format!("creating destination directory {}", dest.display()))?;

    let mut summary = Summary {
        imported: Vec::new(),
        skipped: Vec::new(),
        notes: Vec::new(),
        cleaned: Vec::new(),
    };

    if clean {
        let mut existing: Vec<_> = fs::read_dir(dest)
            .with_context(|| format!("reading destination directory {}", dest.display()))?
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading entries of {}", dest.display()))?;
        existing.sort_by_key(|e| e.file_name());
        for entry in existing {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                summary.cleaned.push(path.display().to_string());
            }
        }
    }

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

        if let Some(reason) = convert_one(&path, &name, dest, force, include_down, &mut summary)?
        {
            summary.skipped.push((name, reason));
        } else {
            summary.imported.push(name);
        }
    }

    // runit itself has no service directory for udev coldplug (`udevadm
    // trigger --action=add && udevadm settle`) — real runit-based distros
    // run it directly from their stage-1 boot script, before runsvdir
    // ever starts a single service, so there's nothing under `src` for
    // this scan to find and convert. Skipping it isn't just cosmetic:
    // without it, PCI/USB devices never get their driver modules loaded
    // via udev's own uevent-triggered `modprobe`, so e.g. a network
    // card's kernel driver may never initialize at all — no interface,
    // not even under the wrong name. Generate the same companion unit
    // `examples/network/udev-trigger.toml` exists for, automatically,
    // whenever something that looks like the udev daemon itself made it
    // into this import.
    let udev_daemons: Vec<String> = summary
        .imported
        .iter()
        .filter(|n| looks_like_udev_daemon(n))
        .cloned()
        .collect();
    for udev_name in udev_daemons {
        if let Some(reason) = write_coldplug_unit(&udev_name, dest, force)? {
            summary.skipped.push((format!("{udev_name}-coldplug"), reason));
        } else {
            summary.imported.push(format!("{udev_name}-coldplug"));
        }
    }

    Ok(summary)
}

/// Heuristic: does this imported service name look like the udev/eudev
/// daemon itself, as opposed to some other unrelated service? Void's own
/// package names its service `udevd`; matched loosely (case-insensitive
/// substring) rather than exactly in case another distro's naming
/// differs slightly.
fn looks_like_udev_daemon(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "udevd" || lower.contains("udev")
}

/// Writes the `<udev_name>-coldplug` companion unit — see the comment in
/// [`convert`] for why this exists. Same existence/`force` handling as
/// [`convert_one`].
fn write_coldplug_unit(udev_name: &str, dest: &Path, force: bool) -> Result<Option<String>> {
    let coldplug_name = format!("{udev_name}-coldplug");
    let dest_file = dest.join(format!("{coldplug_name}.toml"));
    if dest_file.exists() && !force {
        return Ok(Some(format!(
            "{} already exists (use --force to overwrite)",
            dest_file.display()
        )));
    }

    let toml = format!(
        "# Generated alongside the imported '{udev_name}' service: runit performs udev\n\
         # coldplug from its own stage-1 boot script, not as an /etc/sv service, so\n\
         # there was nothing under the source directory for apollo-import to find and\n\
         # convert here — without this, device driver modules that depend on udev's\n\
         # own uevent-triggered modprobe (e.g. a NIC's) may never get loaded at all.\n\
         name = {}\n\
         # The sleep is a real wart, not a style choice: apollod has no readiness/notify\n\
         # protocol (unlike systemd's Type=notify for udevd), so `after` only orders\n\
         # start, not \"actually listening\" — see README.\n\
         exec = [\"/bin/sh\", \"-c\", \"sleep 1 && udevadm trigger --action=add && udevadm settle\"]\n\
         restart = \"no\"\n\
         after = [{}]\n",
        toml_string(&coldplug_name),
        toml_string(udev_name),
    );

    fs::write(&dest_file, toml).with_context(|| format!("writing {}", dest_file.display()))?;
    Ok(None)
}

/// Returns `Ok(Some(reason))` if this service was skipped, `Ok(None)` if
/// it was imported (having already pushed any notes onto `summary`).
fn convert_one(
    path: &Path,
    name: &str,
    dest: &Path,
    force: bool,
    include_down: bool,
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

    // A `down` file means this service starts disabled under runit —
    // often because it's one of several mutually-exclusive alternatives
    // for the same role (e.g. several agetty variants for consoles that
    // may or may not exist on a given machine, or one of two competing
    // time-sync daemons), left for `sv up` or a hardware-detection script
    // to enable selectively. apollo has no "loaded but not started"
    // state, so converting one of these means it auto-starts immediately
    // and, if it's not actually applicable to this machine, crash-loops
    // (found on a real boot: over a dozen agetty variants for
    // nonexistent consoles, plus a duplicate time daemon, all fighting
    // for the same lock file). Skipped by default for that reason;
    // --include-down opts back into converting it anyway.
    let has_down = path.join("down").exists();
    if has_down && !include_down {
        return Ok(Some(
            "has a 'down' file (starts disabled under runit, likely one of several \
             alternatives — see --include-down) — not imported"
                .into(),
        ));
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
    let dir_abs = std::path::absolute(path)
        .with_context(|| format!("resolving absolute path for {}", path.display()))?;

    let mut note_bits = Vec::new();
    if has_down {
        note_bits.push(
            "had a 'down' file (started disabled under runit) — apollo has no equivalent \
             yet, so this unit WILL auto-start; converted anyway because of --include-down"
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
    // runsv always chdirs into the service's own directory before running
    // it, so `run` scripts routinely reference sibling files (`./env`,
    // `./auto`, ...) by relative path — matching that here is what makes
    // those keep working under apollo instead of failing to find them
    // against apollod's own cwd.
    toml.push_str(&format!(
        "working-dir = {}\n",
        toml_string(&dir_abs.display().to_string())
    ));

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
