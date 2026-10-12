//! History tab: four stacked plots (voltages, load, battery, temperature)
//! sharing the time axis, over a chosen period up to now, in a user-set order.

use std::ops::RangeInclusive;

use eframe::egui::{self, Color32, RichText};
use egui_plot::{
    uniform_grid_spacer, Corner, GridInput, GridMark, HoverPosition, Legend, Line, Plot,
    PlotPoint, Text,
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

/// One of the stacked plots; the user can reorder them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Voltage,
    Load,
    Battery,
    Temperature,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Voltage, Kind::Load, Kind::Battery, Kind::Temperature];

    fn key(self) -> &'static str {
        match self {
            Kind::Voltage => "voltage",
            Kind::Load => "load",
            Kind::Battery => "battery",
            Kind::Temperature => "temperature",
        }
    }
}

/// Chart order is a per-user view preference, kept outside config.toml.
fn order_path() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("GearUps");
    Some(dir.join("chart_order.txt"))
}

/// Saved order; unknown names are dropped and missing plots appended.
pub fn load_order() -> Vec<Kind> {
    let text = order_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    parse_order(&text)
}

fn parse_order(text: &str) -> Vec<Kind> {
    let mut order: Vec<Kind> = Vec::new();
    for name in text.split(',') {
        if let Some(k) = Kind::ALL.into_iter().find(|k| k.key() == name.trim()) {
            if !order.contains(&k) {
                order.push(k);
            }
        }
    }
    for k in Kind::ALL {
        if !order.contains(&k) {
            order.push(k);
        }
    }
    order
}

fn save_order(order: &[Kind]) {
    let Some(path) = order_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: Vec<&str> = order.iter().map(|k| k.key()).collect();
    let _ = std::fs::write(path, text.join(","));
}

pub fn show(
    ui: &mut egui::Ui,
    samples: &[Sample],
    period: &mut Period,
    order: &mut [Kind],
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
    let nominal = nominal_battery_v as f64;

    // Each plot sits in a card: title row with the move buttons, padding,
    // border and a gap below.
    let chrome = 20.0 + 2.0 + ui.spacing().item_spacing.y + 2.0 * 16.0 + 2.0 + crate::theme::GAP;
    let n = order.len() as f32;
    let height = ((ui.available_height() - n * chrome) / n).max(120.0);
    let mut moved: Option<(usize, usize)> = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let last = order.len() - 1;
            for (i, &kind) in order.iter().enumerate() {
                let ctx = PlotCtx {
                    samples,
                    height,
                    from,
                    bucket,
                    offset,
                    x_range: x_range.clone(),
                    with_date,
                    can_up: i > 0,
                    can_down: i < last,
                };
                let mv = match kind {
                    Kind::Voltage => plot(
                        ui,
                        kind,
                        "Напруга, В",
                        |y| format!("{y:.1} В"),
                        20.0,
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
                        ctx,
                    ),
                    Kind::Load => plot(
                        ui,
                        kind,
                        "Навантаження, %",
                        move |y| {
                            format!(
                                "{y:.0} % (~{} Вт)",
                                (y / 100.0 * ups_power_w as f64).round()
                            )
                        },
                        20.0,
                        &[Series {
                            name: "Навантаження",
                            color: Color32::from_rgb(171, 71, 188),
                            value: |s| s.load_pct,
                        }],
                        ctx,
                    ),
                    Kind::Battery => plot(
                        ui,
                        kind,
                        "Батарея, В",
                        |y| format!("{y:.2} В"),
                        nominal * 0.05,
                        &[Series {
                            name: "Батарея",
                            color: Level::Warn.color(),
                            value: |s| s.battery_v,
                        }],
                        ctx,
                    ),
                    Kind::Temperature => plot(
                        ui,
                        kind,
                        "Температура, °C",
                        |y| format!("{y:.1} °C"),
                        10.0,
                        &[Series {
                            name: "Температура",
                            color: Color32::from_rgb(0, 150, 136),
                            value: |s| s.temp_c,
                        }],
                        ctx,
                    ),
                };
                match mv {
                    Some(Move::Up) => moved = Some((i, i - 1)),
                    Some(Move::Down) => moved = Some((i, i + 1)),
                    None => {}
                }
            }
        });
    if let Some((a, b)) = moved {
        order.swap(a, b);
        save_order(order);
    }
}

enum Move {
    Up,
    Down,
}

struct PlotCtx<'a> {
    samples: &'a [Sample],
    height: f32,
    from: u64,
    bucket: u64,
    offset: i64,
    x_range: RangeInclusive<f64>,
    with_date: bool,
    can_up: bool,
    can_down: bool,
}

/// Y range that fits every line of the period, at least `min_span` tall so
/// small jitter is not blown up, with headroom for the value labels.
fn fit_y(lines: &[(&Series, Vec<Segment>)], min_span: f64) -> RangeInclusive<f64> {
    let ys = lines.iter().flat_map(|(_, segs)| segs.iter().flatten().map(|p| p[1]));
    let (lo, hi) = ys.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), y| {
        (lo.min(y), hi.max(y))
    });
    if !lo.is_finite() {
        return 0.0..=min_span.max(1.0);
    }
    let span = (hi - lo).max(min_span);
    let mid = (lo + hi) / 2.0;
    let non_negative = lo >= 0.0;
    let (lo, hi) = (lo.min(mid - span / 2.0), hi.max(mid + span / 2.0));
    let bottom = lo - span * 0.1;
    // Values that are never negative should not get a negative axis.
    let bottom = if non_negative { bottom.max(0.0) } else { bottom };
    bottom..=hi + span * 0.3
}

