//! History tab: stacked plots (voltages, load, battery, charge, temperature)
//! sharing the time axis, over a chosen period up to now, in a user-set order.

use std::ops::RangeInclusive;

use eframe::egui::{self, Color32, RichText};
use egui_plot::{
    uniform_grid_spacer, Corner, GridInput, GridMark, HoverPosition, Legend, Line, LineStyle, Plot,
    PlotPoint, Text,
};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

use gear_ups::history::{Reader, Sample, ARCHIVE_SECS, GAP_SECS};

use crate::tray::Level;

/// At most this many buckets per line; each contributes its mean.
const MAX_BUCKETS: u64 = 800;
/// Curve points drawn between two neighbouring points of a line.
const SPLINE_STEPS: usize = 6;
const DAY: i64 = 86_400;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Min10,
    Hour1,
    Hour6,
    Day1,
    Week,
    Month,
    HalfYear,
    All,
}

impl Period {
    const ALL: [Period; 8] = [
        Period::Min10,
        Period::Hour1,
        Period::Hour6,
        Period::Day1,
        Period::Week,
        Period::Month,
        Period::HalfYear,
        Period::All,
    ];

    /// Length in seconds; `All` reaches back to the oldest sample.
    fn secs(self, earliest: Option<u64>, now: u64) -> u64 {
        match self {
            Period::Min10 => 600,
            Period::Hour1 => 3_600,
            Period::Hour6 => 6 * 3_600,
            Period::Day1 => DAY as u64,
            Period::Week => 7 * DAY as u64,
            Period::Month => 30 * DAY as u64,
            Period::HalfYear => 182 * DAY as u64,
            Period::All => earliest
                .map_or(0, |e| now.saturating_sub(e))
                .max(DAY as u64),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Period::Min10 => "10 хв",
            Period::Hour1 => "1 год",
            Period::Hour6 => "6 год",
            Period::Day1 => "доба",
            Period::Week => "тиждень",
            Period::Month => "місяць",
            Period::HalfYear => "пів року",
            Period::All => "увесь час",
        }
    }
}

type Segment = Vec<[f64; 2]>;

/// Splits samples from `from` on into unbroken segments (a gap longer than
/// `GAP_SECS` starts a new one) and thins each to the mean per `bucket`
/// seconds. Non-finite values break the line too.
/// X is local time in seconds (`t + offset`).
fn segments(
    samples: &[Sample],
    from: u64,
    bucket: u64,
    offset: i64,
    gap: u64,
    value: impl Fn(&Sample) -> f32,
) -> Vec<Segment> {
    /// Bucket id, sum of x, sum of y, sample count.
    type Acc = (u64, f64, f64, f64);
    fn flush(out: &mut [Segment], cur: Option<Acc>) {
        if let (Some(seg), Some((_, x, y, n))) = (out.last_mut(), cur) {
            seg.push([x / n, y / n]);
        }
    }

    let bucket = bucket.max(1);
    let mut out: Vec<Segment> = Vec::new();
    let mut cur: Option<Acc> = None;
    let mut prev_t: Option<u64> = None;
    let start = samples.partition_point(|s| s.t < from);
    for s in &samples[start..] {
        let v = value(s);
        if !v.is_finite() {
            flush(&mut out, cur.take());
            prev_t = None;
            continue;
        }
        if prev_t.is_none_or(|p| s.t.saturating_sub(p) > gap) {
            flush(&mut out, cur.take());
            out.push(Vec::new());
        }
        prev_t = Some(s.t);
        let pt = [(s.t as i64 + offset) as f64, v as f64];
        let id = s.t / bucket;
        match &mut cur {
            Some((b, x, y, n)) if *b == id => {
                *x += pt[0];
                *y += pt[1];
                *n += 1.0;
            }
            _ => {
                flush(&mut out, cur.take());
                cur = Some((id, pt[0], pt[1], 1.0));
            }
        }
    }
    flush(&mut out, cur);
    out
}

/// Centered moving average over `radius` points on each side. Neighbours
/// further than `step` from the point are left out, so real jumps (a power
/// cut, the fan starting) stay sharp while sensor jitter and the rounding
/// staircase smooth out.
fn smooth(seg: &[[f64; 2]], radius: usize, step: f64) -> Segment {
    (0..seg.len())
        .map(|i| {
            let [x, y] = seg[i];
            let near = seg[i.saturating_sub(radius)..(i + radius + 1).min(seg.len())]
                .iter()
                .filter(|p| (p[1] - y).abs() <= step);
            let (sum, n) = near.fold((0.0, 0.0), |(s, n), p| (s + p[1], n + 1.0));
            [x, sum / n]
        })
        .collect()
}

