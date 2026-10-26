//! Everything the window shows, gathered off the UI thread, and the actions
//! its buttons trigger.

use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use gear_ups::events::{self, Event};
use gear_ups::hid::Ups;
use gear_ups::history::{self, Sample};
use gear_ups::protocol::{self, Ratings, Status};
use gear_ups::service::SERVICE_NAME;
use gear_ups::snapshot::{self, Snapshot};
use gear_ups::winutil::{data_dir, wide};

const REFRESH: Duration = Duration::from_secs(2);
const LOG_LINES: usize = 300;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

#[derive(Debug, Clone, Default, PartialEq)]
pub enum SvcState {
    #[default]
    Unknown,
    NotInstalled,
    Stopped,
    Pending,
    Running,
}

#[derive(Clone, Default)]
pub struct View {
    pub service: SvcState,
    /// The service's last snapshot, if the file exists.
    pub snapshot: Option<Snapshot>,
    /// Read straight from the UPS while the service is not running.
    pub direct: Option<Result<(Status, f32), String>>,
    pub log: Arc<Vec<String>>,
    /// Notable events of the whole log, oldest first.
    pub events: Arc<Vec<Event>>,
    /// Readings for the chart: the service's history file plus what this
    /// window read directly while the service was stopped.
    pub history: Arc<Mutex<history::Reader>>,
}

pub fn spawn_poller(ctx: eframe::egui::Context) -> Arc<Mutex<View>> {
    let view = Arc::new(Mutex::new(View::default()));
    let shared = Arc::clone(&view);
    thread::spawn(move || {
        let mut log_modified: Option<SystemTime> = None;
        let mut ups: Option<(Ups, f32)> = None;
        let history = shared
            .lock()
            .map(|v| Arc::clone(&v.history))
            .unwrap_or_default();
        loop {
            let service = service_state();
            let snapshot = snapshot::read();
            let direct = if service == SvcState::Running {
                ups = None;
                None
            } else {
                Some(read_direct(&mut ups))
            };
            let log_path = data_dir().join("gear-ups.log");
            let modified = std::fs::metadata(&log_path).and_then(|m| m.modified()).ok();
            let new_log = (modified != log_modified).then(|| {
                log_modified = modified;
                let text = std::fs::read(&log_path)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                (Arc::new(tail(&text)), Arc::new(events::parse(&text)))
            });
            if let Ok(mut h) = history.lock() {
                h.refresh();
                if let Some(Ok((st, _))) = &direct {
                    h.push_live(Sample::from_status(snapshot::unix_now(), st));
                }
            }
            if let Ok(mut v) = shared.lock() {
                v.service = service;
                v.snapshot = snapshot;
                v.direct = direct;
                if let Some((log, events)) = new_log {
                    v.log = log;
                    v.events = events;
                }
            }
            ctx.request_repaint();
            thread::sleep(REFRESH);
        }
    });
    view
}

fn service_state() -> SvcState {
    let Ok(mgr) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return SvcState::Unknown;
    };
    match mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(svc) => match svc.query_status().map(|s| s.current_state) {
            Ok(ServiceState::Running) => SvcState::Running,
            Ok(ServiceState::Stopped) => SvcState::Stopped,
            Ok(_) => SvcState::Pending,
            Err(_) => SvcState::Unknown,
        },
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            SvcState::NotInstalled
        }
        Err(_) => SvcState::Unknown,
    }
}

fn read_direct(ups: &mut Option<(Ups, f32)>) -> Result<(Status, f32), String> {
    if ups.is_none() {
        let u = Ups::open().map_err(|e| e.to_string())?;
        let nominal = u
            .command(protocol::IDX_RATINGS)
            .ok()
            .and_then(|s| Ratings::parse(&s).ok())
            .map(|r| r.battery_v)
            .unwrap_or(24.0);
        *ups = Some((u, nominal));
    }
    let (u, nominal) = ups.as_ref().expect("opened above");
    let reply = u.command(protocol::IDX_STATUS).map_err(|e| e.to_string());
    match reply.and_then(|r| Status::parse(&r)) {
        Ok(s) => Ok((s, *nominal)),
        Err(e) => {
            *ups = None;
            Err(e)
        }
    }
}

