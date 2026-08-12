//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID, and completed tasks expose either `grok -r <uuid>` or
//! `grok --resume <uuid>` in terminal output. The filesystem fallback
//! correlates `<grok-home>/sessions/<group>/<uuid>/` directories: the group is
//! a percent-encoding of the working directory, or a long-name slug whose
//! `.cwd` file names that path.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::{
    CapturePaths, Harness, Invocation, SpawnPlan, is_uuid, last_hint, pin_plan, within_window,
};

pub struct Grok;

impl Harness for Grok {
    fn home_env_var(&self) -> &'static str {
        "GROK_HOME"
    }

    fn home_dot_dir(&self) -> &'static str {
        ".grok"
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("grok", "--resume")
    }

    fn instrument(
        &self,
        inv: &Invocation,
        // Grok instrumentation does not use capture paths or home config.
        _capture: &CapturePaths,
        _home: Option<&Path>,
    ) -> SpawnPlan {
        pin_plan(inv)
    }

    /// Grok has no injected live capture channel.
    fn parse_capture(&self, _payload: &str) -> Option<String> {
        None
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid short or long resume hint names the conversation.
        last_hint(text, &["grok -r ", "grok --resume "])
    }

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let group = unique_group(&self.home_root(home)?.join("sessions"), cwd)?;
        unique_session(&group, spawned)
    }
}

/// Byte-wise URL-encode of a working directory as Grok's session group name.
/// RFC 3986 unreserved bytes stay literal; every other byte becomes uppercase
/// `%XX`. Non-UTF-8 paths have no key. The path is encoded as given.
pub(crate) fn encode_cwd(cwd: &Path) -> Option<String> {
    let s = cwd.to_str()?;
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    Some(out)
}

/// The one sessions subdirectory for `cwd`. Several distinct matches cannot
/// be told apart: a wrong group is worse than none.
fn unique_group(sessions: &Path, cwd: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();

    if let Some(p) = encoded_dir(sessions, cwd) {
        push_unique(&mut found, p);
    }
    let canon = cwd.canonicalize().ok();
    if let Some(ref canon) = canon
        && canon.as_path() != cwd
        && let Some(p) = encoded_dir(sessions, canon)
    {
        push_unique(&mut found, p);
    }

    let given = cwd.to_str();
    let canon_s = canon.as_ref().and_then(|p| p.to_str());
    if let Ok(entries) = fs::read_dir(sessions) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(entry.path().join(".cwd")) else {
                continue;
            };
            let trimmed = text.trim();
            if given == Some(trimmed) || canon_s == Some(trimmed) {
                push_unique(&mut found, entry.path());
            }
        }
    }

    match found.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

fn push_unique(found: &mut Vec<PathBuf>, p: PathBuf) {
    if !found.contains(&p) {
        found.push(p);
    }
}

fn encoded_dir(sessions: &Path, cwd: &Path) -> Option<PathBuf> {
    let p = sessions.join(encode_cwd(cwd)?);
    p.is_dir().then_some(p)
}

