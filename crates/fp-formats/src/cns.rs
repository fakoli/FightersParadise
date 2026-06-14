//! # CNS — Character state file parser
//!
//! Parses MUGEN `.cns` files which define a character's state machine: a set of
//! [`Statedef`] blocks, each containing an ordered list of [`StateController`]
//! blocks. This is the heart of MUGEN's behavior system — controllers fire when
//! their trigger expressions evaluate true, performing actions such as changing
//! animation, applying velocity, or defining a hit.
//!
//! # Format
//!
//! CNS files are INI-like. A character file mixes non-state configuration
//! sections (`[Data]`, `[Size]`, `[Velocity]`, ...) with state definitions.
//! This parser captures only the `[Statedef N]` and `[State N, label]` sections;
//! all other sections are skipped.
//!
//! ```text
//! [Statedef 200]
//! type    = S          ; State-type: S-stand, C-crouch, A-air, L-liedown
//! movetype= A          ; Move-type: A-attack, I-idle, H-gethit
//! physics = S          ; Physics: S-stand, C-crouch, A-air
//! anim    = 200        ; Change animation
//! velset  = 0,0        ; Set velocity (x,y)
//! ctrl    = 0          ; Set ctrl
//!
//! [State 200, 1]
//! type = HitDef
//! triggerall = !pause
//! trigger1 = AnimElem = 3
//! trigger2 = Time > 5
//! trigger2 = Time < 20
//! damage = 23, 0
//! ```
//!
//! # Triggers
//!
//! Each controller's activation condition is an OR of AND-groups:
//!
//! - `triggerall` lines must *all* be true (logical AND across them).
//! - `triggerN` lines are grouped by their number `N`. The controller fires if
//!   *any* group is fully satisfied (OR across groups). Within a group, multiple
//!   lines sharing the same `N` are AND'd together.
//!
//! So `trigger1`, `trigger1`, `trigger2` means
//! `(trigger1a AND trigger1b) OR trigger2`, all gated by every `triggerall`.
//!
//! # Expression values are raw
//!
//! This parser does **not** evaluate or compile any expressions. Trigger
//! conditions and controller parameter values are preserved as raw strings;
//! compilation into bytecode happens in a later phase (`fp-vm`).

use std::collections::BTreeMap;
use std::path::Path;

use fp_core::FpResult;

/// A single state definition: `[Statedef N]` plus its ordered controllers.
///
/// The well-known header parameters are captured as raw strings (see the
/// individual fields). Any additional, less common parameters are stored in
/// [`Statedef::extra`]. Expression values are never evaluated.
#[derive(Debug, Clone, Default)]
pub struct Statedef {
    /// The state number. May be negative (e.g. `-1`, `-2`, `-3` for the
    /// special global states).
    pub number: i32,
    /// `type` — state-type (`S`, `C`, `A`, `L`), raw.
    pub state_type: Option<String>,
    /// `movetype` — move-type (`A`, `I`, `H`), raw.
    pub movetype: Option<String>,
    /// `physics` — physics mode (`S`, `C`, `A`, `N`), raw.
    pub physics: Option<String>,
    /// `anim` — animation to switch to on entry, raw expression.
    pub anim: Option<String>,
    /// `velset` — velocity to set on entry (`x, y`), raw.
    pub velset: Option<String>,
    /// `ctrl` — control flag to set on entry, raw expression.
    pub ctrl: Option<String>,
    /// `juggle` — air-juggle points the move costs, raw expression.
    pub juggle: Option<String>,
    /// `poweradd` — power to add on entry, raw expression.
    pub poweradd: Option<String>,
    /// `facep2` — whether to face P2 on entry, raw expression.
    pub facep2: Option<String>,
    /// `sprpriority` — sprite layering priority, raw expression.
    pub sprpriority: Option<String>,
    /// `hitdefpersist` — keep HitDef across this state change, raw expression.
    pub hitdefpersist: Option<String>,
    /// `movehitpersist` — keep move-hit info across this state change, raw.
    pub movehitpersist: Option<String>,
    /// `hitcountpersist` — keep hit count across this state change, raw.
    pub hitcountpersist: Option<String>,
    /// Any other header parameters, as raw `key -> value` (key lowercased).
    pub extra: BTreeMap<String, String>,
    /// The controllers belonging to this state, in file order.
    pub controllers: Vec<StateController>,
}

/// A group of trigger conditions sharing the same `triggerN` number.
///
/// All `conditions` in a group are AND'd together; the controller fires if any
/// single group (OR across groups) is satisfied. Condition strings are raw and
/// preserved in the order they appeared.
#[derive(Debug, Clone)]
pub struct TriggerGroup {
    /// The group number `N` from `triggerN`.
    pub number: u32,
    /// The raw condition expressions for this group, in file order.
    pub conditions: Vec<String>,
}

