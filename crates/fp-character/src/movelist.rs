//! # Movelist — human-readable move list from a character's `.cmd`
//!
//! Turns a loaded character's parsed [`CmdFile`](fp_formats::cmd::CmdFile)
//! command table into a list of [`MoveEntry`]s suitable for an in-app
//! character-info / movelist screen (T071). Each entry pairs the **raw** command
//! name authored by the character (shown verbatim — never invented) with a
//! human-readable motion string derived from the `command =` token sequence.
//!
//! ## Motion formatting
//!
//! MUGEN `command =` strings are comma-separated tokens of directions
//! (`U D F B` plus diagonals `UF UB DF DB`) and buttons (`a b c x y z s`),
//! decorated with prefix symbols (`~` release, `/` hold, `$` 4-way, `>` strict)
//! and `+` for simultaneous presses. The formatter:
//!
//! 1. Splits the command on `,` into ordered steps.
//! 2. Strips the decoration symbols from each step's tokens (they don't change
//!    the *shape* a player reads), keeping `+` as a simultaneous-press joiner.
//! 3. Recognises the common canned motions — quarter-circles (QCF/QCB),
//!    dragon-punch (DP/`Z`), half-circles (HCF/HCB) and charge motions
//!    (`[B]F`, `[D]U`) — by their directional prefix, and renders the rest as
//!    literal arrows so exotic/odd commands still read sensibly.
//! 4. Appends the trailing button group (e.g. `+P`, `+a`).
//!
//! Everything is best-effort and total: an empty or malformed command yields an
//! empty motion string rather than an error, so a sparse/garbage `.cmd` still
//! renders cleanly (never panics).

use fp_formats::cmd::{CmdCommand, CmdFile};

/// One human-readable move: the character's own command name and a formatted
/// motion string (e.g. name `"fireball"`, motion `"QCF+a"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveEntry {
    /// The raw command name as authored in the `.cmd` (shown verbatim).
    pub name: String,
    /// The human-readable motion (e.g. `"QCF+a"`, `"\u{2192}\u{2193}+x"`). Empty
    /// when the command had no parseable motion.
    pub motion: String,
}

/// Builds a movelist from an optional parsed [`CmdFile`].
///
/// `None` (no `.cmd` referenced or it failed to load) yields an empty list. A
/// present file yields one [`MoveEntry`] per `[Command]`, in file order, with
/// pure directional-"hold" locomotion commands (`holdfwd`, `/$F`, …) filtered
/// out so the list reads as *moves*, not the engine's built-in walk/crouch
/// bindings. Never panics regardless of how malformed the commands are.
#[must_use]
pub fn movelist_from_cmd(cmd: Option<&CmdFile>) -> Vec<MoveEntry> {
    let Some(cmd) = cmd else {
        return Vec::new();
    };
    cmd.commands
        .iter()
        .filter(|c| !is_pure_hold_command(c))
        .map(|c| MoveEntry {
            name: c.name.clone(),
            motion: format_motion(&c.command),
        })
        .collect()
}

/// One classified token within a step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A direction token (uppercase `U D F B UF UB DF DB`).
    Direction(String),
    /// A button token (lowercase `a b c x y z s`), kept verbatim.
    Button(String),
}

/// A "pure hold" command is one whose motion is a single held/4-way direction
/// (e.g. `/$F`, `/F`, `$D`) with no buttons — these are the engine's locomotion
/// bindings (`holdfwd`/`holdback`/…), not player-facing moves. We drop them so
/// the movelist shows specials/attacks, not walk/crouch plumbing.
fn is_pure_hold_command(c: &CmdCommand) -> bool {
    let steps: Vec<&str> = c
        .command
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if steps.len() != 1 {
        return false;
    }
    let raw = steps[0];
    // Must be a "held" direction (`/` prefix) to count as locomotion.
    if !raw.contains('/') {
        return false;
    }
    let toks = parse_step_tokens(raw);
    toks.len() == 1 && matches!(toks[0], Token::Direction(_))
}

