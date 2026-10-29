//! What the service knows, published for the GUI as
//! `%ProgramData%\GearUps\status.json` after every poll.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::protocol::Status;
use crate::telegram::BotState;
use crate::winutil::data_dir;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Seconds since the Unix epoch when this snapshot was written.
    pub updated_unix: u64,
    /// The same moment as local time, for display.
    pub time: String,
    pub connected: bool,
    /// Why the UPS is not reachable, when `connected` is false.
    pub error: Option<String>,
    pub status: Option<Status>,
    pub nominal_battery_v: f32,
    /// How long the UPS has been on battery.
    pub on_battery_secs: Option<u64>,
    /// Time left until the service powers the PC off (on battery only).
    pub pc_shutdown_in_secs: Option<u64>,
    pub pc_shutdown_requested: bool,
    pub ups_off_sent: bool,
    pub poll_interval_secs: u64,
    /// How long the UPS has been repeating the same status line, once that
    /// counts as a hung USB bridge.
    #[serde(default)]
    pub frozen_secs: Option<u64>,
    /// Battery charge counted from the load (see `battery::Gauge`), %.
    #[serde(default)]
    pub battery_charge_pct: Option<f32>,
    /// Capacity the runtime is estimated from: learned, else configured.
    #[serde(default)]
    pub battery_capacity_ah: Option<f32>,
    /// Date of the discharge the capacity was learned from.
    #[serde(default)]
    pub battery_learned_on: Option<String>,
    #[serde(default)]
    pub telegram: Option<BotState>,
}

pub fn path() -> PathBuf {
    data_dir().join("status.json")
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Writes atomically (temp file + rename) so readers never see half a file.
pub fn write(s: &Snapshot) {
    let Ok(text) = serde_json::to_string_pretty(s) else {
        return;
    };
    let tmp = data_dir().join("status.json.tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path());
    }
}

pub fn read() -> Option<Snapshot> {
    serde_json::from_str(&fs::read_to_string(path()).ok()?).ok()
}

impl Snapshot {
    /// True when the service has not refreshed the file for several polls.
    pub fn is_stale(&self) -> bool {
        unix_now().saturating_sub(self.updated_unix) > self.poll_interval_secs.max(1) * 3 + 5
    }
}
