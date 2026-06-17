//! Data-driven Tutorial / Trials runner (T072).
//!
//! A [`TutorialRunner`] walks an ordered [`Vec<Lesson>`]: each [`Lesson`] states
//! a goal (`title` + `instruction`), configures the training dummy + a set of
//! HUD overlays, and watches a [`SuccessCond`]. The runner is **headless and
//! pure** — it consumes a thin stream of [`LessonEvent`]s (connecting hit,
//! recognized command, combo count, block, anti-air, throw connect) that the
//! live game derives from the engine's `Match`/`TickReport`, the input matcher,
//! the combo counter, and the airborne flag — exactly the signals the engine
//! already emits. This keeps lesson logic unit-testable without a window.
//!
//! Two hard guarantees from the task spec:
//!
//! - **Never soft-locks.** [`TutorialRunner::skip`] always advances to the next
//!   lesson (or completes the trial), regardless of the current success state.
//!   A [`SuccessCond::Unsatisfiable`] lesson is auto-skipped so a trial authored
//!   against `trainingdummy` still *loads* against an arbitrary character.
//! - **Bad/missing assets fall back gracefully.** [`load_lessons`] returns the
//!   built-in [`default_lessons`] set when the on-disk `assets/data/tutorial/`
//!   scripts are absent or unparseable; a single malformed lesson `.def` is
//!   warn-logged and skipped rather than aborting the trial.
//!
//! Lesson scripts are *original* clean-room `.def` assets (no Elecbyte content);
//! see `assets/data/tutorial/`. The on-disk format is one `.def` per lesson with
//! a `[Lesson]` section, listed in order by `tutorial.def`'s `[Trial]` `lessonN`
//! keys.

use std::path::Path;

use fp_formats::def::DefFile;

/// How the training dummy behaves while a lesson is active.
///
/// The runner only *carries* this configuration (the live app applies it to P2);
/// it is part of the lesson data so each lesson can set up the dummy it needs
/// (e.g. a jumping dummy for an anti-air lesson, a guarding dummy for a block
/// lesson). Unknown values from a lesson `.def` fall back to [`DummyMode::Stand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DummyMode {
    /// Dummy stands idle and takes hits (default).
    #[default]
    Stand,
    /// Dummy crouches (low guard / low-hit practice).
    Crouch,
    /// Dummy jumps repeatedly (anti-air practice).
    Jump,
    /// Dummy guards all incoming attacks (block-string / RPS practice).
    GuardAll,
    /// Dummy crouch-guards (low-block practice).
    GuardLow,
    /// Dummy attacks on a fixed cadence (attack-block-throw RPS practice).
    Attack,
}

impl DummyMode {
    /// Parses a [`DummyMode`] from a lesson `.def` value (case-insensitive).
    ///
    /// An empty or unrecognized value falls back to [`DummyMode::Stand`] (never
    /// an error — bad content must not break a lesson).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "crouch" => DummyMode::Crouch,
            "jump" => DummyMode::Jump,
            "guardall" | "guard" | "block" => DummyMode::GuardAll,
            "guardlow" | "blocklow" => DummyMode::GuardLow,
            "attack" => DummyMode::Attack,
            _ => DummyMode::Stand,
        }
    }
}

/// Which study overlays a lesson turns on (the F026 overlay family).
///
/// Presentational only — the runner carries the flags; the live app toggles the
/// matching overlay draws. Defaults to all-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayFlags {
    /// Show the Clsn hitbox boxes.
    pub hitboxes: bool,
    /// Show the live input display.
    pub input_display: bool,
    /// Show the per-move frame-data readout.
    pub frame_data: bool,
}

impl OverlayFlags {
    /// Parses a comma/space-separated overlay list from a lesson `.def`
    /// (e.g. `"hitboxes, input"`). Unknown tokens are ignored.
    pub fn parse(s: &str) -> Self {
        let mut f = OverlayFlags::default();
        for tok in s.split([',', ' ', '\t']) {
            match tok.trim().to_ascii_lowercase().as_str() {
                "hitboxes" | "hitbox" | "clsn" | "boxes" => f.hitboxes = true,
                "input" | "inputs" | "inputdisplay" => f.input_display = true,
                "framedata" | "frames" | "frame" => f.frame_data = true,
                _ => {}
            }
        }
        f
    }
}

