//! Session save/load: a `{dir: [command, ...]}` recipe, JSON on disk, one file
//! per session name under the user's config dir.
//!
//! A session is a *recipe*: loading it re-runs the commands; it does not
//! resurrect live processes (that's the daemon's job). Uses `jzon`, not serde:
//! the schema is a flat map that needs no derive.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A session recipe: working directory → the commands to run there. `BTreeMap`
/// so dirs serialize in a stable (alphabetical) order; command order within a
/// dir is preserved by the `Vec`.
pub type SessionConfig = BTreeMap<String, Vec<String>>;

/// Characters replaced with `_` in session filenames.
const DISALLOWED: &[char] = &['*', '"', '/', '\\', '<', '>', ':', '|', '?', '.'];

/// Make `name` safe as a bare filename.
pub fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_control() || DISALLOWED.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .take(255)
        .collect()
}

/// `<config>/fleetcom/sessions`, overridable with `FLEETCOM_CONFIG_DIR`.
pub fn sessions_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("FLEETCOM_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("sessions"));
    }
    dirs::config_dir().map(|c| c.join("fleetcom").join("sessions"))
}

fn to_json(cfg: &SessionConfig) -> String {
    let mut obj = jzon::JsonValue::new_object();
    for (dir, cmds) in cfg {
        let mut arr = jzon::JsonValue::new_array();
        for c in cmds {
            let _ = arr.push(c.as_str());
        }
        let _ = obj.insert(dir, arr);
    }
    obj.pretty(2)
}

fn from_json(text: &str) -> io::Result<SessionConfig> {
    let parsed = jzon::parse(text).map_err(|e| io::Error::other(e.to_string()))?;
    let mut cfg = SessionConfig::new();
    for (dir, val) in parsed.entries() {
        let cmds = val
            .members()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
        cfg.insert(dir.to_string(), cmds);
    }
    Ok(cfg)
}

// --- fs surface, taking an explicit dir so tests avoid the real config path --

pub fn save_in(dir: &Path, name: &str, cfg: &SessionConfig) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let file = dir.join(format!("{}.json", sanitize(name)));
    fs::write(&file, to_json(cfg))?;
    Ok(file)
}

pub fn load_in(dir: &Path, name: &str) -> io::Result<SessionConfig> {
    let file = dir.join(format!("{}.json", sanitize(name)));
    from_json(&fs::read_to_string(file)?)
}

pub fn list_in(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

// --- convenience wrappers over the real config dir ---------------------------

fn no_dir() -> io::Error {
    io::Error::other("no config directory available")
}

pub fn save(name: &str, cfg: &SessionConfig) -> io::Result<PathBuf> {
    save_in(&sessions_dir().ok_or_else(no_dir)?, name, cfg)
}

pub fn load(name: &str) -> io::Result<SessionConfig> {
    load_in(&sessions_dir().ok_or_else(no_dir)?, name)
}

pub fn list() -> Vec<String> {
    sessions_dir().map(|d| list_in(&d)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleetcom_session_test_{tag}"));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trips_dirs_and_commands() {
        let dir = temp("roundtrip");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/proj".into(), vec!["cargo test".into(), "vim".into()]);
        cfg.insert("/tmp".into(), vec!["top".into()]);

        save_in(&dir, "work", &cfg).unwrap();
        assert_eq!(load_in(&dir, "work").unwrap(), cfg);
        assert_eq!(list_in(&dir), vec!["work".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("my/session"), "my_session");
        assert_eq!(sanitize("  a.b  "), "a_b");
    }
}
