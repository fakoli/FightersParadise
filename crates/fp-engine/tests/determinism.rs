//! Synthetic (asset-free) determinism + record/replay regression test (T020).
//!
//! The companion [`tests/kfm_replay.rs`](../kfm_replay.rs) proves the same
//! properties against the genuine Kung Fu Man fixture, but it is **asset-gated**:
//! it runs as a clean no-op whenever the local-only `test-assets/kfm` symlink is
//! absent (CI, fresh checkouts), so on its own it cannot guarantee the
//! determinism contract is actually exercised. This file closes that gap with
//! characters built **entirely from synthetic, in-memory data** (a minimal SFF v1
//! container plus a compiled state graph), so it runs everywhere — CI included —
//! with no external assets.
//!
//! It verifies the three T020 acceptance criteria end to end:
//!
//! 1. **Record captures seed + per-tick inputs; replay reproduces the match.**
//!    A [`MatchRecorder`]-logged run replays into a fresh match built from the
//!    same characters and reproduces the recorded final state byte-for-byte
//!    ([`record_then_replay_reproduces_final_state`]).
//! 2. **Two runs with the same seed + inputs hash identically.** Two independent
//!    matches, seeded with the same match seed and driven by the same input log,
//!    produce an identical final-state **hash**
//!    ([`determinism_same_seed_and_inputs_hash_identically`]).
//! 3. **The harness is documented** — see the module docs on `fp_engine`'s
//!    `replay` module and the worked record→replay example below.
//!
//! # How to record and replay a match
//!
//! ```ignore
//! use fp_engine::{Match, MatchInput, MatchRecorder, MatchState, ReplayLog, replay_match};
//!
//! // 1. Build a match and seed it from a chosen match seed.
//! const SEED: i32 = 0xC0FFEE;
//! let mut game: Match = build_match();
//! game.seed_players(SEED);
//!
//! // 2. Drive the *recorder* instead of the match directly. Each tick is logged.
//! let log: ReplayLog = {
//!     let mut rec = MatchRecorder::new(&mut game, SEED, /* round_seconds */ 99);
//!     loop {
//!         let (p1, p2) = sample_inputs();
//!         rec.tick(p1, p2);
//!         if rec.match_ref().match_state() == MatchState::Over { break; }
//!     }
//!     rec.into_log()
//! };
//!
//! // 3. Persist the log to disk (a compact, versioned bincode blob).
//! let bytes: Vec<u8> = log.encode().expect("encode");
//! // std::fs::write("match.replay", &bytes)?;
//!
//! // 4. Replay: decode the log and feed it into a FRESH match built from the
//! //    SAME two characters. `replay_match` re-seeds from the log and replays
//! //    every recorded input pair; the result reproduces the original exactly.
//! let log = ReplayLog::decode(&bytes).expect("decode");
//! let mut fresh: Match = build_match();
//! replay_match(&mut fresh, &log).expect("replay");
//! assert_eq!(fresh.snapshot().unwrap(), game.snapshot().unwrap());
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use fp_character::{
    Character, CharacterConstants, CompiledController, CompiledExpr, CompiledParam, CompiledState,
    CompiledTriggerGroup, LoadedCharacter,
};
use fp_core::{Rect, SpriteId, Vec2};
use fp_engine::{
    replay_match, Match, MatchInput, MatchRecorder, Player, ReplayLog, StageBounds,
    DEFAULT_MATCH_SEED,
};
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
///
/// Mirrors the in-crate `empty_sff` test helper, but lives here so this external
/// integration test depends only on the public `fp-formats` API.
fn empty_sff() -> SffFile {
    // SFF v1 layout:
    //   [0..12)   "ElecbyteSpr\0" signature
    //   [15]      major version = 1
    //   [16..20)  num_groups   = 1
    //   [20..24)  num_images   = 1
    //   [24..28)  first_subheader_offset = 64
    //   [64..96)  one 32-byte sprite sub-header (next_offset = 0, data_length = 0)
    const SUBHEADER_OFFSET: usize = 64;
    let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[15] = 1; // SFF v1
    buf[16..20].copy_from_slice(&1u32.to_le_bytes());
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
    // Sub-header stays zeroed: next_offset = 0 terminates the walk, data_length =
    // 0 marks it as a linked (no-PCX) sprite.
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
/// fighters remain hittable). Built only from public APIs and in-memory data —
/// it never touches disk.
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
    Match::new(p1, p2, StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH))
}

