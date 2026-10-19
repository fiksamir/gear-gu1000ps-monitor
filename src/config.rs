use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::winutil::{data_dir, log};

const HEADER: &str =
    "# Налаштування служби GearUps. Служба перечитує файл сама, щойно він зміниться.\n";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// How often the UPS is polled.
    pub poll_interval_secs: u64,
    /// Power the PC off after this long on battery (0 = never).
    pub on_battery_shutdown_secs: u64,
    /// Power the PC off as soon as the UPS reports a low battery.
    pub shutdown_on_battery_low: bool,
    /// Turn the UPS output off when Windows powers off (never on restart/sleep).
    pub ups_off_on_pc_shutdown: bool,
    /// Delay between the command and the UPS output going off, 0.5..10 minutes.
    pub ups_off_delay_min: f64,
    /// If the PC shuts down while on battery, let the UPS come back by itself
    /// when mains returns (otherwise it stays off until its button is pressed).
    pub ups_return_when_on_battery: bool,
    /// Leave the UPS on when Windows may install updates during shutdown.
    pub skip_when_update_pending: bool,
    /// Rated active power of the UPS, for showing the load in watts (the UPS
    /// itself reports load only in percent).
    pub ups_power_w: u32,
    /// Capacity of one battery block, Ah, for the runtime estimate.
    pub battery_ah: f64,
    /// Warn when the UPS temperature sensor reaches this, °C (0 = never).
    pub temp_alarm_c: u32,
    /// Beeper state chosen in the window. The UPS forgets it (back on after
    /// the PC is off), so the service sets it again. Unset: left alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beeper_on: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_interval_secs: 5,
            on_battery_shutdown_secs: 600,
            shutdown_on_battery_low: true,
            ups_off_on_pc_shutdown: true,
            ups_off_delay_min: 1.0,
            ups_return_when_on_battery: true,
            skip_when_update_pending: true,
            ups_power_w: 600,
            battery_ah: 7.0,
            temp_alarm_c: 55,
            beeper_on: None,
        }
    }
}

pub fn path() -> PathBuf {
    data_dir().join("config.toml")
}

/// Last modification time of config.toml, used to notice edits.
pub fn modified() -> Option<SystemTime> {
    fs::metadata(path()).and_then(|m| m.modified()).ok()
}

impl Config {
    /// Loads `%ProgramData%\GearUps\config.toml`, writing the defaults if it
    /// does not exist. A broken file falls back to defaults (and is logged)
    /// rather than leaving the PC unprotected.
    pub fn load() -> Config {
        match Config::read() {
            Ok(Some(cfg)) => cfg,
            Ok(None) => {
                let cfg = Config::default();
                let _ = fs::create_dir_all(data_dir());
                let _ = cfg.save();
                cfg
            }
            Err(e) => {
                log(&format!(
                    "config.toml пошкоджено, діють типові налаштування: {e}"
                ));
                Config::default()
            }
        }
    }

    /// Reads the file without side effects: `Ok(None)` when it does not exist.
    pub fn read() -> Result<Option<Config>, String> {
        match fs::read_to_string(path()) {
            Ok(text) => toml::from_str::<Config>(&text)
                .map(|c| Some(c.sanitized()))
                .map_err(|e| e.to_string()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let text = toml::to_string_pretty(&self.clone().sanitized()).map_err(io::Error::other)?;
        fs::write(path(), format!("{HEADER}{text}"))
    }

    pub fn sanitized(mut self) -> Config {
        self.poll_interval_secs = self.poll_interval_secs.clamp(1, 60);
        self.ups_off_delay_min = self.ups_off_delay_min.clamp(0.5, 10.0);
        self.ups_power_w = self.ups_power_w.clamp(100, 10_000);
        self.battery_ah = self.battery_ah.clamp(1.0, 500.0);
        self.temp_alarm_c = self.temp_alarm_c.min(90);
        self
    }
}
