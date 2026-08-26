#!/usr/bin/env bash
#
# install.sh — installs apollo as this machine's real init.
#
# Builds apollod/apolloctl/apollo-import, copies them to /apollo, points
# /sbin/init at /apollo/apollod, imports this machine's current services
# (runit only, for now — see below), and reboots into apollo.
#
# THIS IS NOT the safe, disposable way to try apollo. See README.md,
# "Testing safely on a real distro", for the one-time GRUB `init=` edit —
# that boots apollo for a single session with zero commitment, and a
# normal reboot goes back to the original init automatically. This
# script does the opposite on purpose: it replaces /sbin/init for real,
# on every future boot, until something undoes it. Only run it on a
# machine you've already tried apollo on via the GRUB edit and are happy
# with, and are prepared to recover by hand (see "rollback" below) if a
# later boot doesn't come up.
#
# Must be run as root, on the machine it's installing onto (not copied
# in as a binary built elsewhere — see README's "build on the VM itself"
# note: a binary linked against a different machine's glibc isn't
# guaranteed to run here).

set -euo pipefail

INSTALL_DIR=/apollo
CONFIG_DIR=/etc/apollo/services
LOG_DIR=/var/log/apollo
ROLLBACK_DIR="$INSTALL_DIR/rollback"

ASSUME_YES=0
SKIP_BUILD=0
SKIP_REBOOT=0

usage() {
    cat <<EOF
usage: $0 [-y|--yes] [--skip-build] [--no-reboot]

  -y, --yes      don't ask for confirmation before the destructive steps
                 (replacing /sbin/init) or before rebooting
  --skip-build   use the apollod/apolloctl/apollo-import already built in
                 target/release instead of running cargo build --release
  --no-reboot    do everything except the final reboot
EOF
}

for arg in "$@"; do
    case "$arg" in
        -y|--yes) ASSUME_YES=1 ;;
        --skip-build) SKIP_BUILD=1 ;;
        --no-reboot) SKIP_REBOOT=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; usage >&2; exit 1 ;;
    esac
done

confirm() {
    local prompt="$1"
    [ "$ASSUME_YES" = 1 ] && return 0
    local reply
    read -r -p "$prompt [y/N] " reply
    case "$reply" in
        y|Y|yes|YES|Yes) return 0 ;;
        *) return 1 ;;
    esac
}

if [ "$(id -u)" -ne 0 ]; then
    echo "error: must run as root (writes $INSTALL_DIR, /sbin/init, $CONFIG_DIR)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- detect the current init and where its enabled services live -----------
#
# apollo-import only knows how to read runit service directories right
# now (see crates/apollo-import) — check PID 1 directly rather than
# guessing from installed packages, since e.g. Void without apollo is
# always runit under PID 1 regardless of what else is on disk.
init_comm="$(cat /proc/1/comm 2>/dev/null || true)"
if [ "$init_comm" != "runit" ]; then
    echo "error: apollo-import only supports importing from runit right now" >&2
    echo "       (PID 1 here is '$init_comm', not runit) — see crates/apollo-import" >&2
    echo "       for adding another source, or write units under $CONFIG_DIR by hand." >&2
    exit 1
fi

# Prefer the enabled-services symlink; fall back to what runsvdir is
# actually watching if that symlink is missing/dangling. Matches the
# guidance in README.md's "Importing services from runit" section —
# pointing at /etc/sv instead would import every service Void ships,
# not just what's enabled on this machine.
if [ -e /var/service ]; then
    SRC=/var/service
else
    SRC=/etc/runit/runsvdir/current
fi
if [ ! -d "$SRC" ]; then
    echo "error: couldn't find an enabled runit service directory (tried /var/service, $SRC)" >&2
    exit 1
fi