/// Formats a raw `command =` string into a human-readable motion.
///
/// Total and best-effort: returns an empty string for an empty/whitespace
/// command. Recognises common motions by their directional prefix and falls back
/// to literal arrows for anything else.
#[must_use]
pub fn format_motion(command: &str) -> String {
    // Split into ordered steps; each step is a (possibly `+`-joined) token group.
    let steps: Vec<Vec<Token>> = command
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_step_tokens)
        .collect();
    if steps.is_empty() {
        return String::new();
    }

    // Walk the steps in order, accumulating the directional sequence and the
    // button group. Directions feed motion recognition; buttons (in any step)
    // become the trailing `+P`-style group.
    let mut directions: Vec<String> = Vec::new();
    let mut buttons: Vec<String> = Vec::new();
    for step in &steps {
        for tok in step {
            match tok {
                Token::Direction(d) => directions.push(d.clone()),
                Token::Button(b) => buttons.push(b.clone()),
            }
        }
    }

    let motion = recognise_motion(&directions);
    let buttons_str = if buttons.is_empty() {
        String::new()
    } else {
        buttons.join("+")
    };

    match (motion.is_empty(), buttons_str.is_empty()) {
        (true, true) => String::new(),
        (false, true) => motion,
        (true, false) => buttons_str,
        (false, false) => format!("{motion}+{buttons_str}"),
    }
}

/// Splits one step (between commas) into classified [`Token`]s, dropping the
/// decoration symbols `~ / $ >` (and any leading numeric charge time like the
/// `30` in `~30B`) and splitting on `+` for simultaneous presses.
///
/// Case is **load-bearing** and preserved: MUGEN `.cmd` directions are uppercase
/// (`U D F B`, diagonals `UF`…) while attack buttons are lowercase
/// (`a b c x y z s`). In particular `B` is the Back direction and `b` is the
/// b-button. Unknown tokens are dropped (best-effort, never panics).
fn parse_step_tokens(step: &str) -> Vec<Token> {
    // Strip decoration symbols, whitespace, and digits (charge times like `~30B`).
    let cleaned: String = step
        .chars()
        .filter(|c| !matches!(c, '~' | '/' | '$' | '>' | ' ' | '\t') && !c.is_ascii_digit())
        .collect();
    cleaned
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(classify_token)
        .collect()
}

/// Classifies one raw token into a direction or button, by exact (case-sensitive)
/// spelling. Returns `None` for anything unrecognised so garbage degrades away.
fn classify_token(raw: &str) -> Option<Token> {
    match raw {
        "U" | "D" | "F" | "B" | "UF" | "UB" | "DF" | "DB" => {
            Some(Token::Direction(raw.to_string()))
        }
        "a" | "b" | "c" | "x" | "y" | "z" | "s" => Some(Token::Button(raw.to_string())),
        _ => None,
    }
}

/// Recognises a canned motion from an ordered list of (upper-cased) direction
/// tokens, returning a short label (`"QCF"`, `"DP"`, `"HCB"`, charge `"[B]F"`,
/// …). Falls back to literal arrows joined by spaces for unrecognised shapes.
fn recognise_motion(dirs: &[String]) -> String {
    if dirs.is_empty() {
        return String::new();
    }
    let seq: Vec<&str> = dirs.iter().map(String::as_str).collect();

    // Quarter-circles: D, DF, F  /  D, DB, B
    if seq == ["D", "DF", "F"] {
        return "QCF".to_string();
    }
    if seq == ["D", "DB", "B"] {
        return "QCB".to_string();
    }
    // Dragon punch: F, D, DF  /  B, D, DB
    if seq == ["F", "D", "DF"] {
        return "DP".to_string();
    }
    if seq == ["B", "D", "DB"] {
        return "RDP".to_string();
    }
    // Half-circles: B, DB, D, DF, F  /  F, DF, D, DB, B
    if seq == ["B", "DB", "D", "DF", "F"] {
        return "HCF".to_string();
    }
    if seq == ["F", "DF", "D", "DB", "B"] {
        return "HCB".to_string();
    }
    // Charge motions: two opposing cardinals (Back-then-Forward, Down-then-Up).
    if seq == ["B", "F"] {
        return "[B]F".to_string();
    }
    if seq == ["D", "U"] {
        return "[D]U".to_string();
    }

    // Fallback: literal arrows so exotic commands still read.
    seq.iter()
        .map(|d| arrow_for(d))
        .collect::<Vec<_>>()
        .join("")
}