/// The condition the runner watches to mark a lesson complete.
///
/// The runner evaluates these against the [`LessonEvent`] stream the live game
/// feeds it each tick (the engine already emits all of these signals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuccessCond {
    /// Land a named command (matcher-recognized), e.g. `"fireball"`.
    LandCommand(String),
    /// Block (guard) at least `n` incoming hits.
    BlockNHits(u32),
    /// Reach a combo of at least `n` hits.
    ComboCount(u32),
    /// Connect an attack while the dummy was airborne (an anti-air / DP).
    AntiAir,
    /// Connect a throw.
    ThrowConnected,
    /// Can never be satisfied (e.g. the lesson needs a move this character lacks).
    /// The runner auto-skips an `Unsatisfiable` lesson so trials never soft-lock.
    Unsatisfiable,
}

impl SuccessCond {
    /// Parses a [`SuccessCond`] from a lesson `.def` `success` value.
    ///
    /// Grammar (case-insensitive): `landcommand <name>`, `blocknhits <n>`,
    /// `combocount <n>`, `antiair`, `throwconnected`. An empty or unrecognized
    /// value parses to [`SuccessCond::Unsatisfiable`] so the lesson loads (and is
    /// then auto-skipped) rather than failing — graceful degradation, never panic.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        let (head, rest) = match s.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (s, ""),
        };
        match head.to_ascii_lowercase().as_str() {
            "landcommand" | "command" if !rest.is_empty() => {
                SuccessCond::LandCommand(rest.to_string())
            }
            "blocknhits" | "block" => SuccessCond::BlockNHits(rest.parse().unwrap_or(1).max(1)),
            "combocount" | "combo" => SuccessCond::ComboCount(rest.parse().unwrap_or(2).max(1)),
            "antiair" => SuccessCond::AntiAir,
            "throwconnected" | "throw" => SuccessCond::ThrowConnected,
            _ => SuccessCond::Unsatisfiable,
        }
    }

    /// `true` if no character could ever satisfy this — the runner skips it.
    pub fn is_unsatisfiable(&self) -> bool {
        matches!(self, SuccessCond::Unsatisfiable)
    }
}

/// A single tutorial / trial lesson — pure data, authorable as a `.def`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lesson {
    /// Short title shown to the player (the goal headline).
    pub title: String,
    /// Longer instruction text describing what to do.
    pub instruction: String,
    /// How the dummy is configured for this lesson.
    pub dummy: DummyMode,
    /// Which study overlays are enabled.
    pub overlays: OverlayFlags,
    /// The condition that completes the lesson.
    pub success: SuccessCond,
    /// Advisory frame budget for the player (no enforcement — purely a hint for
    /// the live UI; the runner never times out / soft-locks on it).
    pub timeout_hint: Option<u32>,
}

impl Lesson {
    /// Builds a [`Lesson`] from a parsed `.def`'s `[Lesson]` section.
    ///
    /// Missing keys fall back to safe defaults (empty text, [`DummyMode::Stand`],
    /// no overlays, [`SuccessCond::Unsatisfiable`]) so a sparse or partially
    /// malformed lesson still loads.
    pub fn from_def(def: &DefFile) -> Self {
        let get = |k: &str| def.get("lesson", k).unwrap_or("").to_string();
        Lesson {
            title: get("title"),
            instruction: get("instruction"),
            dummy: DummyMode::parse(&get("dummy")),
            overlays: OverlayFlags::parse(&get("overlays")),
            success: SuccessCond::parse(&get("success")),
            timeout_hint: def.get_parsed("lesson", "timeout"),
        }
    }
}

