//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID; canonical resume commands retain their explicit ID.

use std::path::Path;

use super::{CapturePaths, Harness, SpawnPlan};

pub struct Grok;

impl Harness for Grok {
    fn shape(&self) -> (&'static str, &'static str) {
        ("grok", "--resume")
    }

    fn session_flag(&self) -> Option<&'static str> {
        Some("--session-id")
    }

    /// Nothing rides along: grok has no capture channel to install.
    fn overlay(&self, _capture: &CapturePaths, _home: Option<&Path>) -> SpawnPlan {
        SpawnPlan::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        Intent,
        fixtures::{ID, OTHER, argv, assert_all_opaque, paths},
        plan, shell_words,
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

    /// Managed argv is the intent part alone: Grok exposes no live capture
    /// channel, so nothing rides along either way.
    #[test]
    fn managed_argv_is_the_pin_or_the_resume_and_nothing_else() {
        let fresh = plan(&Grok, &Intent::Fresh, Some(ID), &paths(), None);
        assert_eq!(fresh.args, argv(&["--session-id", ID]));
        assert_eq!(fresh.resume_id.as_deref(), Some(ID));
        assert!(fresh.env.is_empty(), "no capture channel, no capture env");

        let resume = plan(&Grok, &Intent::Resume(OTHER.into()), None, &paths(), None);
        assert_eq!(resume.args, argv(&["--resume", OTHER]));
        assert_eq!(resume.resume_id.as_deref(), Some(OTHER));
        assert!(resume.env.is_empty());
    }

    /// A bare literal `grok` gains only the pinned ID.
    #[test]
    fn bare_literal_suffix_pins_an_id_and_nothing_else() {
        let fresh = plan(&Grok, &Intent::Fresh, Some(ID), &paths(), None);
        assert_eq!(shell_words(&fresh.args), format!(" --session-id '{ID}'"));
    }

    /// The typed resume form needs no pin, overlay, or environment change.
    #[test]
    fn overlay_leaves_the_resume_form_untouched() {
        assert_eq!(Grok.overlay(&paths(), None), SpawnPlan::default());
    }

    #[test]
    fn parse_capture_is_unconditionally_none() {
        // No injected channel exists, so no payload is ever trusted.
        let payload = format!(r#"{{"session_id":"{ID}"}}"#);
        assert_eq!(Grok.parse_capture(&payload, None, None), None);
        assert_eq!(Grok.parse_capture("", None, None), None);
    }
}
