//! Notable events picked out of the service log (gear-ups.log) for the
//! window's "Події" tab: power cuts with their duration, low battery, PC and
//! UPS shutdowns, overheating, a hung USB bridge.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    PowerLost,
    PowerBack,
    BatteryLow,
    PcShutdown,
    UpsOff,
    Hot,
    Cooled,
    Frozen,
    Unfrozen,
    Fault,
    FaultGone,
    Remote,
}

impl Kind {
    /// Bad news (red/orange in the window) as opposed to recovery.
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            Kind::PowerLost
                | Kind::BatteryLow
                | Kind::PcShutdown
                | Kind::UpsOff
                | Kind::Hot
                | Kind::Frozen
                | Kind::Fault
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Local time as written in the log, "YYYY-MM-DD HH:MM:SS".
    pub time: String,
    /// The same time as seconds since 1970, for durations and periods.
    pub at: i64,
    pub kind: Kind,
    pub title: &'static str,
    /// Rest of the log line: readings, reasons.
    pub detail: String,
    /// For `PowerBack`: how long the UPS ran on battery.
    pub on_battery_secs: Option<i64>,
}

/// Log message prefixes and the event they mean.
const RULES: &[(&str, Kind, &str)] = &[
    (
        "мережі немає, живлення від батареї",
        Kind::PowerLost,
        "Зникла мережа",
    ),
    ("мережу відновлено", Kind::PowerBack, "Мережу відновлено"),
    (
        "низький заряд батареї",
        Kind::BatteryLow,
        "Низький заряд батареї",
    ),
    ("вимикаю ПК", Kind::PcShutdown, "Служба вимикає ПК"),
    ("вимкнення ДБЖ через", Kind::UpsOff, "Вимкнення ДБЖ"),
    ("ДБЖ гарячий", Kind::Hot, "ДБЖ перегрівся"),
    (
        "температура ДБЖ знизилась",
        Kind::Cooled,
        "Температура ДБЖ у нормі",
    ),
    (
        "дані від ДБЖ не оновлюються",
        Kind::Frozen,
        "USB-контролер ДБЖ завис",
    ),
    (
        "дані від ДБЖ знову оновлюються",
        Kind::Unfrozen,
        "Дані від ДБЖ знову оновлюються",
    ),
    (
        "ДБЖ повідомляє про несправність",
        Kind::Fault,
        "Несправність ДБЖ",
    ),
    (
        "несправність ДБЖ зникла",
        Kind::FaultGone,
        "Несправність ДБЖ зникла",
    ),
    ("команда з Telegram", Kind::Remote, "Команда з Telegram"),
];

fn rule(text: &str) -> Option<&'static (&'static str, Kind, &'static str)> {
    RULES.iter().find(|r| text.starts_with(r.0))
}

/// Events of the whole log, oldest first.
pub fn parse(log: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut lost_at: Option<i64> = None;
    for line in log.lines() {
        let Some((time, text)) = line.split_at_checked(19) else {
            continue;
        };
        let Some(at) = parse_time(time) else {
            continue;
        };
        let text = text.trim_start();
        // Some messages carry the trigger first: "сон: вимкнення ДБЖ через …".
        let (body, found) = match rule(text) {
            Some(r) => (text, Some(r)),
            None => match text.split_once(": ") {
                Some((_, rest)) => (rest, rule(rest)),
                None => (text, None),
            },
        };
        let Some(&(prefix, kind, title)) = found else {
            // A service restart forgets an outage in progress.
            if text.starts_with("служба запускається") {
                lost_at = None;
            }
            continue;
        };
        let on_battery_secs = match kind {
            Kind::PowerLost => {
                lost_at = Some(at);
                None
            }
            Kind::PowerBack => lost_at.take().map(|from| at - from),
            _ => None,
        };
        out.push(Event {
            time: time.to_string(),
            at,
            kind,
            title,
            detail: detail(&body[prefix.len()..]),
            on_battery_secs,
        });
    }
    out
}

