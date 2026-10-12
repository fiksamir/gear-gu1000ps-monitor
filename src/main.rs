use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use gear_ups::config::{self, Config};
use gear_ups::hid::Ups;
use gear_ups::protocol::{self, Ratings, Status};
use gear_ups::{service, snapshot, winutil};

const VENDOR_SERVICE: &str = "upsSmartServer";
const GUI_EXE: &str = "gear-ups-gui.exe";
const RUN_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "GearUpsGui";
/// Built-in Users group; a SID so it works on any Windows language.
const USERS_SID: &str = "*S-1-5-32-545";

const USAGE: &str = "\
gear-ups — GEAR GU-1000PS (USB 0001:0000, MEC0003)

  gear-ups status [--watch]        стан ДБЖ (з --watch — щосекунди)
  gear-ups info                    номінали та прошивка
  gear-ups events                  останні події вимкнення Windows і як їх розпізнано
  gear-ups ups-off <хв> [--return] --yes
                                   вимкнути вихід ДБЖ через <хв> (0.5..10);
                                   --return: увімкнутись, коли є мережа
  gear-ups cancel                  скасувати заплановане вимкнення ДБЖ
  gear-ups battery-test            тест батареї 10 с (ДБЖ переходить на батарею)
  gear-ups install [--disable-vendor]
                                   встановити або оновити службу GearUps і програму
                                   в треї (від адміністратора);
                                   --disable-vendor: зупинити й вимкнути upsSmartServer
  gear-ups uninstall               видалити службу GearUps
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let result = match args.first().map(String::as_str) {
        Some("run-service") => service::run().map_err(|e| e.to_string()),
        Some("status") => status(flag("--watch")),
        Some("info") => info(),
        Some("events") => events(),
        Some("ups-off") => ups_off(args.get(1), flag("--return"), flag("--yes")),
        Some("cancel") => cancel(),
        Some("battery-test") => battery_test(),
        Some("install") => install(flag("--disable-vendor"))
            .inspect_err(|e| winutil::log(&format!("встановлення: помилка: {e}"))),
        Some("uninstall") => uninstall(),
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("помилка: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open() -> Result<Ups, String> {
    Ups::open().map_err(|e| e.to_string())
}

fn read_status(ups: &Ups) -> Result<Status, String> {
    Status::parse(
        &ups.command(protocol::IDX_STATUS)
            .map_err(|e| e.to_string())?,
    )
}

fn nominal_battery_v(ups: &Ups) -> f32 {
    ups.command(protocol::IDX_RATINGS)
        .ok()
        .and_then(|s| Ratings::parse(&s).ok())
        .map(|r| r.battery_v)
        .unwrap_or(24.0)
}

fn status(watch: bool) -> Result<(), String> {
    let ups = open()?;
    let nominal = nominal_battery_v(&ups);
    loop {
        let s = read_status(&ups)?;
        println!(
            "{}  {}",
            winutil::now_string(),
            service::describe(&s, nominal)
        );
        if !watch {
            return Ok(());
        }
        sleep(Duration::from_secs(1));
    }
}

fn info() -> Result<(), String> {
    let ups = open()?;
    let r = Ratings::parse(
        &ups.command(protocol::IDX_RATINGS)
            .map_err(|e| e.to_string())?,
    )?;
    let i = ups.command(protocol::IDX_INFO).map_err(|e| e.to_string())?;
    println!(
        "номінал: {} В, {} А, батарея {} В, {} Гц",
        r.voltage, r.current, r.battery_v, r.freq_hz
    );
    println!("інфо:    {}", i.trim_start_matches('#').trim());
    Ok(())
}

fn events() -> Result<(), String> {
    let list = winutil::recent_shutdown_events(0, 10).map_err(|e| e.to_string())?;
    if list.is_empty() {
        println!("подій 1074 не знайдено");
    }
    for e in &list {
        println!(
            "{:<9?} {:?}  ({})",
            winutil::classify(&e.kind_text),
            e.kind_text,
            e.process
        );
    }
    println!(
        "оновлення Windows очікують перезавантаження: {}",
        winutil::update_pending()
    );
    Ok(())
}

fn ups_off(minutes: Option<&String>, restore: bool, yes: bool) -> Result<(), String> {
    let minutes: f64 = minutes
        .and_then(|m| m.parse().ok())
        .ok_or("вкажіть затримку у хвилинах, напр. `ups-off 10 --yes`")?;
    let secs = protocol::effective_delay_secs(minutes);
    if !yes {
        return Err(format!(
            "ДБЖ вимкне вихід через {secs} с — усе, що до нього підключено, знеструмиться. \
             Додайте --yes, щоб підтвердити; скасування: `gear-ups cancel`."
        ));
    }
    let ups = open()?;
    let index = protocol::shutdown_index(minutes, restore);
    ups.command(index).map_err(|e| e.to_string())?;
    let s = read_status(&ups)?;
    println!(
        "надіслано (індекс 0x{index:02x}): вимкнення через {secs} с, {}; ДБЖ підтверджує: {}",
        if restore {
            "увімкнеться з мережею"
        } else {
            "залишиться вимкненим"
        },
        if s.shutdown_active() {
            "так"
        } else {
            "НІ"
        }
    );
    Ok(())
}

fn cancel() -> Result<(), String> {
    let ups = open()?;
    ups.command(protocol::IDX_CANCEL)
        .map_err(|e| e.to_string())?;
    sleep(Duration::from_millis(500));
    let s = read_status(&ups)?;
    if s.shutdown_active() {
        return Err("ДБЖ досі повідомляє про заплановане вимкнення".into());
    }
    println!("скасовано; заплановане вимкнення відсутнє");
    Ok(())
}

fn battery_test() -> Result<(), String> {
    let ups = open()?;
    ups.command(protocol::IDX_TEST).map_err(|e| e.to_string())?;
    for _ in 0..15 {
        sleep(Duration::from_secs(1));
        let s = read_status(&ups)?;
        println!(
            "тест: {}, вихід {:.0} В, батарея {:.1} В",
            if s.test_in_progress() {
                "триває"
            } else {
                "ні"
            },
            s.output_v,
            s.battery_v
        );
    }
    Ok(())
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    PathBuf::from(base).join("GearUps")
}

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Prints a step of the installation and records it in the service log, so
/// it is visible in the GUI when `install` ran in a console that has closed.
fn say(msg: &str) {
    println!("{msg}");
    winutil::log(&format!("встановлення: {msg}"));
}

/// Copies `src` over `dst`. A running exe cannot be overwritten but can be
/// renamed, so an existing `dst` is moved aside first.
fn copy_replacing(src: &Path, dst: &Path) -> Result<(), String> {
    if src == dst {
        return Ok(());
    }
    if dst.exists() {
        let old = dst.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(dst, &old);
    }
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|e| format!("копіювання в {}: {e}", dst.display()))
}

