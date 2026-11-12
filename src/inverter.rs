//! Spotting a separate battery inverter in front of the UPS. Mains voltage
//! wanders by several volts within minutes; an inverter holds its output
//! within a volt for hours. So when the input stays that flat for
//! `WINDOW_SECS`, the UPS is most likely fed by an inverter. Mains does now
//! and then hold steady too (seen for up to ~80 min at night), but switching
//! between mains and the inverter drops the input for a moment (the UPS goes
//! on battery), so only a flat stretch that starts right after a dropout or
//! a gap in the readings counts. Still a likelihood, not a fact.

use std::collections::{HashMap, VecDeque};

use crate::history::{Sample, GAP_SECS};

/// How long the input has to stay flat before it counts as an inverter.
pub const WINDOW_SECS: u64 = 15 * 60;
/// Share of the window within two neighbouring volts that turns the
/// detection on...
const ENTER: f32 = 0.95;
/// ...and the share below which it turns off again.
const LEAVE: f32 = 0.8;
/// How soon after the input came back the flat stretch has to start.
const GRACE_SECS: u64 = 5 * 60;
/// Below this the input is a dropout, not a supply.
const MIN_INPUT_V: f32 = 100.0;

/// Follows the input voltage poll by poll.
#[derive(Debug, Default, Clone)]
pub struct Detector {
    /// Readings of the last `WINDOW_SECS`: time and whole volts.
    window: VecDeque<(u64, i32)>,
    counts: HashMap<i32, u32>,
    since: Option<u64>,
    /// First reading after the input dropped out (or after a gap in the
    /// readings): switching between mains and inverter happens only then.
    reset_at: Option<u64>,
    /// Readings per volt of the current stretch, and its last reading.
    stretch: HashMap<i32, u64>,
    last_t: u64,
    /// Readings per volt of all stretches of at least `LEARN_SECS`: the
    /// inverter's usual output is the most common of them.
    learned: HashMap<i32, u64>,
}

/// Stretches at least this long teach the inverter's usual voltage.
const LEARN_SECS: u64 = 60 * 60;

impl Detector {
    /// Feeds one reading; returns since when (unix seconds) the input has
    /// looked like an inverter's, if it does.
    pub fn push(&mut self, t: u64, input_v: f32, on_battery: bool) -> Option<u64> {
        // NaN counts as a dropout too.
        if on_battery || input_v.is_nan() || input_v < MIN_INPUT_V {
            self.reset();
            return None;
        }
        if self
            .window
            .back()
            .is_some_and(|&(last, _)| t <= last || t - last > GAP_SECS)
        {
            self.reset();
        }
        let v = input_v.round() as i32;
        self.reset_at.get_or_insert(t);
        self.last_t = t;
        if self.since.is_some() {
            *self.stretch.entry(v).or_default() += 1;
        }
        self.window.push_back((t, v));
        *self.counts.entry(v).or_default() += 1;
        while let Some(&(old, ov)) = self.window.front() {
            if old + WINDOW_SECS >= t {
                break;
            }
            self.window.pop_front();
            if let Some(c) = self.counts.get_mut(&ov) {
                *c -= 1;
                if *c == 0 {
                    self.counts.remove(&ov);
                }
            }
        }
        let first = self.window.front().map_or(t, |&(first, _)| first);
        if (t - first) * 5 < WINDOW_SECS * 4 {
            // Not enough readings yet to judge.
            return self.since;
        }
        // An inverter's reading flips between two neighbouring volts.
        let band = self
            .counts
            .iter()
            .map(|(v, c)| c + self.counts.get(&(v + 1)).copied().unwrap_or(0))
            .max()
            .unwrap_or(0);
        let share = band as f32 / self.window.len() as f32;
        // Mains turning flat on its own is not a switch to the inverter: the
        // flat stretch has to start soon after the input came back.
        let fresh = self.reset_at.is_some_and(|r| first <= r + GRACE_SECS);
        // Once the inverter's voltage is known, the flat pair of volts has to
        // hold it: quiet mains at 227/228 V is not an inverter at 226 V.
        let usual = self.usual_v().is_none_or(|u| {
            let c = |v: i32| self.counts.get(&v).copied().unwrap_or(0);
            let pair = (c(u - 1) + c(u)).max(c(u) + c(u + 1));
            pair as f32 / self.window.len() as f32 >= ENTER
        });
        match self.since {
            // The flat stretch began with the window, not when it filled.
            None if share >= ENTER && fresh && usual => {
                self.since = Some(first);
                self.stretch = self.counts.iter().map(|(&v, &c)| (v, c as u64)).collect();
            }
            Some(_) if share < LEAVE => self.end_stretch(),
            _ => {}
        }
        self.since
    }

