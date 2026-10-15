//! Readings history for the chart. The service appends one CSV line per poll
//! to `%ProgramData%\GearUps\history.csv`; at `MAX_BYTES` the file becomes
//! `history.csv.old`, so roughly a week is kept at the default poll interval.
//! For longer periods the service also keeps `history-archive.csv`: one line
//! per `ARCHIVE_SECS` with that slot's means, kept for good (about 5 MB a year).
//!
//! Line format: `unix_secs,input_v,output_v,load_pct,battery_v,on_battery,temp_c,charge_pct`.
//! Older lines lack the trailing fields (and `charge_pct` is empty when the
//! UPS did not report it); those read as NaN and are left out of the chart.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::protocol::Status;
use crate::winutil::data_dir;

const MAX_BYTES: u64 = 4_000_000;
/// Samples further apart than this are drawn as separate line segments.
pub const GAP_SECS: u64 = 90;
/// Length of one archive slot.
pub const ARCHIVE_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t: u64,
    pub input_v: f32,
    pub output_v: f32,
    pub load_pct: f32,
    pub battery_v: f32,
    pub on_battery: bool,
    pub temp_c: f32,
    /// Charge reported by the UPS (index 0xf3), NaN when unknown.
    pub charge_pct: f32,
}

impl Sample {
    pub fn from_status(t: u64, s: &Status) -> Sample {
        Sample {
            t,
            input_v: s.input_v,
            output_v: s.output_v,
            load_pct: s.load_pct as f32,
            battery_v: s.battery_v,
            on_battery: s.on_battery(),
            temp_c: s.temp_c,
            charge_pct: s.charge_pct.map_or(f32::NAN, |c| c as f32),
        }
    }

    fn to_line(self) -> String {
        let charge = if self.charge_pct.is_finite() {
            format!("{:.0}", self.charge_pct)
        } else {
            String::new()
        };
        format!(
            "{},{:.1},{:.1},{:.0},{:.2},{},{:.1},{charge}\n",
            self.t,
            self.input_v,
            self.output_v,
            self.load_pct,
            self.battery_v,
            self.on_battery as u8,
            self.temp_c
        )
    }

    fn parse(line: &str) -> Option<Sample> {
        let mut f = line.trim().split(',');
        let mut next = || f.next();
        Some(Sample {
            t: next()?.parse().ok()?,
            input_v: next()?.parse().ok()?,
            output_v: next()?.parse().ok()?,
            load_pct: next()?.parse().ok()?,
            battery_v: next()?.parse().ok()?,
            on_battery: next()? == "1",
            temp_c: next().and_then(|v| v.parse().ok()).unwrap_or(f32::NAN),
            charge_pct: next().and_then(|v| v.parse().ok()).unwrap_or(f32::NAN),
        })
    }
}

pub fn path() -> PathBuf {
    data_dir().join("history.csv")
}

fn old_path() -> PathBuf {
    data_dir().join("history.csv.old")
}

pub fn archive_path() -> PathBuf {
    data_dir().join("history-archive.csv")
}

/// Means of `samples` (one archive slot), stamped at the middle of the slot.
/// Missing (NaN) values are left out of their mean; on battery if any
/// sample was.
pub fn mean(samples: &[Sample]) -> Option<Sample> {
    let first = samples.first()?;
    let avg = |f: fn(&Sample) -> f32| {
        let (sum, n) = samples
            .iter()
            .map(f)
            .filter(|v| v.is_finite())
            .fold((0.0f64, 0u32), |(sum, n), v| (sum + v as f64, n + 1));
        if n == 0 {
            f32::NAN
        } else {
            (sum / n as f64) as f32
        }
    };
    Some(Sample {
        t: first.t / ARCHIVE_SECS * ARCHIVE_SECS + ARCHIVE_SECS / 2,
        input_v: avg(|s| s.input_v),
        output_v: avg(|s| s.output_v),
        load_pct: avg(|s| s.load_pct),
        battery_v: avg(|s| s.battery_v),
        on_battery: samples.iter().any(|s| s.on_battery),
        temp_c: avg(|s| s.temp_c),
        charge_pct: avg(|s| s.charge_pct),
    })
}

/// Gathers the service's samples and appends each finished slot's means to
/// the archive.
#[derive(Default)]
pub struct Archiver {
    slot: Vec<Sample>,
}

impl Archiver {
    pub fn push(&mut self, s: Sample) {
        if self
            .slot
            .first()
            .is_some_and(|f| f.t / ARCHIVE_SECS != s.t / ARCHIVE_SECS)
        {
            if let Some(m) = mean(&self.slot) {
                append_to(&archive_path(), m);
            }
            self.slot.clear();
        }
        self.slot.push(s);
    }
}

