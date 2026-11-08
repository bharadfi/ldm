//! Registers the host with browsers. Additive and idempotent: only our own `com.zdm.host` entries are
//! written, and a differing existing manifest is backed up once as `<name>.bak`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::{CHROME_EXTENSION_ID, FIREFOX_EXTENSION_ID, HOST_NAME};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    WriteFile { path: PathBuf, content: String },
    /// Windows: HKCU key whose default value points at the manifest file.
    RegSet { key: String, manifest: PathBuf },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Chromium,
    Firefox,
}

struct Browser {
    name: &'static str,
    kind: Kind,
    /// Linux: directory for the manifest. Windows: registry key.
    #[cfg_attr(unix, allow(dead_code))]
    reg_key: &'static str,
    linux_dir: &'static str,
}

const BROWSERS: &[Browser] = &[
    Browser { name: "chrome", kind: Kind::Chromium, reg_key: r"Software\Google\Chrome\NativeMessagingHost", linux_dir: ".config/google-chrome/NativeMessagingHosts" },
    Browser { name: "chromium", kind: Kind::Chromium, reg_key: r"Software\Chromium\NativeMessagingHosts", linux_dir: ".config/chromium/NativeMessagingHosts" },
    Browser { name: "brave", kind: Kind::Chromium, reg_key: r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts", linux_dir: ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts" },
    Browser { name: "brave-origin", kind: Kind::Chromium, reg_key: r"Software\BraveSoftware\Brave-Origin\NativeMessagingHosts", linux_dir: ".config/BraveSoftware/Brave-Origin/NativeMessagingHosts" },
    Browser { name: "edge", kind: Kind::Chromium, reg_key: r"Software\Microsoft\Edge\NativeMessagingHosts", linux_dir: ".config/microsoft-edge/NativeMessagingHosts" },
    Browser { name: "firefox", kind: Kind::Firefox, reg_key: r"Software\Mozilla\NativeMessagingHosts", linux_dir: ".mozilla/native-messaging-hosts" },
];

pub fn browser_names() -> Vec<&'static str> {
    BROWSERS.iter().map(|b| b.name).collect()
}

fn manifest(kind: Kind, exe: &Path, extra_chrome_ids: &[String]) -> String {
    let mut m = json!({
        "name": HOST_NAME,
        "description": "ZDM download manager browser integration",
        "path": exe,
        "type": "stdio",
    });
    match kind {
        Kind::Chromium => {
            let mut origins = vec![format!("chrome-extension://{CHROME_EXTENSION_ID}/")];
            origins.extend(extra_chrome_ids.iter().map(|id| format!("chrome-extension://{id}/")));
            m["allowed_origins"] = json!(origins);
        }
        Kind::Firefox => m["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]),
    }
    serde_json::to_string_pretty(&m).unwrap() + "\n"
}

/// What `install` would do for the selected browsers (all when `only` is empty).
pub fn plan(exe: &Path, only: &[String], extra_chrome_ids: &[String], home: &Path, data_dir: &Path) -> Result<Vec<Action>> {
    for o in only {
        anyhow::ensure!(BROWSERS.iter().any(|b| b.name == o), "unknown browser '{o}' (choose from: {})", browser_names().join(", "));
    }
    let mut out = Vec::new();
    for b in BROWSERS.iter().filter(|b| only.is_empty() || only.iter().any(|o| o == b.name)) {
        let content = manifest(b.kind, exe, extra_chrome_ids);
        if cfg!(windows) {
            // One manifest per browser kind in our data dir; the registry points at it.
            let path = data_dir.join("nm").join(format!("{HOST_NAME}.{}.json", b.name));
            out.push(Action::WriteFile { path: path.clone(), content });
            out.push(Action::RegSet { key: format!(r"HKCU\{}\{HOST_NAME}", b.reg_key), manifest: path });
        } else {
            out.push(Action::WriteFile { path: home.join(b.linux_dir).join(format!("{HOST_NAME}.json")), content });
        }
    }
    Ok(out)
}

/// Where one browser stands: is our helper registered with it, and does it look installed on this computer?
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserStatus {
    pub name: &'static str,
    pub registered: bool,
    pub found: bool,
}

/// Program names that tell a browser is installed (Linux).
fn program_names(browser: &str) -> &'static [&'static str] {
    match browser {
        "chrome" => &["google-chrome", "google-chrome-stable"],
        "chromium" => &["chromium", "chromium-browser"],
        "brave" => &["brave-browser", "brave"],
        "brave-origin" => &["brave-origin", "brave-origin-stable"],
        "edge" => &["microsoft-edge", "microsoft-edge-stable"],
        "firefox" => &["firefox"],
        _ => &[],
    }
}

/// Registration is checked on Linux by the manifest file; `path_dirs` are the directories searched for programs.
pub fn status(home: &Path, path_dirs: &[PathBuf]) -> Vec<BrowserStatus> {
    BROWSERS
        .iter()
        .map(|b| BrowserStatus {
            name: b.name,
            registered: home.join(b.linux_dir).join(format!("{HOST_NAME}.json")).is_file(),
            found: program_names(b.name).iter().any(|p| path_dirs.iter().any(|d| d.join(p).is_file())),
        })
        .collect()
}

