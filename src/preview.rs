//! Dashboard-preview resolution: one [`Preview`] per task, selected from
//! emulator state and stabilized with hold timers.
//! Liveness and outcome stay with the glyph and age columns.

use std::time::{Duration, Instant};

use crate::{emulator::Emulator, harness::summary::SummaryAdapter};

// Holds use elapsed time because tick intervals range from the 8 ms frame
// minimum to the 200 ms idle backstop.

/// Minimum interval between rendered title changes.
pub const TITLE_MIN_HOLD: Duration = Duration::from_millis(500);

/// Time a lower-ranked candidate must persist before it replaces the rendered
/// preview.
pub const DEMOTION_HOLD: Duration = Duration::from_millis(600);

/// Preview text for an alternate-screen child with no usable title.
pub const MARKER: &str = "full-screen";

/// Where a preview's text came from. Declared in ascending authority so the
/// derived `Ord` ranks provenance directly: `Anchor > Title > Marker >
/// Floor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PreviewSource {
    /// The last non-blank row of the live screen: unshadowable for
    /// primary-screen programs, so a title never replaces live stream output.
    Floor,
    /// The alternate screen is active with no usable title.
    Marker,
    /// The child's window title, honored only on the alternate screen.
    Title,
    /// Normalized adapter output: the cascade's top tier.
    Anchor,
}

impl PreviewSource {
    /// Lowercase label shared by the wire encoding and the peek footer.
    pub fn label(self) -> &'static str {
        match self {
            PreviewSource::Floor => "floor",
            PreviewSource::Marker => "marker",
            PreviewSource::Title => "title",
            PreviewSource::Anchor => "anchor",
        }
    }
}

/// One resolved preview. `frozen` marks an immutable final value. `rule` is
/// the summary-adapter matcher ID for an Anchor preview and is `None` for
/// other sources.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub text: String,
    pub source: PreviewSource,
    pub rule: Option<&'static str>,
    pub frozen: bool,
}

impl Preview {
    fn floor(text: String) -> Preview {
        Preview {
            text,
            source: PreviewSource::Floor,
            rule: None,
            frozen: false,
        }
    }
}

/// The emulator facts one resolution step reads. A trait so unit tests
/// resolve against synthetic screens without a PTY.
pub trait ScreenFacts {
    fn revision(&self) -> u64;
    fn alt_epoch(&self) -> u64;
    fn alternate_screen(&self) -> bool;
    fn title(&self) -> Option<&str>;
    fn live_floor(&self) -> String;
    /// Every live-viewport row, trailing padding trimmed: the summary
    /// adapters' structural scan input.
    fn live_rows(&self) -> Vec<String>;
    /// The floor snapshotted at the most recent alt-screen exit: what the
    /// 1049l restore left visible; `None` before the first exit.
    fn alt_leave_floor(&self) -> Option<&str>;
}

impl ScreenFacts for Emulator {
    fn revision(&self) -> u64 {
        Emulator::revision(self)
    }

    fn alt_epoch(&self) -> u64 {
        Emulator::alt_epoch(self)
    }

    fn alternate_screen(&self) -> bool {
        Emulator::alternate_screen(self)
    }

    fn title(&self) -> Option<&str> {
        Emulator::title(self)
    }

    fn live_floor(&self) -> String {
        Emulator::live_floor(self)
    }

    fn live_rows(&self) -> Vec<String> {
        Emulator::live_rows(self)
    }

    fn alt_leave_floor(&self) -> Option<&str> {
        Emulator::alt_leave_floor(self)
    }
}

