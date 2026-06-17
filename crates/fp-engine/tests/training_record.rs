//! Training-mode setup record / playback integration test (T068).
//!
//! Built on the same synthetic, asset-free harness as [`tests/determinism.rs`] (a
//! minimal in-memory SFF v1 + a compiled state graph that draws `random` every
//! tick, so the live RNG actually flows and any nondeterminism would surface), so
//! it runs everywhere — CI included — with no external assets.
//!
//! It verifies the three T068 acceptance criteria end to end:
//!
//! 1. **Record captures the dummy's per-frame inputs from a start position until
//!    Stop; Playback replays exactly those inputs, looping from the start.**
//!    ([`record_then_playback_reproduces_dummy_motion`])
//! 2. **Playback is deterministic — the dummy reproduces the identical motion every
//!    loop.** Two full playback loops, with the *same* live player input each loop,
//!    end in byte-identical [`Match::snapshot`] state.
//!    ([`playback_is_deterministic_every_loop`])
//! 3. **Reset re-seats both fighters to the start snapshot each loop.**
//!    ([`reset_reseats_both_fighters_to_start`])

use std::collections::HashMap;

use fp_character::{
    Character, CharacterConstants, CompiledController, CompiledExpr, CompiledParam, CompiledState,
    CompiledTriggerGroup, LoadedCharacter,
};
use fp_core::{Rect, SpriteId, Vec2};
use fp_engine::{
    Match, MatchInput, Player, Side, StageBounds, TrainingPlayback, TrainingRecorder,
    TrainingRecording, DEFAULT_MATCH_SEED, RECORDING_FORMAT_VERSION,
};
use fp_formats::air::{AirFile, AnimAction, AnimFrame};
use fp_formats::sff::SffFile;

/// A symmetric stage half-width, wide enough the two bodies never reach an edge.
const STAGE_HALF_WIDTH: f32 = 200.0;

/// A minimal valid SFF v1 container carrying a single linked (data-less) sprite,
/// so a headless [`LoadedCharacter`] can be built without any asset on disk.
/// Mirrors `tests/determinism.rs`'s `empty_sff`.
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

/// An AIR file with a single action carrying one frame with the given boxes.
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

/// A synthetic character whose state 0 draws `random` into `var(0)` every tick (so
/// the RNG stream flows and nondeterminism would surface) and carries a hurt frame.
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

/// A freshly-built (not-yet-seeded) two-fighter match. Callers seed it.
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

/// The intro lead-in (60 intro frames + the transition tick), mirroring the
/// engine's internal `INTRO_FRAMES`. Kept local to depend only on the public API.
const INTRO_LEADIN: usize = 61;

/// Drives a match out of intro into the fight phase with neutral inputs.
fn into_fight(m: &mut Match) {
    for _ in 0..INTRO_LEADIN {
        m.tick(MatchInput::none(), MatchInput::none());
    }
}

/// A deterministic, varied dummy input script (the "setup" being recorded).
fn dummy_script(n: usize) -> Vec<MatchInput> {
    (0..n)
        .map(|i| MatchInput {
            right: i % 3 == 0,
            left: i % 7 == 0,
            a: i % 5 == 0,
            x: i % 11 == 0,
            up: i % 13 == 0,
            ..MatchInput::none()
        })
        .collect()
}

/// A deterministic live-player input script (the rehearsing player's response).
fn player_script(n: usize) -> Vec<MatchInput> {
    (0..n)
        .map(|i| MatchInput {
            left: i % 4 == 0,
            b: i % 6 == 0,
            down: i % 9 == 0,
            ..MatchInput::none()
        })
        .collect()
}

/// Records a setup: position the match, then drive the recorder feeding the dummy
/// (P2) script and a neutral player (P1) so the captured log is the dummy's only.
fn record_setup(dummy: &[MatchInput]) -> TrainingRecording {
    let mut game = build_match();
    game.seed_players(DEFAULT_MATCH_SEED);
    into_fight(&mut game);
    let mut rec = TrainingRecorder::new(&mut game, Side::P2).expect("start snapshot");
    for &d in dummy {
        // Player side stays neutral while authoring the dummy's setup.
        rec.tick(MatchInput::none(), d);
    }
    assert_eq!(rec.len(), dummy.len(), "recorder logs every dummy frame");
    rec.into_recording()
}

