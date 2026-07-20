//! Dashboard-preview resolution: one [`Preview`] per task, chosen by a
//! provenance cascade over facts the emulator already tracks and debounced
//! through hold timers so the column changes only when meaning changes.
//! Liveness and outcome stay with the glyph and age columns.

use std::time::{Duration, Instant};

use crate::emulator::Emulator;

// Both holds are wall-clock: tick spacing varies ≈25× (8 ms frame minimum
// under load, 200 ms idle backstop), so a tick-counted debounce would be
// load-dependent.

/// Minimum spacing between rendered title changes: above the spinner cadence
/// of title-animating children, below reading annoyance.
pub const TITLE_MIN_HOLD: Duration = Duration::from_millis(500);

/// Continuous absence of a rendered-rank-or-better candidate before a
/// demotion commits: more than one 200 ms quiet-tick gap with margin, so a
/// single-tick transient (a partial repaint, a held sync frame) never demotes
/// the rendered preview.
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
    /// Normalized adapter output: the cascade's top tier. No producer exists
    /// this phase; adapters land later and fill the slot.
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

/// One resolved preview. `frozen` is mutability, orthogonal to `source`: a
/// finished task reports its last source with `frozen: true`, which a
/// `Frozen` variant would erase. `rule` is a fine-grained matcher id; no
/// producer sets it this phase (adapters come later), and it stays
/// daemon-side — the peek footer sees it only in-process, never over the
/// wire.
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
}

