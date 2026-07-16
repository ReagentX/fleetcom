//! JSON session recipes stored one file per name in the user's config directory.
//! Loading a recipe starts new commands; it does not restore live processes.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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

/// Serialize the current schema: `{"name": <original>, "dirs": {...}}`. The
/// stored name is the collision tiebreaker in `save_in` — sanitize maps
/// distinct names ("a/b", "a.b") onto one filename, and only the original
/// distinguishes an overwrite from a clobber.
fn to_json(name: &str, cfg: &SessionConfig) -> String {
    let mut dirs = jzon::JsonValue::new_object();
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
        let _ = dirs.insert(dir, arr);
    }
    let mut obj = jzon::JsonValue::new_object();
    let _ = obj.insert("name", name);
    let _ = obj.insert("dirs", dirs);
    obj.pretty(2)
}

/// Parse either schema, returning the stored original name when present.
/// Detection keys off `dirs` being an *object*: legacy files are flat maps
/// whose top-level values are entry arrays, so even a legacy directory
/// literally named "dirs" cannot masquerade as the wrapper.
fn from_json(text: &str) -> io::Result<(Option<String>, SessionConfig)> {
    let parsed = jzon::parse(text).map_err(|e| io::Error::other(e.to_string()))?;
    let (name, dirs) = if parsed["dirs"].is_object() {
        (parsed["name"].as_str().map(str::to_string), &parsed["dirs"])
    } else {
        (None, &parsed)
    };
    let mut cfg = SessionConfig::new();
    for (dir, val) in dirs.entries() {
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
    Ok((name, cfg))
}

// --- fs surface: callers supply the root. The supervisor resolves it from the
// connection's launch context; `sessions_dir` above is only its process-env
// fallback. Tests point it at scratch dirs the same way. -----------------------

/// Distinguishes concurrent savers' temp files within one process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn save_in(dir: &Path, name: &str, cfg: &SessionConfig) -> io::Result<PathBuf> {
    // Contents are 0600, so filenames are the residual surface: a umask
    // directory on a multi-user host with traversable parents lets anyone
    // enumerate session names and sizes. Create private, and re-tighten a
    // pre-fix directory on its next save — the same on-touch correction the
    // rename below applies to 0644 files. Only `dir` itself is corrected:
    // freshly created parents get 0700 from the builder, but an existing
    // config root (`~/.config`) is shared state this crate never `chmod`s.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let trimmed = name.trim();
    let file_name = format!("{}.json", sanitize(name));
    let file = dir.join(&file_name);

    // Sanitize maps distinct names onto one filename ("a/b" and "a.b" both
    // land at a_b.json), so overwriting on stored-name mismatch would destroy
    // one session while reporting success under the other's name. Refuse
    // before any write: a refusal leaves zero side effects. Files without a
    // stored original — pre-schema saves and torn pre-atomic writes — read as
    // the same session; refusing those would lock users out of re-saving
    // under their own stem.
    match fs::read_to_string(&file) {
        Ok(text) => {
            if let Ok((Some(stored), _)) = from_json(&text)
                && stored != trimmed
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "session \"{trimmed}\" collides with existing \"{stored}\" \
                         (both map to {file_name})"
                    ),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    // Write-temp-then-rename keeps a good copy on disk at every instant:
    // `fs::write` truncates before writing, so a crash mid-save left torn
    // JSON with the prior contents already gone. The temp lives in `dir`
    // itself (rename must not cross filesystems) and ends in `.tmp`, which
    // `list_in`'s `.json` filter never surfaces. Recipes persist full command
    // lines (secrets included), so the temp opens 0600 via `create_new`; the
    // rename carries that mode onto the target, correcting pre-existing 0644
    // files on their next save — intended.
    let pid = std::process::id();
    let (mut tmp_file, tmp) = loop {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".{file_name}.{pid}.{n}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(f) => break (f, candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let written = (|| {
        tmp_file.write_all(to_json(trimmed, cfg).as_bytes())?;
        // Without the sync, the rename can become durable before the data
        // blocks do — resurrecting exactly the torn-file window this path
        // exists to close.
        tmp_file.sync_all()?;
        fs::rename(&tmp, &file)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written?;
    Ok(file)
}

pub fn load_in(dir: &Path, name: &str) -> io::Result<SessionConfig> {
    let file = dir.join(format!("{}.json", sanitize(name)));
    from_json(&fs::read_to_string(file)?).map(|(_, cfg)| cfg)
}

/// Recipe names under `dir`, sorted. The stored original wins over the file
/// stem so callers see the name the user typed; legacy files fall back to
/// their stem, which is already a sanitize fixpoint — either way the returned
/// name loads back to the same file.
pub fn list_in(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                let stored = fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| from_json(&t).ok())
                    .and_then(|(name, _)| name);
                names.push(stored.unwrap_or_else(|| stem.to_string()));
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

    /// String members parse as unadorned entries; flat files carry no name.
    #[test]
    fn parses_the_pre_group_string_only_format() {
        let (name, cfg) = from_json(r#"{"~/proj": ["cargo test", "vim"]}"#).unwrap();
        assert_eq!(name, None);
        assert_eq!(cfg["~/proj"], vec![e("cargo test"), e("vim")]);
    }

    /// Object members may omit the optional `name` field.
    #[test]
    fn parses_the_pre_name_object_format() {
        let (name, cfg) =
            from_json(r#"{"~/proj": [{"cmd": "cargo test", "group": "ci"}]}"#).unwrap();
        assert_eq!(name, None);
        assert_eq!(cfg["~/proj"], vec![ge("cargo test", "ci")]);
    }

    /// Entries without a group or name serialize as strings, inside the
    /// `{"name", "dirs"}` wrapper every save now writes.
    #[test]
    fn group_free_config_writes_string_members_in_the_wrapper() {
        let mut cfg = SessionConfig::new();
        cfg.insert("~/proj".into(), vec![e("cargo test"), e("vim")]);
        cfg.insert("/tmp".into(), vec![e("top")]);

        let expected = "{\n  \"name\": \"work\",\n  \"dirs\": {\n    \"/tmp\": [\n      \"top\"\n    ],\n    \"~/proj\": [\n      \"cargo test\",\n      \"vim\"\n    ]\n  }\n}";
        assert_eq!(to_json("work", &cfg), expected);
    }

    /// Malformed members are omitted rather than decoded into partial entries.
    #[test]
    fn malformed_object_members_drop_without_error() {
        let (_, cfg) = from_json(
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

    /// Recipes persist full command lines (secrets included): the file must
    /// open owner-only, and a save over a pre-schema 0644 file must carry
    /// 0600 onto it via the rename.
    #[test]
    fn saves_owner_only_and_fixes_legacy_permissions() {
        let dir = temp("session_mode");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("run --api-key hunter2")]);

        let file = save_in(&dir, "keys", &cfg).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        save_in(&dir, "keys", &cfg).unwrap();
        assert_eq!(mode(&file), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Filenames are the residual surface once contents are 0600: a fresh
    /// sessions dir is created private (its created parents too), and a
    /// pre-fix umask directory is re-tightened on its next save.
    #[test]
    fn sessions_dir_is_created_private_and_retightened() {
        let base = temp("session_dir_mode");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);

        let nested = base.join("parent").join("sessions");
        save_in(&nested, "fresh", &cfg).unwrap();
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&base.join("parent")), 0o700);

        let loose = base.join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
        save_in(&loose, "old", &cfg).unwrap();
        assert_eq!(mode(&loose), 0o700);
        let _ = fs::remove_dir_all(&base);
    }

    /// The temp is renamed away on success; only the recipe remains.
    #[test]
    fn save_leaves_no_temp_file() {
        let dir = temp("session_notemp");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);

        save_in(&dir, "clean", &cfg).unwrap();
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["clean.json".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// "a/b" and "a.b" sanitize to the same filename; the second save must
    /// refuse rather than clobber, naming both sessions, and the first recipe
    /// must survive intact.
    #[test]
    fn refuses_saves_that_collide_after_sanitize() {
        let dir = temp("session_collide");
        let mut first = SessionConfig::new();
        first.insert("~/one".into(), vec![e("cargo test")]);
        save_in(&dir, "a/b", &first).unwrap();

        let mut second = SessionConfig::new();
        second.insert("~/two".into(), vec![e("vim")]);
        let err = save_in(&dir, "a.b", &second).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("\"a.b\""), "{err}");
        assert!(err.to_string().contains("\"a/b\""), "{err}");
        assert_eq!(load_in(&dir, "a/b").unwrap(), first);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Pre-schema flat files still load, and list falls back to their stem.
    #[test]
    fn loads_and_lists_legacy_flat_schema_files() {
        let dir = temp("session_legacy");
        fs::write(dir.join("old.json"), r#"{"~/proj": ["cargo test"]}"#).unwrap();

        assert_eq!(
            load_in(&dir, "old").unwrap()["~/proj"],
            vec![e("cargo test")]
        );
        assert_eq!(list_in(&dir), vec!["old".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A legacy file carries no stored name, so re-saving under its own stem
    /// is an overwrite of the same session, not a collision.
    #[test]
    fn legacy_file_resaves_under_its_own_stem() {
        let dir = temp("session_legacy_resave");
        fs::write(dir.join("mine.json"), r#"{"~/old": ["vim"]}"#).unwrap();

        let mut cfg = SessionConfig::new();
        cfg.insert("~/new".into(), vec![e("top")]);
        save_in(&dir, "mine", &cfg).unwrap();
        assert_eq!(load_in(&dir, "mine").unwrap(), cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// New-schema files list under the user's original name, not the
    /// sanitized stem they store under; legacy files list by stem. Every
    /// listed name must load back.
    #[test]
    fn lists_stored_names_for_new_schema_files() {
        let dir = temp("session_list_names");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        save_in(&dir, "a/b", &cfg).unwrap();
        fs::write(dir.join("legacy.json"), r#"{"~/x": ["top"]}"#).unwrap();

        assert_eq!(list_in(&dir), vec!["a/b".to_string(), "legacy".to_string()]);
        for n in list_in(&dir) {
            load_in(&dir, &n).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