// ---------------------------------------------------------------------------
// AC1 — Record captures the dummy's inputs; playback replays exactly those.
// ---------------------------------------------------------------------------

/// T068 AC1: a recorded dummy setup, replayed via [`TrainingPlayback`] with the
/// player side held neutral, reproduces the *exact* state a straight run that fed
/// the dummy script directly produces — i.e. playback drives the dummy by replaying
/// exactly the recorded inputs.
#[test]
fn record_then_playback_reproduces_dummy_motion() {
    let dummy = dummy_script(90);
    let recording = record_setup(&dummy);

    assert_eq!(recording.format_version, RECORDING_FORMAT_VERSION);
    assert_eq!(recording.dummy_side, Side::P2);
    assert_eq!(
        recording.len(),
        dummy.len(),
        "recording carries every frame"
    );

    // Reference: a fresh match restored to the recording's start, then ticked with
    // the dummy script on P2 and neutral on P1 — the motion playback must match.
    let mut reference = build_match();
    reference
        .restore_snapshot(&recording.start)
        .expect("restore start");
    for &d in &dummy {
        reference.tick(MatchInput::none(), d);
    }
    let reference_end = reference.snapshot().expect("snapshot reference");

    // Playback: restore start (done in `new`) and tick with neutral player input.
    let mut game = build_match();
    let mut pb = TrainingPlayback::new(&mut game, recording).expect("playback start");
    for _ in 0..dummy.len() {
        pb.tick(MatchInput::none()).expect("playback tick");
    }
    let playback_end = pb.match_ref().snapshot().expect("snapshot playback");

    assert_eq!(
        reference_end, playback_end,
        "playback must replay exactly the recorded dummy inputs"
    );
}

// ---------------------------------------------------------------------------
// AC2 — Playback is deterministic: the dummy reproduces identical motion each loop.
// ---------------------------------------------------------------------------

/// T068 AC2: looping playback is deterministic. Two consecutive loops, fed the
/// *same* live player input each loop, end in byte-identical match state — the
/// dummy reproduces the identical motion every loop.
#[test]
fn playback_is_deterministic_every_loop() {
    let dummy = dummy_script(75);
    let player = player_script(dummy.len());
    let recording = record_setup(&dummy);

    let mut game = build_match();
    let mut pb = TrainingPlayback::new(&mut game, recording).expect("playback start");

    // Loop 1.
    for &p in &player {
        pb.tick(p).expect("loop-1 tick");
    }
    assert!(
        pb.at_loop_boundary(),
        "cursor is at the loop boundary after a full pass"
    );
    let loop1_end = pb.match_ref().snapshot().expect("snapshot loop 1");

    // Loop 2 — the first tick wraps (re-seats the start) then replays from frame 0,
    // fed the SAME player inputs as loop 1.
    for &p in &player {
        pb.tick(p).expect("loop-2 tick");
    }
    let loop2_end = pb.match_ref().snapshot().expect("snapshot loop 2");

    assert_eq!(
        loop1_end, loop2_end,
        "every loop must reproduce byte-identical state (deterministic dummy)"
    );
}

/// The recording persists losslessly through its bincode codec, and a playback of
/// the *reloaded* recording reproduces the same motion — a setup can be saved to
/// disk and reloaded later.
#[test]
fn recording_persists_and_reloads_via_bincode() {
    let dummy = dummy_script(60);
    let recording = record_setup(&dummy);

    let bytes = recording.encode().expect("encode recording");
    let reloaded = TrainingRecording::decode(&bytes).expect("decode recording");
    assert_eq!(
        recording, reloaded,
        "recording survives a bincode round-trip"
    );

    let mut a = build_match();
    let mut pb_a = TrainingPlayback::new(&mut a, recording).expect("playback a");
    let mut b = build_match();
    let mut pb_b = TrainingPlayback::new(&mut b, reloaded).expect("playback b");
    for _ in 0..dummy.len() {
        pb_a.tick(MatchInput::none()).expect("a tick");
        pb_b.tick(MatchInput::none()).expect("b tick");
    }
    assert_eq!(
        pb_a.match_ref().snapshot().expect("snap a"),
        pb_b.match_ref().snapshot().expect("snap b"),
        "a reloaded recording reproduces the recorded motion"
    );
}