/// The instantaneous candidate. Every branch is a fact the emulator tracks;
/// nothing is fabricated:
/// 1. [anchor slot — no producer this phase; adapters claim the top tier]
/// 2. alternate screen: the title while its epoch is current, else the marker
/// 3. primary screen: the live floor
fn cascade(screen: &impl ScreenFacts) -> Preview {
    if screen.alternate_screen() {
        return match screen.title() {
            Some(text) => Preview {
                text: text.to_string(),
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
    Preview::floor(screen.live_floor())
}

/// Candidate-recompute key: `(revision, alt epoch, alt bit, title, finished)`.
/// Titles and the alt bit cannot change without a revision bump (the
/// emulator's advance bookkeeping guarantees it), but the composite is cheap
/// and self-documenting.
type ResolveKey = (u64, u64, bool, Option<String>, bool);

/// Per-task resolution state. Lives on `Task` and resets with it: a rerun
/// replaces the whole `Task`, so run-scoped state needs no external map.
#[derive(Debug)]
pub struct PreviewState {
    rendered: Preview,
    /// Last cascade output, carried while the resolution key is unchanged.
    candidate: Preview,
    /// Latest demoted candidate while the demotion hold runs.
    pending_candidate: Option<Preview>,
    /// Start of the demotion hold: the first resolution whose candidate
    /// ranked below the rendered source. Pending-candidate changes never
    /// reset it — the timer measures continuous absence of
    /// rendered-or-higher, so a flapping demoted candidate cannot postpone
    /// the commit forever.
    downgrade_pending_since: Option<Instant>,
    /// Instant of the last rendered title: the min-hold deadline base.
    last_title_render: Option<Instant>,
    last_key: Option<ResolveKey>,
    /// Alt bit at the most recent live resolution; `finalize`'s teardown
    /// predicate compares it against the final screen.
    last_resolve_alt: bool,
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
            last_resolve_alt: false,
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
    /// without a rescan.
    pub fn resolve(
        &mut self,
        now: Instant,
        finished: bool,
        screen: &impl ScreenFacts,
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
            self.candidate = cascade(screen);
            self.last_key = Some(key);
        }
        self.last_resolve_alt = screen.alternate_screen();
        self.step(now);
        &self.rendered
    }

    /// One pass of the candidate-vs-rendered transition table.
    fn step(&mut self, now: Instant) {
        use std::cmp::Ordering;
        match self.candidate.source.cmp(&self.rendered.source) {
            Ordering::Greater => {
                self.cancel_demotion();
                let cand = self.candidate.clone();
                self.render(cand, now);
            }
            Ordering::Equal => {
                // Rank ≥ rendered: a pending demotion was a one-tick repaint
                // miss; recover with no visible change.
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
                            self.render(cand, now);
                        }
                    }
                    // The floor is live output; anchor text changes are
                    // semantic (unreachable until adapters land). Both
                    // render immediately.
                    PreviewSource::Floor | PreviewSource::Anchor => {
                        let cand = self.candidate.clone();
                        self.render(cand, now);
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
                    self.render(latest, now);
                }
            }
        }
    }

    /// Commit `p` as the rendered preview; title renders stamp the min-hold
    /// deadline base.
    fn render(&mut self, p: Preview, now: Instant) {
        if p.source == PreviewSource::Title {
            self.last_title_render = Some(now);
        }
        self.rendered = p;
    }

    fn cancel_demotion(&mut self) {
        self.pending_candidate = None;
        self.downgrade_pending_since = None;
    }

    /// Freeze the preview once the task's output is complete. Re-resolves
    /// the cascade against the final screen instead of freezing the last
    /// rendered value: output can land between the last resolution tick and
    /// output-complete (a stream's final `test result: ok` flush), and the
    /// final resolve must see it. One carve-out, `alt_torn_down_at_exit`:
    /// the task was on the alternate screen at its last live resolution and
    /// the final screen is primary, so the exit's 1049l restored pre-launch
    /// junk (alt-screen agent CLIs print nothing afterward) and the last
    /// rendered preview freezes instead — a stale but meaningful line under
    /// a truthful outcome glyph beats shell junk. Holds do not apply:
    /// finalization overrides the whole transition table. Idempotent; later
    /// resolutions short-circuit to the frozen value.
    pub fn finalize(&mut self, screen: &impl ScreenFacts) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.cancel_demotion();
        let alt_torn_down_at_exit = self.last_resolve_alt && !screen.alternate_screen();
        if alt_torn_down_at_exit {
            self.rendered.frozen = true;
            return;
        }
        let mut fin = cascade(screen);
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
    }

    /// Each cascade tier maps one emulator fact to one source.
    #[test]
    fn cascade_maps_screen_facts_to_sources() {
        let now = Instant::now();

        let mut st = PreviewState::new();
        let s = FakeScreen::primary("last row");
        let p = st.resolve(now, false, &s).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("last row", PreviewSource::Floor, false)
        );

        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        s.set_title("app");
        let p = st.resolve(now, false, &s).clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));

        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("shell");
        s.enter_alt();
        let p = st.resolve(now, false, &s).clone();
        assert_eq!((p.text.as_str(), p.source), (MARKER, PreviewSource::Marker));

        let mut st = PreviewState::new();
        let s = FakeScreen::primary("");
        let p = st.resolve(now, false, &s).clone();
        assert_eq!((p.text.as_str(), p.source), ("", PreviewSource::Floor));
    }

    /// Rank increases render on the very resolution that observes them.
    #[test]
    fn promotion_renders_immediately() {
        let now = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("building");
        assert_eq!(st.resolve(now, false, &s).source, PreviewSource::Floor);

        s.enter_alt();
        let p = st.resolve(now, false, &s).clone();
        assert_eq!((p.text.as_str(), p.source), (MARKER, PreviewSource::Marker));

        s.set_title("app");
        let p = st.resolve(now, false, &s).clone();
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
        st.resolve(t0, false, &s);

        s.clear_title();
        assert_eq!(
            st.resolve(t0, false, &s).source,
            PreviewSource::Title,
            "a demotion must not render instantly"
        );
        let inside = t0 + DEMOTION_HOLD - Duration::from_millis(1);
        assert_eq!(st.resolve(inside, false, &s).source, PreviewSource::Title);
        let p = st.resolve(t0 + DEMOTION_HOLD, false, &s).clone();
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
        st.resolve(t0, false, &s);

        s.clear_title();
        st.resolve(t0, false, &s);
        // The title returns inside the hold: cancel, no visible change.
        s.set_title("app");
        let p = st.resolve(t0 + ms(300), false, &s).clone();
        assert_eq!((p.text.as_str(), p.source), ("app", PreviewSource::Title));

        // The next demotion measures from its own start, not the old stamp.
        s.clear_title();
        st.resolve(t0 + ms(400), false, &s);
        assert_eq!(
            st.resolve(t0 + ms(900), false, &s).source,
            PreviewSource::Title,
            "the canceled hold must not shorten the fresh one"
        );
        assert_eq!(
            st.resolve(t0 + ms(1_000), false, &s).source,
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
        st.resolve(t0, false, &s);

        // First demoted candidate: the marker.
        s.clear_title();
        st.resolve(t0, false, &s);
        // The pending candidate flaps to a floor; the timer keeps t0.
        s.leave_alt();
        s.set_floor("done 3 tests");
        assert_eq!(
            st.resolve(t0 + Duration::from_millis(300), false, &s).source,
            PreviewSource::Title
        );
        let p = st.resolve(t0 + DEMOTION_HOLD, false, &s).clone();
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
        assert_eq!(st.resolve(t0, false, &s).text, "one");

        s.set_title("two");
        assert_eq!(st.resolve(t0 + ms(200), false, &s).text, "one");
        s.set_title("three");
        assert_eq!(st.resolve(t0 + ms(300), false, &s).text, "one");
        assert_eq!(
            st.resolve(t0 + TITLE_MIN_HOLD, false, &s).text,
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
        assert_eq!(st.resolve(t0, false, &s).text, "compiling foo");
        s.set_floor("compiling bar");
        assert_eq!(st.resolve(t0, false, &s).text, "compiling bar");
    }

    /// An unchanged resolution key carries the candidate without re-reading
    /// the grid; a revision bump recomputes.
    #[test]
    fn unchanged_key_skips_the_candidate_recompute() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("steady");
        st.resolve(t0, false, &s);
        assert_eq!(s.floor_calls.get(), 1);

        st.resolve(t0 + ms(200), false, &s);
        assert_eq!(s.floor_calls.get(), 1, "unchanged key must not re-read");

        s.advance();
        st.resolve(t0 + ms(400), false, &s);
        assert_eq!(s.floor_calls.get(), 2, "a revision bump must recompute");
    }

    /// Primary-at-exit finalization re-resolves: output that landed after
    /// the last resolution tick reaches the frozen preview.
    #[test]
    fn finalize_re_resolves_a_primary_screen() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("running");
        st.resolve(t0, false, &s);

        s.set_floor("test result: ok");
        st.finalize(&s);
        let p = st.resolve(t0, true, &s).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("test result: ok", PreviewSource::Floor, true)
        );
    }

    /// Alt torn down at exit: the restored primary junk must not replace the
    /// last rendered preview, and the frozen value never moves again.
    #[test]
    fn finalize_keeps_the_rendered_preview_across_alt_teardown() {
        let t0 = Instant::now();
        let mut st = PreviewState::new();
        let mut s = FakeScreen::primary("prelaunch junk");
        s.enter_alt();
        s.set_title("agent: working");
        st.resolve(t0, false, &s);

        // The exit's 1049l lands with no live resolution in between.
        s.leave_alt();
        st.finalize(&s);
        let p = st.resolve(t0, true, &s).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("agent: working", PreviewSource::Title, true)
        );

        s.set_floor("stray");
        assert_eq!(
            st.resolve(t0 + Duration::from_secs(5), true, &s).text,
            "agent: working",
            "resolution must short-circuit to the frozen value"
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
        st.resolve(t0, false, &s);

        s.set_title("step 2: done");
        st.finalize(&s);
        let p = st.resolve(t0, true, &s).clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("step 2: done", PreviewSource::Title, true)
        );
    }
}