fn tail(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_LINES)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// Result of a button action, shown under the buttons.
pub type Outcome = Arc<Mutex<Option<Result<String, String>>>>;

fn run_action(
    outcome: &Outcome,
    ctx: &eframe::egui::Context,
    f: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    let outcome = Arc::clone(outcome);
    let ctx = ctx.clone();
    thread::spawn(move || {
        let r = f();
        if let Ok(mut o) = outcome.lock() {
            *o = Some(r);
        }
        ctx.request_repaint();
    });
}

pub fn cancel_ups_shutdown(outcome: &Outcome, ctx: &eframe::egui::Context) {
    run_action(outcome, ctx, || {
        let ups = Ups::open().map_err(|e| e.to_string())?;
        ups.command(protocol::IDX_CANCEL)
            .map_err(|e| e.to_string())?;
        thread::sleep(Duration::from_millis(500));
        let s = Status::parse(
            &ups.command(protocol::IDX_STATUS)
                .map_err(|e| e.to_string())?,
        )?;
        if s.shutdown_active() {
            Err("ДБЖ досі повідомляє про заплановане вимкнення".into())
        } else {
            Ok("Вимкнення ДБЖ скасовано".into())
        }
    });
}

/// Toggles the UPS beeper. Sent exactly once: a retry would toggle back.
/// The UPS does not report the result (status bit 7 ignores the toggle).
pub fn toggle_ups_beeper(outcome: &Outcome, ctx: &eframe::egui::Context) {
    run_action(outcome, ctx, || {
        let ups = Ups::open().map_err(|e| e.to_string())?;
        ups.command_once(protocol::IDX_BEEPER_TOGGLE)
            .map_err(|e| format!("ДБЖ не прийняв команду: {e}"))?;
        Ok("Команду надіслано. Перевірте значок звуку на екрані ДБЖ.".into())
    });
}

pub fn test_ups_off(outcome: &Outcome, ctx: &eframe::egui::Context, minutes: f64) {
    run_action(outcome, ctx, move || {
        let ups = Ups::open().map_err(|e| e.to_string())?;
        ups.command(protocol::shutdown_index(minutes, true))
            .map_err(|e| e.to_string())?;
        let s = Status::parse(
            &ups.command(protocol::IDX_STATUS)
                .map_err(|e| e.to_string())?,
        )?;
        let secs = protocol::effective_delay_secs(minutes);
        if s.shutdown_active() {
            Ok(format!(
                "ДБЖ прийняв команду: вимкнення через {secs} с. Не забудьте скасувати!"
            ))
        } else {
            Err("Команду надіслано, але ДБЖ не повідомляє про заплановане вимкнення".into())
        }
    });
}

fn sibling(exe: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)))
        .unwrap_or_else(|| PathBuf::from(exe))
}

/// Starts `program` as administrator (UAC prompt).
fn elevate(program: &str, params: &str) -> Result<(), String> {
    let verb = wide("runas");
    let file = wide(program);
    let params = wide(params);
    let rc = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if rc as isize > 32 {
        Ok(())
    } else {
        Err("Запуск від імені адміністратора скасовано або не вдався".into())
    }
}

/// Runs `gear-ups.exe install` elevated in a console that stays open.
pub fn install_service() -> Result<String, String> {
    let cli = sibling("gear-ups.exe");
    if !cli.exists() {
        return Err(format!("Не знайдено {}", cli.display()));
    }
    elevate(
        "cmd.exe",
        &format!(
            "/c \"\"{}\" install --disable-vendor & pause\"",
            cli.display()
        ),
    )?;
    Ok("Встановлення запущено в окремому вікні".into())
}

pub fn start_service() -> Result<String, String> {
    elevate("sc.exe", &format!("start {SERVICE_NAME}"))?;
    Ok("Запуск служби…".into())
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
}

pub fn open_data_dir() {
    let _ = std::process::Command::new("explorer.exe")
        .arg(data_dir())
        .spawn();
}