/// Resolve the instantaneous candidate in descending priority:
/// 1. summary adapter: the normalized live status when the CLI's working
///    structure is present, `{model label} · `-prefixed when the adapter
///    reads one from stable chrome
/// 2. alternate screen: the title while its epoch is current, else the marker
/// 3. primary screen: the live floor
fn cascade(screen: &impl ScreenFacts, adapter: Option<&dyn SummaryAdapter>) -> Preview {
    if let Some(a) = adapter
        && let Some((text, rule)) = a.live_preview(screen)
    {
        let text = match a.model_label(screen) {
            Some(label) => format!("{label} · {text}"),
            None => text,
        };
        return Preview {
            text,
            source: PreviewSource::Anchor,
            rule: Some(rule),
            frozen: false,
        };
    }
    if screen.alternate_screen() {
        return match screen.title() {
            // The adapter may rewrite the title for display (claude's
            // rotating spinner-frame prefix canonicalizes so the text
            // stays constant); capture itself remains program-agnostic.
            // Source stays Title and rule stays None: rules are anchor
            // matcher ids, and a rewritten title is still a title.
            Some(text) => Preview {
                text: adapter
                    .and_then(|a| a.normalize_title(text))
                    .unwrap_or_else(|| text.to_string()),
                source: PreviewSource::Title,
                rule: None,
                frozen: false,
            },
            None => Preview {
                text: MARKER.to_string(),
                source: PreviewSource::Marker,
                rule: None,
                frozen: false,
            },
        };
    }
    // Leading indentation is layout, not meaning: codex's status bar (an
    // inline UI's bottom-most row, the floor of an idle codex task) indents
    // itself, and the spaces waste preview width. Trimmed here, not in
    // `live_floor`: the emulator's row stays a faithful fact because it
    // doubles as the teardown-snapshot comparator.
    let floor = screen.live_floor();
    let trimmed = floor.trim_start();
    Preview::floor(if trimmed.len() == floor.len() {
        floor
    } else {
        trimmed.to_string()
    })
}

/// State that invalidates the cached preview candidate.
type ResolveKey = (u64, u64, bool, Option<String>, bool);

/// Per-task preview resolution state. A rerun replaces the `Task` and resets
/// this state.
#[derive(Debug)]
pub struct PreviewState {
    rendered: Preview,
    /// Last cascade output, carried while the resolution key is unchanged.
    candidate: Preview,
    /// Latest demoted candidate while the demotion hold runs.
    pending_candidate: Option<Preview>,
    /// Start of the demotion hold: the first resolution whose candidate
    /// ranked below the rendered source. Pending-candidate changes never
    /// reset it: the timer measures continuous absence of
    /// rendered-or-higher, so a flapping demoted candidate cannot postpone
    /// the commit forever.
    downgrade_pending_since: Option<Instant>,
    /// Instant of the last rendered title: the min-hold deadline base.
    last_title_render: Option<Instant>,
    last_key: Option<ResolveKey>,
    /// Whether the rendered preview was committed on the alternate screen.
    /// Finalization combines this with the exit snapshot to detect a restored
    /// primary screen.
    rendered_under_alt: bool,
    finalized: bool,
}

impl Default for PreviewState {
    fn default() -> PreviewState {
        PreviewState::new()
    }
}

impl PreviewState {
    pub fn new() -> PreviewState {
        let empty = Preview::floor(String::new());
        PreviewState {
            rendered: empty.clone(),
            candidate: empty,
            pending_candidate: None,
            downgrade_pending_since: None,
            last_title_render: None,
            last_key: None,
            rendered_under_alt: false,
            finalized: false,
        }
    }

    /// Whether the preview froze: the caller's once-per-life finalize gate.
    pub fn finalized(&self) -> bool {
        self.finalized
    }

    /// Resolve the rendered preview against the current screen. `now` is a
    /// parameter, never read internally, so tests drive the holds with
    /// synthetic instants. The candidate is recomputed only when the
    /// resolution key changed; hold expiries commit the carried value
    /// without a rescan. `adapter` is the task's summary adapter, fixed for
    /// the task's life, so it needs no slot in the resolution key.
    pub fn resolve(
        &mut self,
        now: Instant,
        finished: bool,
        screen: &impl ScreenFacts,
        adapter: Option<&dyn SummaryAdapter>,
    ) -> &Preview {
        if self.finalized {
            return &self.rendered;
        }
        let key = (
            screen.revision(),
            screen.alt_epoch(),
            screen.alternate_screen(),
            screen.title().map(str::to_owned),
            finished,
        );
        if self.last_key.as_ref() != Some(&key) {
            self.candidate = cascade(screen, adapter);
            self.last_key = Some(key);
        }
        self.step(now, screen.alternate_screen());
        &self.rendered
    }

