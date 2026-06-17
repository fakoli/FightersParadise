//! Integration tests against the real Kung Fu Man (KFM) `kfm.cmd` fixture.
//!
//! These exercise [`fp_input::compile_command`] and [`fp_input::CommandMatcher`]
//! against the genuine MUGEN command list shipped under the workspace's
//! `test-assets/kfm/` directory. Real content covers symbol combinations the
//! synthetic unit tests construct by hand: `$` direction-detect (`/$F`, `$F,x`),
//! `>` strict-immediate (`F, >~F, >F`), `~` release, `/` hold, `+` simultaneous,
//! repeated motions, and inconsistent author whitespace.
//!
//! ## Skip-if-missing
//!
//! `test-assets/` is local-only and may be absent (e.g. on CI). Each test
//! resolves the fixture relative to `CARGO_MANIFEST_DIR` and **early-returns
//! cleanly** when the file is missing, so `cargo test -p fp-input` stays green
//! whether or not the assets are present.

use std::path::{Path, PathBuf};

use fp_input::command::{compile_command, CommandElement};
use fp_input::{
    Button, CommandDef, CommandMatcher, DirToken, Direction, InputBuffer, InputModifier, InputState,
};

/// Resolves a path inside the workspace `test-assets/kfm/` directory.
///
/// Integration tests run with the *crate* dir as the manifest root, so
/// `CARGO_MANIFEST_DIR` is `crates/fp-input`; go up two levels to the workspace
/// root before descending into `test-assets/kfm`.
fn kfm_asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets/kfm")
        .join(rel)
}

/// Returns `Some(path)` if the asset exists, or `None` (with a notice) if not.
fn require(rel: &str) -> Option<PathBuf> {
    let path = kfm_asset(rel);
    if path.exists() {
        Some(path)
    } else {
        eprintln!(
            "skipping real-content check: {} not present (test-assets/ is local-only)",
            path.display()
        );
        None
    }
}

/// Mirrors the `fp-formats` CMD comment rule: a `;` starts a comment that runs
/// to end-of-line. We strip it so `compile_command` receives the same clean
/// value the real loader would hand it.
fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

/// Strips surrounding double quotes from a value (matches the real loader).
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Extracts the raw value of every `command = ...` line from a `.cmd` source,
/// applying the same BOM-strip + comment-strip the real parser uses.
fn extract_command_values(text: &str) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = strip_comment(raw_line).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        if key == "command" {
            let value = strip_quotes(&line[eq + 1..]);
            if !value.trim().is_empty() {
                out.push(value);
            }
        }
    }
    out
}

/// Every real `command = ...` value in KFM's `kfm.cmd` must compile without
/// error and without panicking. This is the broadest acceptance check: it
/// drives `$`, `>`, `~`, `/`, `+`, repeated names, and messy whitespace through
/// `compile_command` exactly as the engine does at load time.
#[test]
fn all_real_kfm_commands_compile() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");
    let values = extract_command_values(&text);

    assert!(
        values.len() >= 20,
        "expected a substantial KFM command list, found only {} values",
        values.len()
    );

    let mut detect_seen = false; // saw at least one `$`-bearing command
    let mut strict_seen = false; // saw at least one `>`-bearing command
    let mut hold_seen = false; // saw at least one `/`-bearing command
    let mut release_seen = false; // saw at least one `~`-bearing command

    for value in &values {
        let elements = compile_command(value)
            .unwrap_or_else(|e| panic!("real KFM command `{value}` failed to compile: {e:?}"));
        assert!(
            !elements.is_empty(),
            "real KFM command `{value}` compiled to zero elements"
        );

        detect_seen |= value.contains('$');
        strict_seen |= value.contains('>');
        hold_seen |= value.contains('/');
        release_seen |= value.contains('~');
    }

    // KFM is known to use all four symbol classes; assert the fixture actually
    // exercised them (guards against silently skipping the interesting lines).
    assert!(detect_seen, "expected at least one `$` command in KFM");
    assert!(hold_seen, "expected at least one `/` command in KFM");
    assert!(release_seen, "expected at least one `~` command in KFM");
    // `>` appears in KFM's comment examples; the live command list may or may
    // not use it, so we don't hard-require it here (covered by unit tests).
    let _ = strict_seen;
}

