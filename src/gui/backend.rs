//! Everything the window shows, gathered off the UI thread, and the actions
//! its buttons trigger.

use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, ShellExecuteW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

use gear_ups::update;

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

/// Where checking for and installing an update stands.
#[derive(Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    /// This build is the latest; the version GitHub has.
    UpToDate(String),
    Available(update::Release),
    /// Downloading or installing: what is going on.
    Working(&'static str),
    Failed(String),
}

/// Asks GitHub for the latest release in the background.
pub fn check_update(state: &Arc<Mutex<UpdateState>>, ctx: &eframe::egui::Context) {
    let (state, ctx) = (Arc::clone(state), ctx.clone());
    set(&state, &ctx, UpdateState::Checking);
    thread::spawn(move || {
        let next = match update::latest() {
            Ok(r) if update::is_newer(&r.version, env!("CARGO_PKG_VERSION")) => {
                UpdateState::Available(r)
            }
            Ok(r) => UpdateState::UpToDate(r.version),
            Err(e) => UpdateState::Failed(e),
        };
        set(&state, &ctx, next);
    });
}

/// Downloads `release`, installs it as administrator, then starts the new
/// window and asks this one to quit.
pub fn install_update(
    release: update::Release,
    state: &Arc<Mutex<UpdateState>>,
    requests: &Arc<crate::tray::Requests>,
    ctx: &eframe::egui::Context,
) {
    let (state, requests, ctx) = (Arc::clone(state), Arc::clone(requests), ctx.clone());
    set(&state, &ctx, UpdateState::Working("Завантажую з GitHub…"));
    thread::spawn(move || {
        let result = update::download(&release).and_then(|cli| {
            set(&state, &ctx, UpdateState::Working("Встановлюю…"));
            run_elevated_and_wait(&cli, "install --disable-vendor")
        });
        match result.and_then(|()| restart_gui()) {
            Ok(()) => {
                requests.quit.store(true, std::sync::atomic::Ordering::SeqCst);
                ctx.request_repaint();
            }
            Err(e) => set(&state, &ctx, UpdateState::Failed(e)),
        }
    });
}

fn set(state: &Mutex<UpdateState>, ctx: &eframe::egui::Context, next: UpdateState) {
    if let Ok(mut s) = state.lock() {
        *s = next;
    }
    ctx.request_repaint();
}

/// Runs `program` as administrator without a console and waits for it.
fn run_elevated_and_wait(program: &std::path::Path, params: &str) -> Result<(), String> {
    let verb = wide("runas");
    let file = wide(&program.to_string_lossy());
    let params = wide(params);
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 || info.hProcess.is_null() {
        return Err("Встановлення скасовано: потрібні права адміністратора".into());
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "Встановлення не вдалося (код {code}), подробиці — у журналі"
        ))
    }
}

/// Starts the installed window; it waits until this one has quit.
fn restart_gui() -> Result<(), String> {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    let gui = PathBuf::from(base).join("GearUps").join("gear-ups-gui.exe");
    std::process::Command::new(&gui)
        .arg(crate::AFTER_UPDATE)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Оновлено, але не вдалося запустити {}: {e}", gui.display()))
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
