//! JSON session recipes stored one file per name in the user's config directory.
//! Loading a recipe starts new commands; it does not restore live processes.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

/// One recipe entry. Entries without a group or name serialize as strings;
/// other entries use objects whose optional fields are written only when set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub cmd: String,
    pub group: Option<String>,
    pub name: Option<String>,
}

/// Session recipe mapping directories to ordered entries.
pub type SessionConfig = BTreeMap<String, Vec<SessionEntry>>;

/// Env var overriding the config root that session recipes live under.
pub const FLEETCOM_CONFIG_DIR: &str = "FLEETCOM_CONFIG_DIR";

/// Characters replaced with `_` in session filenames.
const DISALLOWED: &[char] = &['*', '"', '/', '\\', '<', '>', ':', '|', '?', '.'];

/// Make `name` safe as a bare filename.
fn sanitize(name: &str) -> String {
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

/// Session-recipe directory: `<config root>/sessions`. A caller-supplied
/// `root` wins (the supervisor passes the connecting client's
/// [`FLEETCOM_CONFIG_DIR`]); otherwise the same var from this process's env,
/// else `dirs::config_dir()/fleetcom`.
pub fn sessions_dir(root: Option<PathBuf>) -> Option<PathBuf> {
    root.or_else(|| std::env::var(FLEETCOM_CONFIG_DIR).ok().map(PathBuf::from))
        .or_else(|| dirs::config_dir().map(|c| c.join("fleetcom")))
        .map(|base| base.join("sessions"))
}

fn to_json(cfg: &SessionConfig) -> String {
    let mut obj = jzon::JsonValue::new_object();
    for (dir, entries) in cfg {
        let mut arr = jzon::JsonValue::new_array();
        for e in entries {
            let member = if e.group.is_none() && e.name.is_none() {
                // Entries without optional labels use the string form.
                jzon::JsonValue::from(e.cmd.as_str())
            } else {
                let mut m = jzon::JsonValue::new_object();
                let _ = m.insert("cmd", e.cmd.as_str());
                if let Some(g) = &e.group {
                    let _ = m.insert("group", g.as_str());
                }
                if let Some(n) = &e.name {
                    let _ = m.insert("name", n.as_str());
                }
                m
            };
            let _ = arr.push(member);
        }
        let _ = obj.insert(dir, arr);
    }
    obj.pretty(2)
}

fn from_json(text: &str) -> io::Result<SessionConfig> {
    let parsed = jzon::parse(text).map_err(|e| io::Error::other(e.to_string()))?;
    let mut cfg = SessionConfig::new();
    for (dir, val) in parsed.entries() {
        // Ignore members that match neither supported entry form.
        let entries = val
            .members()
            .filter_map(|m| {
                if let Some(cmd) = m.as_str() {
                    return Some(SessionEntry {
                        cmd: cmd.to_string(),
                        group: None,
                        name: None,
                    });
                }
                // Indexing a non-object yields Null, so malformed members drop here.
                let cmd = m["cmd"].as_str()?.to_string();
                let group = match &m["group"] {
                    g if g.is_null() => None,
                    g => Some(g.as_str()?.to_string()),
                };
                let name = match &m["name"] {
                    n if n.is_null() => None,
                    n => Some(n.as_str()?.to_string()),
                };
                Some(SessionEntry { cmd, group, name })
            })
            .collect();
        cfg.insert(dir.to_string(), entries);
    }
    Ok(cfg)
}

// --- fs surface: callers supply the root. The supervisor resolves it from the
// connection's launch context; `sessions_dir` above is only its process-env
// fallback. Tests point it at scratch dirs the same way. -----------------------

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp;

    /// Unadorned entry: the plain-string member form.
    fn e(cmd: &str) -> SessionEntry {
        SessionEntry {
            cmd: cmd.into(),
            group: None,
            name: None,
        }
    }

    /// Grouped entry: the `{"cmd", "group"}` member form.
    fn ge(cmd: &str, group: &str) -> SessionEntry {
        SessionEntry {
            cmd: cmd.into(),
            group: Some(group.into()),
            name: None,
        }
    }

    /// Named entry: the `{"cmd", "name"}` member form.
    fn ne(cmd: &str, name: &str) -> SessionEntry {
        SessionEntry {
            cmd: cmd.into(),
            group: None,
            name: Some(name.into()),
        }
    }

    /// Grouped and named entry: the full `{"cmd", "group", "name"}` form.
    fn gne(cmd: &str, group: &str, name: &str) -> SessionEntry {
        SessionEntry {
            cmd: cmd.into(),
            group: Some(group.into()),
            name: Some(name.into()),
        }
    }

    #[test]
    fn round_trips_dirs_and_commands() {
        let dir = temp("session_roundtrip");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/proj".into(), vec![e("cargo test"), e("vim")]);
        cfg.insert("/tmp".into(), vec![e("top")]);

        save_in(&dir, "work", &cfg).unwrap();
        assert_eq!(load_in(&dir, "work").unwrap(), cfg);
        assert_eq!(list_in(&dir), vec!["work".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mixed string and object entries survive one serialization round trip.
    #[test]
    fn round_trips_mixed_grouped_and_ungrouped_entries() {
        let dir = temp("session_mixed");
        let mut cfg = SessionConfig::new();
        cfg.insert(
            "~/proj".into(),
            vec![ge("cargo test", "ci"), e("vim"), ge("top", "ops")],
        );

        save_in(&dir, "mixed", &cfg).unwrap();
        assert_eq!(load_in(&dir, "mixed").unwrap(), cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every group/name combination survives serialization.
    #[test]
    fn round_trips_named_entries() {
        let dir = temp("session_named");
        let mut cfg = SessionConfig::new();
        cfg.insert(
            "~/proj".into(),
            vec![
                gne("cargo test", "ci", "unit tests"),
                ne("vim", "editor"),
                ge("top", "ops"),
                e("plain"),
            ],
        );

        save_in(&dir, "named", &cfg).unwrap();
        assert_eq!(load_in(&dir, "named").unwrap(), cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// String members parse as unadorned entries.
    #[test]
    fn parses_the_pre_group_string_only_format() {
        let cfg = from_json(r#"{"~/proj": ["cargo test", "vim"]}"#).unwrap();
        assert_eq!(cfg["~/proj"], vec![e("cargo test"), e("vim")]);
    }

    /// Object members may omit the optional `name` field.
    #[test]
    fn parses_the_pre_name_object_format() {
        let cfg = from_json(r#"{"~/proj": [{"cmd": "cargo test", "group": "ci"}]}"#).unwrap();
        assert_eq!(cfg["~/proj"], vec![ge("cargo test", "ci")]);
    }

    /// Entries without a group or name serialize as strings.
    #[test]
    fn group_free_config_writes_the_pre_group_bytes() {
        let mut cfg = SessionConfig::new();
        cfg.insert("~/proj".into(), vec![e("cargo test"), e("vim")]);
        cfg.insert("/tmp".into(), vec![e("top")]);

        let expected = "{\n  \"/tmp\": [\n    \"top\"\n  ],\n  \"~/proj\": [\n    \"cargo test\",\n    \"vim\"\n  ]\n}";
        assert_eq!(to_json(&cfg), expected);
    }

    /// Malformed members are omitted rather than decoded into partial entries.
    #[test]
    fn malformed_object_members_drop_without_error() {
        let cfg = from_json(
            r#"{"d": [
                {"group": "g"},
                {"cmd": 3},
                {"cmd": "x", "group": 5},
                {"cmd": "y", "name": 5},
                42,
                {"cmd": "bare"},
                {"cmd": "n", "group": null},
                {"cmd": "m", "name": null},
                {"cmd": "ok", "group": "api"},
                {"cmd": "named", "name": "web"},
                "plain"
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            cfg["d"],
            vec![
                e("bare"),
                e("n"),
                e("m"),
                ge("ok", "api"),
                ne("named", "web"),
                e("plain")
            ]
        );
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("my/session"), "my_session");
        assert_eq!(sanitize("  a.b  "), "a_b");
    }
}
