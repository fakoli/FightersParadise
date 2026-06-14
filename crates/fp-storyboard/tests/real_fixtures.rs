//! Real-fixture tests against the bundled KFM motif storyboards.
//!
//! These are **gated**: if `test-assets/kfm-motif-sffv1/` is not present in the
//! checkout, each test logs a skip notice and returns instead of failing, so the
//! suite stays green without the (large) binary fixtures.

use std::path::{Path, PathBuf};

use fp_storyboard::Storyboard;

/// Resolve a fixture path under the workspace `test-assets/kfm-motif-sffv1/`.
///
/// `CARGO_MANIFEST_DIR` points at `crates/fp-storyboard`; go up two levels.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets/kfm-motif-sffv1")
        .join(name)
}

#[test]
fn intro_def_parses_with_expected_structure() {
    let path = fixture("intro.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }

    let sb = Storyboard::load(&path).expect("intro.def must load");

    // spr resolves to intro.sff
    assert_eq!(sb.sprite_path, "intro.sff");

    // localcoord is the standard 320x240
    assert_eq!(sb.localcoord, (320, 240));

    // At least 3 scenes (the real file has 6).
    assert!(
        sb.scenes.len() >= 3,
        "expected >= 3 scenes, got {}",
        sb.scenes.len()
    );

    // Scene 0: end.time = 240, fadein.time = 120.
    let scene0 = &sb.scenes[0];
    assert_eq!(scene0.end_time, 240, "scene 0 end.time");
    assert_eq!(scene0.fadein_time, 120, "scene 0 fadein.time");
    assert_eq!(scene0.fadeout_time, 30, "scene 0 fadeout.time");
    assert_eq!(scene0.bg_name.as_deref(), Some("BG0"));

    // The BG0 group holds the Mountains / Wall / Shadows layers.
    let bg0 = sb
        .bg_groups
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case("BG0"))
        .expect("BG0 group must exist");
    assert_eq!(bg0.layers.len(), 3, "BG0 should have 3 layers");

    let mountains = bg0
        .layers
        .iter()
        .find(|l| l.name.eq_ignore_ascii_case("Mountains"))
        .expect("Mountains layer");
    assert_eq!(mountains.spriteno, (0, 0));
    assert_eq!(mountains.velocity, (6.0, 0.0));
    assert_eq!(mountains.tile, (1, 0));

    let wall = bg0
        .layers
        .iter()
        .find(|l| l.name.eq_ignore_ascii_case("Wall"))
        .expect("Wall layer");
    assert_eq!(wall.spriteno, (1, 0));
    assert_eq!(wall.velocity, (12.0, 0.0));
    assert!(wall.mask, "Wall has mask = 1");

    let shadows = bg0
        .layers
        .iter()
        .find(|l| l.name.eq_ignore_ascii_case("Shadows"))
        .expect("Shadows layer");
    assert_eq!(shadows.spriteno, (5, 0));
    assert_eq!(shadows.velocity, (36.0, 0.0));
    assert_eq!(shadows.trans.as_deref(), Some("sub"));

    // Embedded animations referenced by later scenes' layerN.anim should be
    // available (e.g. action 10, 11, 100..102).
    assert!(
        sb.animations.contains_key(&10),
        "embedded [Begin Action 10] should be parsed"
    );
    assert!(sb.animations.contains_key(&100));

    // A later scene references its layer animation.
    let anim_scene = sb
        .scenes
        .iter()
        .find(|s| s.layers.iter().any(|l| l.anim == Some(10)))
        .expect("a scene should reference anim 10");
    assert!(!anim_scene.layers.is_empty());
}

