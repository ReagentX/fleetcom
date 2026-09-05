//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID; canonical resume commands retain their explicit ID.

use std::path::Path;

use super::{CapturePaths, Harness, Invocation, SpawnPlan, pin_plan};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        fixtures::{ID, assert_all_opaque, paths},
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
}
