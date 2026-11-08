//! Registers the host with browsers. Additive and idempotent: only our own `com.zdm.host` entries are
//! written, and a differing existing manifest is backed up once as `<name>.bak`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::{CHROME_EXTENSION_ID, FIREFOX_EXTENSION_ID, HOST_NAME, LEGACY_CHROME_EXTENSION_ID};

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
            let mut origins = vec![format!("chrome-extension://{CHROME_EXTENSION_ID}/"), format!("chrome-extension://{LEGACY_CHROME_EXTENSION_ID}/")];
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

/// Is our browser extension present in this browser?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtState {
    Installed,
    /// Present but switched off (or waiting for the user to approve it).
    Disabled,
    Missing,
}

impl ExtState {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtState::Installed => "installed",
            ExtState::Disabled => "disabled",
            ExtState::Missing => "missing",
        }
    }
}

fn browser(name: &str) -> Option<&'static Browser> {
    BROWSERS.iter().find(|b| b.name == name)
}

/// Where a Chromium-family browser keeps its profiles (the parent of its NativeMessagingHosts folder).
fn config_root(home: &Path, b: &Browser) -> PathBuf {
    home.join(b.linux_dir).parent().map(Path::to_path_buf).unwrap_or_else(|| home.to_path_buf())
}

/// Reads the browsers' own profile files (read-only) to see whether the extension is there.
pub fn extension_state(home: &Path, name: &str) -> ExtState {
    let Some(b) = browser(name) else { return ExtState::Missing };
    let mut best = ExtState::Missing;
    let rank = |s: ExtState| match s {
        ExtState::Installed => 2,
        ExtState::Disabled => 1,
        ExtState::Missing => 0,
    };
    let mut consider = |s: ExtState| {
        if rank(s) > rank(best) {
            best = s;
        }
    };
    match b.kind {
        Kind::Chromium => {
            let Ok(profiles) = std::fs::read_dir(config_root(home, b)) else { return best };
            for p in profiles.flatten() {
                for file in ["Preferences", "Secure Preferences"] {
                    let Ok(text) = std::fs::read_to_string(p.path().join(file)) else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                    for id in [CHROME_EXTENSION_ID, LEGACY_CHROME_EXTENSION_ID] {
                        let Some(e) = v.pointer(&format!("/extensions/settings/{id}")) else { continue };
                        let off = e.get("disable_reasons").and_then(|d| d.as_array()).is_some_and(|d| !d.is_empty()) || e.get("state").and_then(|s| s.as_i64()) == Some(0);
                        consider(if off { ExtState::Disabled } else { ExtState::Installed });
                    }
                }
            }
        }
        Kind::Firefox => {
            let Ok(profiles) = std::fs::read_dir(home.join(".mozilla/firefox")) else { return best };
            for p in profiles.flatten() {
                let Ok(text) = std::fs::read_to_string(p.path().join("extensions.json")) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                for a in v.get("addons").and_then(|a| a.as_array()).into_iter().flatten() {
                    if a.get("id").and_then(|i| i.as_str()) == Some(FIREFOX_EXTENSION_ID) {
                        consider(if a.get("active").and_then(|x| x.as_bool()) == Some(true) { ExtState::Installed } else { ExtState::Disabled });
                    }
                }
            }
        }
    }
    best
}

/// Asks a Chromium-family browser to install the extension from `crx` the next time it starts: writes its
/// "external extension" entry. Firefox has no such route for unsigned add-ons.
pub fn request_extension(home: &Path, name: &str, crx: &Path, version: &str) -> Result<PathBuf> {
    let b = browser(name).ok_or_else(|| anyhow::anyhow!("unknown browser '{name}'"))?;
    anyhow::ensure!(b.kind == Kind::Chromium, "Firefox only accepts add-ons signed by Mozilla; use about:debugging for now");
    anyhow::ensure!(crx.is_file(), "the extension package was not found: {}", crx.display());
    let dir = config_root(home, b).join("External Extensions");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{CHROME_EXTENSION_ID}.json"));
    std::fs::write(&file, serde_json::to_string_pretty(&json!({ "external_crx": crx, "external_version": version }))? + "\n")?;
    Ok(file)
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
    fn extension_state_is_read_from_profiles_and_request_writes_entry() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(extension_state(home.path(), "brave"), ExtState::Missing);
        let prof = home.path().join(".config/BraveSoftware/Brave-Browser/Default");
        std::fs::create_dir_all(&prof).unwrap();
        std::fs::write(prof.join("Preferences"), format!(r#"{{"extensions":{{"settings":{{"{CHROME_EXTENSION_ID}":{{"disable_reasons":[1]}}}}}}}}"#)).unwrap();
        assert_eq!(extension_state(home.path(), "brave"), ExtState::Disabled);
        std::fs::write(prof.join("Preferences"), format!(r#"{{"extensions":{{"settings":{{"{LEGACY_CHROME_EXTENSION_ID}":{{"disable_reasons":[]}}}}}}}}"#)).unwrap();
        assert_eq!(extension_state(home.path(), "brave"), ExtState::Installed);
        let crx = home.path().join("x.crx");
        std::fs::write(&crx, "x").unwrap();
        let f = request_extension(home.path(), "brave", &crx, "1.2.3").unwrap();
        assert!(std::fs::read_to_string(f).unwrap().contains("\"external_version\": \"1.2.3\""));
        assert!(request_extension(home.path(), "firefox", &crx, "1").is_err());
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