    pub fn since(&self) -> Option<u64> {
        self.since
    }

    /// The inverter's usual output, once a long stretch has shown it.
    pub fn usual_v(&self) -> Option<i32> {
        self.learned
            .iter()
            .max_by_key(|&(v, c)| (*c, -*v))
            .map(|(v, _)| *v)
    }

    /// Ends the current stretch; a long one teaches the usual voltage.
    fn end_stretch(&mut self) {
        if let Some(since) = self.since.take() {
            if self.last_t.saturating_sub(since) >= LEARN_SECS {
                for (v, c) in self.stretch.drain() {
                    *self.learned.entry(v).or_default() += c;
                }
            }
        }
        self.stretch.clear();
    }

    pub fn reset(&mut self) {
        self.end_stretch();
        self.window.clear();
        self.counts.clear();
        self.reset_at = None;
    }
}

/// Collects the inverter stretches of a sample series as `(from, to)`.
#[derive(Debug, Default, Clone)]
pub struct Periods {
    detector: Detector,
    pub list: Vec<(u64, u64)>,
    open: bool,
}

impl Periods {
    /// The detector after the samples so far, to go on from live readings.
    pub fn into_detector(self) -> Detector {
        self.detector
    }

    pub fn push(&mut self, s: &Sample) {
        match self.detector.push(s.t, s.input_v, s.on_battery) {
            Some(_) if self.open => {
                if let Some(p) = self.list.last_mut() {
                    p.1 = s.t;
                }
            }
            Some(since) => {
                match self.list.last_mut() {
                    // Back within a window of the last stretch: the same one.
                    Some(p) if since <= p.1 => p.1 = s.t,
                    _ => self.list.push((since, s.t)),
                }
                self.open = true;
            }
            None => self.open = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(d: &mut Detector, from: u64, volts: impl IntoIterator<Item = f32>) -> Option<u64> {
        let mut out = None;
        for (i, v) in volts.into_iter().enumerate() {
            out = d.push(from + i as u64 * 5, v, false);
        }
        out
    }

    #[test]
    fn flat_input_is_an_inverter_since_the_window_start() {
        let mut d = Detector::default();
        // Under 80 % of the window (12 min) is not enough yet.
        assert_eq!(feed(&mut d, 1000, (0..140).map(|_| 226.0)), None);
        // An odd 227 still counts, and the start is backdated.
        let since = feed(&mut d, 1000 + 140 * 5, (0..20).map(|i| if i == 7 { 227.0 } else { 226.0 }));
        assert_eq!(since, Some(1000));
    }

    #[test]
    fn wandering_mains_is_not() {
        let mut d = Detector::default();
        let mains = (0..400).map(|i| 224.0 + (i * 7 % 9) as f32);
        assert_eq!(feed(&mut d, 0, mains), None);
    }

    #[test]
    fn mains_turning_flat_without_a_dropout_is_not() {
        let mut d = Detector::default();
        let mains = (0..240).map(|i| 224.0 + (i * 7 % 9) as f32);
        assert_eq!(feed(&mut d, 0, mains), None);
        assert_eq!(feed(&mut d, 1200, (0..400).map(|_| 229.0)), None);
        // After a dropout the same flat input does count.
        assert_eq!(d.push(3200, 0.0, true), None);
        assert_eq!(feed(&mut d, 3205, (0..200).map(|_| 226.0)), Some(3205));
    }

    #[test]
    fn one_dip_keeps_it_but_mains_ends_it() {
        let mut d = Detector::default();
        assert!(feed(&mut d, 0, (0..200).map(|_| 226.0)).is_some());
        assert!(d.push(1000, 221.0, false).is_some());
        let mains = (0..60).map(|i| 228.0 + (i % 6) as f32);
        assert_eq!(feed(&mut d, 1005, mains), None);
    }

    #[test]
    fn learns_the_usual_voltage_and_skips_flat_mains_elsewhere() {
        let mut d = Detector::default();
        // A long stretch at 226 V, ended by a dropout.
        assert!(feed(&mut d, 0, (0..800).map(|_| 226.0)).is_some());
        assert_eq!(d.push(4005, 0.0, true), None);
        assert_eq!(d.usual_v(), Some(226));
        // Flat 229 V after a dropout is now quiet mains...
        assert_eq!(feed(&mut d, 4010, (0..300).map(|_| 229.0)), None);
        // ...and so is 227/228 V, though 227 is next to 226...
        assert_eq!(d.push(5510, 0.0, true), None);
        let quiet = (0..300).map(|i| if i % 2 == 0 { 227.0 } else { 228.0 });
        assert_eq!(feed(&mut d, 5515, quiet), None);
        // ...but flat 227 V is still the inverter.
        assert_eq!(d.push(7015, 0.0, true), None);
        assert_eq!(feed(&mut d, 7020, (0..300).map(|_| 227.0)), Some(7020));
    }

    #[test]
    fn battery_and_gaps_reset() {
        let mut d = Detector::default();
        assert!(feed(&mut d, 0, (0..200).map(|_| 226.0)).is_some());
        assert_eq!(d.push(1000, 226.0, true), None);
        assert_eq!(feed(&mut d, 1005, (0..100).map(|_| 226.0)), None);
        assert_eq!(d.push(1005 + 100 * 5 + GAP_SECS + 1, 226.0, false), None);
    }

    #[test]
    fn periods_cover_the_flat_stretch() {
        let mut p = Periods::default();
        let mut t = 0;
        let mut add = |v: f32| {
            t += 5;
            p.push(&Sample {
                t,
                input_v: v,
                output_v: v,
                load_pct: 10.0,
                battery_v: 27.0,
                on_battery: false,
                temp_c: 30.0,
                charge_pct: f32::NAN,
                counted_pct: f32::NAN,
            });
        };
        (0..300).for_each(|_| add(226.0));
        (0..100).for_each(|i| add(224.0 + (i * 7 % 9) as f32));
        assert_eq!(p.list.len(), 1);
        let (from, to) = p.list[0];
        assert_eq!(from, 5);
        assert!((1500..2000).contains(&to));
    }

    /// Replays the real history on this PC: `cargo test replay -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn replay() {
        let mut r = crate::history::Reader::default();
        r.refresh();
        let mut p = Periods::default();
        r.samples.iter().for_each(|s| p.push(s));
        for &(from, to) in &p.list {
            let v: Vec<f32> = r
                .samples
                .iter()
                .filter(|s| (from..=to).contains(&s.t))
                .map(|s| s.input_v)
                .collect();
            let mut counts = HashMap::new();
            v.iter().for_each(|&x| *counts.entry(x as i32).or_insert(0) += 1);
            let (mode, n) = counts.into_iter().max_by_key(|&(_, n)| n).unwrap_or((0, 0));
            let pct = n * 100 / v.len().max(1);
            println!("{from} .. {to}: {} min, {mode} V in {pct} %", (to - from) / 60);
        }
    }
}
