//! History tab: four stacked plots (voltages, load, battery, temperature)
//! sharing the time axis, over a chosen period up to now.

use std::ops::RangeInclusive;

use eframe::egui::{self, Color32, RichText};
use egui_plot::{
    uniform_grid_spacer, Corner, GridInput, GridMark, HoverPosition, Legend, Line, Plot,
};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

use gear_ups::history::{Sample, GAP_SECS};

use crate::tray::Level;

/// At most this many buckets per line; each contributes its min and max.
const MAX_BUCKETS: u64 = 800;
const DAY: i64 = 86_400;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Min10,
    Hour1,
    Hour6,
    Day1,
    Week,
}

impl Period {
    const ALL: [Period; 5] = [
        Period::Min10,
        Period::Hour1,
        Period::Hour6,
        Period::Day1,
        Period::Week,
    ];

    fn secs(self) -> u64 {
        match self {
            Period::Min10 => 600,
            Period::Hour1 => 3_600,
            Period::Hour6 => 6 * 3_600,
            Period::Day1 => DAY as u64,
            Period::Week => 7 * DAY as u64,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Period::Min10 => "10 хв",
            Period::Hour1 => "1 год",
            Period::Hour6 => "6 год",
            Period::Day1 => "доба",
            Period::Week => "тиждень",
        }
    }
}

type Segment = Vec<[f64; 2]>;

/// Splits samples from `from` on into unbroken segments (a gap longer than
/// `GAP_SECS` starts a new one) and thins each to the min and max per
/// `bucket` seconds. Non-finite values break the line too.
/// X is local time in seconds (`t + offset`).
fn segments(
    samples: &[Sample],
    from: u64,
    bucket: u64,
    offset: i64,
    value: impl Fn(&Sample) -> f32,
) -> Vec<Segment> {
    fn flush(out: &mut [Segment], cur: Option<(u64, [f64; 2], [f64; 2])>) {
        let (Some(seg), Some((_, lo, hi))) = (out.last_mut(), cur) else {
            return;
        };
        if lo == hi {
            seg.push(lo);
        } else if lo[0] <= hi[0] {
            seg.extend([lo, hi]);
        } else {
            seg.extend([hi, lo]);
        }
    }

    let bucket = bucket.max(1);
    let mut out: Vec<Segment> = Vec::new();
    let mut cur: Option<(u64, [f64; 2], [f64; 2])> = None;
    let mut prev_t: Option<u64> = None;
    let start = samples.partition_point(|s| s.t < from);
    for s in &samples[start..] {
        let v = value(s);
        if !v.is_finite() {
            flush(&mut out, cur.take());
            prev_t = None;
            continue;
        }
        if prev_t.is_none_or(|p| s.t.saturating_sub(p) > GAP_SECS) {
            flush(&mut out, cur.take());
            out.push(Vec::new());
        }
        prev_t = Some(s.t);
        let pt = [(s.t as i64 + offset) as f64, v as f64];
        let id = s.t / bucket;
        match &mut cur {
            Some((b, lo, hi)) if *b == id => {
                if pt[1] < lo[1] {
                    *lo = pt;
                }
                if pt[1] > hi[1] {
                    *hi = pt;
                }
            }
            _ => {
                flush(&mut out, cur.take());
                cur = Some((id, pt, pt));
            }
        }
    }
    flush(&mut out, cur);
    out
}

/// Local time minus UTC, in seconds, rounded to a quarter hour.
fn utc_offset_secs(now: u64) -> i64 {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let local = t.wHour as i64 * 3_600 + t.wMinute as i64 * 60 + t.wSecond as i64;
    let mut diff = local - (now as i64).rem_euclid(DAY);
    if diff > 14 * 3_600 {
        diff -= DAY;
    } else if diff < -12 * 3_600 {
        diff += DAY;
    }
    (diff as f64 / 900.0).round() as i64 * 900
}

