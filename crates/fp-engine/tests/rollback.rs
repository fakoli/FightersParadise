//! Rollback-readiness audit harness (T077) — the GGPO save/predict/restore/
//! re-sim invariant, plus a documented rollback-budget measurement.
//!
//! Netplay (and frame-perfect netcode in general) is built on a single
//! correctness invariant: a client may **predict** the remote player's input,
//! advance the simulation a few frames ahead, and — when the *real* input
//! arrives and disagrees with the prediction — **roll back** to a saved state and
//! **re-advance** with the corrected inputs. For this to be sound, the re-advanced
//! state must be **byte-for-byte identical** to a simulation that had the correct
//! inputs all along (a "from-scratch canonical run"). Any nondeterminism — HashMap
//! iteration order leaking into state, float NaN, time-based RNG, per-tick scratch
//! surviving a restore — breaks rollback and desyncs the two clients.
//!
//! Fighters Paradise already ships exactly the primitives rollback needs:
//! [`Match::snapshot`] / [`Match::restore_snapshot`] (a fast, deterministic
//! capture/apply of the mutable runtime), on top of the deterministic fixed-60Hz
//! tick. This file is the **audit** that proves the invariant holds under the
//! realistic save→predict→rollback→re-advance loop — not just a single resume
//! point — and it measures the rollback **budget** (snapshot size + capture /
//! restore wall-time per frame) so a future netcode layer knows whether per-frame
//! save-states fit inside a 16.67 ms tick.
//!
//! It builds its fighters **entirely from synthetic, in-memory data** (a minimal
//! SFF v1 container plus a compiled state graph), so it runs everywhere — CI
//! included — with no external assets.
//!
//! # No netcode here
//!
//! This is groundwork only: there is **no transport, no socket, no
//! input-delay/prediction buffer** — just the engine-side proof that the snapshot
//! API satisfies the GGPO re-simulation invariant a transport would rely on.
//!
//! # The GGPO rollback loop (what `rollback_resim_matches_canonical` asserts)
//!
//! ```text
//!   save S      = snapshot() at frame N
//!   predicted   = advance K frames with PREDICTED remote inputs   (the wrong guess)
//!   restore S   = restore_snapshot(S)                              (roll back)
//!   corrected   = advance K frames with the REAL remote inputs     (the fix)
//!   assert corrected-state == canonical-state   (a run that used REAL inputs from N)
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use fp_character::{
    Character, CharacterConstants, CompiledController, CompiledExpr, CompiledParam, CompiledState,
    CompiledTriggerGroup, LoadedCharacter,
};
use fp_core::{Rect, SpriteId, Vec2};
use fp_engine::{Match, MatchInput, Player, StageBounds, DEFAULT_MATCH_SEED};
use fp_formats::air::{AirFile, AnimAction, AnimFrame};
use fp_formats::sff::SffFile;

/// A symmetric stage half-width, wide enough that the two bodies never reach an
/// edge during the scripted runs.
const STAGE_HALF_WIDTH: f32 = 200.0;

/// Builds a minimal valid SFF v1 container in memory carrying a single *linked*
/// (data-less) sprite, so a headless [`LoadedCharacter`] can be constructed
/// without any sprite asset on disk. The v1 parser rejects a zero-sprite file, so
/// we include one linked sprite (`data_length = 0`, which skips PCX decoding); the
/// headless simulation never reads sprite pixels.
fn empty_sff() -> SffFile {
    const SUBHEADER_OFFSET: usize = 64;
    let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[15] = 1; // SFF v1
    buf[16..20].copy_from_slice(&1u32.to_le_bytes());
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
    SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
}

/// An AIR file with a single action `action` carrying one frame with the given
/// attack (Clsn1) and hurt (Clsn2) boxes.
fn air_with(action: i32, clsn1: Vec<Rect>, clsn2: Vec<Rect>) -> AirFile {
    let frame = AnimFrame {
        sprite: SpriteId::new(0, 0),
        ticks: 1,
        clsn1,
        clsn2,
        ..Default::default()
    };
    let mut actions = HashMap::new();
    actions.insert(
        action,
        AnimAction {
            action_number: action,
            frames: vec![frame],
            loopstart: 0,
        },
    );
    AirFile { actions }
}

