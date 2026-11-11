//! Updating from the project's GitHub releases: finds the latest release,
//! tells whether it is newer than this build, and downloads its two exes
//! next to each other so that its `gear-ups.exe install` can put them in place.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

pub const REPO: &str = "fiksamir/gear-gu1000ps-monitor";
/// The files a release carries; `install` copies the GUI from beside the CLI.
pub const ASSETS: [&str; 2] = ["gear-ups.exe", "gear-ups-gui.exe"];

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Version without the leading `v`, e.g. `0.6.0`.
    pub version: String,
    /// Release notes (Markdown as written on GitHub).
    pub notes: String,
    pub page: String,
    /// Download URL per entry of `ASSETS`.
    urls: Vec<String>,
}

/// `0.6.0` or `v0.6.0` as comparable numbers; missing parts count as 0.
fn parse_version(v: &str) -> Option<[u64; 3]> {
    let mut out = [0; 3];
    let v = v.trim().trim_start_matches('v');
    if v.contains(['-', '+']) {
        // Pre-releases are not offered.
        return None;
    }
    for (i, part) in v.split('.').enumerate() {
        *out.get_mut(i)? = part.parse().ok()?;
    }
    Some(out)
}

/// Whether `candidate` is a later version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse_release(v: &Value) -> Result<Release, String> {
    let tag = v["tag_name"].as_str().ok_or("у відповіді GitHub немає версії")?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let urls = ASSETS
        .iter()
        .map(|name| {
            assets
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .and_then(|a| a["browser_download_url"].as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("у випуску {tag} немає файлу {name}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: v["body"].as_str().unwrap_or("").replace("\r\n", "\n"),
        page: v["html_url"].as_str().unwrap_or("").to_string(),
        urls,
    })
}

/// The latest published release.
pub fn latest() -> Result<Release, String> {
    let agent = crate::telegram::agent(Duration::from_secs(20));
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut reply = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("немає зв'язку з GitHub: {e}"))?;
    let status = reply.status();
    if status == 404 {
        return Err("на GitHub ще немає жодного випуску".into());
    }
    if !status.is_success() {
        return Err(format!("GitHub відповів {status}"));
    }
    let v: Value = reply
        .body_mut()
        .read_json()
        .map_err(|e| format!("незрозуміла відповідь GitHub: {e}"))?;
    parse_release(&v)
}

/// Downloads the release's exes into a fresh folder under %TEMP% and
/// returns the path of its `gear-ups.exe`.
pub fn download(r: &Release) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("GearUps-update-{}", r.version));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("папка {}: {e}", dir.display()))?;
    let agent = crate::telegram::agent(Duration::from_secs(300));
    for (name, url) in ASSETS.iter().zip(&r.urls) {
        let mut reply = agent
            .get(url)
            .call()
            .map_err(|e| format!("завантаження {name}: {e}"))?;
        if !reply.status().is_success() {
            return Err(format!("завантаження {name}: GitHub відповів {}", reply.status()));
        }
        let bytes = reply
            .body_mut()
            .with_config()
            .limit(200 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| format!("завантаження {name}: {e}"))?;
        // Every Windows executable starts with "MZ".
        if !bytes.starts_with(b"MZ") {
            return Err(format!("завантажений {name} не схожий на програму"));
        }
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("запис {name}: {e}"))?;
    }
    Ok(dir.join(ASSETS[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.6.1", "0.6.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1", "0.9.9"));
        assert!(!is_newer("v0.6.0", "0.6.0"));
        assert!(!is_newer("v0.5.1", "0.6.0"));
        assert!(!is_newer("v0.7.0-beta", "0.6.0"));
        assert!(!is_newer("nightly", "0.6.0"));
    }

    #[test]
    fn reads_a_release() {
        let v: Value = serde_json::json!({
            "tag_name": "v0.6.0",
            "body": "Зміни\r\nще",
            "html_url": "https://github.com/x/y/releases/tag/v0.6.0",
            "assets": [
                {"name": "gear-ups-gui.exe", "browser_download_url": "https://e/gui"},
                {"name": "gear-ups.exe", "browser_download_url": "https://e/cli"}
            ]
        });
        let r = parse_release(&v).unwrap();
        assert_eq!(r.version, "0.6.0");
        assert_eq!(r.notes, "Зміни\nще");
        assert_eq!(r.urls, ["https://e/cli", "https://e/gui"]);
        let mut missing = v.clone();
        missing["assets"].as_array_mut().unwrap().pop();
        assert!(parse_release(&missing).is_err());
    }

    /// Asks the real GitHub: `cargo test latest_release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn latest_release() {
        let r = latest().unwrap();
        println!("{} {}", r.version, r.page);
    }
}
