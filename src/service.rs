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

use crate::config::{self, Config};
use crate::hid::Ups;
use crate::history;
use crate::protocol::{self, Ratings, Status};
use crate::snapshot::{self, Snapshot};
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
    let mut poll_interval = Duration::from_secs(cfg.poll_interval_secs);
    let mut cfg_modified = config::modified();
    let monitor = Arc::new(Mutex::new(Monitor::new(cfg)));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let handler_monitor = Arc::clone(&monitor);
    let status_handle = service_control_handler::register(SERVICE_NAME, move |ctrl| match ctrl {
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Preshutdown => {
            if let Ok(mut m) = handler_monitor.lock() {
                m.on_power_down("завершення роботи", PRESHUTDOWN_EVENT_WINDOW_SECS, false);
            }
            let _ = stop_tx.send(());
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

    loop {
        if let Ok(mut m) = monitor.lock() {
            let modified = config::modified();
            if modified != cfg_modified {
                cfg_modified = modified;
                let cfg = Config::load();
                if cfg != m.cfg {
                    log(&format!("налаштування змінено: {cfg:?}"));
                    poll_interval = Duration::from_secs(cfg.poll_interval_secs);
                    m.cfg = cfg;
                }
            }
            m.poll();
        }
        match stop_rx.recv_timeout(poll_interval) {
            Err(RecvTimeoutError::Timeout) => continue,
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
    pc_shutdown_requested: bool,
    ups_off_sent: bool,
}

impl Monitor {
    fn new(cfg: Config) -> Monitor {
        Monitor {
            cfg,
            ups: None,
            nominal_battery_v: 24.0,
            last: None,
            last_error: None,
            on_battery_since: None,
            pc_shutdown_requested: false,
            ups_off_sent: false,
        }
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
            .and_then(|s| Status::parse(&s))
        {
            Ok(s) => Some(s),
            Err(e) => {
                log(&format!("ДБЖ не відповів на запит стану: {e}"));
                self.last_error = Some(e);
                self.ups = None;
                None
            }
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

    fn poll(&mut self) {
        if let Some(s) = self.query() {
            // Only fresh readings go to the history; publish() also runs
            // from on_power_down with the last known status.
            history::append(history::Sample::from_status(snapshot::unix_now(), &s));
            self.handle(s);
        }
        self.publish();
    }

    fn handle(&mut self, s: Status) {
        let prev = self.last.take();

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
                }
                if s.battery_low() && !p.battery_low() {
                    log("низький заряд батареї");
                }
                if s.ups_failed() != p.ups_failed() {
                    log(if s.ups_failed() {
                        "ДБЖ повідомляє про несправність"
                    } else {
                        "несправність ДБЖ зникла"
                    });
                }
            }
        }

        if s.on_battery() {
            let since = *self.on_battery_since.get_or_insert_with(Instant::now);
            let limit = self.cfg.on_battery_shutdown_secs;
            let timed_out = limit > 0 && since.elapsed() >= Duration::from_secs(limit);
            let low = self.cfg.shutdown_on_battery_low && s.battery_low();
            if (timed_out || low) && !self.pc_shutdown_requested {
                let why = if low {
                    "низький заряд батареї"
                } else {
                    "надто довго без мережі"
                };
                log(&format!("вимикаю ПК: {why}"));
                match winutil::shutdown_pc(
                    &format!("ДБЖ GEAR: {why}. Комп'ютер буде вимкнено."),
                    60,
                ) {
                    Ok(()) => self.pc_shutdown_requested = true,
                    Err(e) => log(&format!("InitiateShutdown не вдався: {e}")),
                }
            }
        } else {
            self.on_battery_since = None;
        }

        self.last = Some(s);
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
            .filter(|_| limit > 0 && !self.pc_shutdown_requested)
            .map(|secs| limit.saturating_sub(secs));
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
            ups_off_sent: self.ups_off_sent,
            poll_interval_secs: self.cfg.poll_interval_secs,
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
            match winutil::hybrid_shutdown_requested(HYBRID_EVENT_WINDOW_SECS) {
                Ok(true) => {
                    log(&format!(
                        "{source}: winlogon запросив гібридне вимкнення (подія 187)"
                    ));
                    ShutdownKind::PowerOff
                }
                Ok(false) => kind,
                Err(e) => {
                    log(&format!(
                        "{source}: не вдається прочитати журнал подій: {e}"
                    ));
                    kind
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

        let on_battery = self.query().map(|s| s.on_battery()).unwrap_or(false);
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

    /// Back from sleep/hibernate: whatever was armed must not fire now.
    fn on_resume(&mut self) {
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
