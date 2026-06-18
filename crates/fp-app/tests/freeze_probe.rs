//! Headless freeze-repro probe (T051).
//!
//! Builds a two-player [`Match`] the same way `fp-app`'s `build_player` /
//! `build_two_player_match` do (load `.def` -> `Character` standing in state 0 ->
//! `Player::new` -> `Match::new`) and ticks it with neutral inputs for a fixed
//! number of ticks. Driven by the `FP_P1` / `FP_P2` env vars (paths to character
//! `.def` files), it is the reproduction harness for the community-character
//! freeze: run it under a shell `timeout`, and a hanging matchup never returns
//! (the timeout kills it with exit 124) while a healthy matchup completes fast.
//!
//! This test is `#[ignore]`d so it never runs in the normal suite (it needs
//! local, gitignored community content via `FP_P1`/`FP_P2`). Invoke it directly:
//!
//! ```sh
//! FP_P1=test-assets/community/clark/clark.def \
//! FP_P2=test-assets/community/dudley/dudley.def \
//!   timeout 30 cargo test -p fp-app --test freeze_probe -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Instant;

use fp_character::{Character, LoadedCharacter};
use fp_engine::{Match, MatchInput, Player, StageBounds};

const P1_START_X: f32 = -60.0;
const P2_START_X: f32 = 60.0;
const STAGE_HALF_WIDTH: f32 = 220.0;

/// Mirror of `fp-app`'s `build_player`: load a `.def`, stand it in state 0 with
/// control, default palette.
fn build_player(def_path: &PathBuf, start_x: f32) -> Player {
    let loaded = LoadedCharacter::load(def_path)
        .unwrap_or_else(|e| panic!("failed to load {}: {e}", def_path.display()));
    let mut entity = Character::with_constants(loaded.constants);
    entity.pos = fp_core::Vec2::new(start_x, 0.0);
    entity.state_no = 0;
    entity.ctrl = true;
    entity.anim = 0;
    Player::new(entity, loaded)
}

#[test]
#[ignore = "needs local FP_P1/FP_P2 community content; run under `timeout` to detect hangs"]
fn freeze_probe_env_driven() {
    let p1_def: PathBuf = std::env::var("FP_P1")
        .expect("set FP_P1 to a character .def path")
        .into();
    let p2_def: PathBuf = std::env::var("FP_P2")
        .expect("set FP_P2 to a character .def path")
        .into();
    let ticks: u32 = std::env::var("FP_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800);

    let p1 = build_player(&p1_def, P1_START_X);
    let p2 = build_player(&p2_def, P2_START_X);
    let mut m = Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    );

    let start = Instant::now();
    for t in 0..ticks {
        let tick_start = Instant::now();
        m.tick(MatchInput::none(), MatchInput::none());
        let dt = tick_start.elapsed();
        // A single tick must comfortably fit in the 16.67 ms frame budget. Warn
        // loudly if any tick blows past it — a tick that consistently overruns the
        // budget is the spiral-of-death symptom (the app's accumulator never
        // drains, hard-freezing the window). The per-side live helper count is
        // reported alongside since a runaway/expensive helper population is the
        // usual driver.
        if dt.as_millis() > 50 {
            eprintln!(
                "tick {t} took {} ms (frame budget is 16.67 ms); p1 helpers={} p2 helpers={}",
                dt.as_millis(),
                m.p1().helpers().len(),
                m.p2().helpers().len()
            );
        }
    }
    eprintln!(
        "completed {ticks} ticks in {} ms ({:.3} ms/tick avg)",
        start.elapsed().as_millis(),
        start.elapsed().as_secs_f64() * 1000.0 / f64::from(ticks)
    );
}
