//! The window: live state, settings and the service log, all in Ukrainian.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use gear_ups::config::Config;
use gear_ups::protocol::{self, Status};
use gear_ups::snapshot;

use crate::backend::{self, Outcome, SvcState, View};
use crate::chart::{self, Period};
use crate::diagram;
use crate::theme;
use crate::tray::{Level, Requests, Tray};

const TEST_DELAY_MIN: f64 = 1.0;
/// How long a result message under the status cards stays on screen.
const OUTCOME_OK_TTL: Duration = Duration::from_secs(8);
const OUTCOME_ERR_TTL: Duration = Duration::from_secs(20);
/// Frame interval while the power-flow diagram is animating.
const ANIMATION_FRAME: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Chart,
    Settings,
    Log,
}

pub struct GuiApp {
    view: Arc<Mutex<View>>,
    requests: Arc<Requests>,
    tray: Option<Tray>,
    tab: Tab,
    period: Period,
    quitting: bool,
    last_alarm: Option<bool>,
    outcome: Outcome,
    confirm_test: bool,
    saved: Config,
    form: Config,
    settings_msg: Option<Result<String, String>>,
    beeper_outcome: Outcome,
    /// The result message on screen and when it first appeared.
    outcome_since: Option<(Result<String, String>, Instant)>,
}

