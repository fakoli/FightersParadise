//! Conformance test for the shipped, original Training Dummy character.
//!
//! Unlike the real-KFM regression tests (which are asset-gated and skip cleanly
//! when `test-assets/` is absent — green no-ops on CI, gap #36), this fixture
//! **ships in the repository** under `assets/trainingdummy/`. It is therefore
//! NOT asset-gated: it must load on every machine and on CI, giving the engine a
//! genuine end-to-end real-content load + match exercise that runs everywhere.
//!
//! If you change the Training Dummy assets, this is the test that proves they
//! still load and drive a match.

use std::path::{Path, PathBuf};

use fp_character::{Character, LoadedCharacter};
use fp_engine::{Match, MatchInput, Player, StageBounds};

/// Resolves a path inside the workspace `assets/trainingdummy/` directory.
///
/// Integration tests run with the *crate* directory as the manifest root
/// (`crates/fp-app`), so we go up two levels to the workspace root, then into
/// `assets/trainingdummy`.
fn dummy_asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/trainingdummy")
        .join(rel)
}

#[test]
fn training_dummy_def_exists_in_repo() {
    // This fixture is shippable content: it must be present (no skip).
    let def = dummy_asset("trainingdummy.def");
    assert!(
        def.exists(),
        "shipped fixture missing: {} — the Training Dummy must be committed",
        def.display()
    );
}

#[test]
fn training_dummy_loads_via_loader() {
    let def = dummy_asset("trainingdummy.def");
    let loaded = LoadedCharacter::load(&def)
        .unwrap_or_else(|e| panic!("Training Dummy failed to load: {e}"));

    assert_eq!(loaded.name, "Training Dummy");
    // The character authors its own common states; the loader also injects the
    // engine built-in locomotion `[Statedef -1]`. Sanity-check breadth.
    assert!(
        loaded.state_count() >= 8,
        "expected the dummy to define a broad set of states, got {}",
        loaded.state_count()
    );
    assert!(
        !loaded.sff.sprites.is_empty(),
        "the dummy must ship at least one sprite"
    );
    assert!(
        !loaded.air.actions.is_empty(),
        "the dummy must ship at least one animation"
    );
    // Core MUGEN states the engine expects for locomotion are present.
    for required in [0, 11, 20, 40, 200, 5000] {
        assert!(
            loaded.state(required).is_some(),
            "missing required state {required}"
        );
    }
    // The attack's PlaySnd references a sound, so the .snd must have loaded.
    assert!(loaded.snd.is_some(), "the dummy ships a .snd");
}

#[test]
fn training_dummy_drives_a_real_match() {
    // A genuine real-content load + match exercise that runs on CI (the fixture
    // ships). Build two dummies into a Match and tick it; assert it does not
    // panic and stays well-formed.
    let def = dummy_asset("trainingdummy.def");
    let make_player = |start_x: f32| -> Player {
        let loaded = LoadedCharacter::load(&def).expect("load dummy");
        let mut entity = Character::with_constants(loaded.constants);
        entity.pos = fp_core::Vec2::new(start_x, 0.0);
        entity.state_no = 0;
        entity.ctrl = true;
        entity.anim = 0;
        Player::new(entity, loaded)
    };

    let mut m = Match::new(
        make_player(-60.0),
        make_player(60.0),
        StageBounds::new(-220.0, 220.0),
    );

    let start_life = m.p1().life();
    assert!(start_life > 0, "dummy should start with positive life");

    // Tick a few hundred frames: through the intro and into the fight, with P1
    // pressing forward + attack. The engine must never panic on real content.
    let p1 = MatchInput {
        right: true,
        a: true,
        ..Default::default()
    };
    for _ in 0..400 {
        m.tick(p1, MatchInput::default());
    }

    // Still well-formed after the run.
    assert!(m.p1().life() <= m.p1().life_max());
    assert!(m.p2().life() <= m.p2().life_max());
}

