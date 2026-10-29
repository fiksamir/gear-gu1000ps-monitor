//! Telegram bot run by the service: power the PC off or hibernate it from a
//! phone, ask for the status, and get told about power cuts.
//!
//! The service only makes outgoing requests (long polling), so nothing has
//! to be opened on the router. The bot obeys one chat, linked by sending it
//! the one-time code the window shows; everyone else is ignored. Commands
//! older than `STALE_SECS` are dropped: Telegram keeps undelivered messages
//! for a day, and a /shutdown sent while the PC was off must not fire on the
//! next boot.

use std::collections::VecDeque;
use std::fs;
use std::hash::BuildHasher;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::battery;
use crate::events::duration_text;
use crate::config::Config;
use crate::protocol;
use crate::snapshot::{self, Snapshot};
use crate::winutil::{self, data_dir, log};

/// Long-poll wait of getUpdates; the HTTP timeout is a bit longer.
const POLL_SECS: u64 = 50;
/// Messages and button presses older than this are ignored.
const STALE_SECS: u64 = 120;
/// How long a confirmation button stays valid.
const CONFIRM_FOR: Duration = Duration::from_secs(120);
/// Pause after a failed request, and before resending a queued notification.
const RETRY: Duration = Duration::from_secs(15);
/// Network failures in a row before the window shows (and the log keeps) an
/// error: the network is often not up yet right after boot or resume.
const FAILURES_TO_REPORT: u32 = 3;
/// Notifications that could not be sent for this long are dropped.
const NOTE_MAX_AGE: Duration = Duration::from_secs(6 * 3600);
/// Wrong pairing codes before a new code is made.
const MAX_WRONG_CODES: u32 = 5;

/// Countdown of a shutdown asked for from Telegram, seconds.
pub const SHUTDOWN_GRACE_SECS: u32 = 60;

/// Commands the service carries out (hibernation is done by the bot itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    Shutdown,
    Cancel,
}

/// What the window shows about the bot (in status.json).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BotState {
    Off,
    Connecting,
    /// Waiting for "/start <code>" from the owner.
    Pairing { bot: String, code: String },
    Linked { bot: String, who: String },
    Error { message: String },
}

/// A message from the service to the linked chat.
pub struct Note {
    text: String,
    /// A reply to a command: sent even with notifications turned off.
    reply: bool,
    /// Adds a "Скасувати вимкнення" button.
    cancel_button: bool,
    at: Instant,
    time: String,
    /// Dropped once the note is sent or given up on.
    done: Option<Sender<()>>,
}

impl Note {
    pub fn new(text: impl Into<String>) -> Note {
        Note {
            text: text.into(),
            reply: false,
            cancel_button: false,
            at: Instant::now(),
            time: winutil::now_string()[11..16].to_string(),
            done: None,
        }
    }

    pub fn reply(text: impl Into<String>) -> Note {
        Note {
            reply: true,
            ..Note::new(text)
        }
    }

    pub fn with_cancel_button(mut self) -> Note {
        self.cancel_button = true;
        self
    }

    pub fn signal_when_done(mut self, done: Sender<()>) -> Note {
        self.done = Some(done);
        self
    }
}

/// The service's end of the bot.
pub struct Handle {
    pub state: Arc<Mutex<BotState>>,
    pub notes: Sender<Note>,
}

impl Handle {
    pub fn state(&self) -> BotState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(BotState::Off)
    }
}

/// Token and chat of a linked bot, for the notification thread.
type Target = Arc<Mutex<Option<(String, i64)>>>;

/// Starts the bot threads. `on_command` hands a confirmed command to the
/// service; it must not block.
pub fn spawn(on_command: impl Fn(Remote) + Send + 'static) -> Handle {
    let state = Arc::new(Mutex::new(BotState::Off));
    let target: Target = Arc::new(Mutex::new(None));
    let (notes, rx) = mpsc::channel();
    {
        let state = Arc::clone(&state);
        let target = Arc::clone(&target);
        thread::spawn(move || poll_loop(state, target, on_command));
    }
    thread::spawn(move || notify_loop(rx, target));
    Handle { state, notes }
}