struct Summary {
    level: Level,
    title: String,
    detail: String,
    status: Option<Status>,
    nominal: f32,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, view: Arc<Mutex<View>>) -> GuiApp {
        let requests = Arc::new(Requests::default());
        let tray = Tray::new(&cc.egui_ctx, Arc::clone(&requests)).ok();
        let (saved, settings_msg) = match Config::read() {
            Ok(c) => (c.unwrap_or_default(), None),
            Err(e) => (
                Config::default(),
                Some(Err(format!("config.toml пошкоджено: {e}"))),
            ),
        };
        GuiApp {
            view,
            requests,
            tray,
            tab: Tab::Status,
            period: Period::Hour1,
            quitting: false,
            last_alarm: None,
            outcome: Outcome::default(),
            confirm_test: false,
            form: saved.clone(),
            saved,
            settings_msg,
            beeper_outcome: Outcome::default(),
            outcome_since: None,
        }
    }

    fn snapshot_view(&self) -> View {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

fn mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Charge (%) and minutes left while running from the battery.
fn on_battery_estimate(st: &Status, nominal: f32, cfg: &Config) -> (u32, u32) {
    let load_w = diagram::watts(st.load_pct, cfg.ups_power_w) as f32;
    let charge = st.charge_on_battery_pct(nominal, load_w);
    (
        charge,
        protocol::runtime_minutes(nominal, cfg.battery_ah as f32, charge, load_w),
    )
}

fn runtime_text(minutes: u32) -> String {
    match minutes {
        0 => "менше хвилини".into(),
        180.. => "понад 3 год".into(),
        60.. => format!("~{} год {} хв", minutes / 60, minutes % 60),
        _ => format!("~{minutes} хв"),
    }
}

fn summarize(v: &View, cfg: &Config) -> Summary {
    let mut s = Summary {
        level: Level::Off,
        title: String::new(),
        detail: String::new(),
        status: None,
        nominal: 24.0,
    };
    match &v.direct {
        Some(Ok((st, nominal))) => {
            s.status = Some(st.clone());
            s.nominal = *nominal;
        }
        Some(Err(e)) => s.detail = format!("ДБЖ: {e}"),
        None => {}
    }
    match v.service {
        SvcState::NotInstalled => {
            s.title = "Службу не встановлено".into();
            s.detail = "ПК не захищено. Натисніть «Встановити службу».".into();
            return s;
        }
        SvcState::Stopped | SvcState::Unknown => {
            s.title = "Служба GearUps не працює".into();
            s.detail = "ПК не захищено. Запустіть службу.".into();
            return s;
        }
        SvcState::Pending => {
            s.title = "Служба запускається або зупиняється…".into();
            return s;
        }
        SvcState::Running => {}
    }
    let Some(snap) = v.snapshot.as_ref().filter(|s| !s.is_stale()) else {
        s.level = Level::Warn;
        s.title = "Служба не оновлює стан".into();
        s.detail = "Зачекайте кілька секунд або перегляньте журнал.".into();
        return s;
    };
    s.nominal = snap.nominal_battery_v;
    let Some(st) = snap.status.clone().filter(|_| snap.connected) else {
        s.level = Level::Alarm;
        s.title = "ДБЖ не підключено".into();
        s.detail = snap.error.clone().unwrap_or_default();
        return s;
    };
    let elapsed = snapshot::unix_now().saturating_sub(snap.updated_unix);
    if st.shutdown_active() {
        s.level = Level::Alarm;
        s.title = "Заплановано вимкнення ДБЖ".into();
        s.detail = "Вихід ДБЖ скоро вимкнеться. Якщо ви цього не планували, скасуйте.".into();
    } else if st.ups_failed() {
        s.level = Level::Alarm;
        s.title = "ДБЖ повідомляє про несправність".into();
    } else if st.on_battery() {
        s.level = if st.battery_low() {
            Level::Alarm
        } else {
            Level::Warn
        };
        s.title = if st.battery_low() {
            "Від батареї — низький заряд".into()
        } else {
            "Живлення від батареї".into()
        };
        let away = snap.on_battery_secs.unwrap_or(0) + elapsed;
        let (charge, minutes) = on_battery_estimate(&st, s.nominal, cfg);
        let battery = format!("Заряд ~{charge} %, вистачить на {}.", runtime_text(minutes));
        s.detail = if snap.pc_shutdown_requested {
            "ПК вимикається.".into()
        } else if let Some(left) = snap.pc_shutdown_in_secs {
            format!(
                "Без мережі {}. {battery} ПК вимкнеться через {}.",
                mmss(away),
                mmss(left.saturating_sub(elapsed))
            )
        } else {
            format!("Без мережі {}. {battery}", mmss(away))
        };
    } else {
        s.level = Level::Ok;
        s.title = "Живлення від мережі".into();
        s.detail = "ПК захищено.".into();
    }
    s.status = Some(st);
    s
}

impl eframe::App for GuiApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.requests.quit.swap(false, Ordering::SeqCst) {
            self.quitting = true;
        }

        let summary = summarize(&self.snapshot_view(), &self.saved);
        let mut tooltip = format!("ДБЖ GEAR — {}", summary.title);
        if let Some(st) = &summary.status {
            let w = diagram::watts(st.load_pct, self.saved.ups_power_w);
            tooltip += &format!(
                "\nнавантаження {w} Вт ({} %), вхід {:.0} В",
                st.load_pct, st.input_v
            );
            if st.on_battery() {
                let (charge, minutes) = on_battery_estimate(st, summary.nominal, &self.saved);
                tooltip += &format!(
                    "\nзаряд ~{charge} %, вистачить на {}",
                    runtime_text(minutes)
                );
            }
        }
        if let Some(tray) = &mut self.tray {
            tray.set_state(summary.level, &tooltip);
        }
        // Bring the window up (without taking focus) when real power trouble
        // starts; not for service/USB start-up states, which happen every boot.
        // A secondary monitor may not be on the UPS and go dark, so the
        // window moves to the primary one first.
        let alarm = summary.status.as_ref().is_some_and(|s| {
            s.on_battery() || s.battery_low() || s.ups_failed() || s.shutdown_active()
        });
        if alarm && self.last_alarm == Some(false) {
            crate::move_to_primary_monitor();
            crate::show_window(false);
        }
        if summary.status.is_some() {
            self.last_alarm = Some(alarm);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.quitting {
            self.tray = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            // The close button hides to the tray; "Вийти" in the tray menu quits.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            crate::hide_window();
        }
        let view = self.snapshot_view();
        let summary = summarize(&view, &self.saved);

        let header = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 14,
                bottom: 8,
            });
        egui::Panel::top("banner")
            .frame(header)
            .show_separator_line(false)
            .show(ui, |ui| {
                theme::banner(ui, summary.level.color(), &summary.title, &summary.detail);
                ui.add_space(10.0);
                theme::segmented(
                    ui,
                    &mut self.tab,
                    &[
                        (Tab::Status, "🏠  Стан"),
                        (Tab::Chart, "📈  Графік"),
                        (Tab::Settings, "⚙  Налаштування"),
                        (Tab::Log, "☰  Журнал"),
                    ],
                );
            });

        egui::CentralPanel::default()
            .frame(theme::page())
            .show(ui, |ui| match self.tab {
                Tab::Status => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.status_tab(ui, &ctx, &view, &summary);
                        });
                }
                Tab::Chart => {
                    if let Ok(history) = view.history.lock() {
                        chart::show(
                            ui,
                            &history.samples,
                            &mut self.period,
                            summary.nominal,
                            self.saved.ups_power_w,
                            snapshot::unix_now(),
                        );
                    }
                }
                Tab::Settings => self.settings_tab(ui, &ctx, summary.status.as_ref()),
                Tab::Log => log_tab(ui, &view),
            });

        if self.confirm_test {
            self.test_dialog(&ctx);
        }
    }
}