/// A single state controller: a `[State N, label]` block.
///
/// Holds the controller `type`, its trigger conditions, the two universal
/// parameters ([`ignorehitpause`](StateController::ignorehitpause) and
/// [`persistent`](StateController::persistent)), and all remaining
/// controller-specific parameters as raw `key -> value` pairs. No expression is
/// evaluated.
#[derive(Debug, Clone, Default)]
pub struct StateController {
    /// The owning state number (the `N` in `[State N, label]`).
    pub state_number: i32,
    /// The free-form label after the comma in the header (e.g. `"Wood 1"`).
    pub label: String,
    /// The controller `type` (e.g. `HitDef`, `ChangeState`), raw. `None` if the
    /// block had no `type` line (a malformed controller).
    pub controller_type: Option<String>,
    /// `triggerall` conditions — all must be true. Raw, in file order.
    pub triggerall: Vec<String>,
    /// Numbered trigger groups (`trigger1`, `trigger2`, ...), in ascending group
    /// number. The controller fires if any group is satisfied (OR), provided
    /// every [`triggerall`](StateController::triggerall) condition is also true.
    ///
    /// **Deviation from MUGEN, deferred to trigger-compilation (backlog CB6):**
    /// MUGEN truncates the active groups at the first *gap* in the numbering — with
    /// `trigger1`, `trigger2`, `trigger4` (no `trigger3`), `trigger4` is dead and
    /// ignored. This parser intentionally preserves **all** numbered groups it sees
    /// and does not drop post-gap groups; the contiguity rule must be applied by the
    /// consumer that compiles/evaluates these triggers. Treating every group here as
    /// live could fire a controller on inputs MUGEN would ignore.
    pub triggers: Vec<TriggerGroup>,
    /// `ignorehitpause` universal parameter (raw expression), if present.
    pub ignorehitpause: Option<String>,
    /// `persistent` universal parameter (raw expression), if present.
    pub persistent: Option<String>,
    /// All remaining controller parameters, as raw `key -> value` (key
    /// lowercased). Expression values are preserved verbatim.
    pub params: BTreeMap<String, String>,
}

impl StateController {
    /// Returns the conditions for the numbered trigger group `n`, if any group
    /// with that number was parsed.
    pub fn trigger_group(&self, n: u32) -> Option<&[String]> {
        self.triggers
            .iter()
            .find(|g| g.number == n)
            .map(|g| g.conditions.as_slice())
    }
}

/// A parsed CNS file: an ordered list of [`Statedef`] blocks.
///
/// Non-state sections present in real character files (`[Data]`, `[Size]`,
/// etc.) are intentionally skipped; only state definitions are retained.
#[derive(Debug, Clone, Default)]
pub struct CnsFile {
    /// All state definitions, in the order they appear in the file.
    pub statedefs: Vec<Statedef>,
}

impl CnsFile {
    /// Loads and parses a CNS file from the given path.
    ///
    /// # Errors
    ///
    /// Returns [`fp_core::FpError::Io`](fp_core::FpError) if the file cannot be
    /// read. Malformed *content* never fails: bad lines are logged with
    /// `tracing::warn!` and skipped, yielding a usable partial result.
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_str(&text)
    }

    /// Parses a CNS file from a string.
    ///
    /// Tolerates a leading UTF-8 BOM, CRLF line endings, and `;` comments.
    /// Malformed lines are warned about and skipped rather than aborting.
    ///
    /// # Errors
    ///
    /// This function does not currently produce an `Err`; the [`FpResult`]
    /// return type mirrors the other parsers and leaves room for future
    /// unrecoverable conditions. Content problems are recovered in place.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> FpResult<Self> {
        let mut statedefs: Vec<Statedef> = Vec::new();
        // The Statedef currently being filled, if we are inside one.
        let mut current: Option<Statedef> = None;
        // The controller currently being filled, if we are inside a [State ...].
        let mut controller: Option<StateController> = None;
        // Whether the active controller belongs to the current statedef.
        let mut in_statedef = false;

        for raw_line in text.lines() {
            // Strip a leading BOM only on the very first line; `lines()` already
            // strips the trailing `\r` of CRLF endings.
            let raw_line = raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line);
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            if let Some(inner) = section_header(line) {
                if let Some(kind) = SectionKind::parse(inner) {
                    match kind {
                        SectionKind::Statedef(number) => {
                            // Close the previous controller and statedef.
                            finish_controller(&mut controller, &mut current);
                            finish_statedef(&mut current, &mut statedefs);
                            current = Some(Statedef {
                                number,
                                ..Default::default()
                            });
                            in_statedef = true;
                        }
                        SectionKind::State { number, label } => {
                            finish_controller(&mut controller, &mut current);
                            if current.is_none() {
                                // A [State ...] with no preceding [Statedef]:
                                // tolerate it by warning and dropping the block.
                                tracing::warn!(
                                    "CNS: [State {number}, {label}] before any [Statedef]; skipped"
                                );
                                in_statedef = false;
                                continue;
                            }
                            controller = Some(StateController {
                                state_number: number,
                                label,
                                ..Default::default()
                            });
                            in_statedef = true;
                        }
                        SectionKind::Other => {
                            // Non-state section (e.g. [Data]); leave state mode.
                            finish_controller(&mut controller, &mut current);
                            finish_statedef(&mut current, &mut statedefs);
                            in_statedef = false;
                        }
                    }
                } else {
                    tracing::warn!("CNS: malformed section header `[{inner}]`; skipped");
                }
                continue;
            }

            if !in_statedef {
                // Lines inside skipped, non-state sections.
                continue;
            }

            // Inside a state section: parse `key = value`.
            let Some(eq_pos) = line.find('=') else {
                tracing::warn!("CNS: malformed line (no `=`): `{line}`; skipped");
                continue;
            };
            let key = line[..eq_pos].trim().to_ascii_lowercase();
            let value = line[eq_pos + 1..].trim().to_string();
            if key.is_empty() {
                tracing::warn!("CNS: malformed line (empty key): `{line}`; skipped");
                continue;
            }

            if let Some(ctrl) = controller.as_mut() {
                apply_controller_param(ctrl, &key, value);
            } else if let Some(def) = current.as_mut() {
                apply_statedef_param(def, key, value);
            }
        }

        // Flush the final controller and statedef.
        finish_controller(&mut controller, &mut current);
        finish_statedef(&mut current, &mut statedefs);

        tracing::info!("CNS: loaded {} statedefs", statedefs.len());
        Ok(Self { statedefs })
    }

    /// Returns the first [`Statedef`] with the given number, if present.
    pub fn statedef(&self, number: i32) -> Option<&Statedef> {
        self.statedefs.iter().find(|s| s.number == number)
    }
}