// ---------------------------------------------------------------- HTTP

enum ApiError {
    /// No answer: no network, DNS, TLS, timeout.
    Net(String),
    /// Telegram answered with an error.
    Api { code: i64, text: String },
}

impl ApiError {
    fn message(&self) -> String {
        match self {
            ApiError::Net(e) => format!("немає зв'язку з Telegram: {e}"),
            ApiError::Api { code: 401, .. } | ApiError::Api { code: 404, .. } => {
                "Telegram не прийняв токен бота: перевірте його в налаштуваннях".into()
            }
            ApiError::Api { code: 409, .. } => {
                "цей бот уже отримує повідомлення в іншій програмі (або для нього задано webhook)"
                    .into()
            }
            ApiError::Api { code, text } => format!("помилка Telegram {code}: {text}"),
        }
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn call(agent: &ureq::Agent, token: &str, method: &str, body: Value) -> Result<Value, ApiError> {
    // Error texts may quote the URL, which holds the token.
    let scrub = |e: String| e.replace(token, "***");
    let url = format!("https://api.telegram.org/bot{token}/{method}");
    let mut reply: Value = agent
        .post(&url)
        .send_json(&body)
        .map_err(|e| ApiError::Net(scrub(e.to_string())))?
        .into_body()
        .read_json()
        .map_err(|e| ApiError::Net(scrub(e.to_string())))?;
    if reply["ok"].as_bool() == Some(true) {
        Ok(reply["result"].take())
    } else {
        Err(ApiError::Api {
            code: reply["error_code"].as_i64().unwrap_or(0),
            text: scrub(reply["description"].as_str().unwrap_or("").to_string()),
        })
    }
}

fn keyboard(buttons: &[(&str, String)]) -> Value {
    let row: Vec<Value> = buttons
        .iter()
        .map(|(text, data)| json!({ "text": text, "callback_data": data }))
        .collect();
    json!({ "inline_keyboard": [row] })
}

fn cancel_keyboard() -> Value {
    keyboard(&[("Скасувати вимкнення", "cancel".into())])
}

// ---------------------------------------------------------------- link

/// The linked chat, kept by the service in `%ProgramData%\GearUps\telegram.json`
/// (not in config.toml, which the window rewrites from its own copy).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Link {
    /// Numeric part of the token: a different bot starts unlinked.
    bot_id: u64,
    chat_id: Option<i64>,
    who: String,
    /// Next getUpdates offset.
    offset: i64,
}

fn link_path() -> PathBuf {
    data_dir().join("telegram.json")
}

impl Link {
    fn load(bot_id: u64) -> Link {
        fs::read_to_string(link_path())
            .ok()
            .and_then(|t| serde_json::from_str::<Link>(&t).ok())
            .filter(|l| l.bot_id == bot_id)
            .unwrap_or(Link {
                bot_id,
                ..Link::default()
            })
    }

    fn save(&self) {
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = data_dir().join("telegram.json.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, link_path());
        }
    }
}

fn bot_id(token: &str) -> u64 {
    token
        .split(':')
        .next()
        .and_then(|id| id.parse().ok())
        .unwrap_or(0)
}

fn random() -> u64 {
    std::hash::RandomState::new().hash_one(std::time::SystemTime::now())
}

fn new_code() -> String {
    format!("{:06}", random() % 1_000_000)
}

// ---------------------------------------------------------------- polling

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Shutdown,
    Hibernate,
}

struct Pending {
    nonce: String,
    action: Action,
    at: Instant,
}

struct Bot {
    agent: ureq::Agent,
    token: String,
    /// "@name" from getMe; empty until it answered.
    name: String,
    link: Link,
    code: String,
    wrong_codes: u32,
    pending: Option<Pending>,
}