    /// One pass of the candidate-vs-rendered transition table. `alt` is the
    /// alt bit of the screen this resolution ran against; every commit
    /// (demotion-hold expiries included) stamps it onto the render.
    fn step(&mut self, now: Instant, alt: bool) {
        use std::cmp::Ordering;
        match self.candidate.source.cmp(&self.rendered.source) {
            Ordering::Greater => {
                self.cancel_demotion();
                let cand = self.candidate.clone();
                self.render(cand, now, alt);
            }
            Ordering::Equal => {
                // A recovered rank cancels a pending demotion without a
                // visible change.
                self.cancel_demotion();
                if self.candidate == self.rendered {
                    return;
                }
                match self.candidate.source {
                    // Static text: same source, same value.
                    PreviewSource::Marker => {}
                    // Min-hold: at most one rendered title change per
                    // `TITLE_MIN_HOLD`, the deadline fixed from the last
                    // render; the newest carried candidate wins at expiry.
                    PreviewSource::Title => {
                        let held = self
                            .last_title_render
                            .is_some_and(|t| now.duration_since(t) < TITLE_MIN_HOLD);
                        if !held {
                            let cand = self.candidate.clone();
                            self.render(cand, now, alt);
                        }
                    }
                    // The floor is live output; anchor text changes are
                    // semantic (a new verb, a new completion row). Both
                    // render immediately.
                    PreviewSource::Floor | PreviewSource::Anchor => {
                        let cand = self.candidate.clone();
                        self.render(cand, now, alt);
                    }
                }
            }
            Ordering::Less => {
                let since = *self.downgrade_pending_since.get_or_insert(now);
                self.pending_candidate = Some(self.candidate.clone());
                if now.duration_since(since) >= DEMOTION_HOLD
                    && let Some(latest) = self.pending_candidate.take()
                {
                    self.downgrade_pending_since = None;
                    self.render(latest, now, alt);
                }
            }
        }
    }

    /// Commit `p` as the rendered preview under the screen mode that
    /// produced it; title renders stamp the min-hold deadline base.
    fn render(&mut self, p: Preview, now: Instant, alt: bool) {
        if p.source == PreviewSource::Title {
            self.last_title_render = Some(now);
        }
        self.rendered = p;
        self.rendered_under_alt = alt;
    }

    fn cancel_demotion(&mut self) {
        self.pending_candidate = None;
        self.downgrade_pending_since = None;
    }

    /// Freeze the preview once output is complete. An `exit_line` takes
    /// precedence; otherwise the final screen is resolved without hold timers.
    /// If an alternate-screen render is followed only by restoration of the
    /// snapshotted primary floor, the rendered preview is retained. A
    /// different final floor is resolved normally.
    /// Repeated calls are no-ops.
    pub fn finalize(
        &mut self,
        screen: &impl ScreenFacts,
        adapter: Option<&dyn SummaryAdapter>,
        exit_line: Option<String>,
    ) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.cancel_demotion();
        if let Some(text) = exit_line {
            self.rendered = Preview {
                text,
                source: PreviewSource::Anchor,
                rule: None,
                frozen: true,
            };
            return;
        }
        let alt_torn_down_at_exit = self.rendered_under_alt
            && !screen.alternate_screen()
            && screen
                .alt_leave_floor()
                .is_some_and(|snapshot| screen.live_floor() == snapshot);
        if alt_torn_down_at_exit {
            self.rendered.frozen = true;
            return;
        }
        let mut fin = cascade(screen, adapter);
        fin.frozen = true;
        self.rendered = fin;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Synthetic screen facts with a floor-read counter for the
    /// revision-gate test.
    struct FakeScreen {
        revision: u64,
        alt_epoch: u64,
        alt: bool,
        title: Option<String>,
        floor: String,
        /// Floor at the last `leave_alt`, mirroring the emulator's
        /// alt-exit snapshot.
        alt_leave_floor: Option<String>,
        floor_calls: Cell<usize>,
    }

    impl FakeScreen {
        fn primary(floor: &str) -> FakeScreen {
            FakeScreen {
                revision: 1,
                alt_epoch: 0,
                alt: false,
                title: None,
                floor: floor.into(),
                alt_leave_floor: None,
                floor_calls: Cell::new(0),
            }
        }

        /// Bump the revision alone: the emulator counts every grid advance.
        fn advance(&mut self) {
            self.revision += 1;
        }

        fn enter_alt(&mut self) {
            self.alt = true;
            self.alt_epoch += 1;
            self.advance();
        }