/// "14:05", or "10.10 14:05" when `with_date`. `x` is local seconds since 1970.
fn clock(x: f64, with_date: bool) -> String {
    let secs = x.round() as i64;
    let sod = secs.rem_euclid(DAY);
    let hm = format!("{:02}:{:02}", sod / 3_600, sod % 3_600 / 60);
    if !with_date {
        return hm;
    }
    let (_, month, day) = civil_from_days(secs.div_euclid(DAY));
    format!("{day:02}.{month:02} {hm}")
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Grid steps in seconds that fall on round clock times.
fn time_steps(input: GridInput) -> [f64; 3] {
    const STEPS: [f64; 10] = [
        60.0, 300.0, 600.0, 1_800.0, 3_600.0, 7_200.0, 10_800.0, 21_600.0, 43_200.0, 86_400.0,
    ];
    let width = input.bounds.1 - input.bounds.0;
    let i = STEPS
        .iter()
        .position(|&s| width / s <= 8.0)
        .unwrap_or(STEPS.len() - 1);
    [
        STEPS[i.saturating_sub(2)],
        STEPS[i.saturating_sub(1)],
        STEPS[i],
    ]
}

struct Series {
    name: &'static str,
    color: Color32,
    value: fn(&Sample) -> f32,
}

pub fn show(
    ui: &mut egui::Ui,
    samples: &[Sample],
    period: &mut Period,
    nominal_battery_v: f32,
    ups_power_w: u32,
    now: u64,
) {
    let items: Vec<(Period, &str)> = Period::ALL.iter().map(|&p| (p, p.label())).collect();
    crate::theme::segmented(ui, period, &items);
    ui.add_space(crate::theme::GAP);

    let span = period.secs();
    let from = now.saturating_sub(span);
    if samples.last().is_none_or(|s| s.t < from) {
        crate::theme::card(ui, "", |ui| {
            ui.label("За цей період даних немає.");
            ui.label(
                RichText::new("Історію записує служба GearUps після кожного опитування ДБЖ (файл history.csv у папці журналу).")
                    .color(crate::theme::WEAK),
            );
        });
        return;
    }

    let offset = utc_offset_secs(now);
    let bucket = span / MAX_BUCKETS;
    let x_range = (from as i64 + offset) as f64..=(now as i64 + offset) as f64;
    let with_date = span > DAY as u64 / 2;
    let blue = Color32::from_rgb(66, 133, 244);

    // Each plot sits in a card: title line, padding, border and a gap below.
    let chrome = 13.0 + 2.0 + ui.spacing().item_spacing.y + 2.0 * 16.0 + 2.0 + crate::theme::GAP;
    let height = ((ui.available_height() - 4.0 * chrome) / 4.0).max(120.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            plot(
                ui,
                "voltage",
                "Напруга, В",
                |y| format!("{y:.1} В"),
                height,
                [0.0, 250.0],
                &[
                    Series {
                        name: "Вхід (мережа)",
                        color: blue,
                        value: |s| s.input_v,
                    },
                    Series {
                        name: "Вихід",
                        color: Level::Ok.color(),
                        value: |s| s.output_v,
                    },
                ],
                samples,
                (from, bucket, offset, x_range.clone(), with_date),
            );
            plot(
                ui,
                "load",
                "Навантаження, %",
                move |y| {
                    format!(
                        "{y:.0} % (~{} Вт)",
                        (y / 100.0 * ups_power_w as f64).round()
                    )
                },
                height,
                [0.0, 100.0],
                &[Series {
                    name: "Навантаження",
                    color: Color32::from_rgb(171, 71, 188),
                    value: |s| s.load_pct,
                }],
                samples,
                (from, bucket, offset, x_range.clone(), with_date),
            );
            let nominal = nominal_battery_v as f64;
            plot(
                ui,
                "battery",
                "Батарея, В",
                |y| format!("{y:.2} В"),
                height,
                [nominal * 0.85, nominal * 1.2],
                &[Series {
                    name: "Батарея",
                    color: Level::Warn.color(),
                    value: |s| s.battery_v,
                }],
                samples,
                (from, bucket, offset, x_range.clone(), with_date),
            );
            plot(
                ui,
                "temperature",
                "Температура, °C",
                |y| format!("{y:.1} °C"),
                height,
                [20.0, 40.0],
                &[Series {
                    name: "Температура",
                    color: Color32::from_rgb(0, 150, 136),
                    value: |s| s.temp_c,
                }],
                samples,
                (from, bucket, offset, x_range, with_date),
            );
        });
}

