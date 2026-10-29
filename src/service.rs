//! The GearUps Windows service: polls the UPS, powers the PC off on a long
//! outage, and turns the UPS output off when Windows powers off.

use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::battery::{self, Gauge};
use crate::config::{self, Config};
use crate::events;
use crate::hid::Ups;
use crate::history;
use crate::protocol::{self, Ratings, Status};
use crate::snapshot::{self, Snapshot};
use crate::telegram::{self, Note, Remote};
use crate::winutil::{self, log, ShutdownKind};

pub const SERVICE_NAME: &str = "GearUps";

/// How far back a "power off" event 1074 may be when Windows shuts down
/// (apps closing can take a while) and when the system suspends (hybrid
/// shutdown with Fast Startup; kept short so a later sleep is not mistaken).
const PRESHUTDOWN_EVENT_WINDOW_SECS: u32 = 300;
const SUSPEND_EVENT_WINDOW_SECS: u32 = 120;
/// How far back winlogon's hibernate request (event 187) may be when the
/// suspend notification arrives; it is logged a fraction of a second before.
const HYBRID_EVENT_WINDOW_SECS: u32 = 30;
/// How long to wait for that event to reach the log: it is stored a moment
/// after the suspend notification, and the PC sleeps soon after that.
const HYBRID_EVENT_WAIT: Duration = Duration::from_millis(1500);
/// How long after start or resume the service waits before restoring the
/// beeper state (see `restore_beeper`).
const BEEPER_RESTORE_DELAY: Duration = Duration::from_secs(300);
/// Still running this long after asking Windows to power off means the
/// shutdown was aborted elsewhere (`shutdown /a`) or blocked.
const SHUTDOWN_STUCK_AFTER: Duration = Duration::from_secs(180);

/// What wakes the main loop besides the poll timer.
enum Signal {
    Stop,
    Remote(Remote),
}

define_windows_service!(ffi_service_main, service_main);

pub fn run() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        log(&format!("збій служби: {e}"));
    }
}