/// The real KFM `holdfwd` command (`/$F`, time = 1) must compile to a
/// hold + direction-detect forward element, and must fire when Forward is held
/// (including at a vertical angle), proving `$` direction-detect end-to-end on
/// genuine content.
#[test]
fn real_kfm_holdfwd_compiles_and_matches() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");

    // Find the holdfwd command value as the real loader would: the `command`
    // line that follows `name = "holdfwd"`.
    let mut holdfwd_value: Option<String> = None;
    let mut in_holdfwd = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = strip_quotes(&line[eq + 1..]);
        match key.as_str() {
            "name" => in_holdfwd = val.eq_ignore_ascii_case("holdfwd"),
            "command" if in_holdfwd => {
                holdfwd_value = Some(val);
                break;
            }
            _ => {}
        }
    }

    let value = holdfwd_value.expect("KFM kfm.cmd must define a `holdfwd` command");
    // KFM ships `/$F`; tolerate incidental whitespace.
    assert_eq!(
        value.replace(' ', ""),
        "/$F",
        "KFM holdfwd is expected to be `/$F`"
    );

    let elements = compile_command(&value).expect("holdfwd compiles");
    assert_eq!(elements.len(), 1);
    assert_eq!(
        elements[0],
        CommandElement::Dir {
            token: DirToken::F,
            modifier: InputModifier::Hold,
            detect: true,
            strict: false,
            min_hold: 0,
        },
        "holdfwd `/$F` must be hold + direction-detect forward"
    );

    // Build the matcher from the real value and confirm hold-Forward fires it
    // (this is exactly the bridge KFM's walk-state relies on).
    let cmd = CommandDef {
        name: "holdfwd".into(),
        elements,
        time: 1,
        buffer_time: 1,
    };
    let mut matcher = CommandMatcher::new(vec![cmd]);

    // Pure forward (facing right => hardware right is Forward).
    let mut buffer = InputBuffer::new();
    let fwd = InputState {
        direction: Direction {
            right: true,
            ..Default::default()
        },
        ..Default::default()
    };
    buffer.push(fwd);
    matcher.check_commands(&buffer, true);
    assert!(
        matcher.command_active("holdfwd"),
        "real holdfwd `/$F` must fire while Forward is held"
    );

    // Up-forward diagonal must also fire (direction-detect, not exact cardinal).
    let mut matcher = CommandMatcher::new(vec![CommandDef {
        name: "holdfwd".into(),
        elements: compile_command(&value).unwrap(),
        time: 1,
        buffer_time: 1,
    }]);
    let mut buffer = InputBuffer::new();
    let up_fwd = InputState {
        direction: Direction {
            right: true,
            up: true,
            ..Default::default()
        },
        ..Default::default()
    };
    buffer.push(up_fwd);
    matcher.check_commands(&buffer, true);
    assert!(
        matcher.command_active("holdfwd"),
        "real holdfwd `/$F` must also fire on up-forward via direction-detect"
    );

    // Neutral must NOT fire it.
    let mut matcher = CommandMatcher::new(vec![CommandDef {
        name: "holdfwd".into(),
        elements: compile_command(&value).unwrap(),
        time: 1,
        buffer_time: 1,
    }]);
    let mut buffer = InputBuffer::new();
    buffer.push(InputState::default());
    matcher.check_commands(&buffer, true);
    assert!(
        !matcher.command_active("holdfwd"),
        "real holdfwd must not fire on neutral input"
    );
}

/// KFM's `blocking` command exists in two orderings — `$F,x` and `x,$F` — both
/// using `$` direction-detect. Both must compile, confirming `$` works mid- and
/// start-of-sequence on real content.
#[test]
fn real_kfm_blocking_uses_direction_detect() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");

    let mut blocking_values: Vec<String> = Vec::new();
    let mut in_blocking = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = strip_quotes(&line[eq + 1..]);
        match key.as_str() {
            "name" => in_blocking = val.eq_ignore_ascii_case("blocking"),
            "command" if in_blocking => blocking_values.push(val),
            _ => {}
        }
    }

    assert!(
        blocking_values.len() >= 2,
        "KFM defines `blocking` twice (button-order variants); found {}",
        blocking_values.len()
    );
    for value in &blocking_values {
        assert!(
            value.contains('$'),
            "blocking command `{value}` should use `$` direction-detect"
        );
        let elements = compile_command(value).unwrap_or_else(|e| panic!("`{value}` failed: {e:?}"));
        // One direction-detect element (the F) and one button (x).
        assert_eq!(elements.len(), 2, "blocking `{value}` => F + x");
        let has_detect_fwd = elements.iter().any(|e| {
            matches!(
                e,
                CommandElement::Dir {
                    token: DirToken::F,
                    detect: true,
                    ..
                }
            )
        });
        let has_x = elements.iter().any(|e| {
            matches!(
                e,
                CommandElement::Button {
                    button: Button::X,
                    ..
                }
            )
        });
        assert!(has_detect_fwd, "blocking `{value}` lacks `$F`");
        assert!(has_x, "blocking `{value}` lacks the x button");
    }
}