fn wait_stopped(svc: &windows_service::service::Service) {
    for _ in 0..30 {
        match svc.query_status() {
            Ok(s) if s.current_state != ServiceState::Stopped => sleep(Duration::from_millis(500)),
            _ => return,
        }
    }
}

fn install(disable_vendor: bool) -> Result<(), String> {
    let mgr = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| format!("{e} (запустіть від імені адміністратора)"))?;

    if let Ok(vendor) = mgr.open_service(VENDOR_SERVICE, ServiceAccess::QUERY_STATUS) {
        if disable_vendor {
            say(&format!("зупиняю і вимикаю {VENDOR_SERVICE}"));
            run("sc.exe", &["stop", VENDOR_SERVICE]);
            run("sc.exe", &["config", VENDOR_SERVICE, "start=", "disabled"]);
            wait_stopped(&vendor);
        } else if vendor
            .query_status()
            .map(|s| s.current_state != ServiceState::Stopped)
            .unwrap_or(false)
        {
            return Err(format!(
                "{VENDOR_SERVICE} працює і тримає ДБЖ. Повторіть з --disable-vendor \
                 (повернути: sc config {VENDOR_SERVICE} start= auto && sc start {VENDOR_SERVICE})"
            ));
        }
    }

    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::STOP
        | ServiceAccess::START
        | ServiceAccess::CHANGE_CONFIG;
    let existing = mgr.open_service(service::SERVICE_NAME, access).ok();
    if let Some(svc) = &existing {
        say("зупиняю встановлену службу GearUps для оновлення");
        let _ = svc.stop();
        wait_stopped(svc);
    }

    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = dir.join("gear-ups.exe");
    copy_replacing(&current, &target)?;
    let gui_target = dir.join(GUI_EXE);
    match current.parent().map(|p| p.join(GUI_EXE)) {
        Some(gui) if gui.exists() => copy_replacing(&gui, &gui_target)?,
        _ => say(&format!(
            "{GUI_EXE} поруч не знайдено, програму в треї не оновлено"
        )),
    }

    // Create the settings file now and let ordinary users edit it from the GUI.
    let _ = Config::load();
    let cfg_path = config::path();
    if !run(
        "icacls.exe",
        &[
            &cfg_path.to_string_lossy(),
            "/grant",
            &format!("{USERS_SID}:M"),
        ],
    ) {
        say("не вдалося дати користувачам право змінювати config.toml");
    }

    let info = ServiceInfo {
        name: OsString::from(service::SERVICE_NAME),
        display_name: OsString::from("GEAR UPS monitor"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: target.clone(),
        launch_arguments: vec![OsString::from("run-service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let svc = match existing {
        Some(svc) => {
            svc.change_config(&info).map_err(|e| e.to_string())?;
            svc
        }
        None => mgr
            .create_service(&info, access)
            .map_err(|e| e.to_string())?,
    };
    let _ = svc.set_description(
        "Моніторинг ДБЖ GEAR GU-1000PS: вимикає ПК при тривалій відсутності мережі \
         та вимикає ДБЖ після вимкнення ПК.",
    );
    svc.start::<&str>(&[]).map_err(|e| e.to_string())?;

    if gui_target.exists() {
        let command = format!("\"{}\" --tray", gui_target.display());
        if !run(
            "reg.exe",
            &[
                "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f",
            ],
        ) {
            say("не вдалося додати програму в треї до автозапуску");
        }
        let ico = dir.join("gear-ups.ico");
        if let Err(e) = std::fs::write(
            &ico,
            gear_ups::icon::ico(gear_ups::icon::GREEN, &[16, 24, 32, 48, 64, 256]),
        ) {
            say(&format!("не вдалося записати значок: {e}"));
        }
        if !create_shortcut(&gui_target, &ico) {
            say("не вдалося створити ярлик на робочому столі");
        }
    }

    say(&format!(
        "службу встановлено і запущено: {}",
        target.display()
    ));
    println!("налаштування: {}", cfg_path.display());
    println!(
        "журнал:       {}",
        winutil::data_dir().join("gear-ups.log").display()
    );
    Ok(())
}

/// The shared desktop, so the shortcut is there for every user.
fn shortcut_path() -> PathBuf {
    let public = std::env::var_os("PUBLIC").unwrap_or_else(|| "C:\\Users\\Public".into());
    PathBuf::from(public).join("Desktop").join("ДБЖ GEAR.lnk")
}

/// Creates the desktop shortcut through the WScript.Shell COM object.
fn create_shortcut(gui: &Path, ico: &Path) -> bool {
    let quote = |p: &Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{lnk}'); \
         $s.TargetPath = '{exe}'; $s.WorkingDirectory = '{dir}'; $s.IconLocation = '{ico},0'; \
         $s.Description = 'Стан ДБЖ GEAR'; $s.Save()",
        lnk = quote(&shortcut_path()),
        exe = quote(gui),
        ico = quote(ico),
        dir = quote(gui.parent().unwrap_or(gui)),
    );
    run(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
}

fn uninstall() -> Result<(), String> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("{e} (запустіть від імені адміністратора)"))?;
    let svc = mgr
        .open_service(
            service::SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|e| e.to_string())?;
    if svc
        .query_status()
        .map(|s| s.current_state != ServiceState::Stopped)
        .unwrap_or(false)
    {
        let _ = svc.stop();
        wait_stopped(&svc);
    }
    svc.delete().map_err(|e| e.to_string())?;
    run("reg.exe", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    let _ = std::fs::remove_file(shortcut_path());
    let _ = std::fs::remove_file(snapshot::path());
    println!("службу видалено");
    println!("щоб повернути штатну програму: sc config {VENDOR_SERVICE} start= auto && sc start {VENDOR_SERVICE}");
    Ok(())
}