# --- build -------------------------------------------------------------
BIN_DIR="$REPO_ROOT/target/release"
if [ "$SKIP_BUILD" != 1 ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "error: cargo not found — install Rust first (rustup, or your distro's" >&2
        echo "       cargo package), or pass --skip-build if $BIN_DIR is already built." >&2
        exit 1
    fi
    echo "==> building apollo (cargo build --release)"
    ( cd "$REPO_ROOT" && cargo build --release )
fi
for bin in apollod apolloctl apollo-import; do
    if [ ! -x "$BIN_DIR/$bin" ]; then
        echo "error: $BIN_DIR/$bin not found or not executable — build it first" >&2
        echo "       (cargo build --release), or drop --skip-build." >&2
        exit 1
    fi
done

# --- summary + confirmation ---------------------------------------------
echo
echo "About to:"
echo "  - install apollod/apolloctl/apollo-import to $INSTALL_DIR"
echo "  - back up the current /sbin/init under $ROLLBACK_DIR"
echo "  - symlink /sbin/init -> $INSTALL_DIR/apollod"
echo "  - import runit services from $SRC into $CONFIG_DIR (--clean --force)"
if [ "$SKIP_REBOOT" != 1 ]; then
    echo "  - reboot"
fi
echo
if ! confirm "Proceed?"; then
    echo "aborted."
    exit 1
fi

# --- install binaries ----------------------------------------------------
echo "==> installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
install -m 755 "$BIN_DIR/apollod" "$INSTALL_DIR/apollod"
install -m 755 "$BIN_DIR/apolloctl" "$INSTALL_DIR/apolloctl"
install -m 755 "$BIN_DIR/apollo-import" "$INSTALL_DIR/apollo-import"

mkdir -p "$CONFIG_DIR" "$LOG_DIR"

# --- back up + replace /sbin/init -----------------------------------------
CURRENT_INIT_TARGET=""
if [ -L /sbin/init ]; then
    CURRENT_INIT_TARGET="$(readlink -f /sbin/init || true)"
fi

if [ "$CURRENT_INIT_TARGET" = "$INSTALL_DIR/apollod" ]; then
    echo "==> /sbin/init already points at $INSTALL_DIR/apollod, leaving it"
else
    mkdir -p "$ROLLBACK_DIR"
    if [ -L /sbin/init ]; then
        orig="$(readlink /sbin/init)"
        echo "$orig" > "$ROLLBACK_DIR/sbin-init.symlink-target"
        echo "==> backed up: /sbin/init was a symlink to $orig"
    elif [ -e /sbin/init ]; then
        cp -a /sbin/init "$ROLLBACK_DIR/sbin-init.orig"
        echo "==> backed up: /sbin/init (a real file) copied to $ROLLBACK_DIR/sbin-init.orig"
    fi

    cat > "$ROLLBACK_DIR/rollback.sh" <<'EOF'
#!/usr/bin/env bash
# Undoes install.sh's /sbin/init change. Run this against this machine's
# real root — either directly, if it still boots far enough for a root
# shell (rescue/single-user mode, or a GRUB edit back to a known-good
# init), or from a live USB with the real root mounted, passing the
# mount point as $1.
set -euo pipefail
ROOT="${1:-}"
if [ -z "$ROOT" ]; then
    echo "usage: $0 </path/to/mounted/root>  (use / if running on the live system itself)" >&2
    exit 1
fi
BACKUP_DIR="$ROOT/apollo/rollback"
if [ -f "$BACKUP_DIR/sbin-init.symlink-target" ]; then
    target="$(cat "$BACKUP_DIR/sbin-init.symlink-target")"
    ln -sfn "$target" "$ROOT/sbin/init"
    echo "restored: $ROOT/sbin/init -> $target"
elif [ -f "$BACKUP_DIR/sbin-init.orig" ]; then
    cp -a "$BACKUP_DIR/sbin-init.orig" "$ROOT/sbin/init"
    echo "restored: $ROOT/sbin/init from backup"
else
    echo "no backup found under $BACKUP_DIR — nothing to restore" >&2
    exit 1
fi
EOF
    chmod +x "$ROLLBACK_DIR/rollback.sh"
    echo "==> rollback script written to $ROLLBACK_DIR/rollback.sh"

    echo "==> symlinking /sbin/init -> $INSTALL_DIR/apollod"
    rm -f /sbin/init
    ln -s "$INSTALL_DIR/apollod" /sbin/init
fi

# --- import current services ----------------------------------------------
echo "==> importing services from $SRC"
"$INSTALL_DIR/apollo-import" runit "$SRC" "$CONFIG_DIR" --clean --force

echo
echo "Done. /sbin/init now points at $INSTALL_DIR/apollod; units imported"
echo "into $CONFIG_DIR from $SRC. If a future boot doesn't come up, see"
echo "$ROLLBACK_DIR/rollback.sh."

if [ "$SKIP_REBOOT" = 1 ]; then
    echo "==> --no-reboot given, stopping here."
    exit 0
fi

if ! confirm "Reboot now?"; then
    echo "not rebooting — run 'reboot' manually when ready."
    exit 0
fi
reboot