/// A `VarSet` controller firing unconditionally (`trigger1 = 1`) that assigns the
/// expression `expr` into integer variable `idx` (`var(idx) = expr`).
fn varset_controller(idx: i32, expr: &str) -> CompiledController {
    CompiledController {
        state_number: 0,
        label: String::new(),
        controller_type: Some("VarSet".to_string()),
        triggerall: Vec::new(),
        triggers: vec![CompiledTriggerGroup {
            number: 1,
            conditions: vec![CompiledExpr::compile("1")],
        }],
        persistent: None,
        ignorehitpause: None,
        params: [(format!("var({idx})"), CompiledParam::compile(expr))]
            .into_iter()
            .collect(),
    }
}

/// A synthetic [`LoadedCharacter`] whose state 0 draws `random` into `var(0)`
/// every tick (so the live executor's RNG stream actually flows and any
/// nondeterminism would surface) and carries an action-0 hurt frame (so the two
/// fighters remain hittable). Built only from public APIs and in-memory data.
fn rng_probe_loaded() -> LoadedCharacter {
    let mut states = HashMap::new();
    states.insert(
        0,
        CompiledState {
            number: 0,
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("N".to_string()),
            controllers: vec![varset_controller(0, "random")],
            ..Default::default()
        },
    );
    LoadedCharacter {
        name: "rng-probe".to_string(),
        displayname: "rng-probe".to_string(),
        author: String::new(),
        localcoord: (320, 240),
        constants: CharacterConstants::default(),
        states,
        sff: empty_sff(),
        air: air_with(0, Vec::new(), vec![Rect::new(-18.0, -70.0, 36.0, 70.0)]),
        cmd: None,
        snd: None,
        palettes: Vec::new(),
    }
}

/// A freshly-built (not-yet-seeded) two-fighter match using the RNG-probe
/// character on both sides, positioned apart on a wide stage. Callers seed it.
fn build_match() -> Match {
    let mut p1c = Character::new();
    p1c.pos = Vec2::new(-50.0, 0.0);
    let mut p2c = Character::new();
    p2c.pos = Vec2::new(50.0, 0.0);
    let p1 = Player::new(p1c, rng_probe_loaded());
    let p2 = Player::new(p2c, rng_probe_loaded());
    Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    )
}

/// A deterministic, varied `n`-frame input script (distinct per player) that
/// exercises locomotion, facing, RNG, and button presses, so a run has a
/// non-trivial trajectory.
fn scripted_inputs(n: usize) -> Vec<(MatchInput, MatchInput)> {
    (0..n)
        .map(|i| {
            let p1 = MatchInput {
                right: i % 3 == 0,
                left: i % 8 == 0,
                a: i % 5 == 0,
                x: i % 17 == 0,
                up: i % 23 == 0,
                ..MatchInput::none()
            };
            let p2 = MatchInput {
                left: i % 4 == 0,
                right: i % 9 == 0,
                b: i % 6 == 0,
                y: i % 19 == 0,
                down: i % 13 == 0,
                ..MatchInput::none()
            };
            (p1, p2)
        })
        .collect()
}

/// A *different* deterministic input script, used as the "predicted (wrong)"
/// remote input in the rollback loop — it must diverge from [`scripted_inputs`] so
/// the misprediction actually drives the simulation down a wrong branch (otherwise
/// the rollback assertion would be vacuous).
fn mispredicted_inputs(n: usize) -> Vec<(MatchInput, MatchInput)> {
    (0..n)
        .map(|i| {
            // P1 (local) input is irrelevant to the prediction story, but vary it
            // too. P2 (remote) is deliberately the opposite-ish of the real script.
            let p1 = MatchInput {
                left: i % 2 == 0,
                a: i % 7 == 0,
                ..MatchInput::none()
            };
            let p2 = MatchInput {
                right: i % 2 == 0,
                up: i % 3 == 0,
                b: i % 11 == 0,
                ..MatchInput::none()
            };
            (p1, p2)
        })
        .collect()
}

