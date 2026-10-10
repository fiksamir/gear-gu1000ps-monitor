//! Power-flow diagram on the status tab, laid out like the vendor's UPSmart:
//! input readings on the left, output readings on the right, and between
//! them two paths: the bypass through the AVR (on mains) and the battery
//! through the inverter (on battery).
//!
//! ```text
//!                ┌──── [Байпас · AVR] ────┐
//! Вхід ──────────┤                        ├────────── Вихід
//!                └── [Батарея] ── [Інвертор] ┘
//! ```

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Shadow, Shape, Stroke, StrokeKind, Vec2,
};

use gear_ups::protocol::Status;

use crate::tray::Level;

/// Below this the UPS output is considered off.
const OUTPUT_ON_V: f32 = 50.0;
const HEIGHT: f32 = 180.0;
/// Radius of the icon badges at the ends.
const BADGE_R: f32 = 26.0;
const CHEVRON_SPACING: f32 = 18.0;
/// Radius of the rounded wire corners.
const CORNER: f32 = 10.0;
/// Pixels per second the chevrons move.
const FLOW_SPEED: f64 = 25.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mains,
    Battery,
    Off,
}

/// What the diagram shows, derived from the Q1 status bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    pub mains_ok: bool,
    pub avr: bool,
    /// What feeds the load.
    pub source: Source,
    pub charging: bool,
    pub battery_low: bool,
    pub fault: bool,
    pub shutdown_pending: bool,
    pub test: bool,
}

pub fn flow(s: &Status) -> Flow {
    // A battery self-test runs the load from the inverter with mains present.
    let from_battery = s.on_battery() || s.test_in_progress();
    let source = if s.output_v < OUTPUT_ON_V {
        Source::Off
    } else if from_battery {
        Source::Battery
    } else {
        Source::Mains
    };
    Flow {
        mains_ok: !s.on_battery(),
        avr: source == Source::Mains && s.avr_active(),
        source,
        charging: !from_battery,
        battery_low: s.battery_low(),
        fault: s.ups_failed(),
        shutdown_pending: s.shutdown_active(),
        test: s.test_in_progress(),
    }
}

/// Load in watts from the percentage and the UPS's rated active power.
pub fn watts(load_pct: u32, ups_power_w: u32) -> u32 {
    ((load_pct as f32 / 100.0) * ups_power_w as f32).round() as u32
}

struct Palette {
    idle: Color32,
    text: Color32,
    mains: Color32,
    battery: Color32,
    charge: Color32,
    alarm: Color32,
}