/// The one in-window top-level session directory under `group`. Subagent
/// siblings do not count. A unique non-uuid name still yields `None`.
fn unique_session(group: &Path, spawned: SystemTime) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    for entry in fs::read_dir(group).ok()?.flatten() {
        // One directory per session, named by its uuid. Files such as
        // the `prompt_history.jsonl` sibling are not sessions.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let summary = fs::read_to_string(entry.path().join("summary.json"))
            .ok()
            .and_then(|text| jzon::parse(&text).ok());
        if summary
            .as_ref()
            .is_some_and(|v| v["session_kind"].as_str() == Some("subagent"))
        {
            continue;
        }
        let ts = summary
            .as_ref()
            .and_then(|v| v["created_at"].as_str())
            .and_then(parse_created_at)
            .or_else(|| entry.metadata().ok()?.created().ok());
        let Some(ts) = ts else {
            continue;
        };
        if !within_window(ts, spawned) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        candidates.push(name.to_string());
    }
    match candidates.as_slice() {
        [only] if is_uuid(only) => Some(only.clone()),
        _ => None,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac]Z` as grok writes `created_at`. Any other shape
/// fails so the caller can fall back to directory birth time.
fn parse_created_at(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (head, frac) = match s.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (s, None),
    };
    let b = head.as_bytes();
    if b.len() != 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = parse_digits(&head[..4])?;
    let month = u32::try_from(parse_digits(&head[5..7])?).ok()?;
    let day = u32::try_from(parse_digits(&head[8..10])?).ok()?;
    let hour = u32::try_from(parse_digits(&head[11..13])?).ok()?;
    let minute = u32::try_from(parse_digits(&head[14..16])?).ok()?;
    let second = u32::try_from(parse_digits(&head[17..19])?).ok()?;
    let nanos = match frac {
        None => 0,
        Some(f) if !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()) => frac_nanos(f)?,
        _ => return None,
    };
    if !valid_ymd(year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let day_secs = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    let secs = u64::try_from(days.checked_mul(86_400)?.checked_add(day_secs)?).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

fn parse_digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn frac_nanos(frac: &str) -> Option<u32> {
    let take = frac.len().min(9);
    let mut n: u32 = frac[..take].parse().ok()?;
    for _ in take..9 {
        n = n.checked_mul(10)?;
    }
    Some(n)
}

fn valid_ymd(year: i64, month: u32, day: u32) -> bool {
    let mdays = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=mdays).contains(&day)
}

/// Inverse of [`crate::format::civil_from_days`]: days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 {
        i64::from(month) - 3
    } else {
        i64::from(month) + 9
    };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        harness::{
            fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths},
            is_uuid,
        },
        testutil::temp,
    };

    /// Grok-specific opaque shapes: flags, the `-r`/`-s`/`=` spellings the
    /// tool prints but detection refuses, and subcommands. The syntax shared
    /// by every harness is covered by the table test in `harness::tests`.
    #[test]
    fn everything_else_is_opaque_and_never_rewritten() {
        let opaque: Vec<String> = [
            "grok --model grok-4",
            "grok --continue",
            "grok -r",
            "grok -r my-session",
            "grok sessions list",
            "grokk",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("grok -r {ID}"),
            format!("grok -s {ID}"),
            format!("grok --resume {ID} --debug"),
        ])
        .collect();
        assert_all_opaque(&Grok, ID, &opaque);
    }

    /// A bare launch gains only the pinned ID because Grok exposes no live
    /// capture channel.
    #[test]
    fn instrument_pins_an_id_and_nothing_else() {
        let inv = Grok.detect("grok").unwrap();
        let plan = Grok.instrument(&inv, &paths(), None);
        let id = plan.injected_id.expect("a bare launch pins an id");
        assert!(is_uuid(&id));
        assert_eq!(plan.args_suffix, format!(" --session-id '{id}'"));
        assert!(plan.env.is_empty(), "no capture channel, no capture env");
    }

    /// A resume command needs no pin, overlay, or environment change.
    #[test]
    fn instrument_leaves_the_resume_form_untouched() {
        let inv = Grok.detect(&format!("grok --resume {ID}")).unwrap();
        assert_eq!(Grok.instrument(&inv, &paths(), None), SpawnPlan::default());
    }

    #[test]
    fn parse_capture_is_unconditionally_none() {
        // No injected channel exists, so no payload is ever trusted.
        let payload = format!(r#"{{"session_id":"{ID}"}}"#);
        assert_eq!(Grok.parse_capture(&payload), None);
        assert_eq!(Grok.parse_capture(""), None);
    }

    #[test]
    fn scrape_exit_reads_both_spellings_and_takes_the_last() {
        let short = format!("Resume with: grok -r {ID}");
        assert_eq!(Grok.scrape_exit(&short).as_deref(), Some(ID));
        let long = format!("Resume with: grok --resume {ID}");
        assert_eq!(Grok.scrape_exit(&long).as_deref(), Some(ID));

        // The last hint by position wins across spellings, either order.
        let both = format!("grok -r {OTHER}\n...\ngrok --resume {ID}\n");
        assert_eq!(Grok.scrape_exit(&both).as_deref(), Some(ID));
        let both = format!("grok --resume {OTHER}\n...\ngrok -r {ID}\n");
        assert_eq!(Grok.scrape_exit(&both).as_deref(), Some(ID));

        assert_eq!(Grok.scrape_exit("no hint here"), None);
        // A hint whose ID fails validation returns nothing.
        assert_eq!(Grok.scrape_exit("grok -r NOT-A-UUID"), None);
        // A longer hexadecimal run is not an ID.
        assert_eq!(Grok.scrape_exit(&format!("grok -r {ID}ff")), None);
    }

    /// Store keys percent-encode every non-unreserved byte, uppercase hex.
    #[test]
    fn encode_cwd_matches_the_observed_store_names() {
        assert_eq!(
            encode_cwd(Path::new("/Users/chris/Documents/Code/Apple/turret")).as_deref(),
            Some("%2FUsers%2Fchris%2FDocuments%2FCode%2FApple%2Fturret")
        );
        // Dots stay literal.
        assert_eq!(
            encode_cwd(Path::new("/Users/chris/.claude/jobs/ac6e4777/tmp/groktest")).as_deref(),
            Some("%2FUsers%2Fchris%2F.claude%2Fjobs%2Fac6e4777%2Ftmp%2Fgroktest")
        );
        // A literal `%` must encode for the mapping to stay injective.
        assert_eq!(encode_cwd(Path::new("/a%b")).as_deref(), Some("%2Fa%25b"));
        assert_eq!(
            encode_cwd(Path::new("/has space")).as_deref(),
            Some("%2Fhas%20space")
        );
        assert_eq!(encode_cwd(Path::new("/a+b")).as_deref(), Some("%2Fa%2Bb"));
        assert_eq!(encode_cwd(Path::new("/ü")).as_deref(), Some("%2F%C3%BC"));
    }

    #[test]
    fn correlate_fs_requires_a_unique_in_window_session_dir() {
        let home = temp("grok_correlate");
        let cwd = Path::new("/work/proj.rs");
        let dir = home.join("sessions").join("%2Fwork%2Fproj.rs");
        fs::create_dir_all(dir.join(ID)).unwrap();
        // The prompt-history sibling is a file, not a session.
        fs::write(dir.join("prompt_history.jsonl"), "{}").unwrap();
        let now = SystemTime::now();

        assert_eq!(
            Grok.correlate_fs(cwd, now, Some(&home)).as_deref(),
            Some(ID)
        );
        // Outside the window: the session predates the spawn by minutes.
        let late = now + std::time::Duration::from_secs(120);
        assert_eq!(Grok.correlate_fs(cwd, late, Some(&home)), None);
        // Wrong working directory.
        assert_eq!(
            Grok.correlate_fs(Path::new("/other"), now, Some(&home)),
            None
        );

        // A second in-window session makes the match ambiguous.
        fs::create_dir_all(dir.join(OTHER)).unwrap();
        assert_eq!(Grok.correlate_fs(cwd, now, Some(&home)), None);
    }

    #[test]
    fn correlate_fs_rejects_a_unique_non_uuid_dir() {
        let home = temp("grok_nonuuid");
        let cwd = Path::new("/w");
        let dir = home.join("sessions").join("%2Fw");
        fs::create_dir_all(dir.join("not-a-session")).unwrap();
        assert_eq!(Grok.correlate_fs(cwd, SystemTime::now(), Some(&home)), None);
    }

    /// `created_at` as grok writes it: 2026-07-15 is day 20_649 since epoch.
    const GROK_CREATED_AT: &str = "2026-07-15T00:34:19.339081Z";

    fn spec_spawned() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::new(20_649 * 86_400 + 34 * 60 + 19, 339_081_000)
    }

    fn write_summary(dir: &Path, id: &str, cwd: &str, subagent: bool) {
        let kind = if subagent {
            r#","session_kind":"subagent""#
        } else {
            ""
        };
        fs::write(
            dir.join("summary.json"),
            format!(
                r#"{{"info":{{"id":"{id}","cwd":"{cwd}"}},"created_at":"{GROK_CREATED_AT}"{kind}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn correlate_fs_reads_a_long_name_group_via_dot_cwd() {
        let home = temp("grok_longpath");
        let cwd = Path::new("/work/very-long-path-name-that-would-encode-past-the-limit");
        let group = home
            .join("sessions")
            .join("would-encode-past-the-limit-0123456789abcdef");
        fs::create_dir_all(group.join(ID)).unwrap();
        fs::write(group.join(".cwd"), format!("{}\n", cwd.display())).unwrap();
        write_summary(&group.join(ID), ID, cwd.to_str().unwrap(), false);
        assert_eq!(
            Grok.correlate_fs(cwd, spec_spawned(), Some(&home))
                .as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn correlate_fs_follows_a_symlink_cwd_to_the_canonical_group() {
        let tmp = temp("grok_canon");
        let real = tmp.join("real");
        let link = tmp.join("link");
        let home = tmp.join("home");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = real.canonicalize().unwrap();
        let group = home
            .join("sessions")
            .join(encode_cwd(&canonical).expect("canonical path is UTF-8"));
        fs::create_dir_all(group.join(ID)).unwrap();
        write_summary(&group.join(ID), ID, canonical.to_str().unwrap(), false);
        assert_eq!(
            Grok.correlate_fs(&link, spec_spawned(), Some(&home))
                .as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn correlate_fs_ignores_an_in_window_subagent_sibling() {
        let home = temp("grok_subagent");
        let cwd = Path::new("/work/proj.rs");
        let dir = home.join("sessions").join("%2Fwork%2Fproj.rs");
        fs::create_dir_all(dir.join(ID)).unwrap();
        fs::create_dir_all(dir.join(OTHER)).unwrap();
        write_summary(&dir.join(ID), ID, "/work/proj.rs", false);
        write_summary(&dir.join(OTHER), OTHER, "/work/proj.rs", true);
        assert_eq!(
            Grok.correlate_fs(cwd, spec_spawned(), Some(&home))
                .as_deref(),
            Some(ID)
        );
    }

    /// The scraper recovers the exit-hint ID from the corpus terminal bytes.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        assert_corpus_scrape(
            &Grok,
            include_bytes!("../../tests/corpus/grok_resume.bin"),
            "17ac97af-8cfc-46a7-9599-8cea45a687a6",
        );
    }
}