/// A fixed-width content hash of a match's deterministic snapshot bytes.
fn state_hash(m: &Match) -> u64 {
    let bytes = m.snapshot().expect("snapshot must serialize");
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// The number of intro frames a fresh match runs before the round starts; mirrors
/// `fp_engine`'s internal `INTRO_FRAMES` (60) plus the one tick that transitions
/// into the fight. Kept local so the test depends only on the public API.
const INTRO_LEADIN: usize = 61;

/// Drives a match out of the intro into the fight phase with neutral inputs.
fn into_fight(m: &mut Match) {
    for _ in 0..INTRO_LEADIN {
        m.tick(MatchInput::none(), MatchInput::none());
    }
}

// ---------------------------------------------------------------------------
// AC1 — The GGPO save / predict / rollback / re-advance invariant.
// ---------------------------------------------------------------------------

/// The headline rollback audit (T077 AC1): for a range of rollback depths `K`,
/// save the state, advance `K` frames with **predicted** (wrong) remote inputs,
/// restore the save, then re-advance `K` frames with the **corrected** (real)
/// inputs — and assert the result is byte-for-byte identical to a from-scratch
/// **canonical** run that used the real inputs the whole time.
///
/// This is stronger than a single resume point: it runs the full mispredict →
/// roll back → re-simulate cycle a netcode layer performs every time a remote
/// input arrives late, and it does so at every depth from a 1-frame correction up
/// to a deep `MAX_ROLLBACK_FRAMES` rollback.
#[test]
fn rollback_resim_matches_canonical() {
    /// A generous upper bound on rollback depth (netcode typically tolerates ~7–8
    /// frames; we audit well past that to be safe).
    const MAX_ROLLBACK_FRAMES: usize = 12;
    /// Frame at which the save-state is taken (mid-fight, well into the round).
    const SAVE_AT: usize = 40;

    let real = scripted_inputs(SAVE_AT + MAX_ROLLBACK_FRAMES);
    let wrong = mispredicted_inputs(MAX_ROLLBACK_FRAMES);

    for k in 1..=MAX_ROLLBACK_FRAMES {
        // --- Canonical run: real inputs from frame 0 through SAVE_AT + k. ---
        let mut canonical = build_match();
        canonical.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut canonical);
        for &(p1, p2) in &real[..SAVE_AT + k] {
            canonical.tick(p1, p2);
        }
        let canonical_bytes = canonical.snapshot().expect("canonical snapshot");

        // --- Rollback run: real up to SAVE_AT, save, mispredict k, restore, re-advance k. ---
        let mut rb = build_match();
        rb.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut rb);
        for &(p1, p2) in &real[..SAVE_AT] {
            rb.tick(p1, p2);
        }
        // Save the rollback point.
        let saved = rb.snapshot().expect("rollback save");

        // Predict the remote's next k frames *wrong* and advance.
        for &(p1, p2) in &wrong[..k] {
            rb.tick(p1, p2);
        }
        // The misprediction must have actually moved us off the canonical path,
        // or the rollback below would be a no-op and the test vacuous.
        assert_ne!(
            rb.snapshot().expect("mispredicted snapshot"),
            canonical_bytes,
            "k={k}: mispredicted run must diverge from the canonical run"
        );

        // The corrected input arrives: roll back to the save and re-advance with
        // the REAL inputs for those k frames.
        rb.restore_snapshot(&saved).expect("rollback restore");
        for &(p1, p2) in &real[SAVE_AT..SAVE_AT + k] {
            rb.tick(p1, p2);
        }

        // GGPO invariant: the re-simulated state equals the canonical state byte
        // for byte, at every rollback depth.
        assert_eq!(
            rb.snapshot().expect("re-advanced snapshot"),
            canonical_bytes,
            "k={k}: rollback re-simulation diverged from the canonical run \
             (game_time {})",
            canonical.game_time()
        );
    }
}

