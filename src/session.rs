//! JSON session recipes stored one file per name in the user's config directory. On
//! recipe load, start new commands without restoring live processes.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use crate::{
    protocol::{RecoveryEntry, insert_opt_str, opt_str},
    task::{pid_is_dead, positive_pid},
};

/// One recipe entry. Serialize entries without a group or name as strings; use objects
/// for other entries, writing optional fields only when set.
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

/// Session-recipe directory: `<config root>/sessions`. A caller-supplied `root` is
/// preferred (the supervisor passes the connecting client's [`FLEETCOM_CONFIG_DIR`]);
/// otherwise the same var from this process's env, else `dirs::config_dir()/fleetcom`.
pub fn sessions_dir(root: Option<PathBuf>) -> Option<PathBuf> {
    root.or_else(|| std::env::var(FLEETCOM_CONFIG_DIR).ok().map(PathBuf::from))
        .or_else(|| dirs::config_dir().map(|c| c.join("fleetcom")))
        .map(|base| base.join("sessions"))
}

/// Session format version written by `to_json` and accepted by `from_json`.
/// Missing versions are interpreted as version 1; unsupported versions fail.
const FORMAT_VERSION: u64 = 1;

/// Build the recipe's `dirs` object.
fn dirs_json(cfg: &SessionConfig) -> jzon::JsonValue {
    let mut dirs = jzon::JsonValue::new_object();
    for (dir, entries) in cfg {
        let mut arr = jzon::JsonValue::new_array();
        for e in entries {
            let member = if e.group.is_none() && e.name.is_none() {
                // Use the string form for entries without optional labels.
                jzon::JsonValue::from(e.cmd.as_str())
            } else {
                let mut m = jzon::object! { "cmd": e.cmd.as_str() };
                insert_opt_str(&mut m, "group", &e.group);
                insert_opt_str(&mut m, "name", &e.name);
                m
            };
            let _ = arr.push(member);
        }
        let _ = dirs.insert(dir, arr);
    }
    dirs
}

/// Serialize the versioned wrapped schema. Store the name to distinguish names
/// sanitized to the same filename.
fn to_json(name: &str, cfg: &SessionConfig) -> String {
    jzon::object! {
        "version": FORMAT_VERSION,
        "name": name,
        "dirs": dirs_json(cfg),
    }
    .pretty(2)
}

/// Serialize the recipe body for content-based change detection.
pub fn fingerprint_json(cfg: &SessionConfig) -> String {
    dirs_json(cfg).dump()
}

/// Parse wrapped and flat schemas, returning the stored name when present.
/// A wrapped file has an object-valued `dirs`; flat files have entry arrays at
/// the top level, including when a directory is literally named `dirs`.
/// A top-level `version` must be an integer from 1 through [`FORMAT_VERSION`];
/// a missing version is interpreted as 1.
fn from_json(text: &str) -> io::Result<(Option<String>, SessionConfig)> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let parsed = jzon::parse(text).map_err(|e| invalid(e.to_string()))?;
    if !parsed.is_object() {
        return Err(invalid("session root: expected an object".into()));
    }
    // Validate version metadata before detecting the schema shape.
    let version = &parsed["version"];
    if parsed.has_key("version") {
        match version.as_u64() {
            Some(n) if (1..=FORMAT_VERSION).contains(&n) => {}
            Some(n) if n > FORMAT_VERSION => {
                return Err(invalid(format!(
                    "session format version {n} is newer than this fleetcom \
                     (supports {FORMAT_VERSION}); load it with a newer build"
                )));
            }
            // Reject zero, fractional, negative, and non-numeric values.
            _ => {
                return Err(invalid(format!(
                    "session format version {} is not one this fleetcom reads \
                     (supports {FORMAT_VERSION}); load it with a newer build",
                    version.dump()
                )));
            }
        }
    }
    let (name, dirs, flat) = if parsed["dirs"].is_object() {
        let name = opt_str(&parsed["name"])
            .ok_or_else(|| invalid("session field \"name\": expected a string or null".into()))?;
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
        if !val.is_array() {
            return Err(invalid(format!("directory {dir:?}: expected an array")));
        }
        let mut entries = Vec::new();
        for (index, member) in val.members().enumerate() {
            if let Some(cmd) = member.as_str() {
                entries.push(SessionEntry {
                    cmd: cmd.to_string(),
                    group: None,
                    name: None,
                });
                continue;
            }
            let location = format!("directory {dir:?}, entry {}", index + 1);
            if !member.is_object() {
                return Err(invalid(format!(
                    "{location}: expected a command string or an object"
                )));
            }
            let cmd = member["cmd"]
                .as_str()
                .ok_or_else(|| invalid(format!("{location}, field \"cmd\": expected a string")))?;
            let label = |field| {
                opt_str(&member[field]).ok_or_else(|| {
                    invalid(format!(
                        "{location}, field {field:?}: expected a string or null"
                    ))
                })
            };
            entries.push(SessionEntry {
                cmd: cmd.to_string(),
                group: label("group")?,
                name: label("name")?,
            });
        }
        cfg.insert(dir.to_string(), entries);
    }
    Ok((name, cfg))
}

