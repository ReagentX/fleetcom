//! JSON session recipes stored one file per name in the user's config directory.
//! Loading a recipe starts new commands; it does not restore live processes.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
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

/// Sanitized-stem cap that reserves 5 bytes for `.json` in a 255-byte
/// filename component.
const MAX_STEM_BYTES: usize = 250;

/// Sanitize `name` and limit its UTF-8 encoding without splitting a character.
fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = if c.is_control() || DISALLOWED.contains(&c) {
            '_'
        } else {
            c
        };
        if out.len() + c.len_utf8() > MAX_STEM_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// Longest prefix of `s` at most `max` bytes long, on a char boundary.
fn prefix_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

/// Session format version written by `to_json` and accepted by `from_json`.
/// Missing versions are interpreted as version 1; unsupported versions fail.
const FORMAT_VERSION: u64 = 1;

/// Build the `dirs` object alone: the recipe body without the wrapper.
fn dirs_json(cfg: &SessionConfig) -> jzon::JsonValue {
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
    dirs
}

/// Serialize the versioned wrapped schema. The stored name distinguishes
/// names that sanitize to the same filename.
fn to_json(name: &str, cfg: &SessionConfig) -> String {
    let mut obj = jzon::JsonValue::new_object();
    let _ = obj.insert("version", FORMAT_VERSION);
    let _ = obj.insert("name", name);
    let _ = obj.insert("dirs", dirs_json(cfg));
    obj.pretty(2)
}

/// Serialize only the recipe body, for change detection. Excludes the wrapper
/// because its `name` field is volatile in recovery snapshots (the label
/// carries the write time): equal recipes must fingerprint equal.
pub fn fingerprint_json(cfg: &SessionConfig) -> String {
    dirs_json(cfg).dump()
}

