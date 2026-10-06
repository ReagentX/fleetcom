//! Grok has no injectable live-capture channel. A fresh launch instead pins a
//! v4 UUID, and a resume names it.

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
        fixtures::{ID, OTHER, argv, paths},
        plan,
    };

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

    #[test]
    fn parse_capture_is_unconditionally_none() {
        // No injected channel exists, so no payload is ever trusted.
        let payload = format!(r#"{{"session_id":"{ID}"}}"#);
        assert_eq!(Grok.parse_capture(&payload, None, None), None);
        assert_eq!(Grok.parse_capture("", None, None), None);
    }
}