/// Draws the diagram. Returns true while something is animated, so the
/// caller keeps repainting.
pub fn show(ui: &mut egui::Ui, s: &Status, nominal_battery_v: f32, ups_power_w: u32) -> bool {
    let f = flow(s);
    let width = ui.available_width().min(720.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);
    let pal = Palette {
        idle: Color32::from_rgb(214, 219, 226),
        text: crate::theme::TEXT,
        mains: Level::Ok.color(),
        battery: Level::Warn.color(),
        charge: Color32::from_rgb(66, 133, 244),
        alarm: Level::Alarm.color(),
    };
    // Wrap in f64: after days of uptime `time` as f32 is too coarse to animate.
    let phase = ui.input(|i| (i.time * FLOW_SPEED) % CHEVRON_SPACING as f64) as f32;
    let d = Drawer {
        painter: &painter,
        pal: &pal,
        phase,
    };

    let x = |fraction: f32| rect.left() + width * fraction;
    let top = rect.top() + 22.0;
    let mid = rect.top() + 80.0;
    let bottom = rect.top() + 138.0;
    let left = x(0.30);
    let right = x(0.70);
    let mains_at = Pos2::new(x(0.10), mid);
    let pc_at = Pos2::new(x(0.90), mid);
    let battery_rect = Rect::from_center_size(Pos2::new(x(0.41), bottom), Vec2::new(52.0, 28.0));

    let from_mains = f.source == Source::Mains;
    let from_battery = f.source == Source::Battery;
    let input_color = (f.mains_ok && (from_mains || f.charging)).then_some(pal.mains);
    let bypass_color = from_mains.then_some(pal.mains);
    let inverter_color = from_battery.then_some(pal.battery);
    let output_color = match f.source {
        Source::Mains => bypass_color,
        Source::Battery => inverter_color,
        Source::Off => None,
    };
    let badge_gap = Vec2::new(BADGE_R + 6.0, 0.0);

    d.wire(&[mains_at + badge_gap, Pos2::new(left, mid)], input_color);
    d.wire(
        &[
            Pos2::new(left, mid),
            Pos2::new(left, top),
            Pos2::new(right, top),
            Pos2::new(right, mid),
        ],
        bypass_color,
    );
    // On mains the charger feeds the battery from the input side.
    d.wire(
        &[
            Pos2::new(left, mid),
            Pos2::new(left, bottom),
            battery_rect.left_center(),
        ],
        f.charging.then_some(pal.charge),
    );
    d.wire(
        &[
            battery_rect.right_center(),
            Pos2::new(right, bottom),
            Pos2::new(right, mid),
        ],
        inverter_color,
    );
    d.wire(&[Pos2::new(right, mid), pc_at - badge_gap], output_color);
    d.node(Pos2::new(left, mid), input_color);
    d.node(Pos2::new(right, mid), output_color);

    let (bypass_text, bypass_fill) = if f.avr {
        ("Байпас · AVR стабілізує", Some(pal.battery))
    } else {
        ("Байпас · AVR", bypass_color)
    };
    d.pill(
        Pos2::new((left + right) / 2.0, top),
        bypass_text,
        bypass_fill,
    );
    let inverter_fill = if f.fault {
        Some(pal.alarm)
    } else {
        inverter_color
    };
    d.inverter(
        Pos2::new(x(0.585), bottom),
        if f.fault {
            "Інвертор · збій"
        } else {
            "Інвертор"
        },
        inverter_fill,
    );

    let charge = s.charge_estimate_pct(nominal_battery_v);
    let (bars, battery_color) = if from_battery {
        (
            charge.div_ceil(25).clamp(1, 4),
            if f.battery_low {
                pal.alarm
            } else {
                pal.battery
            },
        )
    } else if f.battery_low {
        (1, pal.alarm)
    } else if f.charging {
        (4, pal.charge)
    } else {
        (4, pal.idle)
    };
    d.battery(battery_rect, bars, battery_color);
    let battery_text = if from_battery {
        format!("{:.1} В · ~{charge} %", s.battery_v)
    } else {
        format!("{:.1} В", s.battery_v)
    };
    painter.text(
        battery_rect.center_bottom() + Vec2::new(0.0, 13.0),
        Align2::CENTER_CENTER,
        battery_text,
        FontId::proportional(12.5),
        battery_color,
    );

    // The mains socket on the left and the PC on the right, with readings.
    let mains_color = if f.mains_ok { pal.mains } else { pal.alarm };
    d.end(
        mains_at,
        None,
        "Мережа",
        mains_color,
        &format!("{:.1} В", s.input_v),
        &format!("{:.1} Гц", s.freq_hz),
    );
    let pc_color = if f.source == Source::Off {
        pal.alarm
    } else {
        output_color.unwrap_or(pal.idle)
    };
    d.end(
        pc_at,
        Some("🖥"),
        "Вихід на ПК",
        pc_color,
        &format!("{:.1} В", s.output_v),
        &format!("{} Вт · {} %", watts(s.load_pct, ups_power_w), s.load_pct),
    );

    let note = if f.shutdown_pending {
        Some(("Заплановано вимкнення виходу ДБЖ", pal.alarm))
    } else if f.source == Source::Off {
        Some(("Вихід ДБЖ вимкнено", pal.alarm))
    } else if f.test {
        Some(("Триває тест батареї", pal.battery))
    } else {
        None
    };
    if let Some((text, color)) = note {
        painter.text(
            rect.right_bottom() - Vec2::new(2.0, 6.0),
            Align2::RIGHT_CENTER,
            text,
            FontId::proportional(13.5),
            color,
        );
    }
    output_color.is_some() || input_color.is_some() || f.charging
}