/// KFM defines all four hold-direction commands (`/$F`, `/$B`, `/$U`, `/$D`).
/// Each real value must compile to exactly one hold + direction-detect element
/// on the expected cardinal token — proving `/$<dir>` parses on genuine content,
/// not just the synthetic `/$F` covered by unit tests.
#[test]
fn real_kfm_all_holddir_commands_parse() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");

    // Collect (name -> command-value) for the holddir family by tracking the
    // most recent `name` before each `command` line, exactly as the loader does.
    let mut current_name = String::new();
    let mut found: Vec<(String, String)> = Vec::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = strip_quotes(&line[eq + 1..]);
        match key.as_str() {
            "name" => current_name = val.to_ascii_lowercase(),
            "command" if current_name.starts_with("hold") => {
                found.push((current_name.clone(), val));
            }
            _ => {}
        }
    }

    // KFM ships holdfwd/holdback/holdup/holddown.
    let expected: &[(&str, DirToken)] = &[
        ("holdfwd", DirToken::F),
        ("holdback", DirToken::B),
        ("holdup", DirToken::U),
        ("holddown", DirToken::D),
    ];
    for (name, token) in expected {
        let (_, value) = found
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("KFM kfm.cmd must define a `{name}` command"));
        let elements = compile_command(value)
            .unwrap_or_else(|e| panic!("`{name}` value `{value}` failed to compile: {e:?}"));
        assert_eq!(elements.len(), 1, "`{name}` (`{value}`) => one element");
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: *token,
                modifier: InputModifier::Hold,
                detect: true,
                strict: false,
                min_hold: 0,
            },
            "`{name}` value `{value}` must be hold + direction-detect {token:?}"
        );
    }
}

/// KFM's forward-dash `command = F, F` must compile to two Forward presses and
/// fire when two distinct Forward taps (with a release between) land inside the
/// time window — the genuine double-tap shape, end-to-end on real content.
#[test]
fn real_kfm_forward_dash_double_tap() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");

    // Find the `FF` command value (name = "FF", command = "F, F").
    let mut current_name = String::new();
    let mut ff_value: Option<String> = None;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = strip_quotes(&line[eq + 1..]);
        match key.as_str() {
            "name" => current_name = val.to_ascii_lowercase(),
            "command" if current_name == "ff" => {
                ff_value = Some(val);
                break;
            }
            _ => {}
        }
    }

    let value = ff_value.expect("KFM kfm.cmd must define an `FF` (forward-dash) command");
    assert_eq!(
        value.replace(' ', ""),
        "F,F",
        "KFM forward dash is expected to be `F, F`"
    );

    let elements = compile_command(&value).expect("`F, F` compiles");
    assert_eq!(elements.len(), 2, "`F, F` is two Forward presses");
    for el in &elements {
        assert_eq!(
            el,
            &CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );
    }

    let cmd = CommandDef {
        name: "FF".into(),
        elements,
        time: 15,
        buffer_time: 3,
    };
    let mut matcher = CommandMatcher::new(vec![cmd]);
    let mut buffer = InputBuffer::new();
    let fwd = InputState {
        direction: Direction {
            right: true,
            ..Default::default()
        },
        ..Default::default()
    };
    // Tap, release, tap (facing right => hardware-right is Forward).
    buffer.push(fwd);
    buffer.push(InputState::default());
    buffer.push(fwd);
    matcher.check_commands(&buffer, true);
    assert!(
        matcher.command_active("FF"),
        "two Forward taps with a release between must fire the real `F, F` dash"
    );
}

/// Returns the `command = ...` value for the first `[Command]` whose `name`
/// matches `wanted` (case-insensitive), tracking the most recent `name` line
/// exactly as the real loader does.
fn command_value_by_name(text: &str, wanted: &str) -> Option<String> {
    let mut current_name = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = strip_quotes(&line[eq + 1..]);
        match key.as_str() {
            "name" => current_name = val.to_ascii_lowercase(),
            "command" if current_name == wanted.to_ascii_lowercase() => return Some(val),
            _ => {}
        }
    }
    None
}