impl GuiApp {
    fn status_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        view: &View,
        summary: &Summary,
    ) {
        if let Some(st) = &summary.status {
            theme::card(ui, "СХЕМА ЖИВЛЕННЯ", |ui| {
                if diagram::show(ui, st, summary.nominal, self.saved.ups_power_w)
                    && crate::window_on_screen()
                {
                    ctx.request_repaint_after(ANIMATION_FRAME);
                }
                if view.direct.is_some() {
                    ui.label(
                        RichText::new("Дані зчитано напряму: служба не працює.").color(theme::WEAK),
                    );
                }
            });
            self.tiles(ui, st, summary.nominal);
            theme::card(ui, "ІНДИКАТОРИ", |ui| diagram::indicators(ui, st));
        } else {
            theme::card(ui, "", |ui| {
                ui.label(RichText::new("Немає даних від ДБЖ.").color(theme::WEAK));
            });
        }

        ui.columns(2, |cols| {
            theme::card(&mut cols[0], "СЛУЖБА GEARUPS", |ui| {
                let (text, color) = match view.service {
                    SvcState::Running => ("Працює", Level::Ok.color()),
                    SvcState::Stopped => ("Зупинена", Level::Alarm.color()),
                    SvcState::Pending => ("Змінює стан…", Level::Warn.color()),
                    SvcState::NotInstalled => ("Не встановлена", Level::Alarm.color()),
                    SvcState::Unknown => ("Невідомо", Level::Off.color()),
                };
                theme::lamp(ui, text, true, color);
                if view.service == SvcState::Stopped
                    && ui
                        .add(theme::primary("Запустити службу…", theme::ACCENT))
                        .clicked()
                {
                    self.set_outcome(backend::start_service());
                }
                let install_label = if view.service == SvcState::NotInstalled {
                    "Встановити службу…"
                } else {
                    "Перевстановити / оновити…"
                };
                if ui
                    .button(install_label)
                    .on_hover_text("Потрібні права адміністратора")
                    .clicked()
                {
                    self.set_outcome(backend::install_service());
                }
            });
            theme::card(&mut cols[1], "КЕРУВАННЯ ДБЖ", |ui| {
                let pending = summary.status.as_ref().is_some_and(|s| s.shutdown_active());
                let cancel = if pending {
                    theme::primary("Скасувати вимкнення ДБЖ", Level::Alarm.color())
                } else {
                    egui::Button::new("Скасувати вимкнення ДБЖ")
                };
                if ui.add(cancel).clicked() {
                    backend::cancel_ups_shutdown(&self.outcome, ctx);
                }
                if ui.button("Перевірити вимкнення ДБЖ…").clicked() {
                    self.confirm_test = true;
                }
            });
        });
        if let Some(result) = self.outcome.lock().ok().and_then(|o| o.clone()) {
            let now = Instant::now();
            let since = match &self.outcome_since {
                Some((shown, at)) if *shown == result => *at,
                _ => {
                    self.outcome_since = Some((result.clone(), now));
                    now
                }
            };
            let ttl = if result.is_ok() {
                OUTCOME_OK_TTL
            } else {
                OUTCOME_ERR_TTL
            };
            let age = now - since;
            if age >= ttl {
                if let Ok(mut o) = self.outcome.lock() {
                    *o = None;
                }
                self.outcome_since = None;
            } else {
                ctx.request_repaint_after(ttl - age);
                match result {
                    Ok(m) => ui.label(RichText::new(m).color(Level::Ok.color())),
                    Err(e) => ui.label(RichText::new(e).color(Level::Alarm.color())),
                };
            }
        }
    }

    /// Battery, load and temperature as tiles with gauges.
    fn tiles(&self, ui: &mut egui::Ui, st: &Status, nominal: f32) {
        let volts = format!("{:.1} В", st.battery_v);
        let (battery_value, battery_fill, battery_note) = if st.on_battery() {
            let (charge, minutes) = on_battery_estimate(st, nominal, &self.saved);
            (
                format!("~{charge} %"),
                charge as f32 / 100.0,
                format!("{volts} · вистачить на {}", runtime_text(minutes)),
            )
        } else if st.battery_float(nominal) {
            ("100 %".to_string(), 1.0, format!("{volts} · на підзарядці"))
        } else {
            let charge = st.charge_estimate_pct(nominal);
            (
                "Заряджається".to_string(),
                charge as f32 / 100.0,
                format!("{volts} · ~{charge} %"),
            )
        };
        let battery_color = if st.battery_low() {
            Level::Alarm.color()
        } else if st.on_battery() {
            Level::Warn.color()
        } else {
            Level::Ok.color()
        };
        let load_color = match st.load_pct {
            0..=59 => Level::Ok.color(),
            60..=84 => Level::Warn.color(),
            _ => Level::Alarm.color(),
        };
        let temp_color = if st.temp_c >= 45.0 {
            Level::Alarm.color()
        } else {
            Level::Ok.color()
        };
        ui.columns(3, |cols| {
            theme::tile(
                &mut cols[0],
                "🔋",
                "БАТАРЕЯ",
                &battery_value,
                (battery_fill, battery_color),
                &battery_note,
            );
            theme::tile(
                &mut cols[1],
                "⚡",
                "НАВАНТАЖЕННЯ",
                &format!("{} Вт", diagram::watts(st.load_pct, self.saved.ups_power_w)),
                (st.load_pct as f32 / 100.0, load_color),
                &format!("{} % з {} Вт", st.load_pct, self.saved.ups_power_w),
            );
            theme::tile(
                &mut cols[2],
                "°C",
                "ТЕМПЕРАТУРА",
                &format!("{:.1} °C", st.temp_c),
                (st.temp_c / 60.0, temp_color),
                if st.temp_c >= 45.0 {
                    "Гаряче"
                } else {
                    "У нормі"
                },
            );
        });
    }

    fn set_outcome(&self, r: Result<String, String>) {
        if let Ok(mut o) = self.outcome.lock() {
            *o = Some(r);
        }
    }

    fn test_dialog(&mut self, ctx: &egui::Context) {
        let secs = protocol::effective_delay_secs(TEST_DELAY_MIN);
        egui::Modal::new(egui::Id::new("confirm_test")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Перевірити вимкнення ДБЖ?");
            ui.label(format!(
                "ДБЖ отримає команду вимкнути вихід через {secs} с. Якщо не скасувати, \
                 знеструмиться все, що до нього підключено, разом із цим ПК. \
                 ДБЖ може лишитися вимкненим, доки не натиснете його кнопку."
            ));
            ui.label("Після перевірки одразу натисніть «Скасувати вимкнення ДБЖ».");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("Так, надіслати команду").color(Level::Alarm.color()))
                    .clicked()
                {
                    backend::test_ups_off(&self.outcome, ctx, TEST_DELAY_MIN);
                    self.confirm_test = false;
                }
                if ui.button("Ні").clicked() {
                    self.confirm_test = false;
                }
            });
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: Option<&Status>) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.settings_form(ui);
                self.beeper_card(ui, ctx, status);
            });
    }

    /// The beeper lives in the UPS, not in config.toml, and the UPS does not
    /// report its state reliably (status bit 7 ignores the toggle), so this is
    /// a plain toggle button and the UPS display shows the result.
    fn beeper_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: Option<&Status>) {
        let result = self.beeper_outcome.lock().ok().and_then(|o| o.clone());
        theme::card(ui, "ЗВУК ДБЖ", |ui| {
            if status.is_none() {
                ui.label(RichText::new("Немає зв'язку з ДБЖ.").color(theme::WEAK));
                return;
            }
            if ui.button("Перемкнути звук ДБЖ").clicked() {
                if let Ok(mut o) = self.beeper_outcome.lock() {
                    *o = None;
                }
                backend::toggle_ups_beeper(&self.beeper_outcome, ctx);
            }
            ui.label(
                RichText::new(
                    "Вмикає або вимикає звукові сигнали ДБЖ (зникнення мережі, низький заряд, \
                     перевантаження). Поточний стан видно на екрані ДБЖ: ДБЖ не повідомляє його програмі.",
                )
                .color(theme::WEAK),
            );
            match &result {
                Some(Ok(m)) => {
                    ui.label(RichText::new(m).color(Level::Ok.color()));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(Level::Alarm.color()));
                }
                None => {}
            }
        });
    }

    fn settings_form(&mut self, ui: &mut egui::Ui) {
        let f = &mut self.form;
        theme::card(ui, "ЗАХИСТ ПК", |ui| {
            ui.horizontal(|ui| {
                ui.label("Вимкнути ПК, якщо мережі немає довше ніж");
                let mut minutes = (f.on_battery_shutdown_secs / 60) as f64;
                if theme::stepper(ui, "pc_off", &mut minutes, 0.0, 240.0, 1.0, 0, "хв") {
                    f.on_battery_shutdown_secs = minutes as u64 * 60;
                }
            });
            if f.on_battery_shutdown_secs == 0 {
                ui.label(
                    RichText::new("0 хв: ПК не вимикатиметься через тривалу відсутність мережі.")
                        .color(theme::WEAK),
                );
            }
            ui.checkbox(
                &mut f.shutdown_on_battery_low,
                "Вимикати ПК одразу, коли ДБЖ повідомляє про низький заряд",
            );
        });

        theme::card(
            ui,
            "ВИМКНЕННЯ ДБЖ ПІСЛЯ ВИМКНЕННЯ ПК",
            |ui| {
                ui.checkbox(
                    &mut f.ups_off_on_pc_shutdown,
                    "Вимикати ДБЖ, коли Windows вимикається (не при перезавантаженні чи сні)",
                );
                ui.add_enabled_ui(f.ups_off_on_pc_shutdown, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Затримка перед вимкненням ДБЖ");
                        theme::stepper(
                            ui,
                            "ups_off_delay",
                            &mut f.ups_off_delay_min,
                            0.5,
                            10.0,
                            0.5,
                            1,
                            "хв",
                        );
                    });
                    ui.checkbox(
                        &mut f.ups_return_when_on_battery,
                        "Якщо ПК вимикався від батареї, ДБЖ увімкнеться сам, коли з'явиться мережа",
                    );
                    ui.checkbox(
                        &mut f.skip_when_update_pending,
                        "Не вимикати ДБЖ, коли Windows встановлює оновлення",
                    );
                });
            },
        );

        theme::card(ui, "ІНШЕ", |ui| {
            ui.horizontal(|ui| {
                ui.label("Опитувати ДБЖ кожні");
                let mut secs = f.poll_interval_secs as f64;
                if theme::stepper(ui, "poll", &mut secs, 1.0, 60.0, 1.0, 0, "с") {
                    f.poll_interval_secs = secs as u64;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Потужність ДБЖ (для показу навантаження у Вт)");
                let mut watts = f.ups_power_w as f64;
                if theme::stepper(ui, "power", &mut watts, 100.0, 10_000.0, 50.0, 0, "Вт") {
                    f.ups_power_w = watts as u32;
                }
            });
            ui.label(
                RichText::new(
                    "Активна потужність з етикетки ДБЖ, наприклад «1000VA / 600W» → 600.",
                )
                .color(theme::WEAK),
            );
            ui.horizontal(|ui| {
                ui.label("Ємність батареї (для оцінки часу роботи)");
                theme::stepper(
                    ui,
                    "battery_ah",
                    &mut f.battery_ah,
                    1.0,
                    500.0,
                    1.0,
                    0,
                    "А·год",
                );
            });
            ui.label(
                RichText::new("Ємність одного блоку з наклейки на батареї. У GU-1000PS зазвичай 2 × 12 В 7 А·год → 7.")
                    .color(theme::WEAK),
            );
        });

        ui.horizontal(|ui| {
            let dirty = self.form != self.saved;
            if ui.add_enabled(dirty, theme::primary("Зберегти", theme::ACCENT)).clicked() {
                self.settings_msg = Some(match self.form.save() {
                    Ok(()) => {
                        self.saved = self.form.clone().sanitized();
                        self.form = self.saved.clone();
                        Ok("Збережено. Служба застосує налаштування протягом кількох секунд.".into())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(
                        "Немає прав на запис config.toml. На вкладці «Стан» натисніть «Перевстановити / оновити службу…»: \
                         це надасть права."
                            .into(),
                    ),
                    Err(e) => Err(format!("Не вдалося зберегти: {e}")),
                });
            }
            if ui.add_enabled(dirty, egui::Button::new("Скасувати зміни")).clicked() {
                self.form = self.saved.clone();
                self.settings_msg = None;
            }
            if ui.button("Типові значення").clicked() {
                self.form = Config::default();
            }
        });
        match &self.settings_msg {
            Some(Ok(m)) => {
                ui.label(RichText::new(m).color(Level::Ok.color()));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(Level::Alarm.color()));
            }
            None => {}
        }
    }
}

fn log_tab(ui: &mut egui::Ui, view: &View) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Журнал служби").size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Відкрити папку").clicked() {
                backend::open_data_dir();
            }
        });
    });
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(12)
        .inner_margin(12)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if view.log.is_empty() {
                        ui.label(RichText::new("Журнал порожній.").color(theme::WEAK));
                    }
                    for line in view.log.iter() {
                        ui.label(
                            RichText::new(line)
                                .monospace()
                                .size(12.0)
                                .color(theme::TEXT),
                        );
                    }
                });
        });
}