/// Assigns a parsed header parameter to its dedicated field, or to `extra`.
fn apply_statedef_param(def: &mut Statedef, key: String, value: String) {
    match key.as_str() {
        "type" => def.state_type = Some(value),
        "movetype" => def.movetype = Some(value),
        "physics" => def.physics = Some(value),
        "anim" => def.anim = Some(value),
        "velset" => def.velset = Some(value),
        "ctrl" => def.ctrl = Some(value),
        "juggle" => def.juggle = Some(value),
        "poweradd" => def.poweradd = Some(value),
        "facep2" => def.facep2 = Some(value),
        "sprpriority" => def.sprpriority = Some(value),
        "hitdefpersist" => def.hitdefpersist = Some(value),
        "movehitpersist" => def.movehitpersist = Some(value),
        "hitcountpersist" => def.hitcountpersist = Some(value),
        _ => {
            def.extra.insert(key, value);
        }
    }
}

/// Assigns a parsed controller parameter, routing triggers and universal
/// params to their dedicated fields and everything else into `params`.
fn apply_controller_param(ctrl: &mut StateController, key: &str, value: String) {
    if key == "type" {
        ctrl.controller_type = Some(value);
    } else if key == "triggerall" {
        ctrl.triggerall.push(value);
    } else if key == "ignorehitpause" {
        ctrl.ignorehitpause = Some(value);
    } else if key == "persistent" {
        ctrl.persistent = Some(value);
    } else if let Some(n) = parse_trigger_number(key) {
        // Append to the matching group, preserving group order. Groups are
        // created in first-seen order, which for sane files is ascending.
        match ctrl.triggers.iter_mut().find(|g| g.number == n) {
            Some(group) => group.conditions.push(value),
            None => ctrl.triggers.push(TriggerGroup {
                number: n,
                conditions: vec![value],
            }),
        }
    } else {
        ctrl.params.insert(key.to_string(), value);
    }
}

/// Parses the trailing number of a `triggerN` key (e.g. `trigger12` -> 12).
/// Returns `None` for `triggerall` or anything not of the `trigger<digits>`
/// shape.
fn parse_trigger_number(key: &str) -> Option<u32> {
    let rest = key.strip_prefix("trigger")?;
    if rest.is_empty() {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// Finalizes the in-progress controller (if any) into its owning statedef.
fn finish_controller(controller: &mut Option<StateController>, current: &mut Option<Statedef>) {
    if let Some(ctrl) = controller.take() {
        if ctrl.controller_type.is_none() {
            tracing::warn!(
                "CNS: [State {}, {}] has no `type`; kept as untyped controller",
                ctrl.state_number,
                ctrl.label
            );
        }
        if let Some(def) = current.as_mut() {
            def.controllers.push(ctrl);
        }
    }
}

/// Finalizes the in-progress statedef (if any) into the output list.
fn finish_statedef(current: &mut Option<Statedef>, statedefs: &mut Vec<Statedef>) {
    if let Some(def) = current.take() {
        statedefs.push(def);
    }
}

/// The kind of section header encountered.
enum SectionKind {
    /// `[Statedef N]`.
    Statedef(i32),
    /// `[State N, label]`.
    State {
        /// The owning state number.
        number: i32,
        /// The free-form label.
        label: String,
    },
    /// Any other section we do not track.
    Other,
}

impl SectionKind {
    /// Classifies a section header's inner text (without the brackets).
    /// Returns `None` only when a `Statedef`/`State` header is malformed beyond
    /// recovery (e.g. a non-numeric state number).
    fn parse(inner: &str) -> Option<Self> {
        let lower = inner.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("statedef") {
            // Require a separating space so we don't match "statedeffoo".
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                return Some(SectionKind::Other);
            }
            let num = rest.trim();
            return match num.parse::<i32>() {
                Ok(n) => Some(SectionKind::Statedef(n)),
                Err(_) => None,
            };
        }
        if let Some(rest) = lower.strip_prefix("state") {
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                return Some(SectionKind::Other);
            }
            // Split on the original-case `inner` to keep the label's casing.
            // The header layout is `State <number>, <label>`.
            let after = inner[inner.len() - rest.len()..].trim_start();
            let (num_str, label) = match after.split_once(',') {
                Some((n, l)) => (n.trim(), l.trim().to_string()),
                None => (after.trim(), String::new()),
            };
            return match num_str.parse::<i32>() {
                Ok(n) => Some(SectionKind::State { number: n, label }),
                Err(_) => None,
            };
        }
        Some(SectionKind::Other)
    }
}

/// Returns the inner text of a `[...]` section header, or `None` if the line is
/// not a section header.
fn section_header(line: &str) -> Option<&str> {
    if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
        Some(line[1..line.len() - 1].trim())
    } else {
        None
    }
}