/// A malformed recording blob is a recoverable error, never a panic, and an
/// unknown format version is rejected.
#[test]
fn decode_rejects_bad_input() {
    let recording = record_setup(&dummy_script(10));
    let mut bytes = recording.encode().expect("encode");
    bytes.truncate(bytes.len() / 2);
    assert!(
        TrainingRecording::decode(&bytes).is_err(),
        "truncated blob is a recoverable error"
    );

    let mut bad_version = recording;
    bad_version.format_version = 999;
    let bytes = bad_version.encode().expect("encode");
    assert!(
        TrainingRecording::decode(&bytes).is_err(),
        "unknown format version is rejected"
    );
}

// ---------------------------------------------------------------------------
// AC3 — Reset re-seats both fighters to the start snapshot each loop.
// ---------------------------------------------------------------------------

/// T068 AC3: `reset` (and the automatic loop wrap) re-seats *both* fighters to the
/// recording's start snapshot. After driving playback well into a loop, an explicit
/// reset returns the match to byte-identical start state.
#[test]
fn reset_reseats_both_fighters_to_start() {
    let dummy = dummy_script(80);
    let recording = record_setup(&dummy);
    let start_state = recording.start.clone();

    let mut game = build_match();
    let mut pb = TrainingPlayback::new(&mut game, recording).expect("playback start");

    // The match begins exactly at the start snapshot.
    assert_eq!(
        pb.match_ref().snapshot().expect("snap initial"),
        start_state,
        "playback begins at the recorded start"
    );

    // Drive halfway through the loop (both fighters move), then reset.
    for _ in 0..(dummy.len() / 2) {
        pb.tick(MatchInput::none()).expect("mid tick");
    }
    assert_ne!(
        pb.match_ref().snapshot().expect("snap mid"),
        start_state,
        "state has advanced mid-loop"
    );

    pb.reset().expect("reset");
    assert_eq!(pb.cursor(), 0, "reset rewinds the cursor to frame 0");
    assert_eq!(
        pb.match_ref().snapshot().expect("snap after reset"),
        start_state,
        "reset re-seats both fighters to the recorded start byte-for-byte"
    );
}

/// The automatic loop wrap re-seats the start: after a full pass plus one more
/// tick, the match is back at (then one frame past) the start, not continuing on.
#[test]
fn loop_wrap_reseats_start_automatically() {
    let dummy = dummy_script(40);
    let recording = record_setup(&dummy);
    let start_state = recording.start.clone();

    // The state one frame into a fresh playback (cursor 0 consumed).
    let frame1_state = {
        let mut g = build_match();
        let mut pb = TrainingPlayback::new(&mut g, recording.clone()).expect("probe playback");
        pb.tick(MatchInput::none()).expect("probe tick");
        pb.match_ref().snapshot().expect("probe snap")
    };

    let mut game = build_match();
    let mut pb = TrainingPlayback::new(&mut game, recording).expect("playback");
    // Consume the whole recording.
    for _ in 0..dummy.len() {
        pb.tick(MatchInput::none()).expect("loop-1 tick");
    }
    assert!(pb.at_loop_boundary(), "cursor reached the boundary");

    // One more tick wraps: it re-seats the start, then replays frame 0 — so the
    // resulting state equals the start advanced by exactly one recorded frame.
    pb.tick(MatchInput::none()).expect("wrap tick");
    assert_eq!(
        pb.cursor(),
        1,
        "wrap restarts the cursor and consumes frame 0"
    );
    assert_eq!(
        pb.match_ref().snapshot().expect("snap after wrap"),
        frame1_state,
        "the loop wrapped from the start, not continued past the end"
    );
    // Sanity: the wrapped state is not the raw start (a frame was applied).
    assert_ne!(
        pb.match_ref().snapshot().expect("snap"),
        start_state,
        "the wrap applied frame 0 after re-seating"
    );
}
