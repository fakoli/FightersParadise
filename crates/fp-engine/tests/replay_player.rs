//! Replay-study transport integration tests (T076).
//!
//! Exercises [`fp_engine::ReplayPlayer`] — the VCR-style transport over a
//! [`fp_engine::ReplayLog`] backing the replay-study UI: play, pause, step ±1
//! frame, and seek (scrub) to an arbitrary frame, all via restore-and-re-sim from
//! cached keyframes (the engine tick is forward-only + deterministic).
//!
//! The headline invariant (and the task's verification command) is **seek
//! determinism**: `seek(f)` then `seek(f)` again yields *identical* match
//! snapshots, and a seek lands in the same state regardless of the path the
//! playhead took to get there. That is what makes the scrub bar trustworthy and
//! is proved here against a recorded match's byte-equal snapshots.
//!
//! Fighters are built **entirely from synthetic, in-memory data** (mirroring
//! `determinism.rs`), so the test never touches disk and runs on CI.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use fp_character::{
    Character, CharacterConstants, CompiledController, CompiledExpr, CompiledParam, CompiledState,
    CompiledTriggerGroup, LoadedCharacter,
};
use fp_core::{Rect, SpriteId, Vec2};
use fp_engine::Match;
use fp_engine::{
    MatchInput, MatchRecorder, Player, ReplayLog, ReplayPlayer, StageBounds, DEFAULT_MATCH_SEED,
};
use fp_formats::air::{AirFile, AnimAction, AnimFrame};
use fp_formats::sff::SffFile;

const STAGE_HALF_WIDTH: f32 = 200.0;

/// Minimal in-memory SFF v1 with a single linked (data-less) sprite — enough for a
/// headless [`LoadedCharacter`]; pixels are never read in simulation.
fn empty_sff() -> SffFile {
    const SUBHEADER_OFFSET: usize = 64;
    let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[15] = 1;
    buf[16..20].copy_from_slice(&1u32.to_le_bytes());
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
    SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
}

/// An AIR file with a single action carrying one frame + attack/hurt boxes.
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

/// A `VarSet` controller firing unconditionally that assigns `expr` into `var(idx)`.
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

/// A synthetic character that draws `random` into `var(0)` every tick (so the RNG
/// stream flows and any nondeterminism would surface) and stays hittable.
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

/// A freshly-built (un-seeded) two-fighter match using the RNG-probe character.
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

/// A deterministic, varied input script (distinct per player) so the replay has a
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

/// Records a real match driven by `script` into a [`ReplayLog`] (stamped with the
/// players' fingerprints), so it round-trips through the viewer's identity guard.
fn record_log(script: &[(MatchInput, MatchInput)]) -> ReplayLog {
    let mut game = build_match();
    game.seed_players(DEFAULT_MATCH_SEED);
    let round_seconds = 99;
    // Re-seed via the recorder: `seed_players` was applied above so the recorded
    // seed reproduces it. The recorder does not re-seed, matching `replay_match`.
    let mut rec = MatchRecorder::new(&mut game, DEFAULT_MATCH_SEED, round_seconds);
    for &(p1, p2) in script {
        rec.tick(p1, p2);
    }
    rec.into_log()
}