        fn leave_alt(&mut self) {
            self.alt = false;
            // The emulator snapshots the restored floor at the alt exit.
            self.alt_leave_floor = Some(self.floor.clone());
            self.advance();
        }

        fn set_title(&mut self, t: &str) {
            self.title = Some(t.into());
            self.advance();
        }

        fn clear_title(&mut self) {
            self.title = None;
            self.advance();
        }

        fn set_floor(&mut self, f: &str) {
            self.floor = f.into();
            self.advance();
        }
    }

    impl ScreenFacts for FakeScreen {
        fn revision(&self) -> u64 {
            self.revision
        }

        fn alt_epoch(&self) -> u64 {
            self.alt_epoch
        }

        fn alternate_screen(&self) -> bool {
            self.alt
        }

        fn title(&self) -> Option<&str> {
            self.title.as_deref()
        }

        fn live_floor(&self) -> String {
            self.floor_calls.set(self.floor_calls.get() + 1);
            self.floor.clone()
        }

        fn live_rows(&self) -> Vec<String> {
            vec![self.floor.clone()]
        }

        fn alt_leave_floor(&self) -> Option<&str> {
            self.alt_leave_floor.as_deref()
        }
    }

    /// Fixed-output adapter: the cascade tests here cover the slot's
    /// plumbing (rank, label prefix, freeze), not extraction; that lives
    /// with the adapters in `harness::summary`.
    struct StubAdapter {
        live: Option<(&'static str, &'static str)>,
        label: Option<&'static str>,
    }

    impl SummaryAdapter for StubAdapter {
        fn live_preview(&self, _screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
            self.live.map(|(text, rule)| (text.to_string(), rule))
        }

        fn model_label(&self, _screen: &dyn ScreenFacts) -> Option<String> {
            self.label.map(str::to_string)
        }
    }

