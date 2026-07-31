//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID, and completed tasks expose either `grok -r <uuid>` or
//! `grok --resume <uuid>` in terminal output. The filesystem fallback
//! correlates `<grok-home>/sessions/<encoded-cwd>/<uuid>/` directories.

use std::{path::Path, time::SystemTime};

use super::{CapturePaths, Harness, Invocation, SpawnPlan, last_hint, pin_plan, unique_in_window};

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
        let dir = self
            .home_root(home)?
            .join("sessions")
            .join(encode_cwd(cwd)?);
        unique_in_window(dir, spawned, |entry| {
            // One directory per session, named by its uuid. Files such as
            // the `prompt_history.jsonl` sibling are not sessions.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                return None;
            }
            Some(entry.file_name().to_str()?.to_string())
        })
    }
}

/// Encode an absolute working directory as a Grok session-store key. `/`
/// becomes `%2F`, and `%` becomes `%25` to keep the mapping injective.
/// Non-UTF-8 paths have no representable key.
fn encode_cwd(cwd: &Path) -> Option<String> {
    let s = cwd.to_str()?;
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '/' => out.push_str("%2F"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    Some(out)
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

    /// Store keys encode slashes and percent signs while preserving dots.
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
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn correlate_fs_rejects_a_unique_non_uuid_dir() {
        let home = temp("grok_nonuuid");
        let cwd = Path::new("/w");
        let dir = home.join("sessions").join("%2Fw");
        fs::create_dir_all(dir.join("not-a-session")).unwrap();
        assert_eq!(Grok.correlate_fs(cwd, SystemTime::now(), Some(&home)), None);
        let _ = fs::remove_dir_all(&home);
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
