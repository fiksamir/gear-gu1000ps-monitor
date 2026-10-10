//! Windows plumbing: log file, shutdown-type detection, initiating a PC shutdown.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID, SYSTEMTIME};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
    EvtRenderEventXml,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows_sys::Win32::System::Shutdown::{
    InitiateShutdownW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_POWER,
    SHTDN_REASON_MINOR_ENVIRONMENT, SHUTDOWN_FORCE_OTHERS, SHUTDOWN_POWEROFF,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(base).join("GearUps")
}

pub fn now_string() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Appends a line to the service log (rotated at 1 MB). Logging must never
/// take the service down, so errors are ignored.
pub fn log(msg: &str) {
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("gear-ups.log");
    if fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, dir.join("gear-ups.log.old"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}  {}", now_string(), msg);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    PowerOff,
    Restart,
    Unknown,
}

/// Classifies the (localized) shutdown-type text of a User32 event 1074.
pub fn classify(kind_text: &str) -> ShutdownKind {
    let t = kind_text.to_lowercase();
    const RESTART: &[&str] = &["restart", "reboot", "перезап", "перезавант", "перезагр"];
    const POWER_OFF: &[&str] = &[
        "power off",
        "shutdown",
        "shut down",
        "вимкнення живлення",
        "завершити роботу",
        "завершення роботи",
        "отключение питания",
        "завершение работы",
        "выключ",
    ];
    if RESTART.iter().any(|w| t.contains(w)) {
        ShutdownKind::Restart
    } else if POWER_OFF.iter().any(|w| t.contains(w)) {
        ShutdownKind::PowerOff
    } else {
        ShutdownKind::Unknown
    }
}

/// Combines recent events (newest first). Any restart in the window wins,
/// because cutting power during a reboot is the dangerous mistake.
pub fn decide(kinds: &[ShutdownKind]) -> ShutdownKind {
    if kinds.contains(&ShutdownKind::Restart) {
        ShutdownKind::Restart
    } else {
        kinds.first().copied().unwrap_or(ShutdownKind::Unknown)
    }
}

#[derive(Debug, Clone)]
pub struct ShutdownEvent {
    pub process: String,
    pub kind_text: String,
}

/// User32 event 1074 records newer than `max_age_secs` (0 = any age), newest first.
pub fn recent_shutdown_events(max_age_secs: u32, limit: usize) -> io::Result<Vec<ShutdownEvent>> {
    let age = if max_age_secs > 0 {
        format!(
            " and TimeCreated[timediff(@SystemTime) <= {}]",
            max_age_secs as u64 * 1000
        )
    } else {
        String::new()
    };
    let query = wide(&format!(
        "*[System[Provider[@Name='User32'] and (EventID=1074){age}]]"
    ));
    let channel = wide("System");
    let results = unsafe {
        EvtQuery(
            0,
            channel.as_ptr(),
            query.as_ptr(),
            EvtQueryChannelPath | EvtQueryReverseDirection,
        )
    };
    if results == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    while out.len() < limit {
        let mut ev = 0isize;
        let mut returned = 0u32;
        if unsafe { EvtNext(results, 1, &mut ev, 1000, 0, &mut returned) } == 0 || returned == 0 {
            break;
        }
        if let Some(xml) = render_xml(ev) {
            out.push(ShutdownEvent {
                process: xml_data(&xml, "param1").unwrap_or_default(),
                kind_text: xml_data(&xml, "param5").unwrap_or_default(),
            });
        }
        unsafe { EvtClose(ev) };
    }
    unsafe { EvtClose(results) };
    Ok(out)
}

fn render_xml(ev: isize) -> Option<String> {
    let mut used = 0u32;
    let mut props = 0u32;
    unsafe {
        EvtRender(
            0,
            ev,
            EvtRenderEventXml,
            0,
            null_mut(),
            &mut used,
            &mut props,
        )
    };
    if used == 0 {
        return None;
    }
    let mut buf = vec![0u16; (used as usize).div_ceil(2)];
    let ok = unsafe {
        EvtRender(
            0,
            ev,
            EvtRenderEventXml,
            (buf.len() * 2) as u32,
            buf.as_mut_ptr().cast(),
            &mut used,
            &mut props,
        )
    } != 0;
    if !ok {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn xml_data(xml: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let open = format!("<Data Name={quote}{name}{quote}>");
        if let Some(start) = xml.find(&open) {
            let rest = &xml[start + open.len()..];
            let end = rest.find("</Data>")?;
            return Some(
                rest[..end]
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&amp;", "&"),
            );
        }
    }
    None
}

/// True when Windows may install updates during this shutdown; those can run
/// for many minutes, longer than any UPS delay.
pub fn update_pending() -> bool {
    const KEYS: &[&str] = &[
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
    ];
    KEYS.iter().any(|k| {
        let path = wide(k);
        let mut key: HKEY = null_mut();
        let found =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) } == 0;
        if found {
            unsafe { RegCloseKey(key) };
        }
        found
    })
}

fn enable_shutdown_privilege() -> io::Result<()> {
    let mut token: HANDLE = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let name = wide("SeShutdownPrivilege");
    let mut luid: LUID = unsafe { std::mem::zeroed() };
    let result = if unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if unsafe { AdjustTokenPrivileges(token, 0, &tp, 0, null_mut(), null_mut()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    unsafe { CloseHandle(token) };
    result
}

/// Full (non-hybrid) power-off of the PC after `grace_secs`, with a message
/// shown to logged-on users.
pub fn shutdown_pc(message: &str, grace_secs: u32) -> io::Result<()> {
    enable_shutdown_privilege()?;
    let msg = wide(message);
    let rc = unsafe {
        InitiateShutdownW(
            null(),
            msg.as_ptr(),
            grace_secs,
            SHUTDOWN_POWEROFF | SHUTDOWN_FORCE_OTHERS,
            SHTDN_REASON_MAJOR_POWER | SHTDN_REASON_MINOR_ENVIRONMENT | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_strings_seen_on_this_pc() {
        assert_eq!(classify("Вимкнення живлення"), ShutdownKind::PowerOff);
        assert_eq!(classify("Завершити роботу"), ShutdownKind::PowerOff);
        assert_eq!(classify("перезапустити"), ShutdownKind::Restart);
        assert_eq!(classify("power off"), ShutdownKind::PowerOff);
        assert_eq!(classify("restart"), ShutdownKind::Restart);
        assert_eq!(classify(""), ShutdownKind::Unknown);
    }

    #[test]
    fn restart_anywhere_in_window_wins() {
        use ShutdownKind::*;
        assert_eq!(decide(&[PowerOff, Restart]), Restart);
        assert_eq!(decide(&[PowerOff]), PowerOff);
        assert_eq!(decide(&[Unknown, PowerOff]), Unknown);
        assert_eq!(decide(&[]), Unknown);
    }

    #[test]
    fn extracts_event_data() {
        let xml = r#"<Event><EventData><Data Name="param1">C:\x.exe</Data><Data Name="param5">Вимкнення живлення</Data></EventData></Event>"#;
        assert_eq!(
            xml_data(xml, "param5").as_deref(),
            Some("Вимкнення живлення")
        );
        assert_eq!(xml_data(xml, "param1").as_deref(), Some("C:\\x.exe"));
        assert_eq!(xml_data(xml, "param9"), None);
    }
}
