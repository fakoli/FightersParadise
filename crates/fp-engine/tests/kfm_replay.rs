//! Gated real-KFM determinism + record/replay integration test (#38).
//!
//! Loads the genuine Kung Fu Man fixture (CC BY-NC 3.0, Elecbyte) twice and
//! proves that the full real-content match is deterministic and replayable:
//!
//! - **Two-run determinism:** two matches built from the same KFM `.def`, seeded
//!   with the same match seed and driven by the same scripted input, produce
//!   byte-identical [`Match::snapshot`] blobs at every frame.
//! - **Record → replay:** a [`MatchRecorder`]-logged run replays into a fresh
//!   KFM match and reproduces the exact final state.
//! - **Snapshot / restore round-trip:** saving and restoring a real-content match
//!   returns it to the snapshot point byte-for-byte.
//!
//! The whole test **skips cleanly** when `test-assets/kfm/kfm.def` is absent (the
//! fixture is local-only behind the gitignored `test-assets` symlink and is never
//! committed), exactly like the other asset-gated tests in the workspace.

use std::path::PathBuf;

use fp_character::{Character, LoadedCharacter};
use fp_core::Vec2;
use fp_engine::{
    replay_match, Match, MatchInput, MatchRecorder, Player, ReplayLog, StageBounds,
    DEFAULT_MATCH_SEED,
};

/// MUGEN action 0 == the standing pose / idle state.
const STATE_STAND: i32 = 0;
/// A symmetric stage half-width comfortably wider than two KFM bodies.
const STAGE_HALF_WIDTH: f32 = 320.0;
/// The match seed both runs share (fixed for reproducibility).
const MATCH_SEED: i32 = 0x5EED_2026u32 as i32;

/// Resolves a path under the workspace `test-assets/` directory, relative to this
/// crate's manifest (`crates/fp-engine`).
fn test_asset(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets")
        .join(rel)
}

/// Loads KFM and builds a [`Player`] positioned at `start_x`, mirroring
/// `fp-app`'s `build_player`. Returns `None` (after a skip note) if the fixture is
/// absent or fails to load, so a caller can skip cleanly.
fn build_kfm_player(start_x: f32) -> Option<Player> {
    let def = test_asset("kfm/kfm.def");
    if !def.exists() {
        eprintln!("skipping KFM replay test: {} not present", def.display());
        return None;
    }
    let loaded = match LoadedCharacter::load(&def) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipping KFM replay test: kfm.def failed to load: {e}");
            return None;
        }
    };
    let mut entity = Character::with_constants(loaded.constants);
    entity.pos = Vec2::new(start_x, 0.0);
    entity.state_no = STATE_STAND;
    entity.ctrl = true;
    entity.anim = STATE_STAND;
    Some(Player::new(entity, loaded))
}

/// Builds a freshly-loaded KFM-vs-KFM match (not yet seeded), or `None` to skip.
fn build_kfm_match() -> Option<Match> {
    let p1 = build_kfm_player(-80.0)?;
    let p2 = build_kfm_player(80.0)?;
    Some(Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    ))
}

/// A deterministic, varied 250-frame input script (distinct per player) that
/// drives the fighters out of the intro, walks them around, and presses attack
/// buttons — enough to exercise locomotion, facing, RNG, and combat.
fn kfm_script(n: usize) -> Vec<(MatchInput, MatchInput)> {
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

#[test]
fn kfm_two_runs_are_byte_equal_every_frame() {
    let (Some(mut a), Some(mut b)) = (build_kfm_match(), build_kfm_match()) else {
        return; // fixture absent -> skip cleanly
    };
    a.seed_players(MATCH_SEED);
    b.seed_players(MATCH_SEED);

    for (frame, &(p1, p2)) in kfm_script(250).iter().enumerate() {
        a.tick(p1, p2);
        b.tick(p1, p2);
        // The strongest determinism proof: real-content state matches byte-for-byte
        // every single frame. Any nondeterminism (e.g. HashMap iteration order
        // leaking into the simulation) would diverge these snapshots immediately.
        let sa = a.snapshot().expect("snapshot a");
        let sb = b.snapshot().expect("snapshot b");
        assert_eq!(
            sa,
            sb,
            "real KFM diverged at frame {frame} (game_time {})",
            a.game_time()
        );
    }
}

#[test]
fn kfm_record_then_replay_reproduces_final_state() {
    let Some(mut original) = build_kfm_match() else {
        return; // skip
    };
    original.seed_players(MATCH_SEED);

    // Record every frame through the recorder.
    let script = kfm_script(250);
    let log = {
        let mut rec = MatchRecorder::new(&mut original, MATCH_SEED, 99);
        for &(p1, p2) in &script {
            rec.tick(p1, p2);
        }
        rec.into_log()
    };
    let original_end = original.snapshot().expect("snapshot original");
    assert_eq!(log.len(), script.len(), "log records every frame");

    // Replay into a fresh KFM match -> identical final state.
    let Some(mut fresh) = build_kfm_match() else {
        return;
    };
    replay_match(&mut fresh, &log).expect("replay must succeed");
    let replay_end = fresh.snapshot().expect("snapshot replay");
    assert_eq!(
        original_end, replay_end,
        "replaying the recorded log must reproduce the real-KFM match byte-for-byte"
    );

    // The log also persists losslessly through bincode.
    let bytes = log.encode().expect("encode log");
    let reloaded = ReplayLog::decode(&bytes).expect("decode log");
    assert_eq!(log, reloaded, "replay log survives a bincode round-trip");
}

#[test]
fn kfm_snapshot_restore_round_trips() {
    let Some(mut m) = build_kfm_match() else {
        return; // skip
    };
    m.seed_players(DEFAULT_MATCH_SEED);

    let script = kfm_script(150);
    for &(p1, p2) in &script[..100] {
        m.tick(p1, p2);
    }
    let saved = m.snapshot().expect("snapshot");

    // Diverge, then restore -> back to the snapshot point exactly.
    for &(p1, p2) in &script[100..] {
        m.tick(p1, p2);
    }
    m.restore_snapshot(&saved).expect("restore");
    let restored = m.snapshot().expect("snapshot");
    assert_eq!(
        saved, restored,
        "real-KFM snapshot/restore must round-trip byte-for-byte"
    );
}
