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

// NOTE: the validator itself lives in the `fp-app` *binary* crate's `validate`
// module, which integration tests cannot import (a `bin` crate exposes no lib
// target). The "the shipped fixture validates clean" assertion therefore lives
// as a unit test inside `validate.rs` (`shipped_training_dummy_validates_clean`),
// next to the analysis it guards.