/// A deterministic, varied `n`-frame input script (distinct per player) that
/// exercises locomotion, facing, RNG, and button presses, so the replay has a
/// non-trivial trajectory to reproduce.
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

/// A fixed-width content hash of a match's final, deterministic snapshot — the
/// "final state hash" the determinism criterion compares between two runs.
///
/// The snapshot bytes are themselves the canonical, byte-stable state (a
/// re-serialize yields identical bytes); hashing them with the standard library's
/// `DefaultHasher` collapses them to a single `u64` so two runs can be compared by
/// hash, exactly as the acceptance criterion is phrased.
fn final_state_hash(m: &Match) -> u64 {
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
// AC2 — Two runs with the same seed + inputs produce identical final-state hashes.
// ---------------------------------------------------------------------------

/// The headline determinism regression test (T020 AC2): two independent matches
/// built from the same characters, seeded with the **same** match seed and driven
/// by the **same** per-tick input log, end in a state whose hash is identical.
#[test]
fn determinism_same_seed_and_inputs_hash_identically() {
    const SEED: i32 = 0x5EED_2020u32 as i32;
    let script = scripted_inputs(300);

    let run = |seed: i32| -> u64 {
        let mut m = build_match();
        m.seed_players(seed);
        into_fight(&mut m);
        for &(p1, p2) in &script {
            m.tick(p1, p2);
        }
        final_state_hash(&m)
    };

    let hash_a = run(SEED);
    let hash_b = run(SEED);
    assert_eq!(
        hash_a, hash_b,
        "same seed + same inputs must produce an identical final-state hash"
    );

    // The seed is load-bearing: a different match seed yields a different run, so
    // the equality above is not a vacuous "everything hashes the same" pass.
    let hash_other = run(SEED.wrapping_add(1));
    assert_ne!(
        hash_a, hash_other,
        "a different match seed must change the final-state hash"
    );
}

/// A stronger form of AC2: not just equal *final* hashes, but byte-identical
/// state at **every** frame. Any nondeterminism source (e.g. HashMap iteration
/// order leaking into the simulation) would diverge the two runs immediately.
#[test]
fn determinism_two_runs_are_byte_equal_every_frame() {
    const SEED: i32 = 1234;
    let script = scripted_inputs(300);

    let mut a = build_match();
    let mut b = build_match();
    a.seed_players(SEED);
    b.seed_players(SEED);

    for (frame, &(p1, p2)) in script.iter().enumerate() {
        a.tick(p1, p2);
        b.tick(p1, p2);
        assert_eq!(
            a.snapshot().expect("snapshot a"),
            b.snapshot().expect("snapshot b"),
            "two identical runs diverged at frame {frame} (game_time {})",
            a.game_time()
        );
    }
}

// ---------------------------------------------------------------------------
// AC1 — Recording captures seed + per-tick inputs; replaying reproduces it.
// ---------------------------------------------------------------------------

/// T020 AC1: recording a live match captures its seed + per-tick inputs, and
/// replaying that log into a fresh match reproduces the recorded final state.
#[test]
fn record_then_replay_reproduces_final_state() {
    const SEED: i32 = 4242;
    let script = scripted_inputs(250);

    // Record: seed, then drive the recorder so each tick is both applied and
    // logged. (The recorder begins at frame 0 — before any intro lead-in — so the
    // log is a complete from-scratch input history.)
    let mut original = build_match();
    original.seed_players(SEED);
    let log = {
        // 99 is the engine's default round length (`DEFAULT_ROUND_SECONDS`),
        // stamped into the log metadata; it matches `build_match`'s default round.
        let mut rec = MatchRecorder::new(&mut original, SEED, 99);
        for &(p1, p2) in &script {
            rec.tick(p1, p2);
        }
        rec.into_log()
    };
    let original_end = original.snapshot().expect("snapshot original");

    // The log captured the seed and exactly the frames we fed it.
    assert_eq!(log.match_seed, SEED, "log records the match seed");
    assert_eq!(log.len(), script.len(), "log records every per-tick input");

    // Replay into a fresh match built from the SAME characters: `replay_match`
    // re-seeds from the log and feeds every recorded input pair.
    let mut fresh = build_match();
    replay_match(&mut fresh, &log).expect("replay must succeed");
    let replay_end = fresh.snapshot().expect("snapshot replay");

    assert_eq!(
        original_end, replay_end,
        "replaying the recorded log must reproduce the match byte-for-byte"
    );
    assert_eq!(
        final_state_hash(&original),
        final_state_hash(&fresh),
        "record -> replay must reproduce an identical final-state hash"
    );
}

/// The recorded log persists losslessly through its bincode codec, and replaying
/// the *reloaded* log reproduces the same state — proving a replay can be saved to
/// disk and reproduced later.
#[test]
fn replay_log_persists_and_reloads_via_bincode() {
    const SEED: i32 = 99;
    let mut original = build_match();
    original.seed_players(SEED);
    let log = {
        let mut rec = MatchRecorder::new(&mut original, SEED, 99);
        for &(p1, p2) in &scripted_inputs(120) {
            rec.tick(p1, p2);
        }
        rec.into_log()
    };

    // Encode -> decode round-trips the log losslessly.
    let bytes = log.encode().expect("encode replay log");
    let reloaded = ReplayLog::decode(&bytes).expect("decode replay log");
    assert_eq!(log, reloaded, "replay log survives a bincode round-trip");

    // Replaying the reloaded log reproduces the same final state as the original.
    let mut fresh = build_match();
    replay_match(&mut fresh, &reloaded).expect("replay reloaded log");
    assert_eq!(
        original.snapshot().expect("snapshot original"),
        fresh.snapshot().expect("snapshot replay"),
        "a reloaded replay log reproduces the recorded final state"
    );
}

// ---------------------------------------------------------------------------
// Save-state / rollback: snapshot at frame N, restore, continue == straight run.
// ---------------------------------------------------------------------------

/// A snapshot taken mid-match is a perfect resume point: restoring at frame N and
/// ticking M more frames yields the SAME final state hash as a straight-through
/// run of N+M frames. This is the rollback primitive replay/netplay build on.
#[test]
fn snapshot_restore_is_a_perfect_resume_point() {
    let script = scripted_inputs(120);

    // Straight-through reference run.
    let mut reference = build_match();
    reference.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut reference);
    for &(p1, p2) in &script {
        reference.tick(p1, p2);
    }
    let reference_hash = final_state_hash(&reference);

    // Save-state run: snapshot at frame 60, diverge, restore, then continue.
    let mut resumed = build_match();
    resumed.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut resumed);
    for &(p1, p2) in &script[..60] {
        resumed.tick(p1, p2);
    }
    let saved = resumed.snapshot().expect("snapshot save-state");
    // Drive it down a wrong branch...
    for &(p1, p2) in &script[..30] {
        resumed.tick(p1, p2);
    }
    // ...then restore to the save-state and continue the real script.
    resumed.restore_snapshot(&saved).expect("restore save-state");
    for &(p1, p2) in &script[60..] {
        resumed.tick(p1, p2);
    }

    assert_eq!(
        reference_hash,
        final_state_hash(&resumed),
        "snapshot/restore-and-continue must match a straight-through run"
    );
}