/// A signal the live game feeds the runner each tick.
///
/// These map one-to-one onto signals the engine already emits: a recognized
/// command (the input matcher), a connecting hit + whether the defender was
/// airborne / guarding (`TickReport` / combat resolve / `EvalCtx`), the current
/// combo count (combo counter), and a throw connect (`TargetOp`). The live app
/// translates per-tick state into zero or more of these; tests feed them
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LessonEvent {
    /// The player's input matcher recognized this named command this tick.
    CommandRecognized(String),
    /// The player's attack connected; `defender_airborne` is true if the dummy
    /// was airborne when hit (anti-air), `guarded` if the dummy blocked it.
    HitConnected {
        /// Whether the defender was airborne when the hit landed.
        defender_airborne: bool,
        /// Whether the defender guarded (blocked) the hit.
        guarded: bool,
    },
    /// The player's current combo length (highest seen this combo).
    ComboCount(u32),
    /// A throw connected against the dummy.
    ThrowConnected,
}

/// Outcome of feeding one tick's events to the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// Still working on the current lesson.
    InProgress,
    /// The current lesson was just completed; runner advanced.
    LessonComplete,
    /// The last lesson completed; the whole trial is done.
    TrialComplete,
}

/// Drives an ordered list of [`Lesson`]s, advancing on each success.
///
/// Construct with [`TutorialRunner::new`]; feed per-tick signals via
/// [`TutorialRunner::observe`]. The runner never blocks and never soft-locks:
/// any lesson can be force-advanced with [`TutorialRunner::skip`], and an
/// `Unsatisfiable` lesson is skipped automatically on entry.
#[derive(Debug, Clone)]
pub struct TutorialRunner {
    lessons: Vec<Lesson>,
    index: usize,
    /// Per-lesson running tally of guarded hits (for `BlockNHits`).
    blocked: u32,
    /// Whether the active lesson has been satisfied (true between success and
    /// the caller acknowledging via the returned [`TickOutcome`]).
    done: bool,
}

impl TutorialRunner {
    /// Creates a runner over `lessons`. Auto-skips any leading
    /// [`SuccessCond::Unsatisfiable`] lesson so the runner starts on a real one.
    pub fn new(lessons: Vec<Lesson>) -> Self {
        let mut r = TutorialRunner {
            lessons,
            index: 0,
            blocked: 0,
            done: false,
        };
        r.skip_unsatisfiable();
        r
    }

    /// The lesson currently in progress, or `None` once the trial is complete.
    pub fn current(&self) -> Option<&Lesson> {
        self.lessons.get(self.index)
    }

    /// Index of the current lesson.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Total number of lessons in the trial.
    pub fn len(&self) -> usize {
        self.lessons.len()
    }

    /// `true` if the trial has no lessons.
    pub fn is_empty(&self) -> bool {
        self.lessons.is_empty()
    }

    /// `true` once every lesson has been completed or skipped.
    pub fn is_complete(&self) -> bool {
        self.index >= self.lessons.len()
    }

    /// Force-advances past the current lesson (the always-works Skip).
    ///
    /// Returns [`TickOutcome::TrialComplete`] if that was the last lesson, else
    /// [`TickOutcome::LessonComplete`]. Calling on an already-complete trial is a
    /// no-op returning [`TickOutcome::TrialComplete`].
    pub fn skip(&mut self) -> TickOutcome {
        if self.is_complete() {
            return TickOutcome::TrialComplete;
        }
        self.advance()
    }

    /// Feeds one tick's [`LessonEvent`]s and re-evaluates the current success
    /// condition. Advances automatically when satisfied.
    ///
    /// Returns the [`TickOutcome`] for this tick. Once the trial is complete,
    /// further calls are no-ops returning [`TickOutcome::TrialComplete`].
    pub fn observe(&mut self, events: &[LessonEvent]) -> TickOutcome {
        if self.is_complete() {
            return TickOutcome::TrialComplete;
        }
        for ev in events {
            if self.satisfies(ev) {
                self.done = true;
            }
        }
        if self.done {
            self.advance()
        } else {
            TickOutcome::InProgress
        }
    }