/// Status lamps under the diagram, like UPSmart's.
pub fn indicators(ui: &mut egui::Ui, s: &Status) {
    let ok = Level::Ok.color();
    let warn = Level::Warn.color();
    let alarm = Level::Alarm.color();
    let lamps = [
        ("Зв'язок з ДБЖ", true, ok),
        ("ДБЖ у нормі", !s.ups_failed(), ok),
        ("Мережа в нормі", !s.on_battery(), ok),
        ("Низький заряд", s.battery_low(), alarm),
        ("Триває тест", s.test_in_progress(), warn),
        ("Заплановано вимкнення", s.shutdown_active(), alarm),
    ];
    ui.columns(3, |cols| {
        for (i, (label, on, color)) in lamps.into_iter().enumerate() {
            crate::theme::lamp(&mut cols[i % 3], label, on, color);
        }
    });
}

struct Drawer<'a> {
    painter: &'a egui::Painter,
    pal: &'a Palette,
    /// Distance the chevrons have moved, already wrapped to one spacing.
    phase: f32,
}

impl Drawer<'_> {
    /// A wire through `points` with rounded corners. An active wire is a
    /// solid coloured track with white chevrons running along it.
    fn wire(&self, points: &[Pos2], active: Option<Color32>) {
        let path = rounded(points, CORNER);
        let Some(color) = active else {
            self.painter
                .add(Shape::line(path, Stroke::new(4.0, self.pal.idle)));
            return;
        };
        self.painter
            .add(Shape::line(path.clone(), Stroke::new(8.0, color)));
        let stroke = Stroke::new(1.8, Color32::WHITE);
        let total: f32 = path.windows(2).map(|w| (w[1] - w[0]).length()).sum();
        let mut along = self.phase + 6.0;
        while along < total - 4.0 {
            if let Some((tip, dir)) = point_at(&path, along) {
                let back = tip - dir * 3.5;
                let side = dir.rot90() * 3.0;
                self.painter.line_segment([back + side, tip], stroke);
                self.painter.line_segment([back - side, tip], stroke);
            }
            along += CHEVRON_SPACING;
        }
    }

    /// A junction where the paths split.
    fn node(&self, at: Pos2, active: Option<Color32>) {
        let color = active.unwrap_or(self.pal.idle);
        self.painter
            .circle(at, 6.0, color, Stroke::new(2.5, Color32::WHITE));
    }

    /// A rounded label with a mains sine-wave icon, sitting on a path and
    /// filled when the path is active.
    fn pill(&self, center: Pos2, text: &str, fill: Option<Color32>) {
        let font = FontId::proportional(13.5);
        let galley = self
            .painter
            .layout_no_wrap(text.to_string(), font, Color32::WHITE);
        let icon_w = 18.0;
        let rect = Rect::from_center_size(center, Vec2::new(galley.size().x + icon_w + 34.0, 28.0));
        let shadow = Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_black_alpha(28),
        };
        self.painter.add(shadow.as_shape(rect, 14));
        let (bg, fg, border) = match fill {
            Some(c) => (c, Color32::WHITE, c),
            None => (Color32::WHITE, crate::theme::WEAK, self.pal.idle),
        };
        self.painter
            .rect(rect, 14.0, bg, Stroke::new(1.5, border), StrokeKind::Inside);
        let icon_left = rect.left() + 14.0;
        sine(
            self.painter,
            Pos2::new(icon_left, rect.center().y),
            icon_w,
            3.5,
            2.0,
            Stroke::new(1.8, fg),
        );
        let text_at = Pos2::new(
            icon_left + icon_w + 6.0,
            rect.center().y - galley.size().y / 2.0,
        );
        self.painter
            .galley_with_override_text_color(text_at, galley, fg);
    }

    /// The inverter as a round node with the DC → AC symbol and a label below.
    fn inverter(&self, at: Pos2, label: &str, fill: Option<Color32>) {
        let r = 22.0;
        let shadow = Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_black_alpha(28),
        };
        self.painter
            .add(shadow.as_shape(Rect::from_center_size(at, Vec2::splat(2.0 * r)), r as u8));
        let (bg, fg, ring) = match fill {
            Some(c) => (c, Color32::WHITE, c),
            None => (Color32::WHITE, crate::theme::WEAK, self.pal.idle),
        };
        self.painter.circle(at, r, bg, Stroke::new(2.5, ring));
        // A square with a diagonal: "=" (DC) above it, "~" (AC) below.
        let sq = Rect::from_center_size(at, Vec2::splat(20.0));
        let stroke = Stroke::new(1.6, fg);
        self.painter
            .rect_stroke(sq, 3.0, stroke, StrokeKind::Middle);
        self.painter.line_segment(
            [
                sq.left_bottom() + Vec2::new(1.0, -1.0),
                sq.right_top() + Vec2::new(-1.0, 1.0),
            ],
            stroke,
        );
        for dy in [5.0, 8.5] {
            let y = sq.top() + dy;
            self.painter.line_segment(
                [Pos2::new(sq.left() + 3.0, y), Pos2::new(sq.left() + 9.5, y)],
                stroke,
            );
        }
        sine(
            self.painter,
            Pos2::new(sq.right() - 10.0, sq.bottom() - 5.5),
            7.5,
            2.0,
            1.0,
            stroke,
        );
        self.painter.text(
            at + Vec2::new(0.0, r + 11.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(12.5),
            fill.unwrap_or(crate::theme::WEAK),
        );
    }

    /// A battery with `bars` of 4 charge bars lit.
    fn battery(&self, rect: Rect, bars: u32, color: Color32) {
        let shadow = Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_black_alpha(28),
        };
        self.painter.add(shadow.as_shape(rect, 6));
        self.painter.rect(
            rect,
            6.0,
            Color32::WHITE,
            Stroke::new(2.0, color),
            StrokeKind::Inside,
        );
        let cap = Rect::from_center_size(
            rect.right_center() + Vec2::new(3.5, 0.0),
            Vec2::new(4.0, 11.0),
        );
        self.painter.rect_filled(cap, 2.0, color);
        let inner = rect.shrink(5.0);
        let gap = 2.5;
        let bar_w = (inner.width() - 3.0 * gap) / 4.0;
        for i in 0..4 {
            let r = Rect::from_min_size(
                Pos2::new(inner.left() + i as f32 * (bar_w + gap), inner.top()),
                Vec2::new(bar_w, inner.height()),
            );
            let lit = (i as u32) < bars;
            self.painter.rect_filled(
                r,
                2.0,
                if lit {
                    color
                } else {
                    color.gamma_multiply(0.15)
                },
            );
        }
    }

    /// One end of the diagram: an icon badge with a caption above and two
    /// readings (main and secondary) below.
    fn end(
        &self,
        at: Pos2,
        icon: Option<&str>,
        caption: &str,
        color: Color32,
        main: &str,
        secondary: &str,
    ) {
        self.painter
            .circle(at, BADGE_R, Color32::WHITE, Stroke::new(2.5, color));
        match icon {
            Some(glyph) => crate::theme::badge(self.painter, at, BADGE_R - 2.5, glyph, color),
            None => {
                self.painter
                    .circle_filled(at, BADGE_R - 2.5, color.gamma_multiply(0.14));
                pylon(self.painter, at, 15.0, color);
            }
        }
        let below = at.y + BADGE_R;
        self.painter.text(
            Pos2::new(at.x, at.y - BADGE_R - 11.0),
            Align2::CENTER_CENTER,
            caption,
            FontId::proportional(12.5),
            crate::theme::WEAK,
        );
        self.painter.text(
            Pos2::new(at.x, below + 15.0),
            Align2::CENTER_CENTER,
            main,
            FontId::proportional(18.0),
            self.pal.text,
        );
        self.painter.text(
            Pos2::new(at.x, below + 34.0),
            Align2::CENTER_CENTER,
            secondary,
            FontId::proportional(13.0),
            crate::theme::WEAK,
        );
    }
}