const HELP: &str = "Команди:\n\
    /status — стан живлення і батареї\n\
    /shutdown — вимкнути ПК (через 60 с, можна скасувати)\n\
    /hibernate — гібернація ПК\n\
    /cancel — скасувати вимкнення ПК\n\
    /unlink — відв'язати цей чат від бота";

fn poll_loop(state: Arc<Mutex<BotState>>, target: Target, on_command: impl Fn(Remote)) {
    let set_state = |s: BotState| {
        if let Ok(mut st) = state.lock() {
            *st = s;
        }
    };
    let set_target = |t: Option<(String, i64)>| {
        if let Ok(mut tg) = target.lock() {
            *tg = t;
        }
    };
    let mut bot: Option<Bot> = None;
    let mut failures = 0u32;
    let mut reported: Option<String> = None;
    let mut empty_reads = 0u32;
    loop {
        let token = match Config::read() {
            Ok(cfg) => cfg.unwrap_or_default().telegram_token.0,
            // Broken: the service runs on as it was, and so does the bot.
            Err(_) => match &bot {
                Some(b) => b.token.clone(),
                None => {
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            },
        };
        if token.is_empty() {
            // config.toml is rewritten in place, so a read can catch it
            // empty; only a second empty read in a row turns the bot off.
            empty_reads += 1;
            if empty_reads < 2 {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            if bot.take().is_some() {
                log("Telegram: бота вимкнено в налаштуваннях");
            }
            // Turning the bot off also forgets the linked chat.
            let _ = fs::remove_file(link_path());
            set_target(None);
            set_state(BotState::Off);
            reported = None;
            thread::sleep(Duration::from_secs(5));
            continue;
        }
        empty_reads = 0;
        if bot.as_ref().is_none_or(|b| b.token != token) {
            if bot.is_none() {
                set_state(BotState::Connecting);
            }
            bot = Some(Bot {
                agent: agent(Duration::from_secs(POLL_SECS + 15)),
                link: Link::load(bot_id(&token)),
                token,
                name: String::new(),
                code: new_code(),
                wrong_codes: 0,
                pending: None,
            });
            failures = 0;
        }
        let Some(b) = bot.as_mut() else { continue };
        // Before the first request gets through (no internet after boot),
        // so notifications queue up instead of being dropped.
        set_target(b.link.chat_id.map(|c| (b.token.clone(), c)));

        match b.step(&on_command) {
            Ok(()) => {
                if reported.take().is_some() {
                    log("Telegram: зв'язок відновлено");
                }
                failures = 0;
                set_target(b.link.chat_id.map(|c| (b.token.clone(), c)));
                set_state(match b.link.chat_id {
                    Some(_) => BotState::Linked {
                        bot: b.name.clone(),
                        who: b.link.who.clone(),
                    },
                    None => BotState::Pairing {
                        bot: b.name.clone(),
                        code: b.code.clone(),
                    },
                });
            }
            Err(e) => {
                failures += 1;
                let permanent = matches!(e, ApiError::Api { .. });
                if permanent || failures >= FAILURES_TO_REPORT {
                    let msg = e.message();
                    if reported.as_deref() != Some(msg.as_str()) {
                        log(&format!("Telegram: {msg}"));
                        reported = Some(msg.clone());
                    }
                    set_state(BotState::Error { message: msg });
                }
                thread::sleep(if permanent { RETRY * 4 } else { RETRY });
            }
        }
    }
}

impl Bot {
    /// One getUpdates round (getMe first, until it answers).
    fn step(&mut self, on_command: &impl Fn(Remote)) -> Result<(), ApiError> {
        if self.name.is_empty() {
            let me = self.call("getMe", json!({}))?;
            self.name = format!("@{}", me["username"].as_str().unwrap_or("?"));
            // The command menu in the chat; not worth failing over.
            let commands: Vec<Value> = HELP
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(" — "))
                .map(|(c, d)| json!({ "command": &c[1..], "description": d }))
                .collect();
            let _ = self.call("setMyCommands", json!({ "commands": commands }));
        }
        let updates = self.call(
            "getUpdates",
            json!({
                "offset": self.link.offset,
                "timeout": POLL_SECS,
                "allowed_updates": ["message", "callback_query"],
            }),
        )?;
        let updates = updates.as_array().cloned().unwrap_or_default();
        if updates.is_empty() {
            return Ok(());
        }
        for u in &updates {
            if let Some(id) = u["update_id"].as_i64() {
                self.link.offset = self.link.offset.max(id + 1);
            }
        }
        // Saved before acting, so a shutdown is not repeated after the boot.
        self.link.save();
        for u in &updates {
            if u["message"].is_object() {
                self.on_message(&u["message"], on_command);
            } else if u["callback_query"].is_object() {
                self.on_button(&u["callback_query"], on_command);
            }
        }
        Ok(())
    }

    fn call(&self, method: &str, body: Value) -> Result<Value, ApiError> {
        call(&self.agent, &self.token, method, body)
    }

    fn send(&self, chat: i64, text: &str, markup: Option<Value>) {
        let mut body = json!({ "chat_id": chat, "text": text });
        if let Some(m) = markup {
            body["reply_markup"] = m;
        }
        if let Err(e) = self.call("sendMessage", body) {
            log(&format!("Telegram: не вдалося надіслати відповідь: {}", e.message()));
        }
    }

    fn edit(&self, chat: i64, message_id: i64, text: &str, markup: Option<Value>) {
        let mut body = json!({ "chat_id": chat, "message_id": message_id, "text": text });
        if let Some(m) = markup {
            body["reply_markup"] = m;
        }
        let _ = self.call("editMessageText", body);
    }

    fn on_message(&mut self, msg: &Value, on_command: &impl Fn(Remote)) {
        let (Some(chat), Some(date)) = (msg["chat"]["id"].as_i64(), msg["date"].as_u64()) else {
            return;
        };
        if snapshot::unix_now().saturating_sub(date) > STALE_SECS {
            return;
        }
        let text = msg["text"].as_str().unwrap_or("").trim();
        let mut words = text.split_whitespace();
        // "/status@my_bot" in a group.
        let command = words
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let arg = words.next().unwrap_or("");

        let Some(linked) = self.link.chat_id else {
            self.pair(chat, msg, &command, arg);
            return;
        };
        if chat != linked {
            return;
        }
        match command.as_str() {
            "/status" => self.send(chat, &status_text(), None),
            "/shutdown" => self.confirm(chat, Action::Shutdown),
            "/hibernate" => self.confirm(chat, Action::Hibernate),
            "/cancel" => on_command(Remote::Cancel),
            "/unlink" => {
                log(&format!("Telegram: чат {} відв'язано", self.link.who));
                self.link.chat_id = None;
                self.link.who.clear();
                self.link.save();
                self.code = new_code();
                self.pending = None;
                self.send(
                    chat,
                    "Чат відв'язано. Щоб прив'язати знову, надішліть код із програми GearUps.",
                    None,
                );
            }
            _ => self.send(chat, HELP, None),
        }
    }

    fn pair(&mut self, chat: i64, msg: &Value, command: &str, arg: &str) {
        // Only private chats can be linked: in a group anyone could press the buttons.
        if msg["chat"]["type"].as_str() != Some("private") {
            return;
        }
        let code = if command == "/start" { arg } else { command };
        if code == self.code {
            let from = &msg["from"];
            let mut who = [from["first_name"].as_str(), from["last_name"].as_str()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(user) = from["username"].as_str() {
                who = format!("{who} (@{user})");
            }
            log(&format!("Telegram: бота {} прив'язано до {who}", self.name));
            self.link.chat_id = Some(chat);
            self.link.who = who;
            self.link.save();
            self.send(chat, &format!("Готово, цей чат прив'язано до ПК.\n\n{HELP}"), None);
        } else if !code.is_empty() && code.chars().all(|c| c.is_ascii_digit()) {
            self.wrong_codes += 1;
            if self.wrong_codes >= MAX_WRONG_CODES {
                log("Telegram: забагато невірних кодів прив'язки, створено новий код");
                self.code = new_code();
                self.wrong_codes = 0;
            }
            self.send(chat, "Невірний код.", None);
        } else {
            self.send(
                chat,
                "Щоб керувати ПК, надішліть код із програми GearUps (вкладка «Налаштування», картка Telegram).",
                None,
            );
        }
    }

    fn confirm(&mut self, chat: i64, action: Action) {
        let nonce = format!("{:08x}", random() as u32);
        let (question, yes) = match action {
            Action::Shutdown => ("Вимкнути ПК?", "Так, вимкнути"),
            Action::Hibernate => ("Перевести ПК у гібернацію?", "Так, гібернація"),
        };
        self.send(
            chat,
            question,
            Some(keyboard(&[
                (yes, format!("yes:{nonce}")),
                ("Ні", format!("no:{nonce}")),
            ])),
        );
        self.pending = Some(Pending {
            nonce,
            action,
            at: Instant::now(),
        });
    }

    fn on_button(&mut self, cb: &Value, on_command: &impl Fn(Remote)) {
        let _ = self.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": cb["id"].as_str().unwrap_or("") }),
        );
        let chat = cb["message"]["chat"]["id"].as_i64();
        let message_id = cb["message"]["message_id"].as_i64().unwrap_or(0);
        let (Some(chat), Some(linked)) = (chat, self.link.chat_id) else {
            return;
        };
        if chat != linked {
            return;
        }
        let data = cb["data"].as_str().unwrap_or("");
        let (verb, nonce) = data.split_once(':').unwrap_or((data, ""));
        let pending = self
            .pending
            .take_if(|p| p.nonce == nonce && p.at.elapsed() < CONFIRM_FOR);
        match (verb, pending) {
            ("cancel", _) => on_command(Remote::Cancel),
            ("no", _) => self.edit(chat, message_id, "Добре, нічого не роблю.", None),
            ("yes", Some(p)) if p.action == Action::Shutdown => {
                self.edit(
                    chat,
                    message_id,
                    &format!("ПК вимкнеться через {SHUTDOWN_GRACE_SECS} с."),
                    Some(cancel_keyboard()),
                );
                on_command(Remote::Shutdown);
            }
            ("yes", Some(_)) => {
                self.edit(chat, message_id, "Переводжу ПК у гібернацію.", None);
                log("команда з Telegram: гібернація");
                // Returns once the PC is awake again.
                if let Err(e) = winutil::hibernate() {
                    log(&format!("гібернація не вдалася: {e}"));
                    self.send(chat, &format!("Не вдалося: {e}"), None);
                }
            }
            _ => self.edit(
                chat,
                message_id,
                "Запит застарів. Надішліть команду ще раз.",
                None,
            ),
        }
    }
}

// ---------------------------------------------------------------- notes

fn notify_loop(rx: Receiver<Note>, target: Target) {
    let agent = agent(Duration::from_secs(10));
    let mut queue: VecDeque<Note> = VecDeque::new();
    loop {
        let next = if queue.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(RETRY)
        };
        match next {
            Ok(note) => queue.push_back(note),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        queue.extend(rx.try_iter());

        let Some((token, chat)) = target.lock().ok().and_then(|t| t.clone()) else {
            queue.clear();
            continue;
        };
        let notify = Config::read()
            .ok()
            .flatten()
            .unwrap_or_default()
            .telegram_notify;
        while let Some(note) = queue.front() {
            if (!note.reply && !notify) || note.at.elapsed() > NOTE_MAX_AGE {
                queue.pop_front();
                continue;
            }
            let text = if note.at.elapsed() > Duration::from_secs(60) {
                format!("{} (о {})", note.text, note.time)
            } else {
                note.text.clone()
            };
            let mut body = json!({ "chat_id": chat, "text": text });
            if note.cancel_button {
                body["reply_markup"] = cancel_keyboard();
            }
            match call(&agent, &token, "sendMessage", body) {
                Ok(_) => {
                    if let Some(done) = queue.pop_front().and_then(|n| n.done) {
                        let _ = done.send(());
                    }
                }
                Err(ApiError::Net(_)) => break,
                Err(e) => {
                    log(&format!("Telegram: сповіщення не надіслано: {}", e.message()));
                    queue.pop_front();
                }
            }
        }
    }
}

// ---------------------------------------------------------------- texts

/// The answer to /status, from the service's last snapshot.
fn status_text() -> String {
    let cfg = Config::read().ok().flatten().unwrap_or_default();
    match snapshot::read() {
        Some(snap) if !snap.is_stale() => describe(&snap, &cfg),
        _ => "Служба GearUps не оновлює стан.".into(),
    }
}

fn describe(snap: &Snapshot, cfg: &Config) -> String {
    let Some(st) = snap.status.as_ref().filter(|_| snap.connected) else {
        return format!(
            "ДБЖ не підключено. {}",
            snap.error.clone().unwrap_or_default()
        );
    };
    let mut lines = Vec::new();
    if st.on_battery() {
        lines.push(format!(
            "🔋 Мережі немає {}, ПК працює від батареї.",
            duration_text(snap.on_battery_secs.unwrap_or(0) as i64)
        ));
        let counted = snap.battery_charge_pct.zip(snap.battery_capacity_ah);
        let (charge, minutes) = battery::estimate(
            st,
            snap.nominal_battery_v,
            cfg.ups_power_w,
            cfg.battery_ah as f32,
            counted,
        );
        lines.push(format!(
            "Заряд ~{charge} %, вистачить на {}.",
            battery::runtime_text(minutes)
        ));
        if st.battery_low() {
            lines.push("🪫 Низький заряд батареї.".into());
        }
        if let Some(left) = snap.pc_shutdown_in_secs.filter(|_| !snap.pc_shutdown_requested) {
            lines.push(format!("ПК вимкнеться через {}.", duration_text(left as i64)));
        }
    } else {
        lines.push(format!("🔌 Живлення від мережі, {:.0} В.", st.input_v));
        if let Some(pct) = st.charge_pct {
            lines.push(format!("Заряд батареї {pct} %."));
        }
    }
    if snap.pc_shutdown_requested {
        lines.push("⏻ ПК вимикається.".into());
    }
    lines.push(format!(
        "Навантаження {} Вт ({} %), батарея {:.1} В, ДБЖ {:.0} °C.",
        protocol::watts(st.load_pct, cfg.ups_power_w),
        st.load_pct,
        st.battery_v,
        st.temp_c
    ));
    if snap.frozen_secs.is_some() {
        lines.push(
            "⚠ Дані від ДБЖ не оновлюються: USB-контролер ДБЖ завис, від'єднайте його USB-кабель на 10 с."
                .into(),
        );
    }
    if st.ups_failed() {
        lines.push("⚠ ДБЖ повідомляє про несправність.".into());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_id_is_the_token_prefix() {
        assert_eq!(bot_id("123456:ABC-def"), 123456);
        assert_eq!(bot_id("garbage"), 0);
    }

    #[test]
    fn codes_have_six_digits() {
        let c = new_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
    }

    /// Reaches api.telegram.org over TLS: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn telegram_rejects_a_made_up_token() {
        let token = "123456:made-up";
        match call(&agent(Duration::from_secs(20)), token, "getMe", json!({})) {
            Err(e @ ApiError::Api { code: 401 | 404, .. }) => assert!(!e.message().contains(token)),
            Err(e) => panic!("{}", e.message()),
            Ok(v) => panic!("{v}"),
        }
    }

    #[test]
    fn help_lists_commands_for_the_menu() {
        let n = HELP.lines().skip(1).filter(|l| l.split_once(" — ").is_some()).count();
        assert_eq!(n, 5);
    }
}