/// Creates the archive from the detailed history if there is none yet, so
/// the long periods do not start empty. Only slots that ended before
/// `before` (unix seconds) are written; the service adds the rest.
pub fn backfill_archive(before: u64) {
    let path = archive_path();
    if path.exists() {
        return;
    }
    let mut all = Vec::new();
    for p in [old_path(), self::path()] {
        if let Ok(text) = fs::read_to_string(p) {
            parse_all(&text, &mut all);
        }
    }
    all.sort_by_key(|s| s.t);
    let out: String = all
        .chunk_by(|a, b| a.t / ARCHIVE_SECS == b.t / ARCHIVE_SECS)
        .take_while(|slot| slot[0].t / ARCHIVE_SECS < before / ARCHIVE_SECS)
        .filter_map(mean)
        .map(Sample::to_line)
        .collect();
    let _ = fs::write(&path, out);
}

fn append_to(path: &Path, s: Sample) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(s.to_line().as_bytes());
    }
}

/// Appends a sample. Like the log, history must never take the service
/// down, so errors are ignored.
pub fn append(s: Sample) {
    let path = path();
    if fs::metadata(&path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, old_path());
    }
    append_to(&path, s);
}

fn parse_all(text: &str, out: &mut Vec<Sample>) {
    out.extend(text.lines().filter_map(Sample::parse));
}

/// Follows `history.csv`, reading only what was appended since last time.
#[derive(Default)]
pub struct Reader {
    offset: u64,
    loaded: bool,
    pub samples: Vec<Sample>,
    archive_offset: u64,
    /// 5-minute means from `history-archive.csv`, oldest first.
    pub archive: Vec<Sample>,
}

impl Reader {
    /// Picks up new lines; reloads everything after a rotation.
    /// Returns true if `samples` changed.
    pub fn refresh(&mut self) -> bool {
        let len = fs::metadata(path()).map(|m| m.len()).unwrap_or(0);
        let mut changed = false;
        if !self.loaded || len < self.offset {
            self.loaded = true;
            self.samples.clear();
            self.offset = 0;
            if let Ok(text) = fs::read_to_string(old_path()) {
                parse_all(&text, &mut self.samples);
            }
            changed = true;
        }
        if len > self.offset {
            if let Ok(consumed) = read_from(&path(), self.offset, &mut self.samples) {
                self.offset += consumed;
                changed |= consumed > 0;
            }
        }
        let path = archive_path();
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < self.archive_offset {
            self.archive.clear();
            self.archive_offset = 0;
            changed = true;
        }
        if len > self.archive_offset {
            if let Ok(consumed) = read_from(&path, self.archive_offset, &mut self.archive) {
                self.archive_offset += consumed;
                changed |= consumed > 0;
            }
        }
        changed
    }

    /// Adds a sample read directly from the UPS while the service is stopped.
    pub fn push_live(&mut self, s: Sample) {
        if self.samples.last().is_none_or(|last| s.t > last.t) {
            self.samples.push(s);
        }
    }
}

/// Parses complete lines from `offset` on; returns how many bytes it used
/// (a trailing line still being written is left for next time).
fn read_from(path: &Path, offset: u64, out: &mut Vec<Sample>) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
        return Ok(0);
    };
    parse_all(&String::from_utf8_lossy(&buf[..end]), out);
    Ok(end as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_slot_means() {
        let s = |t, v: f32, temp: f32, on_battery| Sample {
            t,
            input_v: v,
            output_v: v,
            load_pct: v,
            battery_v: v,
            on_battery,
            temp_c: temp,
            charge_pct: f32::NAN,
        };
        let m = mean(&[s(600, 1.0, f32::NAN, false), s(605, 3.0, 40.0, true)]).unwrap();
        assert_eq!(m.t, 750);
        assert_eq!(m.input_v, 2.0);
        assert_eq!(m.temp_c, 40.0);
        assert!(m.on_battery);
        assert!(m.charge_pct.is_nan());
        assert!(mean(&[]).is_none());
    }

    #[test]
    fn line_round_trip() {
        let s = Sample {
            t: 1_760_000_000,
            input_v: 226.0,
            output_v: 225.5,
            load_pct: 12.0,
            battery_v: 27.1,
            on_battery: false,
            temp_c: 22.0,
            charge_pct: 88.0,
        };
        assert_eq!(Sample::parse(&s.to_line()), Some(s));
        let old = Sample::parse("1760000000,226.0,225.5,12,27.10,0,22.0").unwrap();
        assert!(old.charge_pct.is_nan());
        let unknown = Sample::parse("1760000000,226.0,225.5,12,27.10,0,22.0,").unwrap();
        assert!(unknown.charge_pct.is_nan());
        let b = Sample {
            on_battery: true,
            input_v: 0.0,
            ..s
        };
        assert_eq!(Sample::parse(&b.to_line()), Some(b));
    }

    #[test]
    fn skips_bad_lines() {
        let mut v = Vec::new();
        parse_all(
            "garbage\n1,2,3\n1760000000,226.0,226.0,12,27.10,0\n",
            &mut v,
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].load_pct, 12.0);
        assert!(v[0].temp_c.is_nan());
    }
}
