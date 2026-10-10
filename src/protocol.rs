//! Megatec "Q1" protocol as spoken by the MEC0003 USB controller.
//!
//! The controller does not use HID reports: every command is a USB string
//! descriptor request, the descriptor index selects the command, and the
//! returned string is the Megatec reply. Index map taken from NUT's
//! `fabula`/`hunnox` subdrivers (drivers/nutdrv_qx.c).
//!
//! Never probe other indices: on this controller they are actions
//! (beeper toggle, battery test, shutdown).

/// Q1: status line.
pub const IDX_STATUS: u32 = 0x03;
/// I: vendor info.
pub const IDX_INFO: u32 = 0x0c;
/// F: ratings.
pub const IDX_RATINGS: u32 = 0x0d;
/// C: cancel a pending shutdown.
pub const IDX_CANCEL: u32 = 0x0a;

/// Descriptor index for "S<n>R0000" (stay off) or "S<n>R<m>" (come back when
/// mains returns, after ~10 s). `delay_min` is clamped to 0.5..=10 minutes.
///
/// Encoding: high nibble = delay step (1..5 -> 0.5..0.9 min, 6..15 -> 1..10 min),
/// low nibble 0 = stay off, 8 = return.
pub fn shutdown_index(delay_min: f64, restore: bool) -> u32 {
    let delay = delay_min.clamp(0.5, 10.0);
    let step = if delay < 1.0 {
        1 + ((delay - 0.5) * 10.0).round() as u32
    } else {
        6 + (delay.round() as u32 - 1)
    };
    (step << 4) | if restore { 8 } else { 0 }
}

/// Delay actually applied by the UPS for a given request, in seconds.
pub fn effective_delay_secs(delay_min: f64) -> u32 {
    let step = shutdown_index(delay_min, false) >> 4;
    if step < 6 {
        30 + (step - 1) * 6
    } else {
        (step - 5) * 60
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub input_v: f32,
    pub input_fault_v: f32,
    pub output_v: f32,
    pub load_pct: u32,
    pub freq_hz: f32,
    pub battery_v: f32,
    pub temp_c: f32,
    /// Raw status bits, b7 first: utility fail, battery low, AVR active,
    /// UPS failed, line-interactive, test, shutdown active, beeper on.
    pub bits: [bool; 8],
}

fn trim_reply(reply: &str) -> &str {
    reply.trim_matches(|c: char| c == '\r' || c == '\n' || c == '\0' || c == ' ')
}

impl Status {
    pub fn parse(reply: &str) -> Result<Status, String> {
        let line = trim_reply(reply);
        let body = line
            .strip_prefix('(')
            .ok_or_else(|| format!("unexpected Q1 reply: {line:?}"))?;
        let f: Vec<&str> = body.split_whitespace().collect();
        if f.len() != 8 || f[7].len() != 8 {
            return Err(format!("unexpected Q1 reply: {line:?}"));
        }
        let num = |i: usize| -> Result<f32, String> {
            f[i].parse::<f32>()
                .map_err(|_| format!("bad Q1 field {i}: {:?}", f[i]))
        };
        let mut bits = [false; 8];
        for (i, c) in f[7].chars().enumerate() {
            bits[i] = match c {
                '1' => true,
                '0' => false,
                _ => return Err(format!("bad Q1 status bits: {:?}", f[7])),
            };
        }
        Ok(Status {
            input_v: num(0)?,
            input_fault_v: num(1)?,
            output_v: num(2)?,
            load_pct: num(3)? as u32,
            freq_hz: num(4)?,
            battery_v: num(5)?,
            temp_c: num(6)?,
            bits,
        })
    }

    pub fn on_battery(&self) -> bool {
        self.bits[0]
    }
    pub fn battery_low(&self) -> bool {
        self.bits[1]
    }
    pub fn avr_active(&self) -> bool {
        self.bits[2]
    }
    pub fn ups_failed(&self) -> bool {
        self.bits[3]
    }
    pub fn test_in_progress(&self) -> bool {
        self.bits[5]
    }
    pub fn shutdown_active(&self) -> bool {
        self.bits[6]
    }
    pub fn beeper_on(&self) -> bool {
        self.bits[7]
    }

    /// Rough charge estimate from battery voltage. Only meaningful on battery:
    /// on mains the charger holds the float voltage regardless of charge.
    pub fn charge_estimate_pct(&self, nominal_battery_v: f32) -> u32 {
        let cells = (nominal_battery_v / 2.0).max(1.0);
        let per_cell = self.battery_v / cells;
        (((per_cell - 1.75) / (2.10 - 1.75)).clamp(0.0, 1.0) * 100.0).round() as u32
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ratings {
    pub voltage: f32,
    pub current: f32,
    pub battery_v: f32,
    pub freq_hz: f32,
}

impl Ratings {
    pub fn parse(reply: &str) -> Result<Ratings, String> {
        let line = trim_reply(reply);
        let body = line
            .strip_prefix('#')
            .ok_or_else(|| format!("unexpected F reply: {line:?}"))?;
        let f: Vec<f32> = body
            .split_whitespace()
            .map(|s| s.parse::<f32>())
            .collect::<Result<_, _>>()
            .map_err(|_| format!("unexpected F reply: {line:?}"))?;
        if f.len() != 4 {
            return Err(format!("unexpected F reply: {line:?}"));
        }
        Ok(Ratings {
            voltage: f[0],
            current: f[1],
            battery_v: f[2],
            freq_hz: f[3],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_q1_reply() {
        let s = Status::parse("(226.0 000.0 226.0 012 49.9 27.1 22.0 00001000\r").unwrap();
        assert_eq!(s.input_v, 226.0);
        assert_eq!(s.load_pct, 12);
        assert_eq!(s.battery_v, 27.1);
        assert_eq!(s.temp_c, 22.0);
        assert!(!s.on_battery() && !s.battery_low() && !s.shutdown_active());
        assert!(s.bits[4]);
    }

    #[test]
    fn parses_on_battery_flags() {
        let s = Status::parse("(000.0 000.0 220.0 030 50.0 23.0 25.0 11000010").unwrap();
        assert!(s.on_battery() && s.battery_low() && s.shutdown_active());
        assert_eq!(s.charge_estimate_pct(24.0), 48);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Status::parse("UPS No Ack").is_err());
        assert!(Status::parse("(1 2 3)").is_err());
    }

    #[test]
    fn parses_real_ratings() {
        let r = Ratings::parse("#230.0 3.4 24.00 50.0\r").unwrap();
        assert_eq!(
            r,
            Ratings {
                voltage: 230.0,
                current: 3.4,
                battery_v: 24.0,
                freq_hz: 50.0
            }
        );
    }

    #[test]
    fn shutdown_indices_match_nut() {
        assert_eq!(shutdown_index(0.5, false), 0x10);
        assert_eq!(shutdown_index(0.7, false), 0x30);
        assert_eq!(shutdown_index(0.9, false), 0x50);
        assert_eq!(shutdown_index(1.0, false), 0x60);
        assert_eq!(shutdown_index(2.0, true), 0x78);
        assert_eq!(shutdown_index(10.0, false), 0xf0);
        assert_eq!(shutdown_index(0.1, false), 0x10);
        assert_eq!(shutdown_index(99.0, true), 0xf8);
    }

    #[test]
    fn effective_delays() {
        assert_eq!(effective_delay_secs(0.5), 30);
        assert_eq!(effective_delay_secs(0.9), 54);
        assert_eq!(effective_delay_secs(1.0), 60);
        assert_eq!(effective_delay_secs(10.0), 600);
    }
}