/// The message after its prefix, without the command trace the service adds
/// to a UPS shutdown ("…: надіслано індекс 0x10, відповідь \"\"").
fn detail(rest: &str) -> String {
    let rest = rest.trim_start_matches([':', ',', ' ']);
    match rest.find(": надіслано індекс") {
        Some(i) => rest[..i].to_string(),
        None => rest.to_string(),
    }
}

/// Power cuts that ended at or after `since` (same clock as `Event::at`),
/// and their total time on battery.
pub fn outages_since(events: &[Event], since: i64) -> (usize, i64) {
    events
        .iter()
        .filter(|e| e.at >= since && e.kind == Kind::PowerBack)
        .fold((0, 0), |(n, total), e| {
            (n + 1, total + e.on_battery_secs.unwrap_or(0))
        })
}

/// "YYYY-MM-DD HH:MM:SS" as seconds since 1970 (no time zone applied).
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b' ' || b[13] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// "1 год 5 хв", "7 хв", "40 с".
pub fn duration_text(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s} с"),
        (0, _) => format!("{m} хв"),
        (_, 0) => format!("{h} год"),
        _ => format!("{h} год {m} хв"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
2026-10-12 19:27:47  служба запускається, налаштування: Config { }
2026-10-12 19:35:29  мережі немає, живлення від батареї: вхід 190.0 В [ВІД БАТАРЕЇ]
2026-10-12 19:42:00  мережу відновлено: вхід 226.0 В, вихід 226.0 В
2026-10-12 20:00:00  ДБЖ гарячий: 56 °C (поріг 55 °C)
2026-10-12 22:59:20  сон: вимкнення ДБЖ через 30 с (лишиться вимкненим): надіслано індекс 0x10
2026-10-12 23:01:29  стан: вхід 227.0 В
2026-10-13 01:00:00  мережі немає, живлення від батареї: вхід 0 В
2026-10-13 01:00:05  служба запускається, налаштування: Config { }
2026-10-13 01:10:00  мережу відновлено: вхід 226.0 В
";

    #[test]
    fn picks_events_and_outage_durations() {
        let ev = parse(LOG);
        let kinds: Vec<Kind> = ev.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::PowerLost,
                Kind::PowerBack,
                Kind::Hot,
                Kind::UpsOff,
                Kind::PowerLost,
                Kind::PowerBack
            ]
        );
        assert_eq!(ev[1].on_battery_secs, Some(6 * 60 + 31));
        assert_eq!(ev[1].detail, "вхід 226.0 В, вихід 226.0 В");
        assert_eq!(ev[0].detail, "вхід 190.0 В [ВІД БАТАРЕЇ]");
        assert_eq!(ev[3].detail, "30 с (лишиться вимкненим)");
        // The restart in between loses the start of the second outage.
        assert_eq!(ev[5].on_battery_secs, None);
        let since = parse_time("2026-10-12 00:00:00").unwrap();
        assert_eq!(outages_since(&ev, since), (2, 391));
    }

    #[test]
    fn picks_telegram_commands() {
        let ev = parse(
            "2026-10-29 15:20:01  команда з Telegram: запит стану\n\
             2026-10-29 15:21:30  команда з Telegram: вимкнення ПК через 60 с\n\
             2026-10-29 15:21:40  Telegram: зв'язок відновлено\n",
        );
        let got: Vec<(Kind, &str)> = ev.iter().map(|e| (e.kind, e.detail.as_str())).collect();
        assert_eq!(
            got,
            [
                (Kind::Remote, "запит стану"),
                (Kind::Remote, "вимкнення ПК через 60 с")
            ]
        );
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_time("2026-10-12 19:35:29"), Some(1791833729));
        assert_eq!(parse_time("2026-13-12 19:35:29"), None);
        assert_eq!(parse_time("не дата"), None);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration_text(40), "40 с");
        assert_eq!(duration_text(391), "6 хв");
        assert_eq!(duration_text(3600), "1 год");
        assert_eq!(duration_text(3900), "1 год 5 хв");
    }
}