/// Inspect wrapper identity without validating its version or command body: retain the
/// stored name for listings and collision checks even for an unloadable recipe.
fn stored_name(text: &str) -> Option<String> {
    let parsed = jzon::parse(text).ok()?;
    if parsed["dirs"].is_object() {
        parsed["name"].as_str().map(str::to_string)
    } else {
        None
    }
}

// --- fs surface: callers supply the root. The supervisor resolves it from the
// connection's launch context; `sessions_dir` above is only its process-env
// fallback. Tests point it at scratch dirs the same way. -----------------------

/// Distinguishes concurrent savers' temp files within one process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Create missing directories with mode 0700 and remove group and other
/// permissions from `dir`. Existing parent permissions remain unchanged.
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

/// Write `contents` to `<dir>/<file_name>` atomically: a private temp file in `dir`,
/// synced, then renamed over the target. Exclude the temp from `list_in` with a `.tmp`
/// suffix; preserve mode 0600 on rename to the target.
fn write_atomic(dir: &Path, file_name: &str, contents: &str) -> io::Result<PathBuf> {
    let file = dir.join(file_name);
    let pid = std::process::id();
    let (mut tmp_file, tmp) = loop {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        // Shorten the target portion so the decorated temporary filename stays
        // within the 255-byte component limit.
        let suffix = format!(".{pid}.{n}.tmp");
        let stem = crate::format::prefix_bytes(file_name, 254 - suffix.len());
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
            if let Some(stored) = stored_name(&text)
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

/// Return recipe names under `dir`, collated case-insensitively. Use the stored name
/// for wrapped files and the filename stem for flat files.
pub fn list_in(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                let stored = fs::read_to_string(&p).ok().and_then(|t| stored_name(&t));
                names.push(stored.unwrap_or_else(|| stem.to_string()));
            }
        }
    }
    names.sort_by_cached_key(|n| crate::format::collation_key(n));
    names
}

// --- automatic recovery snapshots -------------------------------------------

/// Target snapshot count when no other live writers share the directory.
const RECOVERY_KEEP: usize = 10;

/// Recovery-snapshot directory under a session root.
pub fn recovery_dir(sessions_root: &Path) -> PathBuf {
    sessions_root.join("recovery")
}

/// UTC civil time as `(year, month, day, hour, minute, second)`.
fn civil_utc(t: SystemTime) -> (i64, u32, u32, u64, u64, u64) {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d) = crate::format::civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    (y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Build a `<YYYYMMDD-HHMMSS>-<pid>` recovery filename stem in UTC.
pub fn recovery_stem(start: SystemTime, pid: u32) -> String {
    let (y, m, d, hh, mm, ss) = civil_utc(start);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}-{pid}")
}

