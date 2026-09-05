//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID, and completed tasks expose either `grok -r <uuid>` or
//! `grok --resume <uuid>` in terminal output.

use std::path::Path;

use super::{CapturePaths, Harness, Invocation, SpawnPlan, last_hint, pin_plan};

pub struct Grok;

impl Harness for Grok {
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

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid short or long resume hint names the conversation.
        last_hint(text, &["grok -r ", "grok --resume "])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths},
        is_uuid,
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