/// A content hash of a player's current live-match snapshot — collapses the
/// canonical, byte-stable snapshot bytes to a single `u64` for comparison.
fn frame_hash(p: &ReplayPlayer<'_>) -> u64 {
    let bytes = p.match_ref().snapshot().expect("snapshot serializes");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Headline invariant: seek(f) then seek(f) again yields identical snapshots.
// ---------------------------------------------------------------------------

#[test]
fn seek_is_idempotent_identical_snapshots() {
    let script = scripted_inputs(200);
    let log = record_log(&script);

    let mut game = build_match();
    let mut player = ReplayPlayer::new(&mut game, log).expect("same characters");

    for &f in &[0u32, 1, 7, 50, 137, 200, 199, 30, 60, 61] {
        player.seek(f);
        let bytes_a = player.match_ref().snapshot().expect("snapshot");
        // Re-seek to the SAME frame; state must be byte-identical.
        player.seek(f);
        let bytes_b = player.match_ref().snapshot().expect("snapshot");
        assert_eq!(
            bytes_a, bytes_b,
            "seek({f}) then seek({f}) again must yield byte-identical snapshots"
        );
        assert_eq!(player.current_frame(), f.min(player.len()));
    }
}

#[test]
fn seek_is_path_independent() {
    // Landing on frame f from above (rewind) vs. below (fast-forward) must be the
    // same state — the property that makes the scrub bar trustworthy.
    let script = scripted_inputs(150);
    let log = record_log(&script);

    const TARGET: u32 = 90;

    let from_below = {
        let mut g = build_match();
        let mut p = ReplayPlayer::new(&mut g, log.clone()).expect("chars");
        p.seek(10);
        p.seek(TARGET); // fast-forward up to TARGET
        frame_hash(&p)
    };
    let from_above = {
        let mut g = build_match();
        let mut p = ReplayPlayer::new(&mut g, log.clone()).expect("chars");
        p.seek(149);
        p.seek(TARGET); // rewind down to TARGET
        frame_hash(&p)
    };
    let from_scratch = {
        let mut g = build_match();
        let mut p = ReplayPlayer::new(&mut g, log.clone()).expect("chars");
        p.seek(TARGET);
        frame_hash(&p)
    };
    assert_eq!(from_below, from_scratch, "fast-forward seek diverged");
    assert_eq!(from_above, from_scratch, "rewind seek diverged");
}

#[test]
fn seek_matches_plain_forward_resim() {
    // The viewer's restore-and-re-sim seek must match a from-scratch tick loop —
    // i.e. the keyframe machinery does not change the simulation.
    let script = scripted_inputs(120);
    let log = record_log(&script);

    for &target in &[0u32, 33, 77, 120] {
        // Canonical: a fresh match seeded + ticked `target` times by hand.
        let canonical = {
            let mut g = build_match();
            g.seed_players(DEFAULT_MATCH_SEED);
            for &(p1, p2) in script.iter().take(target as usize) {
                g.tick(p1, p2);
            }
            g.snapshot().expect("snapshot")
        };
        // Viewer seek to the same frame.
        let viewed = {
            let mut g = build_match();
            let mut p = ReplayPlayer::new(&mut g, log.clone()).expect("chars");
            p.seek(target);
            p.match_ref().snapshot().expect("snapshot")
        };
        assert_eq!(
            canonical, viewed,
            "seek({target}) must equal a from-scratch re-sim of {target} frames"
        );
    }
}

// ---------------------------------------------------------------------------
// Transport: play / pause / step ±1.
// ---------------------------------------------------------------------------

#[test]
fn step_forward_then_back_returns_to_same_state() {
    let script = scripted_inputs(80);
    let log = record_log(&script);

    let mut g = build_match();
    let mut p = ReplayPlayer::new(&mut g, log).expect("chars");
    p.seek(40);
    let at_40 = frame_hash(&p);

    p.step_forward();
    assert_eq!(p.current_frame(), 41);
    p.step_back();
    assert_eq!(p.current_frame(), 40);
    assert_eq!(frame_hash(&p), at_40, "step +1 then -1 must restore state");
}

#[test]
fn step_back_at_zero_is_noop() {
    let log = record_log(&scripted_inputs(10));
    let mut g = build_match();
    let mut p = ReplayPlayer::new(&mut g, log).expect("chars");
    assert_eq!(p.current_frame(), 0);
    assert!(!p.step_back(), "step_back at frame 0 reports no movement");
    assert_eq!(p.current_frame(), 0);
}

#[test]
fn advance_plays_through_and_auto_pauses_at_end() {
    let n = 25usize;
    let log = record_log(&scripted_inputs(n));
    let mut g = build_match();
    let mut p = ReplayPlayer::new(&mut g, log).expect("chars");
    p.play();
    assert!(p.is_playing());

    let mut consumed = 0u32;
    while p.advance() {
        consumed += 1;
        assert!(consumed <= n as u32 + 1, "advance ran past the log");
    }
    assert_eq!(consumed, n as u32, "advance consumed every recorded frame");
    assert!(p.at_end());
    assert!(
        !p.is_playing(),
        "reaching the end auto-pauses the transport"
    );
    assert_eq!(p.current_frame(), n as u32);
}

#[test]
fn toggle_play_flips_state() {
    let log = record_log(&scripted_inputs(5));
    let mut g = build_match();
    let mut p = ReplayPlayer::new(&mut g, log).expect("chars");
    assert!(!p.is_playing());
    assert!(p.toggle_play());
    assert!(p.is_playing());
    assert!(!p.toggle_play());
    assert!(!p.is_playing());
}

#[test]
fn seek_clamps_out_of_range_targets() {
    let log = record_log(&scripted_inputs(30));
    let mut g = build_match();
    let mut p = ReplayPlayer::new(&mut g, log).expect("chars");
    assert_eq!(p.seek(9999), 30, "seek past the end clamps to len");
    assert_eq!(p.current_frame(), 30);
    assert!(p.at_end());
}

// ---------------------------------------------------------------------------
// Identity guard + keyframe cadence.
// ---------------------------------------------------------------------------

#[test]
fn new_rejects_mismatched_characters() {
    // A log stamped with one character pair, opened on a match whose fingerprints
    // differ, must be rejected (CharacterMismatch), not silently mis-replayed.
    let script = scripted_inputs(5);
    let mut real = ReplayLog::default();
    // Default log is UNSTAMPED → guard skipped; stamp a bogus P1 fingerprint by
    // recording from a real match then mutating the stored fingerprint.
    let recorded = record_log(&script);
    real.match_seed = recorded.match_seed;
    real.bounds = recorded.bounds;
    real.inputs = recorded.inputs.clone();
    real.p1_fingerprint = fp_character::CharacterFingerprint(0xDEAD_BEEF);
    real.p2_fingerprint = recorded.p2_fingerprint;

    let mut g = build_match();
    let err = ReplayPlayer::new(&mut g, real);
    assert!(err.is_err(), "mismatched P1 fingerprint must be rejected");
}

#[test]
fn keyframe_interval_does_not_change_results() {
    // Tiny vs. large keyframe intervals must produce identical seek results — the
    // cadence is purely a perf knob.
    let script = scripted_inputs(100);
    let log = record_log(&script);

    let with_interval = |interval: u32, target: u32| -> u64 {
        let mut g = build_match();
        let mut p =
            ReplayPlayer::with_keyframe_interval(&mut g, log.clone(), interval).expect("chars");
        p.seek(target);
        frame_hash(&p)
    };
    for &target in &[0u32, 17, 63, 100] {
        assert_eq!(
            with_interval(1, target),
            with_interval(50, target),
            "keyframe interval changed the seek({target}) result"
        );
    }
}