/// Build the snapshot label `autosaved <YYYY-MM-DD HH:MM>` in UTC.
pub fn recovery_label(now: SystemTime) -> String {
    let (y, m, d, hh, mm, _) = civil_utc(now);
    format!("autosaved {y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Atomically replace a mode-0600 recovery snapshot, then prune old snapshots.
/// The written stem and stems naming live process IDs are exempt from pruning.
pub fn save_recovery_in(
    dir: &Path,
    file_stem: &str,
    name: &str,
    cfg: &SessionConfig,
) -> io::Result<PathBuf> {
    ensure_private_dir(dir)?;
    let file = write_atomic(dir, &format!("{file_stem}.json"), &to_json(name, cfg))?;
    prune_recovery(dir, file_stem);
    Ok(file)
}

/// List readable recovery snapshots in descending stem order. Invalid files are
/// skipped, and filename stems are used as labels for files without stored names.
pub fn list_recovery_in(dir: &Path) -> Vec<RecoveryEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok((stored, cfg)) = fs::read_to_string(&p).and_then(|t| from_json(&t)) else {
                continue;
            };
            // Saturate task counts; use age zero for unavailable or future mtimes.
            let tasks =
                u32::try_from(cfg.values().map(Vec::len).sum::<usize>()).unwrap_or(u32::MAX);
            let age_secs = fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                .map_or(0, |d| d.as_secs());
            out.push(RecoveryEntry {
                stem: stem.to_string(),
                label: stored.unwrap_or_else(|| stem.to_string()),
                tasks,
                age_secs,
            });
        }
    }
    out.sort_by(|a, b| b.stem.cmp(&a.stem));
    out
}

/// Accept a nonempty stem without path separators, extensions, or dot-files.
fn valid_recovery_stem(stem: &str) -> bool {
    !stem.is_empty() && !stem.contains(['/', '\\', '.'])
}

/// Load a recovery snapshot by exact filename stem after path validation.
pub fn load_recovery_in(dir: &Path, stem: &str) -> io::Result<SessionConfig> {
    if !valid_recovery_stem(stem) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid recovery stem {stem:?}"),
        ));
    }
    from_json(&fs::read_to_string(dir.join(format!("{stem}.json")))?).map(|(_, cfg)| cfg)
}

/// Parse a recovery stem's trailing positive `i32` process ID.
fn stem_pid(stem: &str) -> Option<i32> {
    let (_, pid) = stem.rsplit_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    positive_pid(pid)
}

/// Return whether a valid PID suffix is not known to be dead. Only `ESRCH`
/// proves death; invalid suffixes receive no liveness protection.
fn stem_names_live_writer(stem: &str) -> bool {
    stem_pid(stem).is_some_and(|pid| !pid_is_dead(pid))
}