fn run_service() -> windows_service::Result<()> {
    let cfg = Config::load();
    log(&format!("служба запускається, налаштування: {cfg:?}"));
    history::backfill_archive(snapshot::unix_now());
    let mut poll_interval = Duration::from_secs(cfg.poll_interval_secs);
    let mut cfg_modified = config::modified();
    let (signal_tx, signal_rx) = mpsc::channel::<Signal>();
    let bot = {
        let tx = signal_tx.clone();
        telegram::spawn(move |r| {
            let _ = tx.send(Signal::Remote(r));
        })
    };
    let monitor = Arc::new(Mutex::new(Monitor::new(cfg, bot)));

    let handler_monitor = Arc::clone(&monitor);
    let status_handle = service_control_handler::register(SERVICE_NAME, move |ctrl| match ctrl {
        ServiceControl::Stop => {
            let _ = signal_tx.send(Signal::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Preshutdown => {
            if let Ok(mut m) = handler_monitor.lock() {
                m.on_power_down("завершення роботи", PRESHUTDOWN_EVENT_WINDOW_SECS, false);
            }
            let _ = signal_tx.send(Signal::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::PowerEvent(param) => {
            if let Ok(mut m) = handler_monitor.lock() {
                match param {
                    PowerEventParam::Suspend => {
                        m.on_power_down("сон", SUSPEND_EVENT_WINDOW_SECS, true)
                    }
                    PowerEventParam::ResumeAutomatic | PowerEventParam::ResumeSuspend => {
                        m.on_resume()
                    }
                    _ => {}
                }
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let set_state = |state: ServiceState, accept: ServiceControlAccept| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };
    set_state(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::POWER_EVENT,
    )?;

    let mut next_poll = Instant::now();
    loop {
        // Not due yet when woken early by a command from Telegram.
        let due = Instant::now() >= next_poll;
        if let Some(mut m) = due.then(|| monitor.lock().ok()).flatten() {
            let modified = config::modified();
            if modified != cfg_modified {
                cfg_modified = modified;
                let cfg = Config::load();
                if cfg != m.cfg {
                    log(&format!("налаштування змінено: {cfg:?}"));
                    poll_interval = Duration::from_secs(cfg.poll_interval_secs);
                    if cfg.battery_ah != m.cfg.battery_ah && m.gauge.capacity_ah.is_some() {
                        log("ємність батареї змінено в налаштуваннях: уточнення за розрядами почнеться заново");
                        m.gauge.forget();
                        m.gauge.save();
                    }
                    if cfg.beeper_on != m.cfg.beeper_on {
                        // The window has just toggled it; give the UPS time
                        // to report the new state before checking it.
                        m.beeper_not_before = Instant::now() + Duration::from_secs(60);
                        m.beeper_tried = false;
                    }
                    m.cfg = cfg;
                }
            }
            m.poll();
        }
        if due {
            next_poll = Instant::now() + poll_interval;
        }
        match signal_rx.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Signal::Remote(r)) => {
                if let Ok(mut m) = monitor.lock() {
                    m.remote(r);
                }
            }
            _ => break,
        }
    }

    log("службу зупинено");
    let _ = std::fs::remove_file(snapshot::path());
    set_state(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

struct Monitor {
    cfg: Config,
    ups: Option<Ups>,
    nominal_battery_v: f32,
    last: Option<Status>,
    last_error: Option<String>,
    on_battery_since: Option<Instant>,
    /// Charge counted from the load, and the capacity learned from discharges.
    gauge: Gauge,
    pc_shutdown_requested: bool,
    pc_shutdown_at: Instant,
    /// When Windows will power off (unix seconds), for the window's countdown.
    pc_shutdown_deadline: u64,
    ups_off_sent: bool,
    /// Last raw Q1 reply and when it last changed: the USB bridge can hang
    /// and keep answering with the same stale line.
    last_raw: Option<String>,
    archiver: history::Archiver,
    raw_changed_at: Instant,
    frozen: bool,
    hot: bool,
    /// Restoring the configured beeper state: not before this moment, and
    /// whether the one try for this start or resume is used up.
    beeper_not_before: Instant,
    beeper_tried: bool,
    bot: telegram::Handle,
    /// The outage shutdown was cancelled from Telegram: not again until
    /// mains returns (a low battery still shuts down).
    timeout_cancelled: bool,
}

/// Mains voltage and load jitter every few seconds, so a Q1 line that stays
/// byte-for-byte identical this long means the USB bridge has hung.
const FROZEN_AFTER: Duration = Duration::from_secs(10 * 60);

impl Monitor {
    fn new(cfg: Config, bot: telegram::Handle) -> Monitor {
        Monitor {
            cfg,
            ups: None,
            nominal_battery_v: 24.0,
            last: None,
            last_error: None,
            on_battery_since: None,
            gauge: Gauge::load(),
            pc_shutdown_requested: false,
            pc_shutdown_at: Instant::now(),
            pc_shutdown_deadline: 0,
            ups_off_sent: false,
            last_raw: None,
            archiver: history::Archiver::default(),
            raw_changed_at: Instant::now(),
            frozen: false,
            hot: false,
            beeper_not_before: Instant::now() + BEEPER_RESTORE_DELAY,
            beeper_tried: false,
            bot,
            timeout_cancelled: false,
        }
    }

    fn notify(&self, note: Note) {
        let _ = self.bot.notes.send(note);
    }

    fn connect(&mut self) -> bool {
        if self.ups.is_some() {
            return true;
        }
        match Ups::open() {
            Ok(ups) => {
                if let Ok(r) = ups
                    .command(protocol::IDX_RATINGS)
                    .map_err(|e| e.to_string())
                    .and_then(|s| Ratings::parse(&s))
                {
                    self.nominal_battery_v = r.battery_v;
                    log(&format!(
                        "ДБЖ підключено: {} В, {} А, батарея {} В, {} Гц",
                        r.voltage, r.current, r.battery_v, r.freq_hz
                    ));
                } else {
                    log("ДБЖ підключено (номінали недоступні)");
                }
                self.ups = Some(ups);
                self.last_error = None;
                true
            }
            Err(e) => {
                let msg = e.to_string();
                if self.last_error.as_deref() != Some(msg.as_str()) {
                    log(&format!("не вдається відкрити ДБЖ: {msg}"));
                }
                self.last_error = Some(msg);
                false
            }
        }
    }

    fn query(&mut self) -> Option<Status> {
        if !self.connect() {
            return None;
        }
        let reply = self.ups.as_ref()?.command(protocol::IDX_STATUS);
        match reply
            .map_err(|e| e.to_string())
            .and_then(|r| Status::parse(&r).map(|s| (r, s)))
        {
            Ok((raw, mut s)) => {
                self.track_freshness(raw);
                s.charge_pct = self
                    .ups
                    .as_ref()?
                    .command(protocol::IDX_CHARGE)
                    .ok()
                    .and_then(|r| protocol::parse_charge(&r));
                Some(s)
            }
            Err(e) => {
                log(&format!("ДБЖ не відповів на запит стану: {e}"));
                self.last_error = Some(e);
                self.ups = None;
                None
            }
        }
    }

    fn track_freshness(&mut self, raw: String) {
        if self.last_raw.as_ref() != Some(&raw) {
            if self.frozen {
                log("дані від ДБЖ знову оновлюються");
                self.notify(Note::new("Дані від ДБЖ знову оновлюються."));
                self.frozen = false;
            }
            self.last_raw = Some(raw);
            self.raw_changed_at = Instant::now();
        } else if !self.frozen && self.raw_changed_at.elapsed() >= FROZEN_AFTER {
            log(&format!(
                "дані від ДБЖ не оновлюються вже {} хв (USB-контролер ДБЖ завис): \
                 від'єднайте USB-кабель ДБЖ на 10 с",
                FROZEN_AFTER.as_secs() / 60
            ));
            self.notify(Note::new(
                "⚠ Дані від ДБЖ не оновлюються: USB-контролер ДБЖ завис, і зникнення мережі \
                 не буде помічено. Від'єднайте USB-кабель ДБЖ на 10 с.",
            ));
            self.frozen = true;
        }
    }

    fn send(&mut self, index: u32, what: &str) -> bool {
        if !self.connect() {
            log(&format!("{what}: ДБЖ недоступний"));
            return false;
        }
        match self.ups.as_ref().map(|u| u.command(index)) {
            Some(Ok(reply)) => {
                log(&format!(
                    "{what}: надіслано індекс 0x{index:02x}, відповідь {:?}",
                    reply.trim()
                ));
                true
            }
            Some(Err(e)) => {
                log(&format!("{what}: помилка: {e}"));
                self.ups = None;
                false
            }
            None => false,
        }
    }

    fn shutdown_started(&mut self, grace_secs: u32) {
        self.pc_shutdown_requested = true;
        self.pc_shutdown_at = Instant::now();
        self.pc_shutdown_deadline = snapshot::unix_now() + grace_secs as u64;
    }

    fn poll(&mut self) {
        if self.pc_shutdown_requested && self.pc_shutdown_at.elapsed() >= SHUTDOWN_STUCK_AFTER {
            log("вимкнення ПК не відбулося, служба знову стежить за живленням");
            self.pc_shutdown_requested = false;
        }
        if let Some(s) = self.query() {
            // Only fresh readings go to the history; publish() also runs
            // from on_power_down with the last known status.
            let sample = history::Sample::from_status(snapshot::unix_now(), &s);
            history::append(sample);
            self.archiver.push(sample);
            self.handle(s);
        }
        self.publish();
    }

    fn handle(&mut self, s: Status) {
        let prev = self.last.take();
        self.restore_beeper(&s);

        match &prev {
            None => {
                log(&format!("стан: {}", describe(&s, self.nominal_battery_v)));
                // A pending shutdown at service start (or after resume) is left
                // over from a power-down that did not happen; never keep it.
                if s.shutdown_active() && !self.ups_off_sent {
                    self.send(protocol::IDX_CANCEL, "скасування застарілого вимкнення ДБЖ");
                }
            }
            Some(p) => {
                if s.on_battery() != p.on_battery() {
                    log(&format!(
                        "{}: {}",
                        if s.on_battery() {
                            "мережі немає, живлення від батареї"
                        } else {
                            "мережу відновлено"
                        },
                        describe(&s, self.nominal_battery_v)
                    ));
                    if s.on_battery() {
                        self.notify_power_lost(&s);
                    } else {
                        let away = self.on_battery_since.map(|t| t.elapsed().as_secs());
                        self.notify(Note::new(match away {
                            Some(secs) => format!(
                                "✅ Мережу відновлено. Світла не було {}.",
                                events::duration_text(secs as i64)
                            ),
                            None => "✅ Мережу відновлено.".into(),
                        }));
                    }
                }
                if s.battery_low() && !p.battery_low() {
                    log("низький заряд батареї");
                    // With the shutdown below the PC goes off at once; that
                    // message is sent before it.
                    if !(s.on_battery() && self.cfg.shutdown_on_battery_low) {
                        self.notify(Note::new("🪫 Низький заряд батареї ДБЖ."));
                    }
                }
                if s.ups_failed() != p.ups_failed() {
                    let text = if s.ups_failed() {
                        "ДБЖ повідомляє про несправність"
                    } else {
                        "несправність ДБЖ зникла"
                    };
                    log(text);
                    self.notify(Note::new(format!("⚠ {text}.")));
                }
            }
        }

        let limit = self.cfg.temp_alarm_c as f32;
        if limit > 0.0 && !self.hot && s.temp_c >= limit {
            log(&format!(
                "ДБЖ гарячий: {:.0} °C (поріг {limit:.0} °C)",
                s.temp_c
            ));
            self.notify(Note::new(format!(
                "🌡 ДБЖ перегрівається: {:.0} °C. Перевірте кулер і вентиляцію.",
                s.temp_c
            )));
            self.hot = true;
        } else if self.hot && (limit == 0.0 || s.temp_c <= limit - 3.0) {
            log(&format!("температура ДБЖ знизилась до {:.0} °C", s.temp_c));
            self.notify(Note::new(format!(
                "Температура ДБЖ знизилась до {:.0} °C.",
                s.temp_c
            )));
            self.hot = false;
        }

        self.count_charge(&s);

        if s.on_battery() {
            let since = *self.on_battery_since.get_or_insert_with(Instant::now);
            let limit = self.cfg.on_battery_shutdown_secs;
            let timed_out = !self.timeout_cancelled
                && limit > 0
                && since.elapsed() >= Duration::from_secs(limit);
            let low = self.cfg.shutdown_on_battery_low && s.battery_low();
            if (timed_out || low) && !self.pc_shutdown_requested {
                let why = if low {
                    "низький заряд батареї"
                } else {
                    "надто довго без мережі"
                };
                log(&format!("вимикаю ПК: {why}"));
                // A low battery leaves well under a minute (28.10: power was
                // cut 55 s after the flag), so shut down without a countdown.
                let grace = if low { 0 } else { 60 };
                if low {
                    // Give the message a few seconds to leave before the PC goes off.
                    let (done, sent) = mpsc::channel();
                    self.notify(
                        Note::new("🪫 Низький заряд батареї: вимикаю ПК.").signal_when_done(done),
                    );
                    let _ = sent.recv_timeout(Duration::from_secs(5));
                } else {
                    self.notify(
                        Note::new(format!(
                            "⏻ Мережі немає {}: ПК вимкнеться через {grace} с.",
                            events::duration_text(limit as i64)
                        ))
                        .with_cancel_button(),
                    );
                }
                match winutil::shutdown_pc(
                    &format!("ДБЖ GEAR: {why}. Комп'ютер буде вимкнено."),
                    grace,
                ) {
                    Ok(()) => self.shutdown_started(grace),
                    Err(e) => log(&format!("InitiateShutdown не вдався: {e}")),
                }
            }
        } else {
            self.on_battery_since = None;
            self.timeout_cancelled = false;
        }

        self.last = Some(s);
    }

    fn count_charge(&mut self, s: &Status) {
        let now = snapshot::unix_now();
        let configured = self.cfg.battery_ah as f32;
        let today = || {
            let date = winutil::now_string();
            let d: Vec<&str> = date[..10].split('-').collect();
            format!("{}.{}.{}", d[2], d[1], d[0])
        };
        let learned = if s.on_battery() {
            let load_w = s.load_pct as f32 / 100.0 * self.cfg.ups_power_w as f32;
            let capacity = self.gauge.capacity(configured);
            let max_secs = (self.cfg.poll_interval_secs * 3).max(15);
            self.gauge
                .on_battery(now, max_secs, self.nominal_battery_v, capacity, load_w);
            let learned = if s.battery_low() {
                self.gauge.on_low(configured, &today())
            } else {
                None
            };
            self.gauge.save();
            learned
        } else if self.gauge.discharge_from.is_some() {
            let learned = self.gauge.on_mains(now, configured, &today());
            self.gauge.save();
            learned
        } else {
            None
        };
        if let Some(l) = learned {
            log(&format!(
                "ємність батареї за цим розрядом: {:.1} А·год (була {:.1})",
                l.new_ah, l.old_ah
            ));
        }
    }

    fn notify_power_lost(&self, s: &Status) {
        let capacity = self.gauge.capacity(self.cfg.battery_ah as f32);
        let counted = Some((self.gauge.charge_pct(snapshot::unix_now(), capacity), capacity));
        let (charge, minutes) = battery::estimate(
            s,
            self.nominal_battery_v,
            self.cfg.ups_power_w,
            self.cfg.battery_ah as f32,
            counted,
        );
        let mut text = format!(
            "⚡ Зникла мережа. ПК працює від батареї: заряд ~{charge} %, вистачить на {}.",
            battery::runtime_text(minutes)
        );
        let limit = self.cfg.on_battery_shutdown_secs;
        if limit > 0 {
            text += &format!(
                " ПК вимкнеться через {}.",
                events::duration_text(limit as i64)
            );
        }
        self.notify(Note::new(text));
    }

    /// A command confirmed in Telegram.
    fn remote(&mut self, r: Remote) {
        match r {
            Remote::Shutdown => {
                if self.pc_shutdown_requested {
                    self.notify(Note::reply("ПК уже вимикається."));
                    return;
                }
                log(&format!(
                    "команда з Telegram: вимкнення ПК через {} с",
                    telegram::SHUTDOWN_GRACE_SECS
                ));
                match winutil::shutdown_pc(
                    "Команда з Telegram: комп'ютер буде вимкнено.",
                    telegram::SHUTDOWN_GRACE_SECS,
                ) {
                    Ok(()) => {
                        self.shutdown_started(telegram::SHUTDOWN_GRACE_SECS);
                        self.publish();
                    }
                    Err(e) => {
                        log(&format!("InitiateShutdown не вдався: {e}"));
                        self.notify(Note::reply(format!("Не вдалося вимкнути ПК: {e}")));
                    }
                }
            }
            Remote::Cancel => match winutil::abort_shutdown() {
                Ok(true) => {
                    log("команда з Telegram: вимкнення ПК скасовано");
                    self.pc_shutdown_requested = false;
                    let limit = self.cfg.on_battery_shutdown_secs;
                    let past_limit = self
                        .on_battery_since
                        .is_some_and(|t| limit > 0 && t.elapsed().as_secs() >= limit);
                    let mut text = String::from("Вимкнення ПК скасовано.");
                    if past_limit {
                        // Otherwise the next poll would start it again.
                        self.timeout_cancelled = true;
                        text += " Через відсутність мережі ПК більше не вимикатиметься, \
                                 доки її не відновлять; при низькому заряді вимкнеться однаково.";
                    }
                    self.notify(Note::reply(text));
                    self.publish();
                }
                Ok(false) => {
                    log("команда з Telegram: скасування, але вимкнення не заплановано");
                    self.notify(Note::reply("Вимкнення ПК не заплановано."));
                }
                Err(e) => {
                    log(&format!(
                        "команда з Telegram: скасувати вимкнення ПК не вдалося: {e}"
                    ));
                    self.notify(Note::reply(format!("Не вдалося скасувати вимкнення: {e}")));
                }
            },
        }
    }

    fn publish(&self) {
        let connected = self.ups.is_some();
        let status = if connected { self.last.clone() } else { None };
        let on_battery = status.as_ref().is_some_and(|s| s.on_battery());
        let on_battery_secs = self
            .on_battery_since
            .filter(|_| on_battery)
            .map(|t| t.elapsed().as_secs());
        let limit = self.cfg.on_battery_shutdown_secs;
        let pc_shutdown_in_secs = on_battery_secs
            .filter(|_| limit > 0 && !self.pc_shutdown_requested && !self.timeout_cancelled)
            .map(|secs| limit.saturating_sub(secs));
        let capacity = self.gauge.capacity(self.cfg.battery_ah as f32);
        snapshot::write(&Snapshot {
            updated_unix: snapshot::unix_now(),
            time: winutil::now_string(),
            connected,
            error: if connected {
                None
            } else {
                self.last_error.clone()
            },
            status,
            nominal_battery_v: self.nominal_battery_v,
            on_battery_secs,
            pc_shutdown_in_secs,
            pc_shutdown_requested: self.pc_shutdown_requested,
            pc_shutdown_at_unix: self
                .pc_shutdown_requested
                .then_some(self.pc_shutdown_deadline),
            ups_off_sent: self.ups_off_sent,
            poll_interval_secs: self.cfg.poll_interval_secs,
            frozen_secs: self.frozen.then(|| self.raw_changed_at.elapsed().as_secs()),
            battery_charge_pct: Some(self.gauge.charge_pct(snapshot::unix_now(), capacity)),
            battery_capacity_ah: Some(capacity),
            battery_learned_on: self.gauge.learned_on.clone(),
            telegram: Some(self.bot.state()),
        });
    }

    /// Called when Windows is about to power down. Turns the UPS output off
    /// only for a positively identified power-off, never for restart or sleep.
    /// `suspend`: also accept winlogon's hybrid-shutdown request, the only
    /// trace a Fast Startup shutdown leaves after a Fast Startup boot.
    fn on_power_down(&mut self, source: &str, window_secs: u32, suspend: bool) {
        if !self.cfg.ups_off_on_pc_shutdown || self.ups_off_sent {
            return;
        }
        let kind = if self.pc_shutdown_requested {
            log(&format!("{source}: вимкнення ПК ініціювала ця служба"));
            ShutdownKind::PowerOff
        } else {
            match winutil::recent_shutdown_events(window_secs, 10) {
                Ok(events) => {
                    for e in &events {
                        log(&format!(
                            "{source}: подія 1074 {:?} від {}",
                            e.kind_text, e.process
                        ));
                    }
                    let kinds: Vec<ShutdownKind> = events
                        .iter()
                        .map(|e| winutil::classify(&e.kind_text))
                        .collect();
                    winutil::decide(&kinds)
                }
                Err(e) => {
                    log(&format!(
                        "{source}: не вдається прочитати журнал подій: {e}"
                    ));
                    ShutdownKind::Unknown
                }
            }
        };
        let kind = if suspend && kind == ShutdownKind::Unknown {
            // The suspend notification arrives in the same instant winlogon's
            // event 187 is raised, before the event log has stored it, so
            // keep asking for a moment.
            let started = Instant::now();
            loop {
                match winutil::hybrid_shutdown_requested(HYBRID_EVENT_WINDOW_SECS) {
                    Ok(true) => {
                        log(&format!(
                            "{source}: winlogon запросив гібридне вимкнення (подія 187, знайдено за {} мс)",
                            started.elapsed().as_millis()
                        ));
                        break ShutdownKind::PowerOff;
                    }
                    Ok(false) if started.elapsed() < HYBRID_EVENT_WAIT => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Ok(false) => break kind,
                    Err(e) => {
                        log(&format!(
                            "{source}: не вдається прочитати журнал подій: {e}"
                        ));
                        break kind;
                    }
                }
            }
        } else {
            kind
        };
        if kind != ShutdownKind::PowerOff {
            log(&format!(
                "{source}: тип вимкнення {kind:?}, ДБЖ лишається увімкненим"
            ));
            return;
        }
        if self.cfg.skip_when_update_pending && winutil::update_pending() {
            log(&format!(
                "{source}: очікують оновлення Windows, ДБЖ лишається увімкненим"
            ));
            return;
        }

        // On mains the UPS would only go to "PWR DN": it passes mains through
        // without battery backup and no USB command brings it back to line
        // mode, so the PC would start unprotected. The UPS's own NLS setting
        // turns it off if power fails while the PC is off.
        let on_battery = self.query().map(|s| s.on_battery()).unwrap_or(false);
        if !on_battery {
            log(&format!(
                "{source}: живлення від мережі, ДБЖ лишається увімкненим"
            ));
            return;
        }
        let restore = on_battery && self.cfg.ups_return_when_on_battery;
        let index = protocol::shutdown_index(self.cfg.ups_off_delay_min, restore);
        let what = format!(
            "{source}: вимкнення ДБЖ через {} с ({})",
            protocol::effective_delay_secs(self.cfg.ups_off_delay_min),
            if restore {
                "увімкнеться з мережею"
            } else {
                "лишиться вимкненим"
            }
        );
        if self.send(index, &what) {
            self.ups_off_sent = true;
            match self.query() {
                Some(s) if s.shutdown_active() => log("ДБЖ підтверджує заплановане вимкнення"),
                Some(_) => log("увага: ДБЖ не повідомляє про заплановане вимкнення"),
                None => {}
            }
            self.publish();
        }
    }

    /// The UPS turns its beeper back on after the PC was off; set it to the
    /// state chosen in the window. The toggle sometimes hangs the UPS's USB
    /// bridge, so only once per start or resume, a few minutes in, and not
    /// on battery (the user may have just silenced it on the UPS).
    fn restore_beeper(&mut self, s: &Status) {
        let Some(want) = self.cfg.beeper_on else {
            return;
        };
        if s.beeper_on() == want
            || s.on_battery()
            || self.beeper_tried
            || Instant::now() < self.beeper_not_before
        {
            return;
        }
        self.beeper_tried = true;
        let what = if want {
            "звук ДБЖ скинувся, вмикаю"
        } else {
            "звук ДБЖ скинувся, вимикаю"
        };
        self.send(protocol::IDX_BEEPER_TOGGLE, what);
    }

    /// Back from sleep/hibernate: whatever was armed must not fire now.
    fn on_resume(&mut self) {
        self.beeper_not_before = Instant::now() + BEEPER_RESTORE_DELAY;
        self.beeper_tried = false;
        log("вихід зі сну");
        self.ups_off_sent = false;
        self.pc_shutdown_requested = false;
        self.last = None;
    }
}

/// Names of the status flags that are set, in display order.
pub fn flags(s: &Status) -> Vec<&'static str> {
    [
        (s.on_battery(), "ВІД БАТАРЕЇ"),
        (s.battery_low(), "НИЗЬКИЙ ЗАРЯД"),
        (s.avr_active(), "AVR"),
        (s.ups_failed(), "НЕСПРАВНІСТЬ"),
        (s.test_in_progress(), "ТЕСТ"),
        (s.shutdown_active(), "ЗАПЛАНОВАНО ВИМКНЕННЯ"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect()
}

pub fn describe(s: &Status, nominal_battery_v: f32) -> String {
    let mut out = format!(
        "вхід {:.1} В, вихід {:.1} В, {:.1} Гц, навантаження {} %, батарея {:.1} В, {:.1} °C",
        s.input_v, s.output_v, s.freq_hz, s.load_pct, s.battery_v, s.temp_c
    );
    if s.on_battery() {
        out += &format!(", заряд ≈{} %", s.charge_estimate_pct(nominal_battery_v));
    }
    let flags = flags(s);
    if !flags.is_empty() {
        out += &format!(" [{}]", flags.join(", "));
    }
    out
}