    /// Evaluates a single event against the current lesson's success condition,
    /// updating any per-lesson tallies. Returns `true` if the lesson is now
    /// satisfied.
    fn satisfies(&mut self, ev: &LessonEvent) -> bool {
        let Some(lesson) = self.lessons.get(self.index) else {
            return false;
        };
        match (&lesson.success, ev) {
            (SuccessCond::LandCommand(name), LessonEvent::CommandRecognized(got)) => {
                got.eq_ignore_ascii_case(name)
            }
            (
                SuccessCond::AntiAir,
                LessonEvent::HitConnected {
                    defender_airborne: true,
                    ..
                },
            ) => true,
            (SuccessCond::ThrowConnected, LessonEvent::ThrowConnected) => true,
            (SuccessCond::ComboCount(n), LessonEvent::ComboCount(got)) => *got >= *n,
            (SuccessCond::BlockNHits(n), LessonEvent::HitConnected { guarded: true, .. }) => {
                self.blocked = self.blocked.saturating_add(1);
                self.blocked >= *n
            }
            _ => false,
        }
    }

    /// Moves to the next lesson, resetting per-lesson state and auto-skipping any
    /// `Unsatisfiable` lesson landed on.
    fn advance(&mut self) -> TickOutcome {
        self.index += 1;
        self.blocked = 0;
        self.done = false;
        self.skip_unsatisfiable();
        if self.is_complete() {
            TickOutcome::TrialComplete
        } else {
            TickOutcome::LessonComplete
        }
    }

    /// Skips forward over any lesson whose success condition can never be met,
    /// so the runner only ever rests on a satisfiable lesson (or completion).
    fn skip_unsatisfiable(&mut self) {
        while let Some(lesson) = self.lessons.get(self.index) {
            if lesson.success.is_unsatisfiable() {
                tracing::warn!(
                    "tutorial: skipping unsatisfiable lesson {} (\"{}\")",
                    self.index,
                    lesson.title
                );
                self.index += 1;
                self.blocked = 0;
                self.done = false;
            } else {
                break;
            }
        }
    }
}

/// The built-in clean-room lesson set, used when on-disk scripts are missing.
///
/// Covers every lesson the task requires: Block High/Low, Attack-Block-Throw
/// (RPS), Throw a Fireball, Anti-air/DP, and a 2-hit BnB. The fireball/anti-air
/// commands match `trainingdummy`'s command set (`fireball` QCF, `dp`).
pub fn default_lessons() -> Vec<Lesson> {
    vec![
        Lesson {
            title: "Block High and Low".to_string(),
            instruction: "Hold back to block standing hits, down-back to block lows. Block 3 hits."
                .to_string(),
            dummy: DummyMode::Attack,
            overlays: OverlayFlags {
                input_display: true,
                ..OverlayFlags::default()
            },
            success: SuccessCond::BlockNHits(3),
            timeout_hint: Some(600),
        },
        Lesson {
            title: "Attack, Block, Throw (RPS)".to_string(),
            instruction: "Beat the guarding dummy: a throw beats block. Land a throw.".to_string(),
            dummy: DummyMode::GuardAll,
            overlays: OverlayFlags::default(),
            success: SuccessCond::ThrowConnected,
            timeout_hint: Some(600),
        },
        Lesson {
            title: "Throw a Fireball".to_string(),
            instruction: "Quarter-circle forward + punch. Throw a fireball.".to_string(),
            dummy: DummyMode::Stand,
            overlays: OverlayFlags {
                input_display: true,
                ..OverlayFlags::default()
            },
            success: SuccessCond::LandCommand("fireball".to_string()),
            timeout_hint: Some(600),
        },
        Lesson {
            title: "Anti-air / DP".to_string(),
            instruction: "The dummy is jumping. Knock it out of the air. Dragon punch!".to_string(),
            dummy: DummyMode::Jump,
            overlays: OverlayFlags {
                hitboxes: true,
                ..OverlayFlags::default()
            },
            success: SuccessCond::AntiAir,
            timeout_hint: Some(600),
        },
        Lesson {
            title: "2-Hit BnB Combo".to_string(),
            instruction: "Chain two hits into a combo. Land a 2-hit combo.".to_string(),
            dummy: DummyMode::Stand,
            overlays: OverlayFlags {
                hitboxes: true,
                frame_data: true,
                ..OverlayFlags::default()
            },
            success: SuccessCond::ComboCount(2),
            timeout_hint: Some(600),
        },
    ]
}