/// Strips a `;` comment from a line, returning the part before it.
fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_statedef_params() {
        let text = "\
[Statedef 200]
type    = S
movetype= A
physics = S
juggle  = 1
velset = 0,0
ctrl = 0
anim = 200
poweradd = 10
sprpriority = 2

[State 200, 1]
type = HitDef
trigger1 = AnimElem = 3
damage = 23, 0
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        let def = &cns.statedefs[0];
        assert_eq!(def.number, 200);
        assert_eq!(def.state_type.as_deref(), Some("S"));
        assert_eq!(def.movetype.as_deref(), Some("A"));
        assert_eq!(def.physics.as_deref(), Some("S"));
        assert_eq!(def.juggle.as_deref(), Some("1"));
        assert_eq!(def.velset.as_deref(), Some("0,0"));
        assert_eq!(def.ctrl.as_deref(), Some("0"));
        assert_eq!(def.anim.as_deref(), Some("200"));
        assert_eq!(def.poweradd.as_deref(), Some("10"));
        assert_eq!(def.sprpriority.as_deref(), Some("2"));
        assert_eq!(def.controllers.len(), 1);
        let c = &def.controllers[0];
        assert_eq!(c.controller_type.as_deref(), Some("HitDef"));
        // The damage line is raw and kept as a parameter.
        assert_eq!(c.params.get("damage").map(String::as_str), Some("23, 0"));
    }

    #[test]
    fn negative_statedef_numbers() {
        let text = "\
[Statedef -1]
type = S

[Statedef -2]
type = S

[Statedef -3]
type = S
";
        let cns = CnsFile::from_str(text).unwrap();
        let nums: Vec<i32> = cns.statedefs.iter().map(|s| s.number).collect();
        assert_eq!(nums, vec![-1, -2, -3]);
    }

    #[test]
    fn multiple_trigger_groups_preserve_order_and_grouping() {
        // group 1 has two AND'd lines; group 2 has two AND'd lines.
        let text = "\
[Statedef 5030]
type = A

[State 5030, X]
type = ChangeState
triggerall = !HitFall
triggerall = alive
trigger1 = HitOver
trigger2 = Vel Y > 0
trigger2 = Pos Y >= 25
value = 5050
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.triggerall, vec!["!HitFall", "alive"]);
        assert_eq!(c.triggers.len(), 2);
        assert_eq!(c.triggers[0].number, 1);
        assert_eq!(c.triggers[0].conditions, vec!["HitOver"]);
        assert_eq!(c.triggers[1].number, 2);
        assert_eq!(c.triggers[1].conditions, vec!["Vel Y > 0", "Pos Y >= 25"]);
        // Convenience accessor.
        assert_eq!(c.trigger_group(2).unwrap().len(), 2);
        assert!(c.trigger_group(9).is_none());
        // value is a raw param.
        assert_eq!(c.params.get("value").map(String::as_str), Some("5050"));
    }

    #[test]
    fn universal_params_routed() {
        let text = "\
[Statedef 0]
type = S

[State 0, 1]
type = ChangeAnim
trigger1 = movecontact
ignorehitpause = 1
persistent = 0
value = 5
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.ignorehitpause.as_deref(), Some("1"));
        assert_eq!(c.persistent.as_deref(), Some("0"));
        // Universal params should not leak into params.
        assert!(!c.params.contains_key("ignorehitpause"));
        assert!(!c.params.contains_key("persistent"));
    }

    #[test]
    fn comments_bom_and_crlf() {
        // Leading BOM, CRLF endings, and `;` comments (line and inline).
        let text = "\u{feff}; a leading comment\r\n[Statedef 100] ; header comment\r\ntype = S ; inline\r\n\r\n[State 100, lbl] ;c\r\ntype = Null\r\ntrigger1 = 1\r\n";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        let def = &cns.statedefs[0];
        assert_eq!(def.number, 100);
        assert_eq!(def.state_type.as_deref(), Some("S"));
        assert_eq!(def.controllers.len(), 1);
        assert_eq!(def.controllers[0].label, "lbl");
        assert_eq!(def.controllers[0].controller_type.as_deref(), Some("Null"));
    }

    #[test]
    fn non_state_sections_skipped() {
        let text = "\
[Data]
life = 1000

[Size]
height = 60

[Statedef 0]
type = S

[State 0, 1]
type = Null
trigger1 = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        // Only the Statedef is captured; [Data]/[Size] are ignored.
        assert_eq!(cns.statedefs.len(), 1);
        assert_eq!(cns.statedefs[0].number, 0);
        // The `life` and `height` keys must not have bled into the statedef.
        assert!(!cns.statedefs[0].extra.contains_key("life"));
        assert!(!cns.statedefs[0].extra.contains_key("height"));
    }

    #[test]
    fn malformed_lines_recovered() {
        // Garbage lines (no `=`, empty key) interleaved with good content, plus
        // a controller that follows its statedef. The parser must skip the bad
        // lines, never panic, and still capture the surrounding good data.
        let text = "\
[Statedef 1]
type = S
this line has no equals sign
= dangling value

[State 1, ok]
type = Null
also no equals here
trigger1 = 1
value = 2
";
        let cns = CnsFile::from_str(text).unwrap();
        // Parsing did not panic and produced a usable result.
        assert_eq!(cns.statedefs.len(), 1);
        let def = &cns.statedefs[0];
        assert_eq!(def.number, 1);
        // The bad header lines did not corrupt the statedef.
        assert_eq!(def.state_type.as_deref(), Some("S"));
        // The good controller survived; its garbage line was dropped.
        assert_eq!(def.controllers.len(), 1);
        assert_eq!(def.controllers[0].label, "ok");
        assert_eq!(
            def.controllers[0].params.get("value").map(String::as_str),
            Some("2")
        );
        assert_eq!(def.controllers[0].trigger_group(1).unwrap(), &["1"]);
    }

    #[test]
    fn state_before_statedef_is_dropped() {
        // A bare `[Statedeffoo]` is treated as a non-state section, which closes
        // any open statedef; a following `[State ...]` then has no owner and is
        // skipped rather than panicking.
        let text = "\
[Statedef 1]
type = S
[Statedeffoo]
[State 1, orphan]
type = Null
trigger1 = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        // The orphaned controller was dropped, not attached anywhere.
        assert!(cns.statedefs[0].controllers.is_empty());
    }

    #[test]
    fn empty_input() {
        let cns = CnsFile::from_str("").unwrap();
        assert!(cns.statedefs.is_empty());
    }

    #[test]
    fn statedef_extra_captures_unknown_params() {
        let text = "\
[Statedef 1]
type = S
hitdefpersist = 1
facep2 = 1
somethingweird = 42
";
        let cns = CnsFile::from_str(text).unwrap();
        let def = &cns.statedefs[0];
        assert_eq!(def.hitdefpersist.as_deref(), Some("1"));
        assert_eq!(def.facep2.as_deref(), Some("1"));
        assert_eq!(def.extra.get("somethingweird").map(String::as_str), Some("42"));
    }

    #[test]
    fn lookup_helpers() {
        let text = "\
[Statedef 10]
type = S

[Statedef 20]
type = C
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedef(20).unwrap().state_type.as_deref(), Some("C"));
        assert!(cns.statedef(999).is_none());
    }

    // --- Additional edge-case / MUGEN-semantics coverage (Proctor) ---

    #[test]
    fn trigger_value_keeps_inner_equals() {
        // A trigger condition itself contains `=` (a comparison). The parser
        // must split on the FIRST `=` only, keeping `AnimElem = 3` as the raw
        // condition. This is the single most important MUGEN-semantic rule:
        // expression values are preserved verbatim.
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = HitDef
trigger1 = AnimElem = 3
trigger2 = var(5) = 1 && Time = 0
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.trigger_group(1).unwrap(), &["AnimElem = 3"]);
        assert_eq!(c.trigger_group(2).unwrap(), &["var(5) = 1 && Time = 0"]);
    }

    #[test]
    fn param_value_keeps_inner_equals_and_operators() {
        // Non-trigger controller params with `=` and boolean operators in the
        // value must also be preserved raw (no evaluation, no second split).
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = VarSet
trigger1 = 1
v = 5
value = (var(0) = 3) + ifelse(p2bodydist X <= 30, 1, 0)
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.params.get("v").map(String::as_str), Some("5"));
        assert_eq!(
            c.params.get("value").map(String::as_str),
            Some("(var(0) = 3) + ifelse(p2bodydist X <= 30, 1, 0)")
        );
    }

    #[test]
    fn trigger_groups_preserve_first_seen_order() {
        // MUGEN groups triggers by number; the parser stores groups in the
        // order each number is first encountered, NOT sorted. For files that
        // declare groups out of order (trigger2 before trigger1), the stored
        // order is first-seen. `trigger_group(n)` still resolves by number.
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = Null
trigger2 = a
trigger1 = b
trigger2 = c
trigger3 = d
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        let order: Vec<u32> = c.triggers.iter().map(|g| g.number).collect();
        assert_eq!(order, vec![2, 1, 3], "groups kept in first-seen order");
        // The two `trigger2` lines coalesce into one group, in line order.
        assert_eq!(c.trigger_group(2).unwrap(), &["a", "c"]);
        assert_eq!(c.trigger_group(1).unwrap(), &["b"]);
        assert_eq!(c.trigger_group(3).unwrap(), &["d"]);
    }

    #[test]
    fn trigger_zero_and_large_numbers() {
        // `trigger0` is a valid numbered trigger (number 0), distinct from
        // `triggerall`. Large group numbers must also parse.
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = Null
triggerall = always
trigger0 = zero
trigger99 = big
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.triggerall, vec!["always"]);
        assert_eq!(c.trigger_group(0).unwrap(), &["zero"]);
        assert_eq!(c.trigger_group(99).unwrap(), &["big"]);
        // `trigger0` must not be confused with `triggerall`.
        assert_eq!(c.triggerall.len(), 1);
    }

    #[test]
    fn case_insensitive_sections_and_keys() {
        // Section names (`statedef`/`state`) and parameter keys are matched
        // case-insensitively; values keep their original case.
        let text = "\
[STATEDEF 7]
Type = S
MoveType = A
Physics = c

[STATE 7, MyLabel]
TYPE = ChangeState
TriggerAll = AliVe
Trigger1 = MoveContact
Value = 50
IgnoreHitPause = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        let def = &cns.statedefs[0];
        assert_eq!(def.number, 7);
        assert_eq!(def.state_type.as_deref(), Some("S"));
        assert_eq!(def.movetype.as_deref(), Some("A"));
        assert_eq!(def.physics.as_deref(), Some("c"));
        let c = &def.controllers[0];
        // Label keeps its original case.
        assert_eq!(c.label, "MyLabel");
        assert_eq!(c.controller_type.as_deref(), Some("ChangeState"));
        // Trigger/param values keep original case.
        assert_eq!(c.triggerall, vec!["AliVe"]);
        assert_eq!(c.trigger_group(1).unwrap(), &["MoveContact"]);
        assert_eq!(c.params.get("value").map(String::as_str), Some("50"));
        assert_eq!(c.ignorehitpause.as_deref(), Some("1"));
    }

    #[test]
    fn duplicate_header_param_last_wins() {
        // A repeated scalar header param overwrites; last value wins. (MUGEN
        // takes the last assignment of a key within a section.)
        let text = "\
[Statedef 1]
type = S
type = C
anim = 1
anim = 2
";
        let cns = CnsFile::from_str(text).unwrap();
        let def = &cns.statedefs[0];
        assert_eq!(def.state_type.as_deref(), Some("C"));
        assert_eq!(def.anim.as_deref(), Some("2"));
    }

    #[test]
    fn duplicate_controller_scalar_param_last_wins() {
        // type and non-trigger params are scalar: last assignment wins. (Only
        // triggerall and triggerN accumulate.)
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = ChangeState
type = ChangeAnim
trigger1 = 1
value = 10
value = 20
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.controller_type.as_deref(), Some("ChangeAnim"));
        assert_eq!(c.params.get("value").map(String::as_str), Some("20"));
    }

    #[test]
    fn state_header_without_label_has_empty_label() {
        // `[State N]` with no comma => empty label, still a valid controller.
        let text = "\
[Statedef 1]
type = S

[State 1]
type = Null
trigger1 = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert_eq!(c.label, "");
        assert_eq!(c.state_number, 1);
        assert_eq!(c.controller_type.as_deref(), Some("Null"));
    }

    #[test]
    fn state_label_may_contain_commas() {
        // Only the first comma separates the number from the label; the rest of
        // the label (including further commas) is preserved verbatim.
        let text = "\
[Statedef 1]
type = S

[State 1, Wood, hand, 3]
type = Null
trigger1 = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs[0].controllers[0].label, "Wood, hand, 3");
    }

    #[test]
    fn statedef_header_with_trailing_junk_is_dropped() {
        // `[Statedef 200, foo]` is not valid MUGEN: the number token fails to
        // parse, so the header is rejected (logged + skipped) and no statedef
        // is produced. The parser must not panic.
        let text = "\
[Statedef 200, foo]
type = S
anim = 200
";
        let cns = CnsFile::from_str(text).unwrap();
        assert!(
            cns.statedefs.is_empty(),
            "malformed Statedef header should yield no statedef"
        );
    }

    #[test]
    fn statedef_like_prefix_is_other_section() {
        // `[Statedeffoo]` must NOT be parsed as a Statedef (requires whitespace
        // after the keyword); it's an Other section. Keys under it are ignored.
        let text = "\
[Statedeffoo]
type = S

[Statedef 5]
type = C
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        assert_eq!(cns.statedefs[0].number, 5);
        assert_eq!(cns.statedefs[0].state_type.as_deref(), Some("C"));
    }

    #[test]
    fn empty_and_whitespace_values_preserved() {
        // An empty value (`anim =`) is kept as Some("") — the key was present.
        // A whitespace-only value trims to "".
        let text = "\
[Statedef 1]
anim =
ctrl =    \x20

[State 1, x]
type = Null
trigger1 =
value =   \x20
";
        let cns = CnsFile::from_str(text).unwrap();
        let def = &cns.statedefs[0];
        assert_eq!(def.anim.as_deref(), Some(""));
        assert_eq!(def.ctrl.as_deref(), Some(""));
        let c = &def.controllers[0];
        // An empty trigger value still creates a group with one empty condition.
        assert_eq!(c.trigger_group(1).unwrap(), &[""]);
        assert_eq!(c.params.get("value").map(String::as_str), Some(""));
    }

    #[test]
    fn boolean_operators_kept_raw_in_conditions() {
        // && / || / ! and comparison chains are preserved exactly; no parsing.
        let text = "\
[Statedef 1]
type = S

[State 1, x]
type = Null
trigger1 = command = \"fwd\" && statetype = S && ctrl
trigger1 = !(time < 10) || power >= 1000
";
        let cns = CnsFile::from_str(text).unwrap();
        let g = cns.statedefs[0].controllers[0].trigger_group(1).unwrap();
        assert_eq!(
            g,
            &[
                "command = \"fwd\" && statetype = S && ctrl",
                "!(time < 10) || power >= 1000",
            ]
        );
    }

    #[test]
    fn inline_comment_after_value_is_stripped_and_trimmed() {
        // A `;` after a value starts a comment; the value is trimmed of the
        // trailing whitespace left behind.
        let text = "\
[Statedef 1]
type = S ;stand
anim = 200    ; the entry animation

[State 1, x]
type = Null
trigger1 = 1 ; always
value = 5;tight comment
";
        let cns = CnsFile::from_str(text).unwrap();
        let def = &cns.statedefs[0];
        assert_eq!(def.state_type.as_deref(), Some("S"));
        assert_eq!(def.anim.as_deref(), Some("200"));
        let c = &def.controllers[0];
        assert_eq!(c.trigger_group(1).unwrap(), &["1"]);
        assert_eq!(c.params.get("value").map(String::as_str), Some("5"));
    }

    #[test]
    fn controller_without_type_is_kept_untyped() {
        // A `[State ...]` block that never sets `type` is retained with
        // controller_type == None (warned, not dropped) so callers can decide.
        let text = "\
[Statedef 1]
type = S

[State 1, broken]
trigger1 = 1
value = 5
";
        let cns = CnsFile::from_str(text).unwrap();
        let c = &cns.statedefs[0].controllers[0];
        assert!(c.controller_type.is_none());
        assert_eq!(c.label, "broken");
        assert_eq!(c.params.get("value").map(String::as_str), Some("5"));
    }

    #[test]
    fn keys_before_any_statedef_are_ignored() {
        // Stray `key = value` lines before the first section (no in_statedef)
        // are silently ignored, not attached anywhere, and don't panic.
        let text = "\
life = 1000
attack = 100

[Statedef 0]
type = S
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        assert!(cns.statedefs[0].extra.is_empty());
    }

    #[test]
    fn duplicate_statedef_numbers_all_retained_lookup_first() {
        // Two statedefs with the same number both appear in `statedefs`, in
        // order. `statedef()` returns the first match.
        let text = "\
[Statedef 5]
type = S
anim = 1

[Statedef 5]
type = C
anim = 2
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 2);
        assert_eq!(cns.statedef(5).unwrap().anim.as_deref(), Some("1"));
    }

    #[test]
    fn empty_statedef_with_no_controllers() {
        // A statedef header followed by nothing (or only another header) is
        // valid and produces a statedef with zero controllers.
        let text = "\
[Statedef 1]

[Statedef 2]
type = S
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 2);
        assert!(cns.statedefs[0].controllers.is_empty());
        assert!(cns.statedefs[0].state_type.is_none());
        assert_eq!(cns.statedefs[1].state_type.as_deref(), Some("S"));
    }

    #[test]
    fn non_state_section_closes_open_statedef() {
        // Encountering a non-state section (`[Data]`) mid-file finalizes the
        // open statedef and stops absorbing keys until the next state section.
        let text = "\
[Statedef 1]
type = S

[State 1, a]
type = Null
trigger1 = 1

[Data]
life = 1000

[Statedef 2]
type = C
";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 2);
        // The first statedef kept its single controller.
        assert_eq!(cns.statedefs[0].controllers.len(), 1);
        // `life` from [Data] did not leak into either statedef.
        assert!(cns.statedefs.iter().all(|s| !s.extra.contains_key("life")));
        assert_eq!(cns.statedefs[1].number, 2);
    }

    #[test]
    fn whitespace_around_keys_and_section_numbers_tolerated() {
        // Tabs and extra spaces around the `=` and inside the section header
        // are tolerated.
        let text = "[Statedef    42  ]\n\ttype\t=\tS\n[State  42 ,  lbl ]\n type =Null\n trigger1= 1 \n";
        let cns = CnsFile::from_str(text).unwrap();
        assert_eq!(cns.statedefs.len(), 1);
        let def = &cns.statedefs[0];
        assert_eq!(def.number, 42);
        assert_eq!(def.state_type.as_deref(), Some("S"));
        let c = &def.controllers[0];
        assert_eq!(c.label, "lbl");
        assert_eq!(c.controller_type.as_deref(), Some("Null"));
        assert_eq!(c.trigger_group(1).unwrap(), &["1"]);
    }

    #[test]
    fn load_missing_file_returns_err() {
        // `load` surfaces an IO error for a missing file (the only Err path);
        // it must not panic.
        let path = Path::new("/nonexistent/definitely/not/here.cns");
        let result = CnsFile::load(path);
        assert!(result.is_err(), "loading a missing file should Err");
    }

    #[test]
    fn never_panics_on_pathological_input() {
        // A grab-bag of malformed input: stray brackets, empty section, lone
        // `=`, key-only lines, unicode, nested-looking brackets. Must not panic
        // and must return a usable (possibly empty) result.
        let texts = [
            "[",
            "]",
            "[]",
            "[Statedef]",       // no number -> header rejected
            "[Statedef ]",      // empty number -> header rejected
            "[Statedef x]",     // non-numeric -> header rejected
            "=",
            "= value with no key",
            "key with no equals",
            "[Statedef 1]\n[State]\n",  // [State] with no number
            "[Statedef 1]\ntype\n",     // key, no '='
            "\u{feff}\u{feff}[Statedef 1]\ntype = S\n", // double BOM (only first stripped)
        ];
        for t in texts {
            let cns = CnsFile::from_str(t).expect("from_str never errors on content");
            // Just exercise the result; specific shapes are covered elsewhere.
            let _ = cns.statedefs.len();
        }
    }

    #[test]
    fn statedef_all_documented_params_captured() {
        // Exercises every dedicated Statedef field named in the acceptance
        // criteria, including the persist flags, all as raw strings.
        let text = "\
[Statedef -2]
type = S
movetype = I
physics = N
anim = 999
velset = 4, -8
ctrl = 1
juggle = 4
poweradd = 60
facep2 = 1
sprpriority = 3
hitdefpersist = 1
movehitpersist = 1
hitcountpersist = 1
";
        let cns = CnsFile::from_str(text).unwrap();
        let d = &cns.statedefs[0];
        assert_eq!(d.number, -2);
        assert_eq!(d.state_type.as_deref(), Some("S"));
        assert_eq!(d.movetype.as_deref(), Some("I"));
        assert_eq!(d.physics.as_deref(), Some("N"));
        assert_eq!(d.anim.as_deref(), Some("999"));
        assert_eq!(d.velset.as_deref(), Some("4, -8"));
        assert_eq!(d.ctrl.as_deref(), Some("1"));
        assert_eq!(d.juggle.as_deref(), Some("4"));
        assert_eq!(d.poweradd.as_deref(), Some("60"));
        assert_eq!(d.facep2.as_deref(), Some("1"));
        assert_eq!(d.sprpriority.as_deref(), Some("3"));
        assert_eq!(d.hitdefpersist.as_deref(), Some("1"));
        assert_eq!(d.movehitpersist.as_deref(), Some("1"));
        assert_eq!(d.hitcountpersist.as_deref(), Some("1"));
    }

    // --- Real-fixture tests (skipped when test-assets/ is absent) ---

    /// Resolves a path under the workspace's `test-assets/` directory.
    fn test_asset(rel: &str) -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR points at crates/fp-formats; go up two levels.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    #[test]
    fn real_fixture_kfm_cns() {
        let path = test_asset("kfm/kfm.cns");
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let cns = CnsFile::load(&path).expect("kfm.cns should parse");
        assert!(
            !cns.statedefs.is_empty(),
            "kfm.cns should have at least one statedef"
        );
        // kfm.cns contains a negative statedef (-3) — make sure it parsed.
        assert!(
            cns.statedefs.iter().any(|s| s.number < 0),
            "kfm.cns should contain a negative statedef"
        );

        // Deep, content-specific checks against known KFM data: the [Statedef -3]
        // block holds a [State -3, Landing Sound] PlaySnd controller whose
        // activation is `triggerall = Time = 1` gating
        // `(stateno = 52) OR (stateno = 106)`, value = `40, 0`. This exercises
        // triggerall, two trigger groups, raw `=` preservation, and a raw param
        // all on real data.
        let sd = cns
            .statedef(-3)
            .expect("kfm.cns should contain [Statedef -3]");
        let landing = sd
            .controllers
            .iter()
            .find(|c| c.label == "Landing Sound")
            .expect("[State -3, Landing Sound] should be present");
        assert_eq!(landing.controller_type.as_deref(), Some("PlaySnd"));
        assert_eq!(landing.triggerall, vec!["Time = 1"]);
        assert_eq!(landing.trigger_group(1).unwrap(), &["stateno = 52"]);
        assert_eq!(landing.trigger_group(2).unwrap(), &["stateno = 106"]);
        assert_eq!(landing.params.get("value").map(String::as_str), Some("40, 0"));

        // Sanity on scale: a real character file has many controllers overall.
        let total_controllers: usize =
            cns.statedefs.iter().map(|s| s.controllers.len()).sum();
        assert!(
            total_controllers > 10,
            "kfm.cns should yield many controllers, got {total_controllers}"
        );
        // Every parsed numbered trigger group is non-empty (no empty groups).
        for s in &cns.statedefs {
            for c in &s.controllers {
                for g in &c.triggers {
                    assert!(
                        !g.conditions.is_empty(),
                        "trigger group {} in [State {}, {}] is empty",
                        g.number,
                        c.state_number,
                        c.label
                    );
                }
            }
        }
    }

    #[test]
    fn real_fixture_common1_cns() {
        let path = test_asset("kfm/common1.cns");
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let cns = CnsFile::load(&path).expect("common1.cns should parse");
        assert!(
            !cns.statedefs.is_empty(),
            "common1.cns should have at least one statedef"
        );
        // Sanity: at least one controller somewhere has a trigger group.
        let has_triggers = cns
            .statedefs
            .iter()
            .flat_map(|s| &s.controllers)
            .any(|c| !c.triggers.is_empty());
        assert!(has_triggers, "common1.cns controllers should have triggers");

        // common1.cns opens with [Statedef 0] (Stand) carrying sprpriority = 0;
        // its first controller is a ChangeAnim with two trigger groups
        // (trigger1 / trigger2). This pins real grouped-trigger parsing.
        let stand = cns
            .statedef(0)
            .expect("common1.cns should define [Statedef 0]");
        assert_eq!(stand.state_type.as_deref(), Some("S"));
        assert_eq!(stand.physics.as_deref(), Some("S"));
        assert_eq!(stand.sprpriority.as_deref(), Some("0"));
        let first = stand
            .controllers
            .first()
            .expect("[Statedef 0] should have at least one controller");
        assert_eq!(first.controller_type.as_deref(), Some("ChangeAnim"));
        assert!(
            first.trigger_group(1).is_some() && first.trigger_group(2).is_some(),
            "first Stand controller should have trigger1 and trigger2 groups"
        );

        // At least one controller somewhere uses a triggerall (common in the
        // guard / movement states) and one of the universal params appears.
        let any_triggerall = cns
            .statedefs
            .iter()
            .flat_map(|s| &s.controllers)
            .any(|c| !c.triggerall.is_empty());
        assert!(any_triggerall, "common1.cns should use triggerall somewhere");
        let any_persistent = cns
            .statedefs
            .iter()
            .flat_map(|s| &s.controllers)
            .any(|c| c.persistent.is_some());
        assert!(
            any_persistent,
            "common1.cns should set the persistent universal param somewhere"
        );

        // No universal/trigger keys should have leaked into the generic params
        // map for any controller (they must be routed to dedicated fields).
        for s in &cns.statedefs {
            for c in &s.controllers {
                for leaked in ["type", "triggerall", "ignorehitpause", "persistent"] {
                    assert!(
                        !c.params.contains_key(leaked),
                        "`{leaked}` leaked into params of [State {}, {}]",
                        c.state_number,
                        c.label
                    );
                }
                assert!(
                    !c.params.keys().any(|k| parse_trigger_number(k).is_some()),
                    "a triggerN key leaked into params of [State {}, {}]",
                    c.state_number,
                    c.label
                );
            }
        }
    }
}