/// Maps a direction token to a unicode arrow glyph for the literal fallback.
fn arrow_for(dir: &str) -> &'static str {
    match dir {
        "U" => "\u{2191}",  // ↑
        "D" => "\u{2193}",  // ↓
        "F" => "\u{2192}",  // →
        "B" => "\u{2190}",  // ←
        "UF" => "\u{2197}", // ↗
        "UB" => "\u{2196}", // ↖
        "DF" => "\u{2198}", // ↘
        "DB" => "\u{2199}", // ↙
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_formats::cmd::CmdDefaults;

    fn cmd(name: &str, command: &str) -> CmdCommand {
        CmdCommand {
            name: name.to_string(),
            command: command.to_string(),
            time: 15,
            buffer_time: 1,
        }
    }

    #[test]
    fn qcf_with_button() {
        assert_eq!(format_motion("~D, DF, F, a"), "QCF+a");
    }

    #[test]
    fn qcb_with_button() {
        assert_eq!(format_motion("~D, DB, B, x"), "QCB+x");
    }

    #[test]
    fn dragon_punch() {
        assert_eq!(format_motion("F, D, DF, a"), "DP+a");
    }

    #[test]
    fn half_circle_forward() {
        assert_eq!(format_motion("B, DB, D, DF, F, c"), "HCF+c");
    }

    #[test]
    fn charge_back_forward() {
        assert_eq!(format_motion("~30B, F, x"), "[B]F+x");
    }

    #[test]
    fn just_a_button() {
        // A lone attack has no directional motion.
        assert_eq!(format_motion("a"), "a");
    }

    #[test]
    fn simultaneous_buttons_joined() {
        assert_eq!(format_motion("a+b"), "a+b");
    }

    #[test]
    fn exotic_falls_back_to_arrows() {
        // An unrecognised sequence renders as literal arrows + button.
        let m = format_motion("U, U, D, x");
        assert_eq!(m, "\u{2191}\u{2191}\u{2193}+x");
    }

    #[test]
    fn empty_command_is_empty() {
        assert_eq!(format_motion(""), "");
        assert_eq!(format_motion("   "), "");
        assert_eq!(format_motion(",,,"), "");
    }

    #[test]
    fn malformed_never_panics() {
        // Garbage tokens must not panic; they degrade to empty / `?`.
        let _ = format_motion("$$$,~~~,///");
        let _ = format_motion("Q, W, E, r, t");
        let _ = format_motion("+++");
    }

    #[test]
    fn movelist_drops_pure_holds_keeps_specials() {
        let file = CmdFile {
            defaults: CmdDefaults::default(),
            commands: vec![
                cmd("a", "a"),
                cmd("holdfwd", "/$F"),
                cmd("holdback", "/$B"),
                cmd("holdup", "/$U"),
                cmd("holddown", "/$D"),
                cmd("fireball", "~D, DF, F, a"),
                cmd("dp", "F, D, DF, a"),
            ],
        };
        let moves = movelist_from_cmd(Some(&file));
        let names: Vec<&str> = moves.iter().map(|m| m.name.as_str()).collect();
        // The four directional holds are filtered; the lone button + specials stay.
        assert_eq!(names, ["a", "fireball", "dp"]);
        let fireball = moves.iter().find(|m| m.name == "fireball").unwrap();
        assert_eq!(fireball.motion, "QCF+a");
        let dp = moves.iter().find(|m| m.name == "dp").unwrap();
        assert_eq!(dp.motion, "DP+a");
    }

    #[test]
    fn no_cmd_is_empty_movelist() {
        assert!(movelist_from_cmd(None).is_empty());
    }

    #[test]
    fn sparse_cmd_renders_what_is_parseable() {
        // A command with only a name and a garbage motion still produces an entry
        // (raw name shown verbatim) — never crashes, shows what's parseable.
        let file = CmdFile {
            defaults: CmdDefaults::default(),
            commands: vec![cmd("MysteryMove", "$@#%")],
        };
        let moves = movelist_from_cmd(Some(&file));
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].name, "MysteryMove");
        // The garbage motion degrades to a literal-arrow fallback (here `?`) or
        // empty — the point is it doesn't panic and the name is preserved.
        let _ = &moves[0].motion;
    }
}