#[allow(clippy::too_many_arguments)]
fn plot(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    value_text: impl Fn(f64) -> String + 'static,
    height: f32,
    include_y: [f64; 2],
    series: &[Series],
    samples: &[Sample],
    (from, bucket, offset, x_range, with_date): (u64, u64, i64, RangeInclusive<f64>, bool),
) {
    crate::theme::card(ui, &title.to_uppercase(), |ui| {
        Plot::new(id)
            .height(height)
            .legend(Legend::default().position(Corner::LeftTop))
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .link_cursor("history", [true, false])
            .include_y(include_y[0])
            .include_y(include_y[1])
            .x_grid_spacer(uniform_grid_spacer(time_steps))
            .x_axis_formatter(move |mark: GridMark, _range: &RangeInclusive<f64>| {
                clock(mark.value, with_date)
            })
            .y_axis_formatter(|mark: GridMark, _range: &RangeInclusive<f64>| {
                format!("{:.0}", mark.value)
            })
            .label_formatter(move |pos: &HoverPosition<'_>| match pos {
                HoverPosition::NearDataPoint {
                    plot_name,
                    position,
                    ..
                } if !plot_name.is_empty() => Some(format!(
                    "{plot_name}: {}\n{}",
                    value_text(position.y),
                    clock(position.x, true)
                )),
                HoverPosition::NearDataPoint { position, .. }
                | HoverPosition::Elsewhere { position } => Some(clock(position.x, true)),
            })
            .show(ui, |plot_ui| {
                plot_ui.set_plot_bounds_x(x_range);
                for s in series {
                    for seg in segments(samples, from, bucket, offset, s.value) {
                        plot_ui.line(Line::new(s.name, seg).color(s.color).width(1.5));
                    }
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: u64, v: f32) -> Sample {
        Sample {
            t,
            input_v: v,
            output_v: v,
            load_pct: v,
            battery_v: v,
            on_battery: false,
            temp_c: v,
        }
    }

    #[test]
    fn splits_on_gaps() {
        let s = [
            sample(100, 1.0),
            sample(105, 2.0),
            sample(105 + GAP_SECS + 1, 3.0),
            sample(500, 4.0),
        ];
        let segs = segments(&s, 0, 1, 0, |s| s.input_v);
        assert_eq!(
            segs,
            vec![
                vec![[100.0, 1.0], [105.0, 2.0]],
                vec![[196.0, 3.0]],
                vec![[500.0, 4.0]]
            ]
        );
    }

    #[test]
    fn keeps_min_and_max_per_bucket() {
        let s = [
            sample(0, 5.0),
            sample(10, 1.0),
            sample(20, 9.0),
            sample(30, 4.0),
            sample(60, 7.0),
        ];
        let segs = segments(&s, 0, 60, 0, |s| s.input_v);
        assert_eq!(segs, vec![vec![[10.0, 1.0], [20.0, 9.0], [60.0, 7.0]]]);
    }

    #[test]
    fn breaks_on_missing_values() {
        let s = [sample(0, 1.0), sample(5, f32::NAN), sample(10, 3.0)];
        assert_eq!(
            segments(&s, 0, 1, 0, |s| s.temp_c),
            vec![vec![[0.0, 1.0]], vec![[10.0, 3.0]]]
        );
    }

    #[test]
    fn starts_at_from_and_applies_offset() {
        let s = [sample(10, 1.0), sample(20, 2.0)];
        assert_eq!(
            segments(&s, 15, 1, 3_600, |s| s.input_v),
            vec![vec![[3_620.0, 2.0]]]
        );
    }

    #[test]
    fn formats_clock() {
        // 2026-10-10 14:05:00 UTC
        let x = 1_791_641_100.0;
        assert_eq!(clock(x, false), "14:05");
        assert_eq!(clock(x, true), "10.10 14:05");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