pub fn describe(a: &Action) -> String {
    match a {
        Action::WriteFile { path, .. } => format!("write {}", path.display()),
        Action::RegSet { key, manifest } => format!("set {key} -> {}", manifest.display()),
    }
}

pub fn apply(actions: &[Action]) -> Result<()> {
    for a in actions {
        match a {
            Action::WriteFile { path, content } => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                match std::fs::read_to_string(path) {
                    Ok(existing) if existing == *content => continue, // idempotent
                    Ok(_) => {
                        let bak = path.with_extension("json.bak");
                        if !bak.exists() {
                            std::fs::copy(path, &bak).with_context(|| format!("backup {}", bak.display()))?;
                        }
                    }
                    Err(_) => {}
                }
                std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
            }
            Action::RegSet { key, manifest } => {
                let st = std::process::Command::new("reg").args(["add", key, "/ve", "/t", "REG_SZ", "/d"]).arg(manifest).arg("/f").output()?;
                anyhow::ensure!(st.status.success(), "reg add {key} failed: {}", String::from_utf8_lossy(&st.stderr));
            }
        }
    }
    Ok(())
}

/// Remove only the entries `plan` would create.
pub fn remove(actions: &[Action]) -> Result<()> {
    for a in actions {
        match a {
            Action::WriteFile { path, .. } => {
                let _ = std::fs::remove_file(path);
            }
            Action::RegSet { key, .. } => {
                let _ = std::process::Command::new("reg").args(["delete", key, "/f"]).output();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn status_reports_registration_and_installed_programs() {
        let home = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        std::fs::write(bin.path().join("firefox"), "").unwrap();
        std::fs::write(bin.path().join("brave-browser"), "").unwrap();
        let p = plan(Path::new("/x"), &["firefox".to_string()], &[], home.path(), home.path()).unwrap();
        apply(&p).unwrap();
        let s = status(home.path(), &[bin.path().to_path_buf()]);
        let get = |n: &str| s.iter().find(|b| b.name == n).unwrap().clone();
        assert_eq!((get("firefox").registered, get("firefox").found), (true, true));
        assert_eq!((get("brave").registered, get("brave").found), (false, true));
        assert_eq!((get("chrome").registered, get("chrome").found), (false, false));
        remove(&p).unwrap();
        assert!(!status(home.path(), &[]).iter().any(|b| b.registered));
    }

    #[test]
    fn linux_plan_and_idempotent_apply() {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new("/opt/zdm/zdm-nmhost");
        let p = plan(exe, &[], &[], home.path(), home.path()).unwrap();
        assert_eq!(p.len(), BROWSERS.len());
        apply(&p).unwrap();
        let ff = std::fs::read_to_string(home.path().join(".mozilla/native-messaging-hosts/com.zdm.host.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&ff).unwrap();
        assert_eq!(v["allowed_extensions"][0], FIREFOX_EXTENSION_ID);
        assert_eq!(v["path"], "/opt/zdm/zdm-nmhost");
        let cr = std::fs::read_to_string(home.path().join(".config/google-chrome/NativeMessagingHosts/com.zdm.host.json")).unwrap();
        assert!(cr.contains(&format!("chrome-extension://{CHROME_EXTENSION_ID}/")));
        apply(&p).unwrap(); // second run changes nothing
        assert!(!home.path().join(".mozilla/native-messaging-hosts/com.zdm.host.json.bak").exists());
        // A differing manifest is backed up once.
        std::fs::write(home.path().join(".mozilla/native-messaging-hosts/com.zdm.host.json"), "old").unwrap();
        apply(&p).unwrap();
        assert_eq!(std::fs::read_to_string(home.path().join(".mozilla/native-messaging-hosts/com.zdm.host.json.bak")).unwrap(), "old");
        remove(&p).unwrap();
        assert!(!home.path().join(".config/chromium/NativeMessagingHosts/com.zdm.host.json").exists());
    }

    #[test]
    fn selecting_browsers_and_unknown_names() {
        let h = Path::new("/h");
        let p = plan(Path::new("/x"), &["firefox".to_string()], &[], h, h).unwrap();
        assert!(p.iter().all(|a| describe(a).to_lowercase().contains("mozilla") || describe(a).contains("firefox")));
        assert!(plan(Path::new("/x"), &["netscape".to_string()], &[], h, h).is_err());
    }

    #[test]
    fn extra_chrome_ids_are_added() {
        let m = manifest(Kind::Chromium, Path::new("/x"), &["abcdefghijklmnopabcdefghijklmnop".to_string()]);
        assert!(m.contains("chrome-extension://abcdefghijklmnopabcdefghijklmnop/"));
        assert!(m.contains(CHROME_EXTENSION_ID));
    }
}