/// Rollback must be **repeatable**: a netcode layer rolls back the *same* save
/// state many times as successive remote inputs trickle in. Restoring one saved
/// snapshot over and over, each time advancing the real inputs, must reproduce the
/// canonical state every single time — proving per-tick scratch is cleared on
/// restore and that restore leaves no residue from the discarded mispredictions.
#[test]
fn repeated_rollback_from_one_save_is_stable() {
    const SAVE_AT: usize = 40;
    const K: usize = 8;
    const ROUNDS: usize = 5;

    let real = scripted_inputs(SAVE_AT + K);
    let wrong = mispredicted_inputs(K);

    // Canonical target hash.
    let mut canonical = build_match();
    canonical.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut canonical);
    for &(p1, p2) in &real[..SAVE_AT + K] {
        canonical.tick(p1, p2);
    }
    let target = state_hash(&canonical);

    let mut rb = build_match();
    rb.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut rb);
    for &(p1, p2) in &real[..SAVE_AT] {
        rb.tick(p1, p2);
    }
    let saved = rb.snapshot().expect("rollback save");

    for round in 0..ROUNDS {
        // Each round: mispredict to a different depth, then roll back to the SAME
        // save and re-advance the real inputs. (Vary the wrong guess per round so
        // we are not just replaying one branch.)
        let depth = (round % K) + 1;
        for &(p1, p2) in &wrong[..depth] {
            rb.tick(p1, p2);
        }
        rb.restore_snapshot(&saved).expect("rollback restore");
        for &(p1, p2) in &real[SAVE_AT..SAVE_AT + K] {
            rb.tick(p1, p2);
        }
        assert_eq!(
            state_hash(&rb),
            target,
            "rollback round {round}: re-simulation diverged from canonical"
        );
        // Restore back to the save for the next round.
        rb.restore_snapshot(&saved).expect("re-arm save");
    }
}

// ---------------------------------------------------------------------------
// AC2 — Documented rollback-budget measurement (snapshot size + save/restore cost).
// ---------------------------------------------------------------------------

/// Measures and documents the **rollback budget**: the serialized snapshot size,
/// and the per-frame `snapshot()` (save) / `restore_snapshot()` (restore)
/// wall-time. A rollback netcode layer save-states every frame and restores up to
/// `MAX_ROLLBACK_FRAMES` per remote-input correction, so these costs must fit
/// comfortably inside a 16.67 ms tick.
///
/// This test asserts only loose, machine-independent sanity bounds (a snapshot is
/// non-empty and re-restorable); the actual numbers are printed for the audit
/// record. Run with `--nocapture` to see them:
///
/// ```text
/// cargo test -p fp-engine --test rollback -- --nocapture
/// ```
#[test]
fn rollback_budget_measurement() {
    /// Iterations to average the per-frame save/restore cost over.
    const ITERS: u32 = 2000;

    let mut m = build_match();
    m.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut m);
    // Advance into a busy mid-fight state so the snapshot is representative.
    for &(p1, p2) in &scripted_inputs(80) {
        m.tick(p1, p2);
    }

    // --- Snapshot size. ---
    let blob = m.snapshot().expect("snapshot");
    let size = blob.len();
    assert!(size > 0, "a mid-fight snapshot must be non-empty");

    // --- Save (capture + encode) cost. ---
    let t_save = Instant::now();
    for _ in 0..ITERS {
        let b = m.snapshot().expect("snapshot");
        // Touch the result so the optimizer cannot elide the work.
        std::hint::black_box(&b);
    }
    let save_per = t_save.elapsed() / ITERS;

    // --- Restore (decode + apply) cost. ---
    let t_restore = Instant::now();
    for _ in 0..ITERS {
        m.restore_snapshot(&blob).expect("restore");
    }
    let restore_per = t_restore.elapsed() / ITERS;

    // A restore must leave the match usable and at the saved state.
    let after = m.snapshot().expect("snapshot after restore");
    assert_eq!(after, blob, "restore must round-trip to the saved state");

    // Documented budget — printed for the audit record (visible with --nocapture).
    println!("=== T077 rollback budget (synthetic 2-fighter match) ===");
    println!("  snapshot size:     {size} bytes");
    println!("  save  (capture+enc): {save_per:?} / frame");
    println!("  restore (dec+apply): {restore_per:?} / frame");
    println!(
        "  budget vs 16.67ms tick: save {:.4}%, restore {:.4}%",
        save_per.as_secs_f64() / 0.016_666_667 * 100.0,
        restore_per.as_secs_f64() / 0.016_666_667 * 100.0,
    );

    // Loose, machine-independent upper bound: a single save or restore of this
    // tiny match is far under one whole 16.67 ms tick. (Generous so CI machines
    // under load never flake; the real numbers are orders of magnitude smaller.)
    let tick = std::time::Duration::from_micros(16_666);
    assert!(
        save_per < tick,
        "save cost {save_per:?} should be well under one 16.67ms tick"
    );
    assert!(
        restore_per < tick,
        "restore cost {restore_per:?} should be well under one 16.67ms tick"
    );
}