#[test]
fn logo_def_parses_without_error() {
    let path = fixture("logo.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let sb = Storyboard::load(&path).expect("logo.def must load");
    assert_eq!(sb.sprite_path, "logo.sff");
    assert!(!sb.scenes.is_empty());
    assert_eq!(sb.scenes[0].end_time, 180);
    assert!(sb.animations.contains_key(&0));
}

#[test]
fn gameover_def_parses_without_error() {
    let path = fixture("gameover.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let sb = Storyboard::load(&path).expect("gameover.def must load");
    assert_eq!(sb.sprite_path, "gameover.sff");
    assert!(sb.scenes.len() >= 2);
    assert_eq!(sb.scenes[0].end_time, 200);
    assert_eq!(sb.scenes[0].fadeout_time, 30);
    // gameover scene 0 has layer0.starttime = 30
    let layer0 = sb.scenes[0]
        .layers
        .iter()
        .find(|l| l.index == 0)
        .expect("layer 0");
    assert_eq!(layer0.starttime, 30);
    // scene 1 has a fadein.col of white
    assert_eq!(sb.scenes[1].fadein_col, Some((255, 255, 255)));
}

#[test]
fn credits_def_does_not_panic() {
    // Bonus: credits.def is also in the motif; just assert it loads if present.
    let path = fixture("credits.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let _sb = Storyboard::load(&path).expect("credits.def must load without panicking");
}

/// credits.def exercises the trickiest grouping case: its BG0 group is followed
/// by `[BG0CtrlDef ...]` and `[BG0Ctrl ...]` controller sections. The parser
/// must keep only the genuine `[BG0 Credits]` layer in the group and must not
/// promote the controller sections to layers or to a second group.
#[test]
fn credits_def_bg_grouping_excludes_controllers() {
    let path = fixture("credits.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let sb = Storyboard::load(&path).expect("credits.def must load");

    assert_eq!(sb.sprite_path, "credits.sff");
    // credits.def sets startscene = 0 explicitly.
    assert_eq!(sb.start_scene, 0);
    // Single scene with bgm and two overlay layers.
    assert_eq!(sb.scenes.len(), 1, "credits has exactly one scene");
    let scene = &sb.scenes[0];
    assert_eq!(scene.end_time, 1600);
    assert_eq!(scene.fadeout_time, 90);
    assert_eq!(scene.fadeout_col, Some((0, 0, 0)));
    assert_eq!(scene.bg_name.as_deref(), Some("BG0"));
    assert_eq!(scene.bgm.as_deref(), Some("credits.mp3"));
    assert_eq!(scene.layerall_pos, (0.0, 0.0));
    // layer0.anim=10, layer1.anim=11.
    assert_eq!(scene.layers.len(), 2, "two overlay layers");
    assert_eq!(scene.layers[0].index, 0);
    assert_eq!(scene.layers[0].anim, Some(10));
    assert_eq!(scene.layers[1].index, 1);
    assert_eq!(scene.layers[1].anim, Some(11));
    assert_eq!(scene.layers[1].offset, (0.0, 215.0));

    // The crucial grouping assertion: exactly one BG0 group, holding only the
    // single real "Credits" layer — the BG0Ctrl/BG0CtrlDef sections must be
    // excluded entirely.
    let bg0_groups: Vec<_> = sb
        .bg_groups
        .iter()
        .filter(|g| g.name.eq_ignore_ascii_case("BG0"))
        .collect();
    assert_eq!(bg0_groups.len(), 1, "exactly one BG0 group");
    let bg0 = bg0_groups[0];
    assert_eq!(
        bg0.layers.len(),
        1,
        "controllers must be excluded; got layers {:?}",
        bg0.layers.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
    assert!(bg0.layers[0].name.eq_ignore_ascii_case("Credits"));
    assert_eq!(bg0.layers[0].spriteno, (0, 0));
    assert_eq!(bg0.layers[0].start, (0.0, 240.0));

    // Embedded actions 10 and 11 (the fade bars) are available.
    assert!(sb.animations.contains_key(&10));
    assert!(sb.animations.contains_key(&11));
}

/// Deeper assertions on intro.def beyond the headline test: the multi-layer
/// scene 5 (Kung Fu Man text) staggers three layers via starttime, and the
/// embedded shaking actions 100-102 are all present.
#[test]
fn intro_def_multilayer_scene_and_actions() {
    let path = fixture("intro.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let sb = Storyboard::load(&path).expect("intro.def must load");

    // Find the scene with three staggered text layers (scene 5).
    let text_scene = sb
        .scenes
        .iter()
        .find(|s| s.layers.len() == 3)
        .expect("a scene with 3 layers (Kung/Fu/Man text)");
    assert_eq!(text_scene.end_time, 180, "scene 5 (KFM text) end.time");
    // Layers stagger: starttimes 30, 60, 90.
    let starttimes: Vec<i32> = text_scene.layers.iter().map(|l| l.starttime).collect();
    assert_eq!(starttimes, vec![30, 60, 90]);
    // Anims 100, 101, 102 in layer-index order.
    let anims: Vec<Option<i32>> = text_scene.layers.iter().map(|l| l.anim).collect();
    assert_eq!(anims, vec![Some(100), Some(101), Some(102)]);

    // All three shaking actions parsed, with multi-frame bodies.
    for n in [100, 101, 102] {
        let act = sb
            .animations
            .get(&n)
            .unwrap_or_else(|| panic!("action {n} must be parsed"));
        assert!(act.frames.len() > 1, "action {n} should be multi-frame");
    }

    // Scene 1 carries a white fadein.col.
    let flash = &sb.scenes[1];
    assert_eq!(flash.fadein_col, Some((255, 255, 255)));
    assert_eq!(flash.clearcolor, Some((221, 248, 248)));
}

/// gameover.def scene 0 carries a fadeout.col that the headline test omits.
#[test]
fn gameover_def_fadeout_color() {
    let path = fixture("gameover.def");
    if !path.exists() {
        eprintln!("skipping: {} not present", path.display());
        return;
    }
    let sb = Storyboard::load(&path).expect("gameover.def must load");
    assert_eq!(sb.scenes[0].fadeout_col, Some((255, 255, 255)));
    assert_eq!(sb.scenes[0].clearcolor, Some((255, 255, 255)));
    // scene 1 fades white -> black via clearcolor black.
    assert_eq!(sb.scenes[1].clearcolor, Some((0, 0, 0)));
}
