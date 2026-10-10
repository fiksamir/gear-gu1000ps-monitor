//! Tray icon whose colour shows the UPS state, with an "open / quit" menu.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Alarm,
    Off,
}

impl Level {
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Level::Ok => [46, 160, 67],
            Level::Warn => [230, 140, 20],
            Level::Alarm => [210, 45, 45],
            Level::Off => [130, 130, 130],
        }
    }

    pub fn color(self) -> egui::Color32 {
        let [r, g, b] = self.rgb();
        egui::Color32::from_rgb(r, g, b)
    }
}

/// The app icon (white bolt on the state colour), `size`×`size` RGBA.
pub fn icon_rgba(level: Level, size: u32) -> Vec<u8> {
    gear_ups::icon::rgba(level.rgb(), size)
}

fn icon(level: Level) -> Option<Icon> {
    Icon::from_rgba(icon_rgba(level, 32), 32, 32).ok()
}

pub struct Tray {
    icon: TrayIcon,
    level: Option<Level>,
    tooltip: String,
}

/// Flags the tray handlers raise for the UI thread.
#[derive(Default)]
pub struct Requests {
    pub quit: AtomicBool,
}

impl Tray {
    /// Must be called on the UI thread (it owns the message loop).
    pub fn new(ctx: &egui::Context, requests: Arc<Requests>) -> Result<Tray, String> {
        let open = MenuItem::new("Відкрити вікно", true, None);
        let quit = MenuItem::new("Вийти", true, None);
        let menu = Menu::new();
        menu.append(&open).map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;

        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        let (r, c) = (Arc::clone(&requests), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                crate::show_window(true);
            } else if e.id == quit_id {
                // Shown so eframe draws a frame and can process the close.
                r.quit.store(true, Ordering::SeqCst);
                crate::show_window(false);
            }
            c.request_repaint();
        }));
        let c = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let open = match e {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => true,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => true,
                _ => false,
            };
            if open {
                crate::show_window(true);
                c.request_repaint();
            }
        }));

        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("ДБЖ GEAR");
        if let Some(i) = icon(Level::Off) {
            builder = builder.with_icon(i);
        }
        let icon = builder.build().map_err(|e| e.to_string())?;
        Ok(Tray {
            icon,
            level: None,
            tooltip: String::new(),
        })
    }

    pub fn set_state(&mut self, level: Level, tooltip: &str) {
        if self.level != Some(level) {
            self.level = Some(level);
            let _ = self.icon.set_icon(icon(level));
        }
        if self.tooltip != tooltip {
            self.tooltip = tooltip.to_string();
            // The shell truncates tooltips at 127 characters.
            let short: String = tooltip.chars().take(120).collect();
            let _ = self.icon.set_tooltip(Some(short));
        }
    }
}
