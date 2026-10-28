//! Battery charge counted from what the load draws, and the battery's real
//! capacity learned from discharges that end in the UPS's low-battery flag.
//!
//! The UPS's own charge reading (BL) and the rated capacity both run far
//! ahead of reality: on 28.10 BL still said 39 % when the UPS raised the
//! low-battery flag and cut the output a minute later, after 24.5 min at
//! ~96 W, while 7 Ah promised about an hour.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{full_runtime_hours, PEUKERT_K};
use crate::winutil::data_dir;

/// What the charger puts back per hour on mains: a small UPS charger gives
/// about 1 A, of which some 80 % ends up stored. Counted as a straight line.
const RECHARGE_AH_PER_HOUR: f32 = 0.8;
/// A discharge teaches the capacity only if it started this close to full.
const FULL_START: f32 = 0.05;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gauge {
    /// Capacity learned from a discharge, Ah; until then the configured one.
    pub capacity_ah: Option<f32>,
    /// Date of the discharge it was learned from.
    pub learned_on: Option<String>,
    /// Share of the capacity used up (weighted by Peukert's law, so it does
    /// not depend on the load): 0 = full, 1 = the UPS's low-battery point.
    pub used: f32,
    /// When `used` was counted.
    pub at_unix: u64,
    /// `used` when the current discharge began; None on mains.
    pub discharge_from: Option<f32>,
}

/// A change in the learned capacity, for the log.
pub struct Learned {
    pub old_ah: f32,
    pub new_ah: f32,
}

pub fn path() -> PathBuf {
    data_dir().join("battery.json")
}

impl Gauge {
    pub fn load() -> Gauge {
        fs::read_to_string(path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path(), text);
        }
    }

    pub fn capacity(&self, configured_ah: f32) -> f32 {
        self.capacity_ah.unwrap_or(configured_ah)
    }

    /// `used` at `now`: on mains the charger has been putting it back.
    pub fn used_at(&self, now: u64, capacity_ah: f32) -> f32 {
        if self.discharge_from.is_some() {
            return self.used;
        }
        let hours = now.saturating_sub(self.at_unix) as f32 / 3600.0;
        (self.used - hours * RECHARGE_AH_PER_HOUR / capacity_ah.max(0.1)).max(0.0)
    }

    pub fn charge_pct(&self, now: u64, capacity_ah: f32) -> f32 {
        ((1.0 - self.used_at(now, capacity_ah)) * 100.0).clamp(0.0, 100.0)
    }

    /// Counts one poll on battery. `max_secs` caps the step, so time the
    /// service did not run (PC off, UPS off) is not counted as drain.
    pub fn on_battery(
        &mut self,
        now: u64,
        max_secs: u64,
        nominal_v: f32,
        capacity_ah: f32,
        load_w: f32,
    ) {
        if self.discharge_from.is_none() {
            self.used = self.used_at(now, capacity_ah);
            self.discharge_from = Some(self.used);
        } else {
            let secs = now.saturating_sub(self.at_unix).min(max_secs) as f32;
            let full = full_runtime_hours(nominal_v, capacity_ah, load_w) * 3600.0;
            if full > 0.0 {
                self.used += secs / full;
            }
        }
        self.at_unix = now;
    }

    /// The UPS reports a low battery: this is where `used` reaches 1. A
    /// discharge from full tells how much the battery really holds.
    pub fn on_low(&mut self, configured_ah: f32, today: &str) -> Option<Learned> {
        let from_full = self.discharge_from.is_some_and(|f| f < FULL_START);
        let learned =
            (from_full && self.used > FULL_START).then(|| self.learn(configured_ah, today));
        self.used = self.used.max(1.0);
        // Whatever follows the flag says nothing more about the capacity.
        self.discharge_from = Some(1.0);
        learned
    }

    /// Mains is back. A discharge from full that ran past the expected end
    /// without the low-battery flag shows the battery holds more.
    pub fn on_mains(&mut self, now: u64, configured_ah: f32, today: &str) -> Option<Learned> {
        let from = self.discharge_from.take()?;
        self.at_unix = now;
        (from < FULL_START && self.used > 1.0).then(|| {
            let learned = self.learn(configured_ah, today);
            self.used = 1.0;
            learned
        })
    }

    /// The capacity at which this discharge would have used exactly all of
    /// it: by Peukert's law the share used scales as capacity^-k.
    fn learn(&mut self, configured_ah: f32, today: &str) -> Learned {
        let old_ah = self.capacity(configured_ah);
        let new_ah = (old_ah * self.used.powf(1.0 / PEUKERT_K)).clamp(0.5, configured_ah * 2.0);
        self.capacity_ah = Some(new_ah);
        self.learned_on = Some(today.to_string());
        Learned { old_ah, new_ah }
    }

    /// A new battery (the configured capacity changed): learn again.
    pub fn forget(&mut self) {
        self.capacity_ah = None;
        self.learned_on = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 96.0;

    fn discharge(g: &mut Gauge, ah: f32, minutes: u64) {
        g.on_battery(0, 10, 24.0, ah, W);
        for t in (5..=minutes * 60).step_by(5) {
            g.on_battery(t, 10, 24.0, ah, W);
        }
    }

    #[test]
    fn learns_the_capacity_that_ends_at_the_flag() {
        let mut g = Gauge::default();
        discharge(&mut g, 7.0, 25);
        assert!(g.used < 0.5);
        let l = g.on_low(7.0, "28.10.2026").unwrap();
        assert_eq!(l.old_ah, 7.0);
        // Run again with the learned capacity: it is used up right at 25 min.
        let mut again = Gauge::default();
        discharge(&mut again, l.new_ah, 25);
        assert!((again.used - 1.0).abs() < 0.01, "{}", again.used);
    }

    #[test]
    fn learns_only_from_a_full_start() {
        let mut g = Gauge {
            used: 0.5,
            ..Gauge::default()
        };
        discharge(&mut g, 7.0, 10);
        assert!(g.on_low(7.0, "x").is_none());
        assert_eq!(g.capacity_ah, None);
        assert_eq!(g.used, 1.0);
    }

    #[test]
    fn raises_the_capacity_when_the_battery_outlasts_it() {
        let mut g = Gauge::default();
        discharge(&mut g, 1.0, 60);
        assert!(g.used > 1.0);
        let l = g.on_mains(3600, 7.0, "x").unwrap();
        assert!(l.new_ah > 1.0);
        assert_eq!(g.used, 1.0);
    }

    #[test]
    fn recharges_on_mains() {
        let mut g = Gauge::default();
        discharge(&mut g, 3.0, 10);
        g.on_mains(600, 7.0, "x");
        let used = g.used;
        assert!(used > 0.2);
        assert!(g.used_at(600 + 3600, 3.0) < used);
        assert_eq!(g.used_at(600 + 4 * 3600, 3.0), 0.0);
        assert_eq!(g.charge_pct(600 + 4 * 3600, 3.0), 100.0);
    }

    #[test]
    fn gaps_are_not_counted_as_drain() {
        let mut g = Gauge::default();
        g.on_battery(0, 10, 24.0, 3.0, W);
        g.on_battery(3600, 10, 24.0, 3.0, W);
        assert!(g.used < 0.01);
    }
}
