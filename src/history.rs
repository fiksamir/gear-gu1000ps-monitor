//! Readings history for the chart. The service appends one CSV line per poll
//! to `%ProgramData%\GearUps\history.csv`; at `MAX_BYTES` the file becomes
//! `history.csv.old`, so roughly a week is kept at the default poll interval.
//!
//! Line format: `unix_secs,input_v,output_v,load_pct,battery_v,on_battery,temp_c`.
//! Lines written before `temp_c` was added have no last field; their
//! temperature reads as NaN and is left out of the chart.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::protocol::Status;
use crate::winutil::data_dir;

const MAX_BYTES: u64 = 4_000_000;
/// Samples further apart than this are drawn as separate line segments.
pub const GAP_SECS: u64 = 90;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t: u64,
    pub input_v: f32,
    pub output_v: f32,
    pub load_pct: f32,
    pub battery_v: f32,
    pub on_battery: bool,
    pub temp_c: f32,
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
        }
    }

    fn to_line(self) -> String {
        format!(
            "{},{:.1},{:.1},{:.0},{:.2},{},{:.1}\n",
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
        })
    }
}

pub fn path() -> PathBuf {
    data_dir().join("history.csv")
}

fn old_path() -> PathBuf {
    data_dir().join("history.csv.old")
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
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(s.to_line().as_bytes());
    }
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
    fn line_round_trip() {
        let s = Sample {
            t: 1_760_000_000,
            input_v: 226.0,
            output_v: 225.5,
            load_pct: 12.0,
            battery_v: 27.1,
            on_battery: false,
            temp_c: 22.0,
        };
        assert_eq!(Sample::parse(&s.to_line()), Some(s));
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