/// A sine wave starting at `start` (left end, on the axis), `width` long.
fn sine(p: &egui::Painter, start: Pos2, width: f32, amplitude: f32, periods: f32, stroke: Stroke) {
    let points = (0..=24)
        .map(|i| {
            let t = i as f32 / 24.0;
            let y = -(t * periods * std::f32::consts::TAU).sin() * amplitude;
            start + Vec2::new(t * width, y)
        })
        .collect();
    p.add(Shape::line(points, stroke));
}

/// A power-line pylon `h` half-height around `c`.
fn pylon(p: &egui::Painter, c: Pos2, h: f32, color: Color32) {
    let pt = |x: f32, y: f32| c + Vec2::new(x * h, y * h);
    // Half-width of the tower body at height `y` (-1 top, 1 bottom).
    let leg = |y: f32| 0.08 + 0.5 * (y + 1.0) / 2.0;
    let stroke = Stroke::new(1.6, color);
    let line = |a: Pos2, b: Pos2| p.line_segment([a, b], stroke);
    line(pt(-leg(-1.0), -1.0), pt(-leg(1.0), 1.0));
    line(pt(leg(-1.0), -1.0), pt(leg(1.0), 1.0));
    line(pt(-leg(-1.0), -1.0), pt(leg(-1.0), -1.0));
    // Cross-arms with hanging insulators.
    for (y, half) in [(-0.62, 0.62), (-0.18, 0.82)] {
        line(pt(-half, y), pt(half, y));
        line(pt(-half, y), pt(-half, y + 0.18));
        line(pt(half, y), pt(half, y + 0.18));
    }
    // Bracing.
    for (y0, y1) in [(-0.62, -0.18), (-0.18, 0.4), (0.4, 1.0)] {
        line(pt(-leg(y0), y0), pt(leg(y1), y1));
        line(pt(leg(y0), y0), pt(-leg(y1), y1));
    }
}

