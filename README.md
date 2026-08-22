# apollo

A Linux init system, written in Rust.

## Scope and design decisions

apollo is being built as a **minimal PID 1**, not a full systemd
replacement: process supervision, service start/stop/restart, and basic
dependency ordering — not socket activation, cgroup resource control, or a
built-in logging daemon. It's a clean-slate design; it does not aim for
compatibility with systemd `.service` files. Service definitions are TOML.

The daemon/CLI split — `apollod` (the supervisor) and `apolloctl` (its
control client) talking over a Unix socket — is in place, developed and
tested as an ordinary process rather than as PID 1. apollod now also does
real PID-1-style reaping and early filesystem setup (mounts, fstab,
hostname — see Architecture below); the mounting code only actually runs
when apollod is PID 1, so it hasn't been exercised against a real kernel
yet, only built and reviewed — see [Testing safely on a real
distro](#testing-safely-on-a-real-distro). Getty units are in place too
(`examples/getty/`), so a real boot should reach a login prompt, and
`apolloctl reboot`/`poweroff`/`halt` (plus SIGTERM/SIGINT sent to apollod
directly) do a full graceful shutdown — see [Shutdown](#shutdown) below.
See [Roadmap](#roadmap) for what's still ahead. See [Tested On](#tested-on)
to make sure that Apollo has been tested and works on your distro.

## Layout

```
crates/
  apollo-proto/   shared IPC types and wire protocol (Request/Response, framing)
  apollod/        the supervisor daemon
  apolloctl/      the CLI control client
  apollo-import/  offline converter: other init systems' services -> apollo units
examples/services/ toy unit files for local dev testing (no root needed)
examples/getty/    real getty units meant for an actual /etc/apollo/services
examples/network/  real udev/dbus/NetworkManager units, ditto
```

### Architecture

`apollod` is structured as a single-threaded actor: a `Supervisor` owns a
`HashMap<String, UnitRuntime>` of all unit state, and it is the *only*
thing that ever touches that map. Everything else — the control-socket
listener, per-connection handler threads, and the reaper thread — only
ever communicates with it by sending an `Event` over an `mpsc` channel
and (for commands) waiting on a reply channel. This avoids locking in the
core logic entirely; see `crates/apollod/src/supervisor.rs`.

Concretely:
- **`config.rs`** — loads `*.toml` unit files from a directory, and
  topologically sorts them by their `after` field to decide start order.
  An `after` name that doesn't match a loaded unit (e.g. a not-yet-modeled
  `network.target`) is treated as already satisfied.
- **`registry.rs`** — `UnitRuntime`, the state kept for one loaded unit
  (parsed config + current state + pid + restart bookkeeping).
- **`supervisor.rs`** — the event loop and all state transitions. Units
  are tracked only by pid; process exits arrive as `Event::ProcessExited`
  from the reaper (below), and `handle_exit` looks up which unit (if any)
  a pid belongs to. Every spawned unit gets a `pre_exec` hook that resets
  its inherited (blocked) signal mask before `exec` — `fork()` carries
  apollod's own blocked `SIGCHLD`/`SIGTERM`/`SIGINT` into every child, and
  without undoing that, `SIGTERM` (e.g. from `apolloctl stop`) would just
  sit pending against a mask nothing in that process ever unblocks,
  silently ignored forever. Also owns shutdown: see
  [Shutdown](#shutdown) below.
- **`reaper.rs`** — one dedicated thread doing two jobs. Real PID-1-style
  reaping: `SIGCHLD`/`SIGTERM`/`SIGINT` are blocked on the main thread
  before anything else happens (so all later threads, including every
  spawned unit — see the note on `pre_exec` below — inherit them
  blocked), and this thread synchronously `sigwait()`s for them, draining
  every ready exit with `waitpid(-1, WNOHANG)` on each `SIGCHLD` wakeup.
  This reaps *every* child that ends up parented to apollod — not just
  units it started — which is what makes it correct to run as actual
  PID 1, where any orphaned process on the system can be reparented here.
  The same thread turns `SIGTERM`/`SIGINT` (what the kernel sends PID 1
  on a plain `kill`/Ctrl-Alt-Del) into an `Event::Shutdown`, then keeps
  running — it must not exit early just because a shutdown started, since
  `Supervisor::shutdown` depends on it continuing to reap units as they're
  stopped one by one.
- **`mounts.rs`** — early-boot filesystem setup: first, remount `/`
  read-write (the kernel mounts the real root according to the `ro`/`rw`
  boot cmdline flag, most bootloaders default that to `ro`, and it's
  init's job to flip it — skip this and every unit that writes anything
  to a path on `/` fails, even though the tmpfs/devtmpfs mounts below
  keep working regardless). Then a default `PATH` if the environment
  doesn't already have one (PID 1 as exec'd by the kernel typically has
  *no* environment at all — without this, every bare-name command apollod
  or any unit shells out to would fail to resolve). Then `/proc`, `/sys`,
  devtmpfs on `/dev`, `/dev/pts`, `/dev/shm`, `/run` (tmpfs), cgroup2 on
  `/sys/fs/cgroup`, then `mount -a` + `swapon -a` against `/etc/fstab` and
  setting the hostname from `/etc/hostname`. Gated in `main.rs` on
  `is_pid1()` (`getpid() == 1`) — a no-op on every dev/test invocation,
  since none of it makes sense unless apollod actually is the init
  process for this boot.
- **`ipc.rs`** — accepts connections on the control socket; each
  connection is one `Request` in, one `Response` out.
- **`main.rs`** — wires the above together, and is where PID 1's
  never-exit constraint is actually enforced: `boot()` (the real startup
  sequence) runs inside `catch_unwind`, and both a panic and a normal
  setup `Err` funnel into `fatal()`, which holds the process open instead
  of exiting when running as PID 1 (see Roadmap step 2).
- **`apollo-proto`** — `Request`/`Response`/`UnitInfo` types shared by
  `apollod` and `apolloctl`, plus length-prefixed JSON framing
  (`write_message`/`read_message`) over the socket.

### Unit files

One `*.toml` file per service, in a directory apollod is pointed at:

```toml
name = "sshd"
exec = ["/usr/sbin/sshd", "-D"]
restart = "on-failure"   # "no" (default) | "always" | "on-failure"
after = ["network.target"]
working-dir = "/etc/sshd" # optional; unset inherits apollod's own cwd

[env]
SOME_VAR = "value"
```

- `exec` is an argv array — no shell involved unless you put one there
  yourself (`["/bin/sh", "-c", "..."]`).
- `after` only orders startup among *other loaded units*; there's no
  notion of targets/runlevels yet.
- `working-dir` is optional and unset by default. Mainly useful for a
  command that relies on relative paths — `apollo-import runit` sets it
  automatically (see below) to match `runsv`'s own chdir-before-exec
  behavior.
- Restart attempts are capped at 5 per unit per daemon run (see
  `MAX_RESTARTS` in `supervisor.rs`) to avoid burning CPU on a crash loop.
  A unit that hits the cap is marked `failed` and left alone. Each
  policy-driven restart (not a manual `apolloctl restart`, which stays
  immediate) also waits 1s (`RESTART_BACKOFF`) before respawning, so a
  unit that fails instantly on every attempt still takes ~5 real seconds
  to exhaust its 5 attempts rather than doing it in a single burst.

### Importing services from runit

`apollo-import runit <src> <dest>` converts a directory of runit
services (one subdirectory per service, each with at least an executable
`run` script) into apollo `*.toml` units under `<dest>`. `<src>` should
be the real, final location of the service directories — the generated
unit's `exec` points straight at each one's `run` script there; nothing
is copied. `--force` overwrites a unit file that already exists at the
destination; `--clean` removes *every* existing `*.toml` in `<dest>`
first (see below for why that's the safer choice for a full re-import).

**On Void specifically, `<src>` should be the *enabled* services
directory, not `/etc/sv`.** `/etc/sv` is the master copy of *every*
service the installed packages ship, applicable to this machine or not;
what's actually enabled here lives as symlinks back into `/etc/sv` under
`/var/service` — or, if that turns out to be a dangling symlink (varies
by install), the directory it's *supposed* to point at,
`/etc/runit/runsvdir/current` (what the running `runsvdir` process
itself is actually watching and starting services from — check with
`ls -la /var/service` first, and fall back to the latter if it's
broken). Pointing this tool at `/etc/sv` imports everything Void ships
regardless of whether it makes sense on this machine — this is the
*real* fix for the "extra agetty variants and a duplicate time daemon"
problem described below, not the `down`-file check on its own: most of
those unwanted services aren't actually marked `down` inside `/etc/sv`
at all, they're simply never symlinked into the enabled directory, which
the `down` check has no way to see if it's only ever looking inside
`/etc/sv`. Entries there are symlinks rather than plain directories,
which this tool handles fine either way — a directory check follows
symlinks, and the generated `exec`/`working-dir` still resolve to the
real files.

Each generated unit execs the original `run` script directly (no `sh -c`
wrapper) — the same way `runsv` itself invokes it, relying on the
script's own shebang line — with `working-dir` set to the service's own
directory, again matching `runsv` (it always chdirs there before running
a service, which is why `run` scripts routinely reference sibling files
like `./env` or `./auto` by relative path — without this they'd fail to
find them against apollod's own cwd instead; found on a real boot via
`wpa_supplicant`'s `run` script failing on `. ./auto`). That's deliberate,
not laziness: `run` scripts commonly do their own privilege-dropping/
env-loading/redirection internally (`chpst`, `envdir`, `setuidgid`, ...),
and executing the script unchanged (same binary, same cwd) means none of
that needs to be understood or reimplemented here — it keeps working
exactly as it did under runit. `restart` is always set to `"always"`
(runit has no native one-shot concept to map to apollo's `"no"`/
`"on-failure"` — `runsv` restarts a service's `run` script whenever it
exits, forever, by default).

**Services with a runit `down` file (starts disabled) are also skipped by
default, not converted** — a secondary safety net on top of pointing
`<src>` at `/var/service` (above), for the services that *are* enabled
but still meant to start held down until something else brings them up
(`sv up`, or a hardware-detection script). apollo has no "loaded but not
started" state to preserve that with, so converting one means it
auto-starts immediately regardless. Pass `--include-down` to convert
these anyway if you really want them (each still gets a `# NOTE:`
comment and a printed warning, same as the two items below).

Found on a real boot, from both problems at once: importing all of
`/etc/sv/` wholesale (instead of `/var/service`) produced over a dozen
simultaneously crash-looping agetty units — one per console type Void
ships an `agetty-*` service for, whether or not that console actually
exists on this machine — plus a duplicate time-sync daemon fighting an
already-running one for the same lock file.

**A re-import doesn't remove anything on its own — use `--clean` for a
full re-import, or old generated units linger.** Found the hard way:
re-running the importer after upgrading it to skip `down` files (above)
correctly stopped *generating* those units, but the ones from an earlier,
less careful import were still sitting in `<dest>` from before — nothing
had asked to remove them, so apollod loaded and auto-started them right
alongside the new, correct set, same crash-loop as if nothing had
changed. `--clean` deletes every `*.toml` in `<dest>` before converting,
so a re-import actually reflects the current run — but it can't tell an
apollo-import-generated file apart from a hand-written one, so don't
point `<dest>` straight at a directory with both mixed in if using it.

**A `<name>-coldplug` unit is generated automatically alongside any
service that looks like the udev/eudev daemon itself** (matched loosely
on the name, e.g. Void's `udevd`) — `after = ["<name>"]`, running
`udevadm trigger --action=add && udevadm settle` once. runit-based
distros run this from their own stage-1 boot script, not as an `/etc/sv`
service, so there's nothing under `<src>` for a directory scan to find
and convert here on its own. Skipping it isn't cosmetic: udev coldplug
is what triggers driver modules to actually load for already-present
hardware via its uevent-triggered `modprobe`, so without it a device
whose module isn't loaded some other way may never initialize at all —
found on a real boot via a VM's NIC not showing up under any name at
all, not even the wrong one, because its kernel driver never loaded.
Same `sleep 1` caveat as `examples/network/udev-trigger.toml` (see
Network, above): apollod has no readiness/notify protocol, so this is a
blunt fix for a real race, not a design choice.

Two more things about a source service have no apollo equivalent (yet)
and just get a warning printed plus a `# NOTE:` comment in the generated
file, rather than being silently dropped:

- A `log/` subdirectory (a companion log service, piped from the main
  one by `runsv` itself at the file-descriptor level) — apollo has no
  log capture yet (Roadmap step 9), so the unit's output just inherits
  apollod's own console instead.
- A `finish` script (run by `runsv` on exit, for cleanup) — apollo has
  no equivalent hook; not migrated.

A service with no `run` file, or one that isn't executable, is skipped
with a reason printed rather than guessed at.

### Getty

`examples/getty/` has two units meant to actually be deployed (copy them
into `/etc/apollo/services/`, not just used as dev-testing toys):
`getty-tty1` (a virtual-console login prompt — `agetty ... tty1`) and
`getty-serial` (a serial-console one, for a headless VM — `agetty -L
ttyS0 ...`). Both are ordinary `restart = "always"` units; no getty-
specific code exists in apollod, since the generic restart-on-exit
mechanism already covers it — `agetty` exec's into `login` on successful
auth (keeping the same pid), and when that session ends the pid exits and
apollod respawns a fresh `agetty`, same as it would for any other
long-running unit.

One real, current wrinkle: apollod's own log lines (`eprintln!`) go to
whatever console it inherited, same as every unit's stdout/stderr right
now (no log capture yet — Roadmap step 9). If that happens to be the same
physical console a getty is attached to, apollod's own output can
interleave with the login prompt. Cosmetic, not a functional problem —
but it goes away once step 9 gives units (and probably apollod's own
diagnostics) somewhere else to write.

### Network

`examples/network/` has four units, meant to actually be deployed
together (copy into `/etc/apollo/services/`): `udevd`, `udev-trigger`,
`dbus`, `networkmanager`. Start order (from `after`, resolved the same
way as any other unit's dependencies): `udevd` and `dbus` first (no
dependencies between them), then `udev-trigger` (`after = ["udevd"]`),
then `networkmanager` last (`after = ["dbus", "udev-trigger"]`).

No new apollod code was needed for any of this — same as getty, it's all
just unit files exercising the existing `exec`/`restart`/`after`
mechanism. A few real wrinkles worth knowing before deploying these,
though:

- **Binary paths are distro-specific.** `udevd.toml` points at Fedora's
  `systemd-udevd` (`/usr/lib/systemd/systemd-udevd`); a non-systemd
  distro (Void, in particular) typically ships `eudev` instead, at a
  different path — adjust before use.
- **No `systemd-tmpfiles`.** `/run/dbus` and `/run/NetworkManager`
  normally get created by it from package-shipped configs; apollo
  doesn't run it (not in scope), so `dbus.toml` and
  `networkmanager.toml` each `mkdir -p` their own directory by hand in
  the unit's `exec` before launching the real binary.
- **No readiness protocol.** `after` only orders *start*, not readiness
  — apollod has no equivalent of systemd's `sd_notify`/`Type=notify`, so
  the next unit in an `after` chain starts as soon as the previous one is
  spawned, not once it's actually listening/ready. `udev-trigger.toml`
  papers over this against `udevd` with a flat `sleep 1` before
  triggering, which is a real wart, not a design choice — it can race on
  a slow enough boot. `dbus`/`networkmanager` haven't shown the same
  issue in review, but haven't been verified on a real boot either. A
  proper readiness protocol would remove the need for this but is a
  bigger feature, not planned for now.
- **Autoconnect isn't guaranteed out of the box.** Whether
  NetworkManager actually brings an interface up via DHCP with no
  further configuration depends on its own defaults/config
  (`/etc/NetworkManager/NetworkManager.conf`, `no-auto-default`) and
  whatever connection profiles already exist under
  `/etc/NetworkManager/system-connections/` — that's NetworkManager's
  own behavior, not something apollo controls.

Not safe to test by just running apollod as an ordinary dev/test process
on a real machine, unlike the toy units in `examples/services/` — these
touch real system paths (`/run/dbus`, the real system D-Bus socket) and
would fight with (or disrupt networking on) whatever's already running
there. Verification is real-boot-only, same as the early mounts (step 3)
were before the Void Linux boot confirmed them.

### Shutdown

`apolloctl reboot`/`poweroff`/`halt` (and SIGTERM/SIGINT sent to apollod
directly — SIGINT reboots, matching the kernel's Ctrl-Alt-Del-to-PID-1
convention; SIGTERM powers off) all funnel into one sequence in
`Supervisor::shutdown`:

1. Reply `Ok` to the caller immediately, *before* doing anything slow —
   stopping every unit can take seconds, and there's no reason to leave
   `apolloctl` hanging for it.
2. Stop every unit in the reverse of its start order, one at a time,
   waiting for each to actually exit (SIGTERM, then SIGKILL after a 5s
   timeout) before moving to the next — mirroring dependency order
   exactly reversed, the same way `after` decided start order.
3. Only if apollod is actually PID 1: `sync()`, `umount -a -r` against
   whatever `/etc/fstab` mounted, then `reboot(2)`.
4. Otherwise (dev/test mode): skip step 3 entirely and just exit. This
   isn't a testing shortcut bolted on afterward — it's the same
   `is_pid1()` gate `mounts.rs` uses, because syncing and unmounting the
   *real* filesystems of whatever machine a dev-testing apollod instance
   happens to be running on is never correct, regardless of testing
   concerns.

Verified in dev mode: all three `apolloctl` subcommands, plus SIGTERM and
SIGINT sent directly to apollod, correctly stop every unit (confirmed via
the log and process list, not just the reply) and exit — consistently
within milliseconds when a unit responds to SIGTERM immediately, as all
the example units do. What's *not* exercised here, and can't be: the
actual `sync`/`umount`/`reboot(2)` steps, gated behind `is_pid1()` same as
`mounts.rs` — that's for the Fedora VM.

## Building

```sh
cargo build
```

## Running locally (not as PID 1)

`apollod` defaults to `/etc/apollo/services` and
`/run/apollo/control.sock`, which need root. For local development,
override both:

```sh
./target/debug/apollod \
  --config-dir ./examples/services \
  --socket /tmp/apollo.sock &

./target/debug/apolloctl --socket /tmp/apollo.sock list
./target/debug/apolloctl --socket /tmp/apollo.sock status ping
./target/debug/apolloctl --socket /tmp/apollo.sock restart ping
./target/debug/apolloctl --socket /tmp/apollo.sock stop ping
```

`examples/services/` has three units to try this against: `hello` (a
one-shot), `greeter` (a one-shot that runs `after = ["hello"]`), and
`ping` (a `restart = "always"` loop, useful for exercising
stop/start/restart/status). `examples/getty/` can be pointed at the same
way (`--config-dir ./examples/getty`) to try the getty units against
this machine's own `/dev/tty1`/`/dev/ttyS0` — safe to do; `agetty` just
sits blocked waiting for a login, same as `ping` sits in its loop.

## Current limitations

These are expected at this milestone, not bugs:

- **SIGKILL still bypasses everything.** `kill -9 apollod` (or any signal
  other than SIGTERM/SIGINT) skips the graceful shutdown sequence
  entirely and just orphans apollod's children — same as the old
  "stopping apollod doesn't stop its children" caveat, just narrower now
  that plain SIGTERM/SIGINT are handled. Expected (SIGKILL can't be
  caught by anything, ever), but worth remembering when force-killing a
  test instance.
- Early mounts (`mounts.rs`) only ever run when apollod is PID 1, so
  they're unexercised by any dev/test invocation — verification happens
  on a real boot, e.g. the Void Linux VM (see Roadmap step 3).
- Units are tracked only by the single pid apollod spawned. A unit that
  forks further children (double-forking daemons, in particular) is
  invisible to apollo beyond that first pid — `stop` won't reach them,
  and their exit doesn't affect the unit's tracked state. Process-group-
  or cgroup-based tracking (Roadmap step 6) fixes this.
- `after` is the only dependency relation; no targets, no `wants`/
  `requires` distinction, no parallelism control beyond what the
  topological order allows (units are currently started serially, in
  order — not fanned out in parallel per dependency level).
- No log capture — child stdout/stderr currently just inherit apollod's
  own, as seen in the examples above. For a getty unit specifically, this
  means apollod's own log lines can visually interleave with the login
  prompt on the same console (see the Getty section above).

## Roadmap

Ordered toward the concrete goal of booting a real distro (Fedora, in a
VM) to a usable login prompt and shutting it down cleanly.

**Tested on:**

- **Void Linux (VM): working.** Boots to a login prompt via the getty
  units and `apolloctl poweroff`/`reboot` shut it down cleanly — no
  hung units, no SIGKILL fallback. Found and fixed the read-only-root
  issue above on this boot. No systemd involved (Void's own init is
  runit), so this is also a useful cross-check that nothing here
  accidentally depended on systemd-specific behavior.
- **Fedora (VM): not yet working.** First boot attempt via the `init=`
  GRUB edit landed in dracut's own initramfs emergency shell rather than
  reaching apollod at all — the likely cause under investigation is the
  `init=` path/binary itself (dracut refuses to `switch_root` to a
  target that doesn't exist or isn't executable), not necessarily a bug
  in apollod. Unresolved as of this writing.

1. ~~Daemon/CLI split with a control socket~~ (done)
2. ~~Real PID 1 reaping~~ (done) — `SIGCHLD` blocked on the main thread
   before any other thread or unit process exists, a dedicated thread
   `sigwait()`s on it, and each wakeup drains every ready exit with
   `waitpid(-1, WNOHANG)` (signals don't queue 1:1, so one wakeup can mean
   several exits). Reaps *any* child parented to apollod, known unit or
   not. (apollod's own termination signals — SIGTERM/SIGINT/Ctrl-Alt-Del —
   are now handled too; see step 5.) A main-thread panic or setup error is
   now also caught rather than allowed to exit the process — see
   `main.rs::fatal` — since PID 1 exiting panics the *kernel*, not just
   apollod: `main()` wraps `boot()` in `catch_unwind`, and both a panic
   and a normal early-setup `Err` funnel into the same fallback, which
   parks the main thread forever instead of exiting when PID 1 (still
   needs a reset to recover, but that's the same recovery model already
   in use for this whole testing phase — see below — not a new risk this
   adds). In dev/test mode this is invisible: exits normally, same as
   before. Verified in dev mode for both the `Err` path (bad
   `--config-dir`: exits 1, doesn't hang) and, with a temporary forced
   `panic!()` plus a temporary `is_pid1()` override to actually exercise
   the hang branch without needing to be real PID 1: confirmed apollod
   holds instead of exiting, then reverted both changes.
3. ~~Early mounts~~ (done, verified on a real boot — Void Linux VM) —
   remount `/` read-write (the kernel mounts real root according to the
   `ro`/`rw` boot cmdline flag; most bootloaders default `ro`, and it's
   init's job to flip it — missing initially, found on the first real
   boot: units could start but any of them writing to a path on `/`
   failed silently), then `/proc`, `/sys`, devtmpfs on `/dev`, `/dev/pts`,
   `/dev/shm`, `/run` (tmpfs), and cgroup2 (unified) on `/sys/fs/cgroup` —
   most modern daemons, including udev, assume the last one is already
   there. Then `mount -a` + `swapon -a` against `/etc/fstab` (shell out to
   util-linux's own binaries rather than reimplementing fstab parsing),
   and set the hostname from `/etc/hostname`. A failure on any one step
   logs and moves on rather than aborting the rest of boot.
4. ~~Getty~~ (done) — `examples/getty/getty-tty1.toml` and
   `getty-serial.toml`, plain `restart = "always"` units, no dedicated
   code needed. Verified in dev mode: apollod starts both against real
   `/dev/tty1`/`/dev/ttyS0` device nodes, they sit correctly blocked
   waiting for a login (confirmed `agetty` itself works stand-alone with
   a hard timeout first), and restart/stop behave exactly like any other
   long-running unit. What's *not* verified outside a real boot is the
   actual interactive login flow end-to-end — that's for the Fedora VM.
5. ~~`apolloctl reboot` / `poweroff` / `halt`~~ (done) — see
   [Shutdown](#shutdown) above for the full sequence. On Fedora,
   `reboot`/`poweroff`/`halt`/`shutdown` are symlinks to `systemctl`,
   which needs systemd-PID1's D-Bus socket — none of that exists once
   apollo is PID 1, so without this there would be no clean shutdown path
   for the VM at all. Also closes out step 2's remaining item: apollod's
   own SIGTERM/SIGINT are now handled (reaper.rs). Found and fixed two
   real bugs building this — worth recording since they're the kind of
   thing that only shows up under actual testing, not code review:
   forked units silently ignored SIGTERM entirely (they inherit apollod's
   own blocked signal mask across `fork()`, since `exec()` doesn't reset
   it — fixed with a `pre_exec` hook), and the reaper thread was
   `return`ing right after dispatching a signal-triggered shutdown,
   which meant nothing was left reaping `SIGCHLD` for the rest of the
   sequence — every unit stopped during that window sat as an unreaped
   zombie instead of being detected as exited.
6. **Process-group-based `stop`.** Currently only the immediate pid
   apollo spawned is signalled; anything that double-forks/daemonizes
   escapes tracking entirely. `setsid()` each unit's child and signal its
   whole process group (`kill(-pgid, ...)`) instead of just the one pid.
   A cgroup-per-unit approach (kill via `cgroup.kill`) is the more robust
   long-term fix and falls out naturally once cgroup2 is mounted (step 3).
7. ~~udev~~ (implemented as unit files, unverified against a real boot) —
   see [Network](#network) below. No new apollod code: `systemd-udevd`
   runs fine as an ordinary foreground unit under a non-systemd PID 1
   (same as the getty units), and coldplug is a second one-shot unit
   (`udevadm trigger --action=add && udevadm settle`) ordered `after` it.
8. ~~D-Bus + NetworkManager~~ (implemented as unit files, unverified
   against a real boot) — see [Network](#network) below. Also no new
   apollod code — `dbus-daemon --nofork` and `NetworkManager --no-daemon`
   as units, `networkmanager` ordered `after = ["dbus", "udev-trigger"]`.
9. **Log capture.** Redirect each unit's stdout/stderr to
   `/var/log/apollo/<name>.log` instead of inheriting apollod's own —
   needed for debugging boot issues on a console you can't scroll back.
10. Parallel startup within a dependency level.

**Explicitly deferred:** SELinux policy loading. Fedora runs enforcing by
default; getting policy load right at the correct point in early boot
(before anything creates mislabeled files) is a substantial chunk of work
on its own. Test with `enforcing=0` on the kernel cmdline (or
`/etc/selinux/config` set to permissive) rather than blocking on this.

### Testing safely on a real distro

Don't replace `/sbin/init`. Test via a one-time GRUB edit adding
`init=/path/to/apollod` to the kernel command line — that boots apollo
for that session only, and a normal reboot goes back to systemd. This is
the standard way alt-init projects (runit, OpenRC, etc.) get tested: a
bug that hangs boot just means resetting the VM, not reinstalling it —
whether the hang is the fail-safe above holding on purpose, or an actual
kernel panic from something SIGKILL bypassed, the recovery is the same
either way.

Steps, for the Fedora VM:

1. **Build on the VM itself**, not by copying a binary built elsewhere —
   a binary linked against this dev machine's glibc isn't guaranteed to
   run against Fedora's. `git clone`/copy the repo in, install Rust
   (`rustup` or `dnf install cargo`), `cargo build --release`.
2. **SELinux**: boot Fedora normally first and either set
   `/etc/selinux/config` to `permissive`, or plan to add `enforcing=0` on
   the kernel cmdline in step 4 — SELinux policy loading isn't
   implemented (see Explicitly deferred, above) and enforcing mode will
   just get in the way of evaluating apollo itself.
3. **Install unit files**: `sudo mkdir -p /etc/apollo/services` and copy
   in `examples/getty/getty-tty1.toml` (graphical/framebuffer console —
   e.g. virt-manager's default "Console" view) and/or `getty-serial.toml`
   (serial console — e.g. QEMU `-serial mon:stdio`, or virt-manager with
   a serial device added and set to a text console). Either or both;
   whichever matches how you'll actually be watching the VM. Start with
   *just* these — no other services — to keep the first boot's variables
   down to apollod itself.
4. **Boot**: at the GRUB menu, press `e` on the normal boot entry, find
   the line starting `linux`, and append `init=/path/to/apollod` (and
   `enforcing=0` if you didn't set permissive mode already) — then
   `Ctrl-X`/F10 to boot *that edit only*, not save it.
5. **Watch**: apollod's own log lines and the getty login prompt share
   the same console right now (see Getty, above) — expect them
   interleaved, that's cosmetic. Log in, and from that shell,
   `apolloctl list`/`status` should work against the real default socket
   (`/run/apollo/control.sock`) with no `--socket` override needed.
   **No networking will be up** on this first boot — deliberately, per
   step 3 above, to keep this first boot's variables down to just
   apollod/getty — so it's console-only; SSH won't reach it. Once this
   boots cleanly, `examples/network/` (see [Network](#network)) can be
   added on a subsequent boot to bring connectivity up too.
6. **Shut down**: `apolloctl poweroff` (or `reboot`, or `halt`) from that
   shell. A `reboot` is safe to try without complication — the GRUB edit
   from step 4 was one-time, so the *next* boot goes back to systemd
   automatically, not back into apollo.

## Tested On
| Distro     | Tester             | Status                       |
|------------|--------------------|------------------------------|
| Void Linux | Linux User Lucario | Network devices not working  |
| Fedora     | Linux User Lucario | Does not boot                |

## License

GNU GPL v3 (see [LICENSE.md](LICENSE.md) for full license)