/// KFM's required `recovery` command is `x+y` — a simultaneous two-button combo.
/// It must compile to a single Simultaneous group and fire only when BOTH x and
/// y land on the same frame, not on separate frames.
#[test]
fn real_kfm_recovery_is_simultaneous_two_button() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");
    let value =
        command_value_by_name(&text, "recovery").expect("KFM must define a `recovery` command");
    assert_eq!(
        value.replace(' ', ""),
        "x+y",
        "KFM recovery is expected to be `x+y`"
    );

    let elements = compile_command(&value).expect("`x+y` compiles");
    assert_eq!(elements.len(), 1, "`x+y` is one simultaneous group");
    match &elements[0] {
        CommandElement::Simultaneous(parts) => {
            assert_eq!(parts.len(), 2, "x+y has two members");
            assert!(parts.iter().any(|e| matches!(
                e,
                CommandElement::Button {
                    button: Button::X,
                    ..
                }
            )));
            assert!(parts.iter().any(|e| matches!(
                e,
                CommandElement::Button {
                    button: Button::Y,
                    ..
                }
            )));
        }
        other => panic!("expected Simultaneous, got {other:?}"),
    }

    let make = || CommandDef {
        name: "recovery".into(),
        elements: compile_command(&value).unwrap(),
        time: 1,
        buffer_time: 2,
    };

    // Both buttons same frame => fires.
    let mut matcher = CommandMatcher::new(vec![make()]);
    let mut buffer = InputBuffer::new();
    let mut both = InputState::default();
    both.set_button(Button::X, true);
    both.set_button(Button::Y, true);
    buffer.push(both);
    matcher.check_commands(&buffer, true);
    assert!(
        matcher.command_active("recovery"),
        "x and y on the same frame must fire `recovery`"
    );

    // x then y on separate frames => must NOT fire (time = 1 window, and they
    // are not simultaneous regardless).
    let mut matcher = CommandMatcher::new(vec![make()]);
    let mut buffer = InputBuffer::new();
    let mut only_x = InputState::default();
    only_x.set_button(Button::X, true);
    let mut only_y = InputState::default();
    only_y.set_button(Button::Y, true);
    buffer.push(only_x);
    buffer.push(only_y);
    matcher.check_commands(&buffer, true);
    assert!(
        !matcher.command_active("recovery"),
        "x and y on separate frames must NOT fire the simultaneous `recovery`"
    );
}

/// KFM's `down_a` command is `/$D,a` — hold direction-detect Down, then press a.
/// This is the real-content analogue of the synthetic `/$D,a` unit test: the `a`
/// press must land while Down is held (at any vertical-inclusive angle), and the
/// command must NOT fire when Down is absent.
#[test]
fn real_kfm_down_a_hold_detect_then_button() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read kfm.cmd");
    let value = command_value_by_name(&text, "down_a").expect("KFM must define a `down_a` command");
    assert_eq!(
        value.replace(' ', ""),
        "/$D,a",
        "KFM down_a is expected to be `/$D,a`"
    );

    let elements = compile_command(&value).expect("`/$D,a` compiles");
    assert_eq!(elements.len(), 2, "`/$D,a` is hold-detect-Down then a");
    assert_eq!(
        elements[0],
        CommandElement::Dir {
            token: DirToken::D,
            modifier: InputModifier::Hold,
            detect: true,
            strict: false,
            min_hold: 0,
        },
        "first element must be hold + direction-detect Down"
    );
    assert_eq!(
        elements[1],
        CommandElement::Button {
            button: Button::A,
            modifier: InputModifier::Press,
            strict: false,
            min_hold: 0,
        }
    );

    let make = || CommandDef {
        name: "down_a".into(),
        elements: compile_command(&value).unwrap(),
        time: 15,
        buffer_time: 3,
    };

    // Down held, then a pressed while still holding Down => fires.
    let mut matcher = CommandMatcher::new(vec![make()]);
    let mut buffer = InputBuffer::new();
    let down = InputState {
        direction: Direction {
            down: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut down_a = down;
    down_a.set_button(Button::A, true);
    buffer.push(down);
    buffer.push(down_a);
    matcher.check_commands(&buffer, true);
    assert!(
        matcher.command_active("down_a"),
        "Down held with a pressed must fire real `/$D,a`"
    );

    // a pressed with NO Down held => must not fire.
    let mut matcher = CommandMatcher::new(vec![make()]);
    let mut buffer = InputBuffer::new();
    buffer.push(InputState::default());
    let mut just_a = InputState::default();
    just_a.set_button(Button::A, true);
    buffer.push(just_a);
    matcher.check_commands(&buffer, true);
    assert!(
        !matcher.command_active("down_a"),
        "a without Down held must NOT fire `/$D,a`"
    );
}