/// Points on each side averaged into every point: about a sixtieth of the
/// period, so the sensors' rounding staircase (whole degrees, 0.1 V) is
/// smoothed out at every scale. A point is one bucket, at least one poll.
fn smooth_radius(span: u64, bucket: u64) -> usize {
    ((span / 60) / bucket.max(5)).clamp(3, 15) as usize
}

/// Gaussian average over time (sigma in seconds), cut at three sigma.
/// Neighbours further than `step` from the point are left out, as in
/// `smooth`, so real jumps stay sharp.
fn smooth_time(points: &[[f64; 2]], sigma: f64, step: f64) -> Segment {
    let reach = 3.0 * sigma;
    let mut lo = 0;
    points
        .iter()
        .map(|&[x, y]| {
            while points[lo][0] < x - reach {
                lo += 1;
            }
            let (sum, weight) = points[lo..]
                .iter()
                .take_while(|p| p[0] <= x + reach)
                .filter(|p| (p[1] - y).abs() <= step)
                .fold((0.0, 0.0), |(sum, weight), p| {
                    let d = (p[0] - x) / sigma;
                    let w = (-0.5 * d * d).exp();
                    (sum + w * p[1], weight + w)
                });
            [x, sum / weight]
        })
        .collect()
}

/// The line as a Catmull-Rom curve through its points, for drawing. Each
/// piece stays within the values of its two ends, so a sharp jump (a power
/// cut) does not overshoot.
fn spline(seg: &[[f64; 2]]) -> Segment {
    if seg.len() < 3 {
        return seg.to_vec();
    }
    let mut out = Vec::with_capacity((seg.len() - 1) * SPLINE_STEPS + 1);
    for i in 0..seg.len() - 1 {
        let p0 = seg[i.saturating_sub(1)];
        let (p1, p2) = (seg[i], seg[i + 1]);
        let p3 = seg[(i + 2).min(seg.len() - 1)];
        let (lo, hi) = (p1[1].min(p2[1]), p1[1].max(p2[1]));
        for k in 0..SPLINE_STEPS {
            let t = k as f64 / SPLINE_STEPS as f64;
            let (t2, t3) = (t * t, t * t * t);
            let at = |a: f64, b: f64, c: f64, d: f64| {
                0.5 * (2.0 * b
                    + (c - a) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                    + (3.0 * b - a - 3.0 * c + d) * t3)
            };
            let x = p1[0] + (p2[0] - p1[0]) * t;
            let y = at(p0[1], p1[1], p2[1], p3[1]).clamp(lo, hi);
            out.push([x, y]);
        }
    }
    out.push(seg[seg.len() - 1]);
    out
}

/// Local time minus UTC, in seconds, rounded to a quarter hour.
pub(crate) fn utc_offset_secs(now: u64) -> i64 {
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

/// What the time axis shows; short enough that the labels fit side by
/// side in a half-width plot (egui_plot hides labels that overlap).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Labels {
    /// "14:05".
    Time,
    /// "14:00", with "13.10" at midnight.
    TimeOrDate,
    /// "13.10".
    Date,
    /// "13.10.26".
    DateYear,
}

impl Labels {
    fn for_span(span: u64) -> Labels {
        match span / DAY as u64 {
            _ if span <= DAY as u64 / 2 => Labels::Time,
            0..=2 => Labels::TimeOrDate,
            3..=300 => Labels::Date,
            _ => Labels::DateYear,
        }
    }
}

/// Axis label for `x`, local seconds since 1970.
fn axis_label(x: f64, labels: Labels) -> String {
    let secs = x.round() as i64;
    let sod = secs.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(secs.div_euclid(DAY));
    match labels {
        Labels::Time => hm(sod),
        Labels::TimeOrDate if sod != 0 => hm(sod),
        Labels::TimeOrDate | Labels::Date => format!("{day:02}.{month:02}"),
        Labels::DateYear => format!("{day:02}.{month:02}.{:02}", year % 100),
    }
}