/// Best-effort pruning that protects `keep_stem` and snapshots whose PID is
/// not known to be dead. Of the remaining JSON files, retain the lexically
/// greatest [`RECOVERY_KEEP`] minus one. Filesystem errors are ignored.
fn prune_recovery(dir: &Path, keep_stem: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let keep_name = format!("{keep_stem}.json");
    let mut snapshots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some(keep_name.as_str()))
        .filter(|p| {
            !p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(stem_names_live_writer)
        })
        .collect();
    if snapshots.len() < RECOVERY_KEEP {
        return;
    }
    snapshots.sort();
    for old in &snapshots[..snapshots.len() - (RECOVERY_KEEP - 1)] {
        let _ = fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{dead_pid, temp};

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
    }

    /// Saved session names use case-insensitive collation.
    #[test]
    fn list_in_collates_case_insensitively() {
        let dir = temp("session_list_collate");
        let mut cfg = SessionConfig::new();
        cfg.insert("/tmp".into(), vec![e("top")]);
        for name in ["Zed", "apple", "Beta"] {
            save_in(&dir, name, &cfg).unwrap();
        }
        assert_eq!(
            list_in(&dir),
            vec!["apple".to_string(), "Beta".to_string(), "Zed".to_string()]
        );
    }

    /// Preserve mixed string and object entries through one serialization round trip.
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
    }

    /// Preserve every group/name combination through serialization.
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
    }

    /// A flat schema treats `version` as metadata, not a directory.
    #[test]
    fn flat_version_member_does_not_become_a_directory() {
        let (name, cfg) = from_json(r#"{"version": 1, "~/proj": ["vim"]}"#).unwrap();
        assert_eq!(name, None);
        assert!(!cfg.contains_key("version"));
        assert_eq!(cfg["~/proj"], vec![e("vim")]);
    }

    #[test]
    fn rejects_invalid_roots_and_syntax() {
        for text in ["null", "true", "42", r#""text""#, "[]"] {
            let err = from_json(text).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{text}");
            assert_eq!(err.to_string(), "session root: expected an object");
        }
        assert_eq!(
            from_json("{not json").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_non_array_directories_with_escaped_keys() {
        let dir = "d\"\\\n";
        for value in ["null", "true", "42", r#""command""#, "{}"] {
            let mut body = jzon::JsonValue::new_object();
            body.insert(dir, jzon::parse(value).unwrap()).unwrap();
            for recipe in [body.clone(), jzon::object! { "dirs": body }] {
                let err = from_json(&recipe.dump()).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{recipe}");
                assert_eq!(
                    err.to_string(),
                    format!("directory {dir:?}: expected an array")
                );
            }
        }
        for value in ["null", "true", "42", r#""command""#] {
            let err = from_json(&format!(r#"{{"dirs": {value}}}"#)).unwrap_err();
            assert_eq!(err.to_string(), "directory \"dirs\": expected an array");
        }
    }

    /// Reject the whole recipe on one invalid entry, including preceding valid
    /// commands.
    #[test]
    fn rejects_malformed_entries_with_directory_position_and_field() {
        let mut cases = Vec::new();
        for value in ["null", "true", "42", "[]"] {
            cases.push((
                value.to_string(),
                "expected a command string or an object".to_string(),
            ));
        }
        cases.push(("{}".into(), "field \"cmd\": expected a string".into()));
        for field in ["cmd", "group", "name"] {
            for value in ["null", "true", "42", "[]", "{}"] {
                if field != "cmd" && value == "null" {
                    continue;
                }
                let mut entry = jzon::object! { "cmd": "secret-command" };
                entry[field] = jzon::parse(value).unwrap();
                let expected = if field == "cmd" {
                    "a string"
                } else {
                    "a string or null"
                };
                cases.push((
                    entry.dump(),
                    format!("field {field:?}: expected {expected}"),
                ));
            }
        }
        for (entry, expected) in cases {
            let body = format!(r#"{{"d": ["valid-command", {entry}]}}"#);
            for text in [body.clone(), format!(r#"{{"dirs": {body}}}"#)] {
                let err = from_json(&text).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{text}");
                let separator = if expected.starts_with("field") {
                    ", "
                } else {
                    ": "
                };
                assert_eq!(
                    err.to_string(),
                    format!("directory \"d\", entry 2{separator}{expected}")
                );
            }
        }
    }

    #[test]
    fn rejects_invalid_wrapper_names_and_explicit_null_versions() {
        for value in ["true", "42", "[]", "{}"] {
            let err = from_json(&format!(r#"{{"name": {value}, "dirs": {{}}}}"#)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                err.to_string(),
                "session field \"name\": expected a string or null"
            );
        }
        for value in ["null", "-1", "1.5", "true", "[]", "{}"] {
            for body in [r#""dirs": {}"#, r#""d": []"#] {
                let err = from_json(&format!(r#"{{"version": {value}, {body}}}"#)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                assert!(
                    err.to_string().contains(&format!("version {value}")),
                    "{err}"
                );
                assert!(err.to_string().contains("supports 1"), "{err}");
            }
        }
    }

    #[test]
    fn accepts_empty_recipes_and_dirs_named_directory() {
        for text in [
            "{}",
            r#"{"version": 1}"#,
            r#"{"dirs": {}}"#,
            r#"{"name": null, "dirs": {}}"#,
        ] {
            assert_eq!(from_json(text).unwrap(), (None, SessionConfig::new()));
        }
        let (_, cfg) = from_json(r#"{"dirs": [], "name": [""], "": []}"#).unwrap();
        assert_eq!(
            cfg,
            SessionConfig::from([
                ("dirs".into(), vec![]),
                ("name".into(), vec![e("")]),
                ("".into(), vec![]),
            ])
        );
    }

    #[test]
    fn preserves_authored_strings_nullable_labels_and_unknown_fields() {
        let body = r#"{"d": ["", {"cmd": "bare"}, {"cmd": "n", "group": null},
            {"cmd": "m", "name": null}, {"cmd": "", "group": "", "name": ""},
            {"cmd": "  echo x\n", "group": "  api  ", "name": "\tweb\t", "extra": false}]}"#;
        for text in [
            body.to_string(),
            format!(r#"{{"name": "", "dirs": {body}, "extra": false}}"#),
        ] {
            let (_, cfg) = from_json(&text).unwrap();
            assert_eq!(
                cfg["d"],
                vec![
                    e(""),
                    e("bare"),
                    e("n"),
                    e("m"),
                    gne("", "", ""),
                    gne("  echo x\n", "  api  ", "\tweb\t"),
                ]
            );
        }
        assert_eq!(
            from_json(r#"{"name": "", "dirs": {}}"#).unwrap().0,
            Some("".into())
        );
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("my/session"), "my_session");
        assert_eq!(sanitize("  a.b  "), "a_b");
    }

    /// Keep at most 250 ASCII bytes in the stem.
    #[test]
    fn caps_names_at_250_bytes() {
        assert_eq!(sanitize(&"a".repeat(250)), "a".repeat(250));
        let capped = sanitize(&"a".repeat(251));
        assert_eq!(capped, "a".repeat(250));
        assert_eq!(format!("{capped}.json").len(), 255);
    }

    /// Never split a multibyte character at the stem cap.
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
    }

    /// The temp is renamed away on success; only the recipe remains.
    #[test]
    fn save_leaves_no_temp_file() {
        let dir = temp("session_notemp");
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);

        save_in(&dir, "clean", &cfg).unwrap();
        let names: Vec<String> = fs::read_dir(&*dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["clean.json".to_string()]);
    }

    /// Reject a save under a different name sanitized to an occupied filename.
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
    }

    /// Retain stored identity for invalid bodies and future versions.
    #[test]
    fn unloadable_wrappers_keep_picker_names_and_collision_protection() {
        let dir = temp("session_invalid_identity");
        let file = dir.join("a_b.json");
        for text in [
            r#"{"name": "a/b", "dirs": {"d": ["valid", {"cmd": false}]}}"#,
            r#"{"name": "a/b", "dirs": {"d": null}}"#,
            r#"{"version": 2, "name": "a/b", "dirs": {}}"#,
        ] {
            fs::write(&file, text).unwrap();
            assert!(load_in(&dir, "a/b").is_err());
            assert_eq!(list_in(&dir), ["a/b"]);
            let err = save_in(&dir, "a.b", &SessionConfig::new()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(&file).unwrap(), text.as_bytes());
            save_in(&dir, "a/b", &SessionConfig::new()).unwrap();
            assert!(load_in(&dir, "a/b").unwrap().is_empty());
        }
    }

    #[test]
    fn corrupt_and_nameless_recipes_keep_filename_fallback_and_allow_overwrite() {
        let dir = temp("session_nameless_identity");
        for text in [
            "{not json",
            "null",
            r#"{"dirs": {}}"#,
            r#"{"name": null, "dirs": {}}"#,
            r#"{"name": false, "dirs": {}}"#,
            r#"{"name": ["cmd"], "dirs": []}"#,
        ] {
            fs::write(dir.join("mine.json"), text).unwrap();
            assert_eq!(list_in(&dir), ["mine"]);
            save_in(&dir, "mine", &SessionConfig::new()).unwrap();
            assert!(load_in(&dir, "mine").unwrap().is_empty());
        }
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
    }

    /// Fixed instant at 2026-07-14 09:30:15 UTC.
    fn recovery_instant() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_784_021_415)
    }

    /// Out-of-range PID used for dead-writer fixtures.
    const DEAD_FIXTURE_PID: u32 = 9_999_999;

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

    /// Snapshots preserve their schema, label, contents, and permissions.
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
    }

    /// Retain the lexically greatest [`RECOVERY_KEEP`] filenames on prune.
    #[test]
    fn recovery_prune_keeps_the_newest_ten() {
        let base = temp("session_recovery_prune");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        for i in 1..=12u32 {
            let stem = format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        let mut names: Vec<String> = fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        let expected: Vec<String> = (3..=12u32)
            .map(|i| format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}.json"))
            .collect();
        assert_eq!(names, expected, "prune must drop exactly the oldest two");
    }

    /// Retain the just-written stem on prune even when it is oldest.
    #[test]
    fn recovery_prune_exempts_the_active_stem() {
        let base = temp("session_recovery_prune_active");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        for i in 1..=10u32 {
            let stem = format!("20260715-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-15 09:30", &cfg).unwrap();
        }

        // This dead-PID stem sorts below every existing snapshot.
        let active_stem = format!("20260714-093000-{DEAD_FIXTURE_PID}");
        let active =
            save_recovery_in(&rec, &active_stem, "autosaved 2026-07-15 09:30", &cfg).unwrap();
        assert!(active.exists(), "the just-written snapshot must survive");

        let mut names: Vec<String> = fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut expected = vec![format!("{active_stem}.json")];
        expected
            .extend((2..=10u32).map(|i| format!("20260715-0930{i:02}-{DEAD_FIXTURE_PID}.json")));
        assert_eq!(
            names, expected,
            "the active file plus the nine newest others must remain"
        );
    }

    /// Remove nothing when pruning fewer than [`RECOVERY_KEEP`] files.
    #[test]
    fn recovery_prune_below_limit_removes_nothing() {
        let base = temp("session_recovery_prune_few");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        for i in 1..=5u32 {
            let stem = format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        assert_eq!(
            fs::read_dir(&rec).unwrap().flatten().count(),
            5,
            "no file may be pruned below the retention limit"
        );
    }

    /// Retain an older snapshot on prune when its PID is still live.
    #[test]
    fn recovery_prune_exempts_live_pid_stems() {
        let base = temp("session_recovery_prune_live");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        // The oldest candidate names this live test process.
        let live_stem = format!("20260101-000000-{}", std::process::id());
        save_recovery_in(&rec, &live_stem, "autosaved 2026-01-01 00:00", &cfg).unwrap();
        for i in 1..=11u32 {
            let stem = format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        let mut names: Vec<String> = fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut expected = vec![format!("{live_stem}.json")];
        expected
            .extend((2..=11u32).map(|i| format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}.json")));
        assert_eq!(
            names, expected,
            "the live writer's file must survive; the oldest dead file must not"
        );
    }

    /// A snapshot from an exited process is eligible for pruning.
    #[test]
    fn recovery_prune_removes_dead_pid_stems() {
        let base = temp("session_recovery_prune_dead");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        let oldest = format!("20260101-000000-{}", dead_pid());
        save_recovery_in(&rec, &oldest, "autosaved 2026-01-01 00:00", &cfg).unwrap();
        for i in 1..=10u32 {
            let stem = format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        assert!(
            !rec.join(format!("{oldest}.json")).exists(),
            "a dead writer's snapshot is an ordinary prune candidate"
        );
        assert_eq!(
            fs::read_dir(&rec).unwrap().flatten().count(),
            RECOVERY_KEEP,
            "dead-stem retention must converge to the bound"
        );
    }

    /// Invalid PID suffixes receive no liveness protection.
    #[test]
    fn recovery_prune_ignores_malformed_pid_suffixes() {
        let base = temp("session_recovery_prune_malformed");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![e("vim")]);
        let malformed = [
            "20260101-000000-x42",         // non-numeric pid
            "20260101-000001-99999999999", // past i32::MAX
            "20260101-000002-",            // empty pid
            "20260101-000003-0",           // zero is not a positive pid
        ];
        for stem in malformed {
            save_recovery_in(&rec, stem, "autosaved 2026-01-01 00:00", &cfg).unwrap();
        }
        for i in 1..=10u32 {
            let stem = format!("20260714-0930{i:02}-{DEAD_FIXTURE_PID}");
            save_recovery_in(&rec, &stem, "autosaved 2026-07-14 09:30", &cfg).unwrap();
        }

        for stem in malformed {
            assert!(
                !rec.join(format!("{stem}.json")).exists(),
                "malformed stem {stem:?} must be pruned like any candidate"
            );
        }
        assert_eq!(
            fs::read_dir(&rec).unwrap().flatten().count(),
            RECOVERY_KEEP,
            "only the well-formed newest files may remain"
        );
    }

    /// The recovery directory is excluded from named-session listings.
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
    }

    /// List by descending stem; skip corrupt files.
    #[test]
    fn recovery_listing_is_newest_first_and_skips_corrupt_files() {
        let base = temp("session_recovery_list");
        let rec = recovery_dir(&base);
        assert!(
            list_recovery_in(&rec).is_empty(),
            "a missing recovery dir must list empty"
        );

        let mut one = SessionConfig::new();
        one.insert("~/a".into(), vec![e("vim")]);
        let mut three = SessionConfig::new();
        three.insert("~/a".into(), vec![e("vim"), e("top")]);
        three.insert("~/b".into(), vec![e("make")]);
        save_recovery_in(
            &rec,
            "20260714-093015-11",
            "autosaved 2026-07-14 09:30",
            &one,
        )
        .unwrap();
        save_recovery_in(
            &rec,
            "20260715-070000-22",
            "autosaved 2026-07-15 07:00",
            &three,
        )
        .unwrap();
        fs::write(rec.join("20260716-000000-33.json"), "{not json").unwrap();

        let entries = list_recovery_in(&rec);
        assert_eq!(
            entries.len(),
            2,
            "the corrupt snapshot must drop alone: {entries:?}"
        );
        assert_eq!(entries[0].stem, "20260715-070000-22");
        assert_eq!(entries[0].label, "autosaved 2026-07-15 07:00");
        assert_eq!(entries[0].tasks, 3);
        assert_eq!(entries[1].stem, "20260714-093015-11");
        assert_eq!(entries[1].label, "autosaved 2026-07-14 09:30");
        assert_eq!(entries[1].tasks, 1);
        assert!(
            entries.iter().all(|en| en.age_secs < 3600),
            "just-written files must read near-zero ages: {entries:?}"
        );
    }

    #[test]
    fn recovery_listing_skips_invalid_shapes_and_retains_empty_recipes() {
        let rec = temp("session_recovery_shapes");
        for (index, text) in [
            "null",
            "[]",
            r#"{"d": null}"#,
            r#"{"d": ["valid", 42]}"#,
            r#"{"dirs": {"d": [{"cmd": "x", "name": false}]}}"#,
            r#"{"version": null, "dirs": {}}"#,
        ]
        .iter()
        .enumerate()
        {
            fs::write(rec.join(format!("invalid-{index}.json")), text).unwrap();
        }
        fs::write(rec.join("empty-flat.json"), "{}").unwrap();
        fs::write(
            rec.join("empty-wrapped.json"),
            r#"{"name": "empty", "dirs": {"d": []}}"#,
        )
        .unwrap();
        let entries = list_recovery_in(&rec);
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.stem.as_str(), e.label.as_str(), e.tasks))
            .collect();
        assert_eq!(
            summary,
            [
                ("empty-wrapped", "empty", 0),
                ("empty-flat", "empty-flat", 0)
            ]
        );
    }

    /// Reject empty, dotted, or path-shaped stems on recovery load.
    #[test]
    fn load_recovery_in_loads_by_stem_and_rejects_traversal() {
        let base = temp("session_recovery_load");
        let rec = recovery_dir(&base);
        let mut cfg = SessionConfig::new();
        cfg.insert("~/p".into(), vec![gne("cargo run", "api", "server")]);
        save_recovery_in(
            &rec,
            "20260714-093015-11",
            "autosaved 2026-07-14 09:30",
            &cfg,
        )
        .unwrap();

        assert_eq!(load_recovery_in(&rec, "20260714-093015-11").unwrap(), cfg);
        for bad in ["../x", "a/b", "a.b", "a\\b", ""] {
            let err = load_recovery_in(&rec, bad).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidInput,
                "stem {bad:?} must be refused, got {err}"
            );
        }
        assert_eq!(
            load_recovery_in(&rec, "20990101-000000-1")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