    /// The anchor tier outranks the title, carries its rule id, and prepends
    /// the model label when the adapter reads one.
    #[test]
    fn anchor_outranks_title_and_prepends_the_label() {
        let now = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        let adapter = StubAdapter {
            live: Some(("Working", "stub:working")),
            label: Some("model-x"),
        };
        let p = st.resolve(now, false, &s, Some(&adapter)).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.rule),
            (
                "model-x · Working",
                PreviewSource::Anchor,
                Some("stub:working")
            )
        );

        // Without a label the anchor renders the bare status.
        let bare = StubAdapter {
            live: Some(("Working", "stub:working")),
            label: None,
        };
        let mut st = PreviewState::new();
        let p = st.resolve(now, false, &s, Some(&bare)).clone();
        assert_eq!(p.text, "Working");
    }

    /// A lost anchor is a demotion: the title returns only after the hold,
    /// exactly like any other rank drop.
    #[test]
    fn anchor_loss_demotes_through_the_hold() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        let working = StubAdapter {
            live: Some(("Working", "stub:working")),
            label: None,
        };
        st.resolve(t0, false, &s, Some(&working));

        let idle = StubAdapter {
            live: None,
            label: None,
        };
        s.advance();
        assert_eq!(
            st.resolve(t0, false, &s, Some(&idle)).source,
            PreviewSource::Anchor,
            "a lost anchor must not demote instantly"
        );
        let p = st
            .resolve(t0 + DEMOTION_HOLD, false, &s, Some(&idle))
            .clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));
    }

    /// Finalization's re-resolve includes the anchor tier: a completion row
    /// present on the final primary screen freezes as an Anchor preview.
    #[test]
    fn finalize_freezes_the_final_anchor() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("building");
        let adapter = StubAdapter {
            live: Some(("Ran echo ok", "stub:ran")),
            label: None,
        };
        st.resolve(t0, false, &s, None);

        s.advance();
        st.finalize(&s, Some(&adapter), None);
        let p = st.resolve(t0, true, &s, Some(&adapter)).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.rule, p.frozen),
            ("Ran echo ok", PreviewSource::Anchor, Some("stub:ran"), true)
        );
    }

    /// An adapter exit line outranks the final screen and freezes verbatim.
    #[test]
    fn finalize_prefers_an_adapter_exit_line() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let s = FakeScreen::primary("junk floor");
        st.resolve(t0, false, &s, None);
        st.finalize(&s, None, Some("session done".to_string()));
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("session done", PreviewSource::Anchor, true)
        );
    }

    /// Each cascade tier maps one emulator fact to one source.
    #[test]
    fn cascade_maps_screen_facts_to_sources() {
        let now = Instant::now();

        let mut st = PreviewState::new();
        let s = FakeScreen::primary("last row");
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("last row", PreviewSource::Floor, false)
        );

        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));

        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), (MARKER, PreviewSource::Marker));

        let mut st = PreviewState::new();
        let s = FakeScreen::primary("");
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), ("", PreviewSource::Floor));
    }

    /// Rank increases render on the very resolution that observes them.
    #[test]
    fn promotion_renders_immediately() {
        let now = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("building");
        assert_eq!(
            st.resolve(now, false, &s, None).source,
            PreviewSource::Floor
        );

        s.enter_alt();
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), (MARKER, PreviewSource::Marker));

        s.set_title("app");
        let p = st.resolve(now, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));
    }

    /// A demotion holds the rendered preview through `DEMOTION_HOLD` and
    /// commits at expiry.
    #[test]
    fn demotion_commits_only_after_the_hold() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        st.resolve(t0, false, &s, None);

        s.clear_title();
        assert_eq!(
            st.resolve(t0, false, &s, None).source,
            PreviewSource::Title,
            "a demotion must not render instantly"
        );
        let inside = t0 + DEMOTION_HOLD - Duration::from_millis(1);
        assert_eq!(
            st.resolve(inside, false, &s, None).source,
            PreviewSource::Title
        );
        let p = st.resolve(t0 + DEMOTION_HOLD, false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), (MARKER, PreviewSource::Marker));
    }

    /// A rank≥ candidate inside the hold cancels the pending demotion with
    /// no visible change, and a fresh demotion gets its own full window.
    #[test]
    fn rank_recovery_inside_the_hold_cancels_pending() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        st.resolve(t0, false, &s, None);

        s.clear_title();
        st.resolve(t0, false, &s, None);
        // The title returns inside the hold: cancel, no visible change.
        s.set_title("app");
        let p = st.resolve(t0 + ms(300), false, &s, None).clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));

        // The next demotion starts a new hold interval.
        s.clear_title();
        st.resolve(t0 + ms(400), false, &s, None);
        assert_eq!(
            st.resolve(t0 + ms(900), false, &s, None).source,
            PreviewSource::Title,
            "the canceled hold must not shorten the fresh one"
        );
        assert_eq!(
            st.resolve(t0 + ms(1_000), false, &s, None).source,
            PreviewSource::Marker
        );
    }

    /// A flapping pending candidate does not reset the demotion timer, and
    /// the latest stored candidate commits at expiry.
    #[test]
    fn flapping_pending_keeps_the_timer_and_commits_the_latest() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        st.resolve(t0, false, &s, None);

        // First demoted candidate: the marker.
        s.clear_title();
        st.resolve(t0, false, &s, None);
        // The pending candidate flaps to a floor; the timer keeps t0.
        s.leave_alt();
        s.set_floor("done 3 tests");
        assert_eq!(
            st.resolve(t0 + Duration::from_millis(300), false, &s, None)
                .source,
            PreviewSource::Title
        );
        let p = st.resolve(t0 + DEMOTION_HOLD, false, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source),
            ("done 3 tests", PreviewSource::Floor),
            "the latest pending candidate commits, not the first"
        );
    }

    /// Two title changes inside `TITLE_MIN_HOLD` render nothing; the newest
    /// carried candidate wins at the deadline.
    #[test]
    fn title_changes_render_at_most_once_per_min_hold() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("one");
        assert_eq!(st.resolve(t0, false, &s, None).text, "one");

        s.set_title("two");
        assert_eq!(st.resolve(t0 + ms(200), false, &s, None).text, "one");
        s.set_title("three");
        assert_eq!(st.resolve(t0 + ms(300), false, &s, None).text, "one");
        assert_eq!(
            st.resolve(t0 + TITLE_MIN_HOLD, false, &s, None).text,
            "three",
            "the newest candidate wins at the deadline"
        );
    }

    /// Same-source floor text is live output: it renders immediately.
    #[test]
    fn floor_text_renders_immediately() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("compiling foo");
        assert_eq!(st.resolve(t0, false, &s, None).text, "compiling foo");
        s.set_floor("compiling bar");
        assert_eq!(st.resolve(t0, false, &s, None).text, "compiling bar");
    }

    /// An unchanged resolution key carries the candidate without re-reading
    /// the grid; a revision bump recomputes.
    #[test]
    fn unchanged_key_skips_the_candidate_recompute() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("steady");
        st.resolve(t0, false, &s, None);
        assert_eq!(s.floor_calls.get(), 1);

        st.resolve(t0 + ms(200), false, &s, None);
        assert_eq!(s.floor_calls.get(), 1, "unchanged key must not re-read");

        s.advance();
        st.resolve(t0 + ms(400), false, &s, None);
        assert_eq!(s.floor_calls.get(), 2, "a revision bump must recompute");
    }

    /// A resize invalidates the key and recomputes the reflowed floor even
    /// when the alternate-screen state and title are unchanged.
    #[test]
    fn a_resize_shaped_revision_bump_recomputes_the_candidate() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("a long row that fit");
        assert_eq!(st.resolve(t0, false, &s, None).text, "a long row that fit");

        s.set_floor("a long row");
        assert_eq!(
            st.resolve(t0, false, &s, None).text,
            "a long row",
            "the reflowed floor must render, not the carried candidate"
        );
    }

    /// Primary-at-exit finalization re-resolves: output that landed after
    /// the last resolution tick reaches the frozen preview.
    #[test]
    fn finalize_re_resolves_a_primary_screen() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("running");
        st.resolve(t0, false, &s, None);

        s.set_floor("test result: ok");
        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("test result: ok", PreviewSource::Floor, true)
        );
    }

    /// Alternate-screen teardown with an unchanged restored primary floor
    /// retains and freezes the last rendered preview.
    #[test]
    fn finalize_keeps_the_rendered_preview_across_alt_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s, None);

        // The exit's 1049l lands with no live resolution in between.
        s.leave_alt();
        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("agent: working", PreviewSource::Title, true)
        );

        s.set_floor("stray");
        assert_eq!(
            st.resolve(t0 + Duration::from_secs(5), true, &s, None).text,
            "agent: working",
            "resolution must short-circuit to the frozen value"
        );
    }

    /// A resolution between alternate-screen teardown and reader EOF retains
    /// the alternate-screen render through the demotion hold and finalization.
    #[test]
    fn finalize_keeps_the_render_when_a_resolve_saw_the_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s, None);

        // Teardown lands and a tick resolves before output completes.
        s.leave_alt();
        let p = st.resolve(t0, false, &s, None).clone();
        assert_eq!(
            p.source,
            PreviewSource::Title,
            "premise: the demotion hold keeps the title rendered"
        );

        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("agent: working", PreviewSource::Title, true)
        );
    }

    /// When the demotion hold commits a primary-screen floor after teardown,
    /// finalization resolves the final primary screen.
    #[test]
    fn finalize_trusts_the_screen_after_a_primary_commit() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s, None);

        // The child returns to the primary screen and keeps printing; the
        // hold expires and commits the floor, stamped primary.
        s.leave_alt();
        s.set_floor("wrote 12 files");
        st.resolve(t0, false, &s, None);
        let p = st.resolve(t0 + DEMOTION_HOLD, false, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source),
            ("wrote 12 files", PreviewSource::Floor),
            "premise: the hold committed the primary floor"
        );

        s.set_floor("exit summary");
        st.finalize(&s, None, None);
        let p = st.resolve(t0 + DEMOTION_HOLD, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("exit summary", PreviewSource::Floor, true),
            "the primary-stamped render must not resurrect the title"
        );
    }

    /// Primary output written after alternate-screen teardown changes the
    /// snapshotted floor and is frozen even inside the demotion hold.
    #[test]
    fn finalize_freezes_primary_output_written_after_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s, None);

        // Teardown observed (snapshot: "prelaunch junk"), title still held.
        s.leave_alt();
        assert_eq!(st.resolve(t0, false, &s, None).source, PreviewSource::Title);

        // A real final line lands before exit, inside the hold.
        s.set_floor("done");
        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("done", PreviewSource::Floor, true),
            "legitimate primary output must not be discarded"
        );
    }

    /// Control-only chunks after teardown can advance the revision without
    /// moving the floor. Finalization compares visible content and retains
    /// the held preview.
    #[test]
    fn finalize_holds_through_control_only_output_after_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s, None);

        s.leave_alt();
        assert_eq!(st.resolve(t0, false, &s, None).source, PreviewSource::Title);

        // A later advance changes the revision (and drops the title) but
        // leaves the floor untouched: nothing visible moved.
        s.clear_title();
        assert_eq!(st.resolve(t0, false, &s, None).source, PreviewSource::Title);

        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("agent: working", PreviewSource::Title, true),
            "control-only chunks must not defeat the carve-out"
        );
    }

    /// When one read contains 1049l and subsequent primary-screen output, the
    /// exit snapshot excludes that output and finalization freezes it.
    #[test]
    fn finalize_freezes_coalesced_output_after_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut emu = Emulator::new(24, 80, 100);
        emu.process(b"prelaunch junk\r\n");
        emu.process(b"\x1b[?1049h\x1b]0;working\x07app body");
        assert_eq!(
            st.resolve(t0, false, &emu, None).source,
            PreviewSource::Title,
            "premise: the title rendered under the alt screen"
        );

        // Teardown and the final line arrive in one read.
        emu.process(b"\x1b[?1049ldone\r\n");
        assert_eq!(
            emu.alt_leave_floor(),
            Some("prelaunch junk"),
            "premise: the snapshot is the restore, not the successor line"
        );
        st.finalize(&emu, None, None);
        let p = st.resolve(t0, true, &emu, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("done", PreviewSource::Floor, true)
        );
    }

    /// A resize between teardown and finalization re-snapshots an unchanged
    /// restored floor, so the held title remains eligible to freeze.
    #[test]
    fn finalize_holds_across_a_resize_after_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut emu = Emulator::new(24, 40, 100);
        emu.process(b"prelaunch junk that will wrap\r\n");
        emu.process(b"\x1b[?1049h\x1b]0;working\x07app body");
        assert_eq!(
            st.resolve(t0, false, &emu, None).source,
            PreviewSource::Title
        );

        emu.process(b"\x1b[?1049l");
        let before = emu.live_floor();
        emu.resize(24, 20);
        assert_ne!(
            emu.live_floor(),
            before,
            "premise: the reflow moved the floor"
        );
        st.finalize(&emu, None, None);
        let p = st.resolve(t0, true, &emu, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("working", PreviewSource::Title, true),
            "a reflow is not post-teardown output"
        );
    }

    /// A resize after primary output preserves the mismatch with the
    /// alternate-screen exit snapshot, so finalization freezes the output.
    #[test]
    fn finalize_freezes_output_across_a_resize_after_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut emu = Emulator::new(24, 40, 100);
        emu.process(b"prelaunch junk that will wrap\r\n");
        emu.process(b"\x1b[?1049h\x1b]0;working\x07app body");
        assert_eq!(
            st.resolve(t0, false, &emu, None).source,
            PreviewSource::Title
        );

        emu.process(b"\x1b[?1049l");
        emu.process(b"done\r\n");
        emu.resize(24, 20);
        st.finalize(&emu, None, None);
        let p = st.resolve(t0, true, &emu, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("done", PreviewSource::Floor, true),
            "real post-teardown output must survive the resize"
        );
    }

    /// Death on the alt screen is not a teardown: the final alt screen is
    /// authoritative and freezes as-is, catching a last-moment title change.
    #[test]
    fn finalize_on_the_alt_screen_freezes_the_final_cascade() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("step 1");
        st.resolve(t0, false, &s, None);

        s.set_title("step 2: done");
        st.finalize(&s, None, None);
        let p = st.resolve(t0, true, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("step 2: done", PreviewSource::Title, true)
        );
    }

    /// Floor previews remove leading layout indentation.
    #[test]
    fn floor_preview_trims_leading_indentation() {
        let mut st = PreviewState::new();
        let t0 = Instant::now();
        let s = FakeScreen::primary("  gpt-5.6-sol high · fleetcom · 89.9K used");
        let p = st.resolve(t0, false, &s, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source),
            (
                "gpt-5.6-sol high · fleetcom · 89.9K used",
                PreviewSource::Floor
            )
        );
    }
}
