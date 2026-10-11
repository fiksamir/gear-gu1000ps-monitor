//! `gear-ups-gui`: tray icon and window for the GearUps service.
//! `--tray` starts with the window hidden (used by autostart).

#![windows_subsystem = "windows"]

mod app;
mod backend;
mod chart;
mod diagram;
mod theme;
mod tray;

use std::ptr::{null, null_mut};
use std::sync::Arc;

use eframe::egui;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, SetForegroundWindow,
    SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW,
    SW_SHOWNOACTIVATE,
};

use gear_ups::winutil::wide;

const TITLE: &str = "ДБЖ GEAR";

/// Returns false if another copy is already running (and brings it up).
fn single_instance() -> bool {
    let name = wide("Local\\GearUpsGui");
    // The handle is intentionally leaked: it must live as long as the process.
    unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return true;
    }
    show_window(true);
    false
}

/// Shows (and restores) the main window through Win32. Works from any
/// thread and while eframe is not drawing because the window is hidden.
/// With `activate` false it does not take focus (e.g. from a game).
pub fn show_window(activate: bool) {
    let title = wide(TITLE);
    let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
    if hwnd.is_null() {
        return;
    }
    unsafe {
        if activate {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        } else {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// Hides the main window to the tray through Win32. Must match
/// `show_window`: winit does not see Win32 visibility changes, so after one
/// it skips `ViewportCommand::Visible(false)` as already hidden.
pub fn hide_window() {
    let title = wide(TITLE);
    let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
    if !hwnd.is_null() {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

/// Centers the main window in the primary monitor's work area unless it is
/// already on that monitor. Keeps the size; a maximized window is restored
/// first, since moving it would not take it off its monitor.
pub fn move_to_primary_monitor() {
    let title = wide(TITLE);
    let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let current = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if current == primary {
            return;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mut rect: RECT = std::mem::zeroed();
        if GetMonitorInfoW(primary, &mut info) == 0 || GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        if IsZoomed(hwnd) != 0 {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            ShowWindow(hwnd, SW_RESTORE);
            GetWindowRect(hwnd, &mut rect);
        }
        let work = info.rcWork;
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let x = work.left + ((work.right - work.left - w) / 2).max(0);
        let y = work.top + ((work.bottom - work.top - h) / 2).max(0);
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// True when the main window is on screen (shown and not minimized), so
/// animation frames are worth drawing.
pub fn window_on_screen() -> bool {
    let title = wide(TITLE);
    let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
    !hwnd.is_null() && unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 }
}

fn main() -> eframe::Result<()> {
    if !single_instance() {
        return Ok(());
    }
    let start_hidden = std::env::args().any(|a| a == "--tray");
    let icon = egui::IconData {
        rgba: tray::icon_rgba(tray::Level::Ok, 64),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([720.0, 830.0])
            .with_min_inner_size([560.0, 520.0])
            .with_icon(Arc::new(icon))
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            let view = backend::spawn_poller(cc.egui_ctx.clone());
            Ok(Box::new(app::GuiApp::new(cc, view)))
        }),
    )
}