/// Parse wrapped and flat schemas, returning the stored name when present.
/// A wrapped file has an object-valued `dirs`; flat files have entry arrays at
/// the top level, including when a directory is literally named `dirs`.
/// A top-level `version` must be an integer from 1 through [`FORMAT_VERSION`];
/// a missing version is interpreted as 1.
fn from_json(text: &str) -> io::Result<(Option<String>, SessionConfig)> {
    let parsed = jzon::parse(text).map_err(|e| io::Error::other(e.to_string()))?;
    // Validate version metadata before detecting the schema shape.
    let version = &parsed["version"];
    if !version.is_null() {
        match version.as_u64() {
            Some(n) if (1..=FORMAT_VERSION).contains(&n) => {}
            Some(n) if n > FORMAT_VERSION => {
                return Err(io::Error::other(format!(
                    "session format version {n} is newer than this fleetcom \
                     (supports {FORMAT_VERSION}); load it with a newer build"
                )));
            }
            // Reject zero, fractional, negative, and non-numeric values.
            _ => {
                return Err(io::Error::other(format!(
                    "session format version {} is not one this fleetcom reads \
                     (supports {FORMAT_VERSION}); load it with a newer build",
                    version.dump()
                )));
            }
        }
    }
    let (name, dirs, flat) = if parsed["dirs"].is_object() {
        let name = parsed["name"].as_str().map(str::to_string);
        (name, &parsed["dirs"], false)
    } else {
        (None, &parsed, true)
    };
    let mut cfg = SessionConfig::new();
    for (dir, val) in dirs.entries() {
        // In a flat file, `version` is metadata beside the directory keys.
        if flat && dir == "version" {
            continue;
        }
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

/// Create missing directories with 0700 and restrict `dir` itself to 0700.
/// Existing parent directories remain unchanged.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `contents` to `<dir>/<file_name>` atomically: a private temp file in
/// `dir`, synced, then renamed over the target. The `.tmp` suffix keeps the
/// temp out of `list_in`, and the rename gives the target mode 0600.
fn write_atomic(dir: &Path, file_name: &str, contents: &str) -> io::Result<PathBuf> {
    let file = dir.join(file_name);
    let pid = std::process::id();
    let (mut tmp_file, tmp) = loop {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        // Shorten the target portion so the decorated temporary filename stays
        // within the 255-byte component limit.
        let suffix = format!(".{pid}.{n}.tmp");
        let stem = prefix_bytes(file_name, 254 - suffix.len());
        let candidate = dir.join(format!(".{stem}{suffix}"));
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
        tmp_file.write_all(contents.as_bytes())?;
        // Persist the contents before publishing the temp file as the target.
        tmp_file.sync_all()?;
        fs::rename(&tmp, &file)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written?;
    Ok(file)
}

pub fn save_in(dir: &Path, name: &str, cfg: &SessionConfig) -> io::Result<PathBuf> {
    ensure_private_dir(dir)?;
    let trimmed = name.trim();
    let file_name = format!("{}.json", sanitize(name));
    let file = dir.join(&file_name);

    // Refuse a stored-name mismatch because distinct names can sanitize to the
    // same filename. Files without a parseable stored name remain overwritable.
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

    write_atomic(dir, &file_name, &to_json(trimmed, cfg))
}

pub fn load_in(dir: &Path, name: &str) -> io::Result<SessionConfig> {
    let file = dir.join(format!("{}.json", sanitize(name)));
    from_json(&fs::read_to_string(file)?).map(|(_, cfg)| cfg)
}

/// Return sorted recipe names under `dir`. Wrapped files use their stored name;
/// flat files use the filename stem.
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

// --- recovery snapshots: the supervisor's automatic fleet backups, written
// under `<sessions root>/recovery` in the ordinary wrapped format. Phase 1
// only writes; listing and loading them arrive in later phases. ---------------

/// Snapshots kept per recovery directory; older ones are pruned after each
/// write.
const RECOVERY_KEEP: usize = 10;

/// Recovery-snapshot directory under a session root. Created 0700 on first
/// write; `list_in` never descends into it (directories fail its `.json`
/// extension filter), so snapshots stay out of the session picker.
pub fn recovery_dir(sessions_root: &Path) -> PathBuf {
    sessions_root.join("recovery")
}

/// UTC civil time for `t`: (year, month, day, hour, minute, second).
/// UTC because the recovery stems must sort lexically by age: local time
/// repeats an hour at DST fall-back.
fn civil_utc(t: SystemTime) -> (i64, u32, u32, u64, u64, u64) {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d) = crate::harness::civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    (y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Incarnation filename stem `<YYYYMMDD-HHMMSS>-<pid>` from the supervisor's
/// start time and process id. The pid separates a daemon from a concurrent
/// `--foreground` client sharing one config root; the leading UTC stamp makes
/// lexical order age order, which pruning relies on. Digits and dashes only,
/// so the stem needs no `sanitize` pass.
pub fn recovery_stem(start: SystemTime, pid: u32) -> String {
    let (y, m, d, hh, mm, ss) = civil_utc(start);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}-{pid}")
}

/// Human label stored in a snapshot's `name` field: `autosaved <YYYY-MM-DD
/// HH:MM>` (UTC) from the write time. The label exists so a recovery file
/// copied by hand into `sessions/` becomes an ordinary, sensibly-named
/// session with no tooling: `list_in` shows the stored name, and loading it
/// needs nothing new.
pub fn recovery_label(now: SystemTime) -> String {
    let (y, m, d, hh, mm, _) = civil_utc(now);
    format!("autosaved {y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Write one recovery snapshot through the same atomic temp-rename, 0600, and
/// version-1 path as `save_in`, then prune. `save_in`'s stored-name collision
/// check does not apply: the incarnation owns `<file_stem>.json` outright and
/// always overwrites it.
pub fn save_recovery_in(
    dir: &Path,
    file_stem: &str,
    name: &str,
    cfg: &SessionConfig,
) -> io::Result<PathBuf> {
    ensure_private_dir(dir)?;
    let file = write_atomic(dir, &format!("{file_stem}.json"), &to_json(name, cfg))?;
    prune_recovery(dir);
    Ok(file)
}

/// Best-effort prune: keep the newest [`RECOVERY_KEEP`] snapshots by filename
/// (stems are UTC timestamps, so lexical order is age order) and ignore every
/// error -- a snapshot that cannot be removed must not fail the write that
/// just succeeded.
fn prune_recovery(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut snapshots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    if snapshots.len() <= RECOVERY_KEEP {
        return;
    }
    snapshots.sort();
    for old in &snapshots[..snapshots.len() - RECOVERY_KEEP] {
        let _ = fs::remove_file(old);
    }
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

    /// String members parse as unadorned entries; flat files have no stored name.
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

    /// Entries without a group or display name use strings in the wrapper.
    #[test]
    fn group_free_config_writes_string_members_in_the_wrapper() {
        let mut cfg = SessionConfig::new();
        cfg.insert("~/proj".into(), vec![e("cargo test"), e("vim")]);
        cfg.insert("/tmp".into(), vec![e("top")]);

        let expected = "{\n  \"version\": 1,\n  \"name\": \"work\",\n  \"dirs\": {\n    \"/tmp\": [\n      \"top\"\n    ],\n    \"~/proj\": [\n      \"cargo test\",\n      \"vim\"\n    ]\n  }\n}";
        assert_eq!(to_json("work", &cfg), expected);
    }

    /// Saved files include the accepted format version.
    #[test]
    fn save_writes_version_1_and_load_accepts_it() {
        let dir = temp("session_version_roundtrip");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);

        let file = save_in(&dir, "versioned", &cfg).unwrap();
        assert!(
            fs::read_to_string(&file)
                .unwrap()
                .contains("\"version\": 1")
        );
        assert_eq!(load_in(&dir, "versioned").unwrap(), cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing version is interpreted as version 1.
    #[test]
    fn missing_version_means_version_1() {
        let (name, cfg) = from_json(r#"{"name": "old", "dirs": {"~/proj": ["vim"]}}"#).unwrap();
        assert_eq!(name, Some("old".to_string()));
        assert_eq!(cfg["~/proj"], vec![e("vim")]);
    }

    /// An explicit `"version": 1` passes the gate.
    #[test]
    fn explicit_version_1_loads() {
        let (_, cfg) =
            from_json(r#"{"version": 1, "name": "v", "dirs": {"~/proj": ["vim"]}}"#).unwrap();
        assert_eq!(cfg["~/proj"], vec![e("vim")]);
    }

    /// A newer format fails with an error naming both versions.
    #[test]
    fn newer_version_refuses_naming_both_versions() {
        let err = from_json(r#"{"version": 2, "name": "v", "dirs": {}}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "session format version 2 is newer than this fleetcom (supports 1); \
             load it with a newer build"
        );
    }

    /// Version zero uses the unsupported-version error.
    #[test]
    fn version_zero_refuses_as_unreadable_not_newer() {
        let err = from_json(r#"{"version": 0, "name": "v", "dirs": {}}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "session format version 0 is not one this fleetcom reads \
             (supports 1); load it with a newer build"
        );
    }

    /// A non-numeric version uses the unsupported-version error.
    #[test]
    fn non_numeric_version_refuses() {
        let err = from_json(r#"{"version": "2.0", "name": "v", "dirs": {}}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "session format version \"2.0\" is not one this fleetcom reads \
             (supports 1); load it with a newer build"
        );
    }

    /// `load_in` propagates unsupported-version errors.
    #[test]
    fn refused_load_yields_err_with_nothing_to_resave() {
        let dir = temp("session_version_refuse");
        fs::write(
            dir.join("future.json"),
            r#"{"version": 3, "name": "future", "dirs": {"~/p": ["vim"]}}"#,
        )
        .unwrap();

        let err = load_in(&dir, "future").unwrap_err();
        assert!(err.to_string().contains("version 3"), "{err}");
        assert!(err.to_string().contains("supports 1"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A flat schema treats `version` as metadata, not a directory.
    #[test]
    fn flat_version_member_does_not_become_a_directory() {
        let (name, cfg) = from_json(r#"{"version": 1, "~/proj": ["vim"]}"#).unwrap();
        assert_eq!(name, None);
        assert!(!cfg.contains_key("version"));
        assert_eq!(cfg["~/proj"], vec![e("vim")]);
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

    /// The stem cap keeps 250 ASCII bytes and drops the remainder.
    #[test]
    fn caps_names_at_250_bytes() {
        assert_eq!(sanitize(&"a".repeat(250)), "a".repeat(250));
        let capped = sanitize(&"a".repeat(251));
        assert_eq!(capped, "a".repeat(250));
        assert_eq!(format!("{capped}.json").len(), 255);
    }

    /// The stem cap never splits a multibyte character.
    #[test]
    fn cap_drops_a_multibyte_char_whole() {
        // 249 bytes used; the 2-byte 'é' would reach 251.
        let capped = sanitize(&format!("{}é", "a".repeat(249)));
        assert_eq!(capped, "a".repeat(249));
    }

    /// Long names save and load within the filename component limit.
    #[test]
    fn long_names_save_within_name_max() {
        let dir = temp("session_long_name");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        let name = "n".repeat(255);

        let file = save_in(&dir, &name, &cfg).unwrap();
        assert_eq!(file.file_name().unwrap().len(), 255);
        assert_eq!(load_in(&dir, &name).unwrap(), cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Recipe files are owner-only, including after replacing a 0644 file.
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

    /// Session directories are created private and existing permissive session
    /// directories are restricted on save.
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

    /// Saves reject a different name that sanitizes to an occupied filename.
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

    /// Flat-schema files load and list by filename stem.
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

    /// A flat-schema file can be replaced under its filename stem.
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

    /// Wrapped files list by stored name; flat files list by filename stem.
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

    /// 2026-07-14 09:30:15 UTC, matching `civil_from_days`'s test vector.
    fn recovery_instant() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_784_021_415)
    }

    /// The stem is `<YYYYMMDD-HHMMSS>-<pid>`; the label is the write minute.
    #[test]
    fn recovery_stem_and_label_render_utc() {
        assert_eq!(
            recovery_stem(recovery_instant(), 4242),
            "20260714-093015-4242"
        );
        assert_eq!(
            recovery_label(recovery_instant()),
            "autosaved 2026-07-14 09:30"
        );
    }

    /// A snapshot round-trips through `load_in` with version 1, the human
    /// label, and the private-permission idiom of ordinary saves.
    #[test]
    fn recovery_snapshot_round_trips_with_version_and_label() {
        let base = temp("session_recovery_roundtrip");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![gne("cargo run", "api", "server")]);
        let stem = recovery_stem(recovery_instant(), 4242);
        let label = recovery_label(recovery_instant());

        let file = save_recovery_in(&rec, &stem, &label, &cfg).unwrap();
        assert_eq!(file, rec.join("20260714-093015-4242.json"));
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        let (stored, parsed) = from_json(&text).unwrap();
        assert_eq!(stored.as_deref(), Some("autosaved 2026-07-14 09:30"));
        assert_eq!(parsed, cfg);
        assert_eq!(load_in(&rec, &stem).unwrap(), cfg);

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&rec), 0o700);
        assert_eq!(mode(&file), 0o600);
        let _ = fs::remove_dir_all(&base);
    }

    /// Prune keeps exactly the newest [`RECOVERY_KEEP`] snapshots by filename.
    #[test]
    fn recovery_prune_keeps_the_newest_ten() {
        let base = temp("session_recovery_prune");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        for i in 1..=12u32 {
            let stem = format!("20260714-0930{i:02}-77");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        let mut names: Vec<String> = fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        let expected: Vec<String> = (3..=12u32)
            .map(|i| format!("20260714-0930{i:02}-77.json"))
            .collect();
        assert_eq!(names, expected, "prune must drop exactly the oldest two");
        let _ = fs::remove_dir_all(&base);
    }

    /// The `recovery/` subdirectory never appears in a session listing:
    /// directories fail `list_in`'s `.json` extension filter.
    #[test]
    fn list_ignores_the_recovery_subdirectory() {
        let dir = temp("session_list_recovery");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        save_in(&dir, "real", &cfg).unwrap();
        save_recovery_in(
            &recovery_dir(&dir),
            &recovery_stem(recovery_instant(), 7),
            &recovery_label(recovery_instant()),
            &cfg,
        )
        .unwrap();

        assert_eq!(list_in(&dir), vec!["real".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