/// "10.10 14:05" for the tooltip.
fn date_time(x: f64) -> String {
    let secs = x.round() as i64;
    let (_, month, day) = civil_from_days(secs.div_euclid(DAY));
    format!("{day:02}.{month:02} {}", hm(secs.rem_euclid(DAY)))
}

fn hm(sod: i64) -> String {
    format!("{:02}:{:02}", sod / 3_600, sod % 3_600 / 60)
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
    const STEPS: [f64; 16] = [
        60.0,
        300.0,
        600.0,
        1_800.0,
        3_600.0,
        7_200.0,
        10_800.0,
        21_600.0,
        43_200.0,
        86_400.0,
        172_800.0,
        604_800.0,
        1_209_600.0,
        2_592_000.0,
        5_184_000.0,
        7_776_000.0,
    ];
    let width = input.bounds.1 - input.bounds.0;
    let i = STEPS
        .iter()
        // egui_plot hides labels closer than ~60 px; a half-width plot fits six.
        .position(|&s| width / s <= 6.0)
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
    Charge,
    Temperature,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Voltage,
        Kind::Load,
        Kind::Battery,
        Kind::Charge,
        Kind::Temperature,
    ];

    fn key(self) -> &'static str {
        match self {
            Kind::Voltage => "voltage",
            Kind::Load => "load",
            Kind::Battery => "battery",
            Kind::Charge => "charge",
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
    history: &Reader,
    period: &mut Period,
    order: &mut [Kind],
    nominal_battery_v: f32,
    ups_power_w: u32,
    now: u64,
) {
    let items: Vec<(Period, &str)> = Period::ALL.iter().map(|&p| (p, p.label())).collect();
    crate::theme::segmented(ui, period, &items);
    ui.add_space(crate::theme::GAP);

    let (samples, archive) = (&history.samples[..], &history.archive[..]);
    // Older than the detailed history: the archive's 5-minute means.
    let detailed_from = samples.first().map_or(u64::MAX, |s| s.t);
    let older = &archive[..archive.partition_point(|s| s.t < detailed_from)];
    let earliest = older.first().or(samples.first()).map(|s| s.t);
    let span = period.secs(earliest, now);
    let from = now.saturating_sub(span);
    if samples.last().or(older.last()).is_none_or(|s| s.t < from) {
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
    // Long periods are mostly short stretches between PC-off gaps; coarser
    // buckets smooth those out.
    let buckets = if span > 2 * DAY as u64 {
        MAX_BUCKETS / 2
    } else {
        MAX_BUCKETS
    };
    let bucket = span / buckets;
    let x_range = (from as i64 + offset) as f64..=(now as i64 + offset) as f64;
    let labels = Labels::for_span(span);
    let blue = Color32::from_rgb(66, 133, 244);
    let nominal = nominal_battery_v as f64;

    // Each plot sits in a card: title row with the move buttons, padding,
    // border and a gap below.
    let chrome = 20.0 + 2.0 + ui.spacing().item_spacing.y + 2.0 * 16.0 + 2.0 + crate::theme::GAP;
    // A wide window shows the plots two per row, in reading order.
    let cols = if ui.available_width() >= crate::theme::WIDE_MIN {
        2
    } else {
        1
    };
    let rows = order.len().div_ceil(cols) as f32;
    // `ui.columns` adds its own spacing below each row.
    let row_chrome = chrome
        + if cols > 1 {
            ui.spacing().item_spacing.y
        } else {
            0.0
        };
    let height = ((ui.available_height() - rows * row_chrome) / rows).max(120.0);
    let mut moved: Option<(usize, usize)> = None;
    let draw = |ui: &mut egui::Ui, kind: Kind, ctx: PlotCtx| match kind {
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
        Kind::Charge => plot(
            ui,
            kind,
            "Заряд батареї, %",
            |y| format!("{y:.0} %"),
            20.0,
            &[Series {
                name: "Заряд (від ДБЖ)",
                color: Color32::from_rgb(67, 160, 71),
                value: |s| s.charge_pct,
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
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let last = order.len() - 1;
            let mut cell = |ui: &mut egui::Ui, i: usize| {
                let ctx = PlotCtx {
                    span,
                    samples,
                    older,
                    height,
                    from,
                    bucket,
                    offset,
                    x_range: x_range.clone(),
                    labels,
                    can_up: i > 0,
                    can_down: i < last,
                };
                match draw(ui, order[i], ctx) {
                    Some(Move::Up) => moved = Some((i, i - 1)),
                    Some(Move::Down) => moved = Some((i, i + 1)),
                    None => {}
                }
            };
            for first in (0..order.len()).step_by(cols) {
                if cols == 1 {
                    cell(ui, first);
                } else {
                    ui.columns(cols, |c| {
                        for (j, ui) in c.iter_mut().enumerate() {
                            if first + j < order.len() {
                                cell(ui, first + j);
                            }
                        }
                    });
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
    span: u64,
    samples: &'a [Sample],
    /// Archive means from before `samples` begin.
    older: &'a [Sample],
    height: f32,
    from: u64,
    bucket: u64,
    offset: i64,
    x_range: RangeInclusive<f64>,
    labels: Labels,
    can_up: bool,
    can_down: bool,
}

/// Y range that fits every line of the period, at least `min_span` tall so
/// small jitter is not blown up, with headroom for the value labels.
fn fit_y(lines: &[(&Series, Vec<Segment>)], min_span: f64) -> RangeInclusive<f64> {
    let ys = lines
        .iter()
        .flat_map(|(_, segs)| segs.iter().flatten().map(|p| p[1]));
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
    let bottom = if non_negative {
        bottom.max(0.0)
    } else {
        bottom
    };
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
            .map(|s| {
                let mut segs = segments(
                    c.older,
                    c.from,
                    c.bucket,
                    c.offset,
                    2 * ARCHIVE_SECS + GAP_SECS,
                    s.value,
                );
                segs.extend(segments(
                    c.samples, c.from, c.bucket, c.offset, GAP_SECS, s.value,
                ));
                let segs = if c.span > 2 * DAY as u64 {
                    // Long periods are short stretches between PC-off gaps;
                    // one curve smoothed over time across them reads better.
                    let all = segs.concat();
                    vec![smooth_time(&all, c.span as f64 / 120.0, min_span / 2.0)]
                } else {
                    segs.iter()
                        .map(|g| smooth(g, smooth_radius(c.span, c.bucket), min_span / 2.0))
                        .collect()
                };
                (s, segs)
            })
            .collect();
        let y_range = fit_y(&lines, min_span);
        // Charge never exceeds 100 %, so the axis stops there.
        let y_range = if kind == Kind::Charge {
            *y_range.start()..=y_range.end().min(101.0)
        } else {
            y_range
        };
        let (y_top, y_span) = (*y_range.end(), y_range.end() - y_range.start());
        let hover_text = value_text.clone();
        let hover_points: Vec<(&'static str, Segment)> = lines
            .iter()
            .map(|(s, segs)| (s.name, segs.concat()))
            .collect();
        // Nearest sample counts only within a bucket or a polling gap.
        let step = if c.older.is_empty() {
            GAP_SECS
        } else {
            ARCHIVE_SECS
        };
        let tolerance = (c.bucket * 2).max(step) as f64;
        let labels = c.labels;
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
                axis_label(mark.value, labels)
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
                Some(text + &date_time(x))
            })
            .show(ui, |plot_ui| {
                plot_ui.set_plot_bounds_x(c.x_range);
                plot_ui.set_plot_bounds_y(y_range);
                for (s, segs) in &lines {
                    for seg in segs {
                        plot_ui.line(Line::new(s.name, spline(seg)).color(s.color).width(1.5));
                    }
                    // Gaps (PC off, no readings) are bridged with a faint
                    // dashed line, so the trend reads as one line.
                    for pair in segs.windows(2) {
                        if let (Some(&a), Some(&b)) = (pair[0].last(), pair[1].first()) {
                            // Unnamed, so the legend keeps the series colour.
                            plot_ui.line(
                                Line::new("", vec![a, b])
                                    .color(s.color.gamma_multiply(0.45))
                                    .width(1.0)
                                    .style(LineStyle::dashed_loose()),
                            );
                        }
                    }
                    // The latest value stays visible at the end of the line.
                    if let Some(&[x, y]) = segs.last().and_then(|seg| seg.last()) {
                        // Below the line when there is no room above it.
                        let anchor = if y > y_top - y_span * 0.15 {
                            egui::Align2::RIGHT_TOP
                        } else {
                            egui::Align2::RIGHT_BOTTOM
                        };
                        plot_ui.text(
                            Text::new(
                                "",
                                PlotPoint::new(x, y),
                                RichText::new(value_text(y)).color(s.color).strong(),
                            )
                            .anchor(anchor),
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
            charge_pct: v,
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
        let segs = segments(&s, 0, 1, 0, GAP_SECS, |s| s.input_v);
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
    fn averages_per_bucket() {
        let s = [
            sample(0, 5.0),
            sample(10, 1.0),
            sample(20, 9.0),
            sample(30, 4.0),
            sample(60, 7.0),
        ];
        let segs = segments(&s, 0, 60, 0, GAP_SECS, |s| s.input_v);
        assert_eq!(segs, vec![vec![[15.0, 4.75], [60.0, 7.0]]]);
    }

    #[test]
    fn time_smoothing_spans_gaps_but_keeps_jumps() {
        let pts = [[0.0, 40.0], [10.0, 44.0], [1000.0, 42.0], [1010.0, 10.0]];
        let out = smooth_time(&pts, 400.0, 5.0);
        // Points across the gap blend; the jump to 10 is left out.
        assert!(out[1][1] > 40.0 && out[1][1] < 44.0, "{:?}", out[1]);
        assert!((out[2][1] - 42.0).abs() < 2.0, "{:?}", out[2]);
        assert_eq!(out[3], [1010.0, 10.0]);
    }

    #[test]
    fn spline_passes_through_points_without_overshoot() {
        let seg = [[0.0, 10.0], [1.0, 10.0], [2.0, 0.0], [3.0, 0.0]];
        let out = spline(&seg);
        assert_eq!(out.len(), 3 * SPLINE_STEPS + 1);
        assert_eq!(out[0], [0.0, 10.0]);
        assert_eq!(out[SPLINE_STEPS], [1.0, 10.0]);
        assert_eq!(out[2 * SPLINE_STEPS], [2.0, 0.0]);
        assert_eq!(*out.last().unwrap(), [3.0, 0.0]);
        assert!(out.iter().all(|p| (0.0..=10.0).contains(&p[1])));
    }

    #[test]
    fn smooths_jitter_but_keeps_jumps() {
        let seg = [
            [0.0, 45.0],
            [1.0, 46.0],
            [2.0, 45.0],
            [3.0, 30.0],
            [4.0, 30.0],
        ];
        let out = smooth(&seg, 1, 5.0);
        assert_eq!(out[1], [1.0, 136.0 / 3.0]);
        // The jump from 45 to 30 is not averaged away.
        assert_eq!(out[2], [2.0, 45.5]);
        assert_eq!(out[3], [3.0, 30.0]);
    }

    #[test]
    fn breaks_on_missing_values() {
        let s = [sample(0, 1.0), sample(5, f32::NAN), sample(10, 3.0)];
        assert_eq!(
            segments(&s, 0, 1, 0, GAP_SECS, |s| s.temp_c),
            vec![vec![[0.0, 1.0]], vec![[10.0, 3.0]]]
        );
    }

    #[test]
    fn starts_at_from_and_applies_offset() {
        let s = [sample(10, 1.0), sample(20, 2.0)];
        assert_eq!(
            segments(&s, 15, 1, 3_600, GAP_SECS, |s| s.input_v),
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
            vec![
                Kind::Temperature,
                Kind::Load,
                Kind::Voltage,
                Kind::Battery,
                Kind::Charge
            ]
        );
        assert_eq!(parse_order(""), Kind::ALL.to_vec());
    }

    #[test]
    fn formats_time_labels() {
        // 2026-10-10 14:05:00 UTC
        let x = 1_791_641_100.0;
        let midnight = 1_791_590_400.0;
        assert_eq!(axis_label(x, Labels::Time), "14:05");
        assert_eq!(axis_label(x, Labels::TimeOrDate), "14:05");
        assert_eq!(axis_label(midnight, Labels::TimeOrDate), "10.10");
        assert_eq!(axis_label(x, Labels::Date), "10.10");
        assert_eq!(axis_label(x, Labels::DateYear), "10.10.26");
        assert_eq!(date_time(x), "10.10 14:05");
        assert_eq!(Labels::for_span(3_600), Labels::Time);
        assert_eq!(Labels::for_span(86_400), Labels::TimeOrDate);
        assert_eq!(Labels::for_span(7 * 86_400), Labels::Date);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