/// Loads the ordered lesson list from a tutorial directory, falling back to the
/// built-in [`default_lessons`] set on any failure.
///
/// The expected layout is an index file `<dir>/tutorial.def` whose `[Trial]`
/// section lists lesson files in order (`lesson1 = block.def`, `lesson2 = ...`),
/// each pointing at a one-`[Lesson]`-section `.def`. A missing/unreadable index,
/// an empty trial, or a directory with no parseable lessons all degrade to the
/// built-in set — the trial always loads. A single malformed lesson `.def` is
/// warn-logged and skipped without dropping the rest.
pub fn load_lessons(dir: &Path) -> Vec<Lesson> {
    let index_path = dir.join("tutorial.def");
    let Ok(index) = DefFile::load(&index_path) else {
        tracing::warn!(
            "tutorial: no index at {} — using built-in lessons",
            index_path.display()
        );
        return default_lessons();
    };

    let Some(trial) = index.sections.get("trial") else {
        tracing::warn!("tutorial: {} has no [Trial] section", index_path.display());
        return default_lessons();
    };

    // Collect lesson refs in `lessonN` order (lesson1, lesson2, ...).
    let mut refs: Vec<(u32, String)> = trial
        .iter()
        .filter_map(|(k, v)| {
            let n: u32 = k.strip_prefix("lesson")?.parse().ok()?;
            Some((n, v.clone()))
        })
        .collect();
    refs.sort_by_key(|(n, _)| *n);

    let mut lessons = Vec::new();
    for (_, rel) in &refs {
        let path = DefFile::resolve_path(&index_path, rel);
        match DefFile::load(&path) {
            Ok(def) => lessons.push(Lesson::from_def(&def)),
            Err(e) => tracing::warn!("tutorial: skipping lesson {}: {e}", path.display()),
        }
    }

    if lessons.is_empty() {
        tracing::warn!(
            "tutorial: no loadable lessons under {} — using built-in lessons",
            dir.display()
        );
        return default_lessons();
    }
    lessons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(airborne: bool, guarded: bool) -> LessonEvent {
        LessonEvent::HitConnected {
            defender_airborne: airborne,
            guarded,
        }
    }

    #[test]
    fn parses_dummy_overlays_and_success() {
        assert_eq!(DummyMode::parse("JUMP"), DummyMode::Jump);
        assert_eq!(DummyMode::parse("guard"), DummyMode::GuardAll);
        assert_eq!(DummyMode::parse("nonsense"), DummyMode::Stand);

        let f = OverlayFlags::parse("hitboxes, input ,framedata");
        assert!(f.hitboxes && f.input_display && f.frame_data);
        assert_eq!(OverlayFlags::parse(""), OverlayFlags::default());

        assert_eq!(
            SuccessCond::parse("landcommand fireball"),
            SuccessCond::LandCommand("fireball".to_string())
        );
        assert_eq!(
            SuccessCond::parse("blocknhits 3"),
            SuccessCond::BlockNHits(3)
        );
        assert_eq!(
            SuccessCond::parse("combocount 2"),
            SuccessCond::ComboCount(2)
        );
        assert_eq!(SuccessCond::parse("antiair"), SuccessCond::AntiAir);
        assert_eq!(SuccessCond::parse("throw"), SuccessCond::ThrowConnected);
        // Unknown / empty -> Unsatisfiable (loads, then auto-skips).
        assert!(SuccessCond::parse("???").is_unsatisfiable());
        assert!(SuccessCond::parse("").is_unsatisfiable());
    }

    #[test]
    fn runs_default_lessons_in_order_to_completion() {
        let mut r = TutorialRunner::new(default_lessons());
        assert_eq!(r.len(), 5);
        assert_eq!(r.index(), 0);

        // Lesson 1: BlockNHits(3) — two guarded hits is not enough.
        assert_eq!(
            r.observe(&[hit(false, true), hit(false, true)]),
            TickOutcome::InProgress
        );
        // A non-guarded hit does not count toward the block tally.
        assert_eq!(r.observe(&[hit(false, false)]), TickOutcome::InProgress);
        // Third guarded hit completes the lesson.
        assert_eq!(r.observe(&[hit(false, true)]), TickOutcome::LessonComplete);
        assert_eq!(r.index(), 1);

        // Lesson 2: ThrowConnected.
        assert_eq!(r.observe(&[hit(false, true)]), TickOutcome::InProgress);
        assert_eq!(
            r.observe(&[LessonEvent::ThrowConnected]),
            TickOutcome::LessonComplete
        );
        assert_eq!(r.index(), 2);

        // Lesson 3: LandCommand("fireball") — case-insensitive, wrong command ignored.
        assert_eq!(
            r.observe(&[LessonEvent::CommandRecognized("dp".to_string())]),
            TickOutcome::InProgress
        );
        assert_eq!(
            r.observe(&[LessonEvent::CommandRecognized("FireBall".to_string())]),
            TickOutcome::LessonComplete
        );
        assert_eq!(r.index(), 3);

        // Lesson 4: AntiAir — a grounded hit does not count; an airborne one does.
        assert_eq!(r.observe(&[hit(false, false)]), TickOutcome::InProgress);
        assert_eq!(r.observe(&[hit(true, false)]), TickOutcome::LessonComplete);
        assert_eq!(r.index(), 4);

        // Lesson 5: ComboCount(2) — a 1-hit combo is not enough.
        assert_eq!(
            r.observe(&[LessonEvent::ComboCount(1)]),
            TickOutcome::InProgress
        );
        assert_eq!(
            r.observe(&[LessonEvent::ComboCount(2)]),
            TickOutcome::TrialComplete
        );
        assert!(r.is_complete());
        assert!(r.current().is_none());

        // Further observes are no-ops.
        assert_eq!(
            r.observe(&[LessonEvent::ComboCount(9)]),
            TickOutcome::TrialComplete
        );
    }

    #[test]
    fn skip_always_advances_and_never_soft_locks() {
        let mut r = TutorialRunner::new(default_lessons());
        for _ in 0..r.len() - 1 {
            assert_eq!(r.skip(), TickOutcome::LessonComplete);
        }
        assert_eq!(r.skip(), TickOutcome::TrialComplete);
        assert!(r.is_complete());
        // Skipping a completed trial stays complete (no panic, no underflow).
        assert_eq!(r.skip(), TickOutcome::TrialComplete);
    }

    #[test]
    fn auto_skips_unsatisfiable_lessons() {
        let lessons = vec![
            Lesson {
                title: "needs a move this char lacks".to_string(),
                instruction: String::new(),
                dummy: DummyMode::Stand,
                overlays: OverlayFlags::default(),
                success: SuccessCond::Unsatisfiable,
                timeout_hint: None,
            },
            Lesson {
                title: "throw".to_string(),
                instruction: String::new(),
                dummy: DummyMode::GuardAll,
                overlays: OverlayFlags::default(),
                success: SuccessCond::ThrowConnected,
                timeout_hint: None,
            },
            Lesson {
                title: "also unsatisfiable".to_string(),
                instruction: String::new(),
                dummy: DummyMode::Stand,
                overlays: OverlayFlags::default(),
                success: SuccessCond::Unsatisfiable,
                timeout_hint: None,
            },
        ];
        // Starts on lesson 1 (index 1), having skipped the leading unsatisfiable.
        let mut r = TutorialRunner::new(lessons);
        assert_eq!(r.index(), 1);
        assert_eq!(r.current().unwrap().title, "throw");
        // Completing it skips the trailing unsatisfiable lesson and finishes.
        assert_eq!(
            r.observe(&[LessonEvent::ThrowConnected]),
            TickOutcome::TrialComplete
        );
        assert!(r.is_complete());
    }

    #[test]
    fn each_condition_fires_exactly_once_per_lesson() {
        // ComboCount latches: once satisfied, the runner advances and does not
        // re-fire on the same combo signal for the (now-different) lesson.
        let mut r = TutorialRunner::new(vec![
            Lesson {
                title: "a".to_string(),
                instruction: String::new(),
                dummy: DummyMode::Stand,
                overlays: OverlayFlags::default(),
                success: SuccessCond::ComboCount(2),
                timeout_hint: None,
            },
            Lesson {
                title: "b".to_string(),
                instruction: String::new(),
                dummy: DummyMode::Stand,
                overlays: OverlayFlags::default(),
                success: SuccessCond::LandCommand("dp".to_string()),
                timeout_hint: None,
            },
        ]);
        assert_eq!(
            r.observe(&[LessonEvent::ComboCount(3)]),
            TickOutcome::LessonComplete
        );
        // The same combo event must NOT satisfy lesson "b".
        assert_eq!(
            r.observe(&[LessonEvent::ComboCount(3)]),
            TickOutcome::InProgress
        );
        assert_eq!(
            r.observe(&[LessonEvent::CommandRecognized("dp".to_string())]),
            TickOutcome::TrialComplete
        );
    }

    #[test]
    fn empty_trial_is_immediately_complete() {
        let mut r = TutorialRunner::new(vec![]);
        assert!(r.is_empty());
        assert!(r.is_complete());
        assert_eq!(r.skip(), TickOutcome::TrialComplete);
        assert_eq!(r.observe(&[]), TickOutcome::TrialComplete);
    }

    #[test]
    fn lesson_from_def_parses_section() {
        let def = DefFile::from_str(
            "[Lesson]\n\
             title = Throw a Fireball\n\
             instruction = QCF + punch\n\
             dummy = stand\n\
             overlays = input, hitboxes\n\
             success = landcommand fireball\n\
             timeout = 600\n",
        )
        .unwrap();
        let lesson = Lesson::from_def(&def);
        assert_eq!(lesson.title, "Throw a Fireball");
        assert_eq!(lesson.instruction, "QCF + punch");
        assert_eq!(lesson.dummy, DummyMode::Stand);
        assert!(lesson.overlays.input_display && lesson.overlays.hitboxes);
        assert_eq!(
            lesson.success,
            SuccessCond::LandCommand("fireball".to_string())
        );
        assert_eq!(lesson.timeout_hint, Some(600));
    }

    #[test]
    fn sparse_def_falls_back_to_safe_defaults() {
        let def = DefFile::from_str("[Lesson]\ntitle = bare\n").unwrap();
        let lesson = Lesson::from_def(&def);
        assert_eq!(lesson.title, "bare");
        assert_eq!(lesson.dummy, DummyMode::Stand);
        assert_eq!(lesson.overlays, OverlayFlags::default());
        // No success key -> Unsatisfiable (loads, then auto-skipped by the runner).
        assert!(lesson.success.is_unsatisfiable());
        assert_eq!(lesson.timeout_hint, None);
    }

    #[test]
    fn load_lessons_falls_back_when_dir_missing() {
        let lessons = load_lessons(Path::new("/nonexistent/tutorial/dir/xyz"));
        // Degrades to the built-in set rather than returning empty.
        assert_eq!(lessons.len(), default_lessons().len());
    }

    #[test]
    fn ships_required_lesson_topics() {
        // The built-in set must cover every topic the task requires.
        let l = default_lessons();
        assert!(l
            .iter()
            .any(|x| matches!(x.success, SuccessCond::BlockNHits(_))));
        assert!(l.iter().any(|x| x.success == SuccessCond::ThrowConnected));
        assert!(l
            .iter()
            .any(|x| matches!(&x.success, SuccessCond::LandCommand(c) if c == "fireball")));
        assert!(l.iter().any(|x| x.success == SuccessCond::AntiAir));
        assert!(l
            .iter()
            .any(|x| matches!(x.success, SuccessCond::ComboCount(_))));
    }
}
