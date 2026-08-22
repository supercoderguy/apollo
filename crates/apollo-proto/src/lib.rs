//! Shared IPC types and wire protocol between `apollod` and `apolloctl`.
//!
//! The protocol is a simple length-prefixed JSON exchange over a Unix
//! domain socket: one [`Request`] in, one [`Response`] out, per connection.
//! It favors readability over throughput — the control plane is low
//! traffic, so this is not a place worth spending complexity budget yet.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

/// Default path of the control socket `apollod` listens on and `apolloctl`
/// connects to, when neither overrides it. Requires permission to create
/// `/run/apollo/` (i.e. root, or a system with a writable `/run`), so local
/// development typically overrides this via `--socket`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/apollo/control.sock";

/// A request sent from `apolloctl` to `apollod`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// List all loaded units and their current state.
    ListUnits,
    /// Get the state of a single unit.
    Status { name: String },
    /// Start a unit that is not currently running.
    Start { name: String },
    /// Stop a running unit (sends SIGTERM).
    Stop { name: String },
    /// Stop and then restart a unit.
    Restart { name: String },
    /// Stop every unit (reverse of their start order), then reboot the
    /// machine. Only actually calls `reboot(2)` if apollod is PID 1 — see
    /// `supervisor.rs` in apollod.
    Reboot,
    /// Like [`Request::Reboot`], but powers the machine off.
    Poweroff,
    /// Like [`Request::Reboot`], but halts the machine without powering
    /// it off.
    Halt,
}

/// A response sent from `apollod` back to `apolloctl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// The request was accepted; no data to return.
    Ok,
    /// The request could not be fulfilled.
    Error(String),
    /// Result of [`Request::ListUnits`].
    Units(Vec<UnitInfo>),
    /// Result of [`Request::Status`].
    Unit(UnitInfo),
}

/// Snapshot of a single unit's runtime state, as reported over IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitInfo {
    pub name: String,
    pub state: UnitState,
    pub pid: Option<u32>,
    pub restart_count: u32,
    pub exit_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitState {
    /// Configuration parsed, process not yet started.
    Loaded,
    /// Process is currently running.
    Running,
    /// A SIGTERM was sent and we're waiting for the process to exit.
    Stopping,
    /// Process exited on its own or was stopped, and won't be restarted.
    Stopped,
    /// Process exited unsuccessfully and restart attempts (if any) were
    /// exhausted.
    Failed,
}

impl fmt::Display for UnitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            UnitState::Loaded => "loaded",
            UnitState::Running => "running",
            UnitState::Stopping => "stopping",
            UnitState::Stopped => "stopped",
            UnitState::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// Write a length-prefixed JSON message: a 4-byte big-endian length
/// followed by that many bytes of JSON.
pub fn write_message<W: Write, T: Serialize>(mut w: W, msg: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(to_io_err)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large to frame"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Maximum accepted message size: control-plane messages are small, so this
/// is a generous cap meant only to reject obviously corrupt framing.
const MAX_MESSAGE_LEN: u32 = 10 * 1024 * 1024;

/// Read a length-prefixed JSON message written by [`write_message`].
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(mut r: R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len} exceeds max {MAX_MESSAGE_LEN}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(to_io_err)
}

fn to_io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}
