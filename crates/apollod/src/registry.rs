//! Runtime state for a single loaded unit.

use crate::config::UnitConfig;
use apollo_proto::{UnitInfo, UnitState};

#[derive(Debug)]
pub struct UnitRuntime {
    pub config: UnitConfig,
    pub state: UnitState,
    pub pid: Option<u32>,
    pub restart_count: u32,
    pub exit_status: Option<String>,
    /// Set while an explicit `Stop` is in flight, so the exit handler
    /// doesn't apply the unit's restart policy to a deliberate stop.
    pub user_stopped: bool,
    /// Set while an explicit `Restart` is in flight, so the exit handler
    /// respawns the unit once regardless of its restart policy.
    pub pending_restart: bool,
}

impl UnitRuntime {
    pub fn new(config: UnitConfig) -> Self {
        Self {
            config,
            state: UnitState::Loaded,
            pid: None,
            restart_count: 0,
            exit_status: None,
            user_stopped: false,
            pending_restart: false,
        }
    }

    pub fn to_info(&self) -> UnitInfo {
        UnitInfo {
            name: self.config.name.clone(),
            state: self.state,
            pid: self.pid,
            restart_count: self.restart_count,
            exit_status: self.exit_status.clone(),
        }
    }
}