/// T024 — Keyboard input-trace evidence (logged, non-asset-gated).
///
/// The CLAUDE.md live-debug workflow asks for *either* a screen-capture *or* a
/// logged input-trace proving the fighter responds to keyboard movement and at
/// least one attack. A subagent cannot drive a real window + screen-capture, so
/// this is the **logged input-trace** half: it feeds the engine the exact
/// absolute `MatchInput` snapshots the app's keyboard sampler emits, prints a
/// per-event trace of the fighter's response, and asserts the shipped Training
/// Dummy *responds to every documented input* by entering the corresponding
/// state/animation:
///
/// - Right (= forward, facing right)  -> walk animation 20
/// - Up                               -> jump-start animation 40
/// - Down                             -> crouch animation 11
/// - the `a` attack button            -> attack animation 200
///
/// Each is the input -> command (`holdfwd`/`holdup`/`holddown`/`a`) -> state
/// transition path that makes the fighter playable, verified end-to-end through
/// the engine's public `MatchInput` seam. Runs everywhere (the fixture ships),
/// so the playability path is exercised on every machine and on CI.
#[test]
fn keyboard_input_trace_fighter_responds_to_movement_and_attack() {
    use fp_engine::RoundState;

    let def = dummy_asset("trainingdummy.def");
    if !def.exists() {
        // The dummy is shippable content and normally present; never panic if a
        // stripped checkout lacks it.
        eprintln!("skipping input-trace: {} not present", def.display());
        return;
    }
    let make_player = |start_x: f32| -> Player {
        let loaded = LoadedCharacter::load(&def).expect("load dummy");
        let mut entity = Character::with_constants(loaded.constants);
        entity.pos = fp_core::Vec2::new(start_x, 0.0);
        entity.state_no = 0;
        entity.ctrl = true;
        entity.anim = 0;
        Player::new(entity, loaded)
    };

    /// Builds a fresh live (post-intro) two-dummy match with P1 left/facing
    /// right and P2 idle, so each input is driven from a clean standing state.
    fn fresh_live_match(make_player: &impl Fn(f32) -> Player) -> Match {
        let mut m = Match::new(
            make_player(-100.0),
            make_player(100.0),
            StageBounds::new(-220.0, 220.0),
        );
        for _ in 0..120 {
            if m.round_state() == RoundState::Fight {
                break;
            }
            m.tick(MatchInput::default(), MatchInput::default());
        }
        assert_eq!(
            m.round_state(),
            RoundState::Fight,
            "fight must go live before driving keyboard input"
        );
        m
    }

    /// Holds `input` for up to `frames`, returning whether P1 ever entered
    /// `target_anim` (the state response we expect for that input).
    fn drives_to_anim(m: &mut Match, input: MatchInput, frames: u32, target_anim: i32) -> bool {
        for _ in 0..frames {
            m.tick(input, MatchInput::default());
            if m.p1().anim() == target_anim {
                return true;
            }
        }
        false
    }

    eprintln!("[input-trace] === T024 keyboard input trace (Training Dummy) ===");

    // --- Movement: Right (= walk forward when facing right) -> walk anim 20. ---
    // This is exactly what `match_input_from_keyboard` emits for held D / Right.
    let mut m = fresh_live_match(&make_player);
    let walked = drives_to_anim(
        &mut m,
        MatchInput {
            right: true,
            ..MatchInput::default()
        },
        20,
        20,
    );
    eprintln!("[input-trace] RIGHT (forward) -> walk anim 20: {walked}");
    assert!(
        walked,
        "holding Right (forward) must put P1 into the walk-forward state (anim 20)"
    );

    // --- Movement: Up -> jump-start anim 40. ---
    let mut m = fresh_live_match(&make_player);
    let jumped = drives_to_anim(
        &mut m,
        MatchInput {
            up: true,
            ..MatchInput::default()
        },
        20,
        40,
    );
    eprintln!("[input-trace] UP -> jump-start anim 40: {jumped}");
    assert!(
        jumped,
        "holding Up must put P1 into the jump-start state (anim 40)"
    );

    // --- Movement: Down -> crouch anim 11. ---
    let mut m = fresh_live_match(&make_player);
    let crouched = drives_to_anim(
        &mut m,
        MatchInput {
            down: true,
            ..MatchInput::default()
        },
        20,
        11,
    );
    eprintln!("[input-trace] DOWN -> crouch anim 11: {crouched}");
    assert!(
        crouched,
        "holding Down must put P1 into the crouch state (anim 11)"
    );

    // --- Attack: press the `a` button (light attack) -> attack anim 200. ---
    // This is what `match_input_from_keyboard` emits for the U key. A Press
    // command needs a rising edge, so press on frame 1 then release.
    let mut m = fresh_live_match(&make_player);
    let mut attacked = false;
    for f in 1..=10 {
        let inp = if f == 1 {
            MatchInput {
                a: true,
                ..MatchInput::default()
            }
        } else {
            MatchInput::default()
        };
        m.tick(inp, MatchInput::default());
        if m.p1().anim() == 200 {
            attacked = true;
            break;
        }
    }
    eprintln!("[input-trace] A button -> attack anim 200: {attacked}");
    assert!(
        attacked,
        "pressing the `a` button must put P1 into its attack state (anim 200)"
    );

    eprintln!("[input-trace] === fighter responded to walk, jump, crouch AND attack ===");
}

// NOTE: the validator itself lives in the `fp-app` *binary* crate's `validate`
// module, which integration tests cannot import (a `bin` crate exposes no lib
// target). The "the shipped fixture validates clean" assertion therefore lives
// as a unit test inside `validate.rs` (`shipped_training_dummy_validates_clean`),
// next to the analysis it guards.
