//! Early-boot filesystem setup: the virtual filesystems a modern Linux
//! system assumes are already present (`/proc`, `/sys`, devtmpfs on
//! `/dev`, cgroup2, ...), bringing up whatever else `/etc/fstab` lists,
//! and setting the hostname.
//!
//! Only meaningful when apollod is actually running as PID 1 — see
//! [`is_pid1`] — nothing in that /proc etc. is apollod's job otherwise
//! (see `main.rs`, which checks before calling [`run`]).

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use std::fs;
use std::path::Path;
use std::process::Command;

/// True when apollod is running as PID 1, i.e. this is an actual boot and
/// not a normal development/test invocation.
pub fn is_pid1() -> bool {
    std::process::id() == 1
}

struct EarlyMount {
    target: &'static str,
    fstype: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
}

fn early_mounts() -> Vec<EarlyMount> {
    vec![
        EarlyMount {
            target: "/proc",
            fstype: "proc",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            data: None,
        },
        EarlyMount {
            target: "/sys",
            fstype: "sysfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            data: None,
        },
        EarlyMount {
            target: "/dev",
            fstype: "devtmpfs",
            flags: MsFlags::MS_NOSUID,
            data: Some("mode=0755"),
        },
        EarlyMount {
            // Needs /dev (just above) mounted first, since it's a
            // subdirectory of it.
            target: "/dev/pts",
            fstype: "devpts",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            // gid=5 is the "tty" group's conventional gid across
            // mainline distros (Fedora included) — the same value
            // systemd itself hardcodes for this mount.
            data: Some("mode=0620,ptmxmode=0666,gid=5"),
        },
        EarlyMount {
            target: "/dev/shm",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=1777"),
        },
        EarlyMount {
            target: "/run",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=0755,size=10%"),
        },
        EarlyMount {
            // Needs /sys mounted first: this is a directory node the
            // kernel exposes under sysfs, not one apollod creates itself.
            target: "/sys/fs/cgroup",
            fstype: "cgroup2",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            data: None,
        },
    ]
}

/// Performs early-boot filesystem setup. Meant to be called once, as
/// early as possible in `main`, before anything else touches the
/// filesystem.
///
/// A failure on any one step is logged and does not abort the rest of the
/// sequence — better to boot with e.g. cgroup2 missing and units failing
/// loudly later than to refuse to boot at all over it. `EBUSY` (already
/// mounted) is treated as expected, not an error, since it's normal for
/// e.g. `/dev` to already be a devtmpfs by the time this runs.
pub fn run() {
    ensure_default_path();

    for m in early_mounts() {
        if let Err(e) = fs::create_dir_all(m.target) {
            eprintln!("apollod: couldn't create mount point {}: {e}", m.target);
            continue;
        }
        do_mount(&m);
    }

    run_command("mount", &["-a"]);
    run_command("swapon", &["-a"]);
    set_hostname();
}

/// PID 1 as exec'd by the kernel typically has no environment at all — no
/// `PATH` in particular. Without one, `run_command` below (and `umount`
/// in `supervisor.rs`'s shutdown sequence) would fail to find `mount`,
/// `swapon`, `umount` by bare name, and so would any unit whose `exec`
/// itself runs further commands by bare name, since children inherit
/// this process's environment. systemd sets a default `PATH` for exactly
/// this reason; do the same here, first thing, before anything that
/// might need it runs.
fn ensure_default_path() {
    if std::env::var_os("PATH").is_some() {
        return;
    }
    let default = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    // Safety: single-threaded at this point in startup — run() is called
    // from main() before any other thread exists.
    unsafe {
        std::env::set_var("PATH", default);
    }
    eprintln!("apollod: no PATH in the environment, defaulting to {default}");
}

fn do_mount(m: &EarlyMount) {
    // For these virtual/pseudo filesystems there's no real block device
    // backing them; using the fstype name as the "source" too matches
    // both util-linux's own `mount -t <fstype> <fstype> <target>`
    // convention and what systemd's early mount setup does.
    let result = mount(
        Some(m.fstype),
        Path::new(m.target),
        Some(m.fstype),
        m.flags,
        m.data,
    );
    match result {
        Ok(()) => eprintln!("apollod: mounted {} ({})", m.target, m.fstype),
        Err(Errno::EBUSY) => eprintln!("apollod: {} already mounted", m.target),
        Err(e) => eprintln!("apollod: failed to mount {} ({}): {e}", m.target, m.fstype),
    }
}

/// Brings up everything else listed in `/etc/fstab` (extra partitions,
/// swap, ...) by shelling out to the real `mount`/`swapon` rather than
/// reimplementing fstab parsing ourselves.
fn run_command(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {
            eprintln!("apollod: `{program} {}` succeeded", args.join(" "));
        }
        Ok(status) => {
            eprintln!(
                "apollod: `{program} {}` exited with {status}",
                args.join(" ")
            );
        }
        Err(e) => eprintln!("apollod: failed to run `{program}`: {e}"),
    }
}

fn set_hostname() {
    let contents = match fs::read_to_string("/etc/hostname") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("apollod: not setting hostname: reading /etc/hostname: {e}");
            return;
        }
    };
    let name = contents.trim();
    if name.is_empty() {
        return;
    }
    match nix::unistd::sethostname(name) {
        Ok(()) => eprintln!("apollod: hostname set to '{name}'"),
        Err(e) => eprintln!("apollod: failed to set hostname to '{name}': {e}"),
    }
}