/// Value of the point nearest to `x` (points sorted by x), if one lies
/// within `tolerance`.
fn value_at(points: &[[f64; 2]], x: f64, tolerance: f64) -> Option<f64> {
    let i = points.partition_point(|p| p[0] < x);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter_map(|j| points.get(j))
        .min_by(|a, b| (a[0] - x).abs().total_cmp(&(b[0] - x).abs()))
        .filter(|p| (p[0] - x).abs() <= tolerance)
        .map(|p| p[1])
}

/// Draws one plot card and returns the move the user asked for, if any.
fn plot(
    ui: &mut egui::Ui,
    kind: Kind,
    title: &str,
    value_text: impl Fn(f64) -> String + Clone + 'static,
    min_span: f64,
    series: &[Series],
    c: PlotCtx<'_>,
) -> Option<Move> {
    let mut mv = None;
    crate::theme::card(ui, "", |ui| {
        ui.horizontal(|ui| {
            crate::theme::caption(ui, &title.to_uppercase());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(c.can_down, egui::Button::new("⬇").small())
                    .on_hover_text("Опустити графік")
                    .clicked()
                {
                    mv = Some(Move::Down);
                }
                if ui
                    .add_enabled(c.can_up, egui::Button::new("⬆").small())
                    .on_hover_text("Підняти графік")
                    .clicked()
                {
                    mv = Some(Move::Up);
                }
            });
        });
        let lines: Vec<(&Series, Vec<Segment>)> = series
            .iter()
            .map(|s| (s, segments(c.samples, c.from, c.bucket, c.offset, s.value)))
            .collect();
        let y_range = fit_y(&lines, min_span);
        let hover_text = value_text.clone();
        let hover_points: Vec<(&'static str, Segment)> = lines
            .iter()
            .map(|(s, segs)| (s.name, segs.concat()))
            .collect();
        // Nearest sample counts only within a bucket or a polling gap.
        let tolerance = (c.bucket * 2).max(GAP_SECS) as f64;
        let with_date = c.with_date;
        Plot::new(kind.key())
            .height(c.height)
            .legend(Legend::default().position(Corner::LeftTop))
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .link_cursor("history", [true, false])
            .x_grid_spacer(uniform_grid_spacer(time_steps))
            .x_axis_formatter(move |mark: GridMark, _range: &RangeInclusive<f64>| {
                clock(mark.value, with_date)
            })
            .y_axis_formatter(|mark: GridMark, _range: &RangeInclusive<f64>| {
                format!("{:.0}", mark.value)
            })
            .label_formatter(move |pos: &HoverPosition<'_>| {
                // Every line's value at the cursor time, not only the line
                // the cursor happens to be near.
                let x = match pos {
                    HoverPosition::NearDataPoint { position, .. }
                    | HoverPosition::Elsewhere { position } => position.x,
                };
                let mut text = String::new();
                for (name, points) in &hover_points {
                    if let Some(y) = value_at(points, x, tolerance) {
                        text += &format!("{name}: {}\n", hover_text(y));
                    }
                }
                Some(text + &clock(x, true))
            })
            .show(ui, |plot_ui| {
                plot_ui.set_plot_bounds_x(c.x_range);
                plot_ui.set_plot_bounds_y(y_range);
                for (s, segs) in &lines {
                    for seg in segs {
                        plot_ui.line(Line::new(s.name, seg.clone()).color(s.color).width(1.5));
                    }
                    // The latest value stays visible at the end of the line.
                    if let Some(&[x, y]) = segs.last().and_then(|seg| seg.last()) {
                        plot_ui.text(
                            Text::new(
                                "",
                                PlotPoint::new(x, y),
                                RichText::new(value_text(y)).color(s.color).strong(),
                            )
                            .anchor(egui::Align2::RIGHT_BOTTOM),
                        );
                    }
                }
            });
    });
    mv
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
    fn fits_y_to_data() {
        let s = Series {
            name: "t",
            color: Color32::BLACK,
            value: |s| s.temp_c,
        };
        let r = fit_y(&[(&s, vec![vec![[0.0, 44.0], [1.0, 46.0]]])], 10.0);
        assert!(*r.start() < 40.0 && *r.end() > 50.0, "{r:?}");
        let r = fit_y(&[(&s, vec![vec![[0.0, 2.0]]])], 10.0);
        assert_eq!(*r.start(), 0.0);
    }

    #[test]
    fn finds_value_near_cursor() {
        let pts = [[0.0, 1.0], [10.0, 2.0], [20.0, 3.0]];
        assert_eq!(value_at(&pts, 12.0, 5.0), Some(2.0));
        assert_eq!(value_at(&pts, 16.0, 5.0), Some(3.0));
        assert_eq!(value_at(&pts, 40.0, 5.0), None);
        assert_eq!(value_at(&[], 1.0, 5.0), None);
    }

    #[test]
    fn restores_chart_order() {
        assert_eq!(
            parse_order("temperature, load,bogus,load"),
            vec![Kind::Temperature, Kind::Load, Kind::Voltage, Kind::Battery]
        );
        assert_eq!(parse_order(""), Kind::ALL.to_vec());
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