/// `points` with each inner corner replaced by a short curve of radius `r`.
fn rounded(points: &[Pos2], r: f32) -> Vec<Pos2> {
    let mut out = vec![points[0]];
    for w in points.windows(3) {
        let (a, p, b) = (w[0], w[1], w[2]);
        let r = r.min((a - p).length() / 2.0).min((b - p).length() / 2.0);
        let start = p + (a - p).normalized() * r;
        let end = p + (b - p).normalized() * r;
        for i in 0..=6 {
            let t = i as f32 / 6.0;
            // Quadratic Bézier with the corner as the control point.
            let q = start.to_vec2() * (1.0 - t) * (1.0 - t)
                + p.to_vec2() * 2.0 * t * (1.0 - t)
                + end.to_vec2() * t * t;
            out.push(q.to_pos2());
        }
    }
    out.push(points[points.len() - 1]);
    out
}

/// The point `dist` along `path` and the path's direction there.
fn point_at(path: &[Pos2], mut dist: f32) -> Option<(Pos2, Vec2)> {
    for w in path.windows(2) {
        let len = (w[1] - w[0]).length();
        if len <= f32::EPSILON {
            continue;
        }
        if dist <= len {
            let dir = (w[1] - w[0]) / len;
            return Some((w[0] + dir * dist, dir));
        }
        dist -= len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(bits: &str, output_v: f32) -> Status {
        Status::parse(&format!(
            "(226.0 000.0 {output_v:05.1} 012 50.0 27.1 22.0 {bits}"
        ))
        .unwrap()
    }

    #[test]
    fn on_mains() {
        let f = flow(&status("00001000", 226.0));
        assert_eq!(f.source, Source::Mains);
        assert!(f.mains_ok && f.charging && !f.avr && !f.fault);
    }

    #[test]
    fn on_mains_with_avr() {
        let f = flow(&status("00101000", 226.0));
        assert_eq!(f.source, Source::Mains);
        assert!(f.avr);
    }

    #[test]
    fn on_battery() {
        let f = flow(&status("10001000", 220.0));
        assert_eq!(f.source, Source::Battery);
        assert!(!f.mains_ok && !f.charging && !f.battery_low);
    }

    #[test]
    fn on_battery_low() {
        let f = flow(&status("11001000", 220.0));
        assert_eq!(f.source, Source::Battery);
        assert!(f.battery_low);
    }

    #[test]
    fn battery_test() {
        let f = flow(&status("00001100", 226.0));
        assert_eq!(f.source, Source::Battery);
        assert!(f.mains_ok && f.test && !f.charging);
    }

    #[test]
    fn shutdown_pending_keeps_feeding() {
        let f = flow(&status("00001010", 226.0));
        assert_eq!(f.source, Source::Mains);
        assert!(f.shutdown_pending);
    }

    #[test]
    fn output_off_still_charges() {
        let f = flow(&status("00001000", 0.0));
        assert_eq!(f.source, Source::Off);
        assert!(f.mains_ok && f.charging);
    }

    #[test]
    fn load_in_watts() {
        assert_eq!(watts(14, 600), 84);
        assert_eq!(watts(0, 600), 0);
        assert_eq!(watts(100, 600), 600);
    }

    #[test]
    fn ups_failed() {
        let f = flow(&status("00011000", 226.0));
        assert!(f.fault);
    }
}
