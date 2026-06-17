//! # Content-import repair: CNS/CMD repaired-text overlay
//!
//! MUGEN community content is messy, and the engine already *tolerates* messy
//! CNS/CMD at load time by `tracing::warn!`-ing and skipping each bad line (see
//! [`fp_formats::cns`]). That keeps the engine running, but it floods the log on
//! every load and leaves the original file untouched.
//!
//! This module is the **content-import** counterpart: a one-shot, line-level
//! transform that rewrites the *provably-safe* problem shapes into harmless
//! comments (or, for colon-separated headers, the documented comma form) and
//! writes the result to a separate **overlay** file under a cache/output
//! directory. The overlay then re-parses through the very same
//! [`fp_formats::cns::CnsFile`] parser with **zero `CNS:` warnings**, while the
//! original asset on disk is never modified.
//!
//! The transform is deliberately conservative — it only touches the four shapes
//! the parser would otherwise warn on, and **never** rewrites a line that
//! carries a real `key = value` pair:
//!
//! | Shape | Example | Repair | Category |
//! |-------|---------|--------|----------|
//! | Stray line (no `=`, not a header) | `Special cancelling` | comment out (`"; "`) | [`RepairKind::StrayLine`] |
//! | Empty key (`= value`) | `= 5` | comment out (`"; "`) | [`RepairKind::EmptyKey`] |
//! | Colon-separated header | `[State 9999: Foo]` | colon → comma *in the header only* | [`RepairKind::ColonHeader`] |
//! | Malformed header | `[GarbageHeader` | comment out (`"; [unparsed] "`) | [`RepairKind::MalformedHeader`] |
//!
//! A file with none of those shapes round-trips **byte-identical**. `.cmd` is
//! parsed as CNS, so it shares this exact classifier.
//!
//! ## Clean-room write guard
//!
//! The overlay is the engine's *output*, not source content, so it must never be
//! written back into a tracked `assets/` tree (which would risk mixing repaired
//! third-party text into the clean-room asset set). [`write_overlay`] enforces
//! this: a destination whose path contains an `assets` component is refused with
//! [`fp_core::FpError`], regardless of the repair outcome.

use std::path::{Component, Path};

use fp_core::{FpError, FpResult};
use fp_formats::air::{begin_action_number, salvage_frame_columns};
use fp_formats::cns::{section_header, strip_comment, SectionKind};
use serde::Serialize;

/// The kind of repair applied to a single CNS/CMD line.
///
/// Each variant corresponds to exactly one of the parser's recoverable
/// `CNS:`-warning shapes; the overlay rewrites the line so that shape no longer
/// warns on re-parse.
// Several variants below (`PartialSff`/`PartialSnd`/`Transcoded`/`AiVarHint`, and
// the `JunkColumn`/`DeadFrame` aliases for the AIR-overlay categories) are the
// stable forward model T082 defines for the import pipeline; the binary does not
// yet construct every one (later F034 passes do), so they are dead in the bin
// target only. They are part of the deliberate public model, not accidental dead
// code — keep them.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairKind {
    /// A non-blank line with no `=` that is not a section header (e.g. a stray
    /// keyword like `Special cancelling` or a bare token `t`). Commented out.
    StrayLine,
    /// A line that opens a section (`[`) but does not parse as a recoverable
    /// `[Statedef N]` / `[State N, label]` header — either it is not closed with
    /// `]` (`[GarbageHeader`) or its state number is non-numeric. Commented out
    /// with an `[unparsed]` marker.
    MalformedHeader,
    /// A `key = value` line whose key (the text before the first `=`) is empty
    /// (e.g. `= 5`). Commented out.
    EmptyKey,
    /// A trigger / parameter expression that was **empty** at compile time and
    /// silently became the const-`0` fallback. The repair is to drop it (an empty
    /// expression contributes nothing); recorded under [`Tier::Repaired`].
    EmptyExpr,
    /// A trigger / parameter expression that failed to compile but was **not**
    /// empty (a typo / truncated source). The loader substitutes const-`0`, so the
    /// trigger would never fire / the parameter reads `0`; a human must look —
    /// recorded under [`Tier::Flagged`].
    TruncatedExpr,
    /// A frame line carried a column with trailing junk (e.g. the `2..A` ticks
    /// column); the column was salvaged to its leading integer. [`Tier::Repaired`].
    JunkColumn,
    /// A well-formed, parseable header that uses a colon as its number/label
    /// separator (e.g. `[State 9999: Foo]`). The colon is rewritten to a comma
    /// **in the header only**.
    ColonHeader,
    /// A dead AIR frame (sprite absent / degenerate) that `--prune` removed from
    /// the overlay. [`Tier::Repaired`].
    DeadFrame,
    /// An SFF sprite with degenerate `0×0` dimensions that is **not** linked to a
    /// real sprite (it owns no pixels and renders nothing). An advisory: the
    /// engine already treats a missing/empty sprite as invisible, so it is recorded
    /// under [`Tier::Advisory`] rather than flagged.
    ZeroDimSprite,
    /// An AIR frame references a `(group, image)` the SFF does not contain (the
    /// frame would draw nothing). Recorded under [`Tier::Flagged`].
    MissingSpriteRef,
    /// An SFF that parsed only partially (a recoverable structural problem the
    /// loader skipped past). [`Tier::Flagged`].
    PartialSff,
    /// A SND that parsed only partially. [`Tier::Flagged`].
    PartialSnd,
    /// Content that was transcoded from a non-UTF-8 encoding (e.g. Shift-JIS) on
    /// load. An advisory note. [`Tier::Advisory`].
    Transcoded,
    /// A heuristic hint that an AI-difficulty `var(...)` slot may need attention.
    /// An advisory note. [`Tier::Advisory`].
    AiVarHint,
}

impl RepairKind {
    /// The stable, human-facing category label (used as the JSON `kind` and for
    /// per-category counts). Stable across releases — downstream tooling keys off
    /// it, so never rename a variant's label without a migration.
    #[must_use]
    pub fn category(self) -> &'static str {
        match self {
            RepairKind::StrayLine => "StrayLine",
            RepairKind::MalformedHeader => "MalformedHeader",
            RepairKind::EmptyKey => "EmptyKey",
            RepairKind::EmptyExpr => "EmptyExpr",
            RepairKind::TruncatedExpr => "TruncatedExpr",
            RepairKind::JunkColumn => "JunkColumn",
            RepairKind::ColonHeader => "ColonHeader",
            RepairKind::DeadFrame => "DeadFrame",
            RepairKind::ZeroDimSprite => "ZeroDimSprite",
            RepairKind::MissingSpriteRef => "MissingSpriteRef",
            RepairKind::PartialSff => "PartialSff",
            RepairKind::PartialSnd => "PartialSnd",
            RepairKind::Transcoded => "Transcoded",
            RepairKind::AiVarHint => "AiVarHint",
        }
    }

    /// The default [`Tier`] this repair kind reports under. Provably-safe
    /// rewrites are [`Tier::Repaired`]; informational notes are [`Tier::Advisory`];
    /// everything a human should resolve is [`Tier::Flagged`].
    #[must_use]
    pub fn tier(self) -> Tier {
        match self {
            RepairKind::StrayLine
            | RepairKind::MalformedHeader
            | RepairKind::EmptyKey
            | RepairKind::EmptyExpr
            | RepairKind::JunkColumn
            | RepairKind::ColonHeader
            | RepairKind::DeadFrame => Tier::Repaired,
            RepairKind::TruncatedExpr
            | RepairKind::MissingSpriteRef
            | RepairKind::PartialSff
            | RepairKind::PartialSnd => Tier::Flagged,
            RepairKind::ZeroDimSprite | RepairKind::Transcoded | RepairKind::AiVarHint => {
                Tier::Advisory
            }
        }
    }
}

/// A single repair the overlay applied, recording the source line (1-based) and
/// the original line text for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// The repair category.
    pub kind: RepairKind,
    /// 1-based line number in the source file.
    pub line_no: usize,
    /// The original (pre-repair) line text, with line endings stripped.
    pub original: String,
}

/// The result of repairing a CNS/CMD text: the rewritten overlay text plus the
/// list of repairs applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CnsOverlay {
    /// The repaired text, ready to be written to an overlay file and re-parsed.
    pub text: String,
    /// Every repair applied, in source order.
    pub repairs: Vec<Repair>,
}

impl CnsOverlay {
    /// Returns the number of repairs of a given [`RepairKind`].
    #[must_use]
    pub fn count(&self, kind: RepairKind) -> usize {
        self.repairs.iter().filter(|r| r.kind == kind).count()
    }

    /// Returns `true` when no repair was applied (i.e. the input was clean and
    /// the overlay text is byte-identical to the input).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.repairs.is_empty()
    }
}

/// What a single line maps to after classification.
enum LineAction {
    /// Keep the line exactly as-is.
    Keep,
    /// Replace the line with this text and record the repair.
    Repair(String, RepairKind),
}

/// Classifies a single source line and decides its overlay form.
///
/// `raw` is the line **without** its line ending. The classifier mirrors the
/// parser's own preprocessing (strip a leading BOM on line 1 handled by the
/// caller, then [`strip_comment`] + trim) so it sees a line exactly as the
/// parser would before deciding it is a problem.
fn classify_line(raw: &str) -> LineAction {
    let trimmed = strip_comment(raw).trim();

    // Blank lines and comment-only lines never warn — pass them through.
    if trimmed.is_empty() {
        return LineAction::Keep;
    }

    // Section headers.
    if trimmed.starts_with('[') {
        // A well-formed `[...]` header (closed, per the parser's own rule).
        if let Some(inner) = section_header(trimmed) {
            match SectionKind::parse(inner) {
                // Unrecoverable header (e.g. non-numeric state number): comment.
                None => {
                    return LineAction::Repair(
                        format!("; [unparsed] {raw}"),
                        RepairKind::MalformedHeader,
                    );
                }
                Some(_) => {
                    // Parseable. If it uses a colon separator in the *header*,
                    // rewrite that single colon to a comma — but never touch a
                    // colon that lives in a value or elsewhere on the line.
                    if let Some(repaired) = colon_header_to_comma(raw) {
                        return LineAction::Repair(repaired, RepairKind::ColonHeader);
                    }
                    return LineAction::Keep;
                }
            }
        }
        // Starts with `[` but is not a closed `[...]` header (e.g.
        // `[GarbageHeader`): the parser would treat it as a malformed line, so
        // comment it out as an unparsed header.
        return LineAction::Repair(format!("; [unparsed] {raw}"), RepairKind::MalformedHeader);
    }

    // Lines that carry a `=`: only an EMPTY key is a problem; a real
    // `key = value` is left untouched (the conservative core invariant).
    if let Some(eq) = trimmed.find('=') {
        let key = trimmed[..eq].trim();
        if key.is_empty() {
            return LineAction::Repair(format!("; {raw}"), RepairKind::EmptyKey);
        }
        return LineAction::Keep;
    }

    // A non-blank, non-header line with no `=`: a stray line.
    LineAction::Repair(format!("; {raw}"), RepairKind::StrayLine)
}

/// Rewrites the first colon in a header line to a comma, returning the new line,
/// or `None` if the header contains no colon (so nothing to repair).
///
/// Operates on the **bracketed header region only**: it finds the `[`…`]` span
/// of the header and rewrites the first `:` inside it, leaving any text after
/// the closing `]` (e.g. a trailing comment) untouched.
fn colon_header_to_comma(raw: &str) -> Option<String> {
    let open = raw.find('[')?;
    let close = raw[open..].find(']').map(|i| open + i)?;
    let header = &raw[open..=close];
    let colon = header.find(':')?;
    // Absolute index of the colon within `raw`.
    let abs = open + colon;
    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..abs]);
    out.push(',');
    out.push_str(&raw[abs + 1..]);
    Some(out)
}

/// Splits `text` into lines, preserving each line's original terminator so the
/// overlay can be reassembled byte-identically when no repair is applied.
///
/// Returns `(content, terminator)` pairs where `terminator` is `"\r\n"`, `"\n"`,
/// or `""` (final line with no trailing newline).
fn split_keep_terminators(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(nl) => {
                // Include a preceding `\r` in the terminator (CRLF).
                let (content, term) = if nl > 0 && rest.as_bytes()[nl - 1] == b'\r' {
                    (&rest[..nl - 1], &rest[nl - 1..=nl])
                } else {
                    (&rest[..nl], &rest[nl..=nl])
                };
                out.push((content, term));
                rest = &rest[nl + 1..];
            }
            None => {
                out.push((rest, ""));
                rest = "";
            }
        }
    }
    out
}

/// Repairs a CNS/CMD text into an overlay, preserving comments, ordering,
/// indentation, and line endings.
///
/// A clean input (no repairable shapes) yields an overlay whose `text` is
/// **byte-identical** to the input. `.cmd` content is parsed as CNS and shares
/// this classifier — call it for both.
#[must_use]
pub fn repair_cns_text(text: &str) -> CnsOverlay {
    let lines = split_keep_terminators(text);
    let mut out = String::with_capacity(text.len() + 16);
    let mut repairs = Vec::new();

    for (idx, (content, term)) in lines.iter().enumerate() {
        // The parser strips a leading BOM only on the first line; do the same so
        // a BOM does not derail the line-1 classification (and is preserved).
        let (bom, body) = match content.strip_prefix('\u{feff}') {
            Some(b) => ("\u{feff}", b),
            None => ("", *content),
        };
        match classify_line(body) {
            LineAction::Keep => {
                out.push_str(content);
            }
            LineAction::Repair(repaired, kind) => {
                repairs.push(Repair {
                    kind,
                    line_no: idx + 1,
                    original: (*content).to_string(),
                });
                out.push_str(bom);
                out.push_str(&repaired);
            }
        }
        out.push_str(term);
    }

    CnsOverlay { text: out, repairs }
}

/// Returns `true` if any path component (case-insensitively) is `assets` — the
/// tracked clean-room asset tree the overlay must never be written into.
fn path_touches_assets(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(os) => os
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("assets")),
        _ => false,
    })
}

/// Writes an overlay's repaired text to `dest`, refusing any destination inside
/// an `assets/` tree.
///
/// # Errors
///
/// - [`FpError::Other`] if `dest` lies inside an `assets/` directory (the
///   clean-room write guard) — the file is **not** written in that case.
/// - [`FpError::Io`] if the parent directory cannot be created or the file
///   cannot be written.
pub fn write_overlay(overlay: &CnsOverlay, dest: &Path) -> FpResult<()> {
    write_overlay_text(&overlay.text, dest)
}

/// Shared overlay-write core: enforces the clean-room `assets/` write guard,
/// creates the parent directory, and writes `text` to `dest`.
fn write_overlay_text(text: &str, dest: &Path) -> FpResult<()> {
    if path_touches_assets(dest) {
        return Err(FpError::Other(format!(
            "refusing to write import overlay inside an assets/ tree: {} \
             (overlays are engine output, not clean-room source content)",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(dest, text.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// AIR overlay: column salvage + opt-in dead-frame pruning (T084)
// ---------------------------------------------------------------------------

/// The kind of repair the AIR overlay applied to a single `.air` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AirRepairKind {
    /// A frame line carried a column with trailing junk (e.g. the `2..A` ticks
    /// column seen in real content); the column was salvaged to its leading
    /// integer (`2`). The frame is **kept** — this is a fidelity repair, applied
    /// regardless of `--prune`.
    JunkColumn,
    /// A frame references a sprite that is **absent** from the `.sff` (or a
    /// degenerate non-linked `0×0` sprite). Without `--prune` the frame is left
    /// in place and only flagged (a diagnostic, **not** a rewrite). The overlay
    /// `text` is unchanged for this line.
    MissingSpriteRef,
    /// A dead frame (see [`AirRepairKind::MissingSpriteRef`]) that `--prune`
    /// actually **removed**: the frame line is dropped from the overlay `text`.
    /// Pruning never removes an action's last surviving frame — that frame is
    /// downgraded to a [`AirRepairKind::MissingSpriteRef`] flag instead, so every
    /// action keeps at least one frame (AIR hard-errors on zero actions).
    DeadFrame,
}

/// A single AIR repair, recording the source line (1-based), the action it
/// belongs to (if any), and the original line text for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AirRepair {
    /// The repair category.
    pub kind: AirRepairKind,
    /// 1-based line number in the source file.
    pub line_no: usize,
    /// The `[Begin Action N]` this frame belongs to, or `None` if it precedes
    /// the first action header.
    pub action: Option<i32>,
    /// The original (pre-repair) line text, with line endings stripped.
    pub original: String,
}

/// The result of repairing an AIR text into an overlay: the rewritten text plus
/// the list of repairs applied (and flags raised).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AirOverlay {
    /// The repaired AIR text, ready to be written and re-parsed.
    pub text: String,
    /// Every repair applied / flag raised, in source order.
    pub repairs: Vec<AirRepair>,
}

impl AirOverlay {
    /// Returns the number of repairs of a given [`AirRepairKind`].
    #[must_use]
    pub fn count(&self, kind: AirRepairKind) -> usize {
        self.repairs.iter().filter(|r| r.kind == kind).count()
    }

    /// Returns `true` when no repair was applied and no flag was raised.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.repairs.is_empty()
    }
}

/// One classified source line, retained through the two-pass overlay build.
struct AirLine<'a> {
    /// Content without its line terminator.
    content: &'a str,
    /// Original line terminator (`"\r\n"`, `"\n"`, or `""`).
    term: &'a str,
    /// 1-based source line number.
    line_no: usize,
    /// The action this line belongs to (set on `[Begin Action]`, carried down).
    action: Option<i32>,
    /// What to do with this line, decided in pass 1 and finalized in pass 2.
    role: AirLineRole,
}

/// The classified role of a source line.
enum AirLineRole {
    /// Not a frame line — emit verbatim.
    Passthrough,
    /// A frame line. `salvaged` is the column-salvaged text (== content when no
    /// junk), `had_junk` records whether a `JunkColumn` repair applies, and
    /// `dead` records whether the referenced sprite is absent/degenerate.
    Frame {
        salvaged: String,
        had_junk: bool,
        dead: bool,
    },
}

/// Repairs an AIR text into an overlay, salvaging junk frame columns and,
/// when `prune` is set, removing dead frames whose `(group, image)` does not
/// resolve to a renderable sprite according to `sprite_present`.
///
/// `sprite_present(group, image)` is the dead-frame oracle — pass
/// [`fp_formats::sff::SffFile::has_renderable_sprite`]. A frame is *dead* when
/// `sprite_present` returns `false` (the sprite is absent or a non-linked `0×0`
/// entry); linked / by-design-`0×0` sprites resolve to real pixels and are
/// reported present, so they are never pruned.
///
/// Behavior:
/// - **Junk columns** (`2..A` → `2`) are always salvaged and reported
///   [`AirRepairKind::JunkColumn`] — independent of `prune`.
/// - **Dead frames** with `prune == false` are only flagged
///   ([`AirRepairKind::MissingSpriteRef`]); the overlay text keeps the (salvaged)
///   line.
/// - **Dead frames** with `prune == true` are removed from the overlay text and
///   reported [`AirRepairKind::DeadFrame`] — **except** the last surviving frame
///   of an action, which is downgraded to a flag so the action never empties.
///
/// A clean input with `prune == false` yields a byte-identical overlay.
pub fn repair_air_text(
    text: &str,
    prune: bool,
    sprite_present: impl Fn(u16, u16) -> bool,
) -> AirOverlay {
    let raw_lines = split_keep_terminators(text);

    // Pass 1: classify every line, salvage columns, detect dead frames.
    let mut current_action: Option<i32> = None;
    let mut lines: Vec<AirLine> = Vec::with_capacity(raw_lines.len());
    for (idx, (content, term)) in raw_lines.iter().enumerate() {
        // A BOM only ever lands on line 1; the AIR parser strips it before its
        // own header check, so do the same when classifying.
        let body = content.strip_prefix('\u{feff}').unwrap_or(content);

        if let Some(num) = begin_action_number(body) {
            current_action = Some(num);
            lines.push(AirLine {
                content,
                term,
                line_no: idx + 1,
                action: current_action,
                role: AirLineRole::Passthrough,
            });
            continue;
        }

        let role = match salvage_frame_columns(body) {
            Some(fr) => AirLineRole::Frame {
                dead: !sprite_present(fr.group, fr.image),
                salvaged: fr.salvaged,
                had_junk: fr.had_junk,
            },
            None => AirLineRole::Passthrough,
        };
        lines.push(AirLine {
            content,
            term,
            line_no: idx + 1,
            action: current_action,
            role,
        });
    }

    // Count live (non-dead) frames per action so pruning never empties one.
    // `None` (frames before the first header) groups under a single bucket.
    let mut live_frames: std::collections::HashMap<Option<i32>, usize> =
        std::collections::HashMap::new();
    for line in &lines {
        if let AirLineRole::Frame { dead: false, .. } = line.role {
            *live_frames.entry(line.action).or_insert(0) += 1;
        }
    }
    // Track how many frames of each action still remain as we walk (so the
    // *last* remaining frame of an action is never pruned away).
    let mut remaining: std::collections::HashMap<Option<i32>, usize> =
        std::collections::HashMap::new();
    for line in &lines {
        if let AirLineRole::Frame { .. } = line.role {
            *remaining.entry(line.action).or_insert(0) += 1;
        }
    }

    // Pass 2: assemble the overlay text and the repair list.
    let mut out = String::with_capacity(text.len() + 16);
    let mut repairs = Vec::new();
    for line in &lines {
        match &line.role {
            AirLineRole::Passthrough => {
                out.push_str(line.content);
                out.push_str(line.term);
            }
            AirLineRole::Frame {
                salvaged,
                had_junk,
                dead,
            } => {
                let bom = if line.content.starts_with('\u{feff}') {
                    "\u{feff}"
                } else {
                    ""
                };
                // The salvaged text never carries the BOM (it was stripped before
                // classification); re-prepend it so line 1 keeps its BOM.
                let salvaged_line = format!("{bom}{salvaged}");

                if *had_junk {
                    repairs.push(AirRepair {
                        kind: AirRepairKind::JunkColumn,
                        line_no: line.line_no,
                        action: line.action,
                        original: (*line.content).to_string(),
                    });
                }

                let live_in_action = live_frames.get(&line.action).copied().unwrap_or(0);
                let entry = remaining.entry(line.action).or_insert(0);
                let frames_left = *entry;
                *entry = frames_left.saturating_sub(1);

                if *dead {
                    // Would pruning empty the action? It does when this action has
                    // no live frames AND this is the last frame line still standing.
                    let would_empty = live_in_action == 0 && frames_left <= 1;
                    if prune && !would_empty {
                        // Remove the frame line entirely (do not emit it). The
                        // surrounding lines (and this line's terminator) vanish
                        // with it — a pure line-level deletion.
                        repairs.push(AirRepair {
                            kind: AirRepairKind::DeadFrame,
                            line_no: line.line_no,
                            action: line.action,
                            original: (*line.content).to_string(),
                        });
                        continue;
                    }
                    // Not pruned (either `--prune` off, or pruning would empty the
                    // action): keep the (salvaged) line and only flag it.
                    repairs.push(AirRepair {
                        kind: AirRepairKind::MissingSpriteRef,
                        line_no: line.line_no,
                        action: line.action,
                        original: (*line.content).to_string(),
                    });
                }

                out.push_str(&salvaged_line);
                out.push_str(line.term);
            }
        }
    }

    AirOverlay { text: out, repairs }
}

/// Writes an AIR overlay's repaired text to `dest`, refusing any destination
/// inside an `assets/` tree (the same clean-room write guard as
/// [`write_overlay`]).
///
/// # Errors
///
/// - [`FpError::Other`] if `dest` lies inside an `assets/` directory — the file
///   is **not** written.
/// - [`FpError::Io`] if the parent cannot be created or the file cannot be
///   written.
pub fn write_air_overlay(overlay: &AirOverlay, dest: &Path) -> FpResult<()> {
    write_overlay_text(&overlay.text, dest)
}

// ---------------------------------------------------------------------------
// Import report: tiered human + stable-JSON rendering + severity gate (T085)
// ---------------------------------------------------------------------------

/// The severity tier a repair / flag falls under in the import report.
///
/// The tier drives both the human grouping and the `--strict` gate: a report is
/// considered to have failed `--strict` iff its [`Tier::Flagged`] list is
/// non-empty (a flag is something the import could *not* provably auto-repair and
/// a human should look at). [`Tier::Repaired`] and [`Tier::Advisory`] never affect
/// the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The import rewrote the content into a provably-safe shape (e.g. commented
    /// out a stray line, salvaged a junk AIR column, pruned a dead frame). No
    /// human action needed; recorded for transparency.
    Repaired,
    /// The import detected a problem it did **not** rewrite — a human should look.
    /// The only thing that trips `--strict`.
    Flagged,
    /// An informational note (no problem, no rewrite). Never trips `--strict`.
    Advisory,
}

impl Tier {
    /// The stable, human-facing label for this tier (used as the JSON key and the
    /// human-report section heading).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tier::Repaired => "repaired",
            Tier::Flagged => "flagged",
            Tier::Advisory => "advisory",
        }
    }
}

/// The stable category label for a CNS/CMD [`RepairKind`] (used for per-category
/// counts and as the JSON `kind`).
#[must_use]
fn cns_category(kind: RepairKind) -> &'static str {
    kind.category()
}

/// The tier an AIR [`AirRepairKind`] is reported under. A salvaged junk column or
/// a pruned dead frame is a provably-safe rewrite ([`Tier::Repaired`]); a
/// missing-sprite reference that was only *flagged* (not pruned) is something a
/// human should resolve ([`Tier::Flagged`]).
#[must_use]
fn air_tier(kind: AirRepairKind) -> Tier {
    match kind {
        AirRepairKind::JunkColumn | AirRepairKind::DeadFrame => Tier::Repaired,
        AirRepairKind::MissingSpriteRef => Tier::Flagged,
    }
}

/// The stable category label for an AIR [`AirRepairKind`].
#[must_use]
fn air_category(kind: AirRepairKind) -> &'static str {
    match kind {
        AirRepairKind::JunkColumn => "JunkColumn",
        AirRepairKind::DeadFrame => "DeadFrame",
        AirRepairKind::MissingSpriteRef => "MissingSpriteRef",
    }
}

/// A single line in the import report: one repair or flag, attributed to a source
/// `file:line`, with its tier and stable category.
///
/// Field order matters: it is the key the report sorts by (`file`, then `line_no`,
/// then `category`) so the human and JSON faces are deterministic across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportEntry {
    /// The source file the repair was found in (as the user named it on the CLI).
    pub file: String,
    /// 1-based source line number (`None` only for whole-file advisories that have
    /// no single owning line).
    pub line_no: Option<usize>,
    /// The severity tier ([`Tier::Repaired`] / [`Tier::Flagged`] / [`Tier::Advisory`]).
    pub tier: Tier,
    /// The stable category label (e.g. `"StrayLine"`, `"MissingSpriteRef"`).
    pub kind: String,
    /// The original (pre-repair) line text, trimmed of surrounding whitespace.
    pub original: String,
    /// The replacement text the repair substituted, when one applies. `None` for a
    /// drop (e.g. an [`RepairKind::EmptyExpr`] removed entirely), for a flag (no
    /// rewrite), or for an advisory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

impl ReportEntry {
    /// The sort key that makes the report deterministic: `(file, line_no, kind)`.
    ///
    /// `line_no: None` sorts before any numbered line (a whole-file advisory leads
    /// its file's section).
    fn sort_key(&self) -> (&str, usize, &str) {
        (
            self.file.as_str(),
            self.line_no.unwrap_or(0),
            self.kind.as_str(),
        )
    }
}

/// A tiered, deterministic report of every repair/flag an import produced over one
/// or more source files.
///
/// Built incrementally with [`ImportReport::add_cns`] / [`ImportReport::add_air`],
/// then rendered to a human face ([`ImportReport::render`]) or a stable, sorted
/// JSON document ([`ImportReport::to_json`]). The `--strict` gate keys off
/// [`ImportReport::has_flags`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    /// Every report entry, kept sorted by `(file, line_no, kind)` so both faces are
    /// byte-stable across runs on identical input.
    pub entries: Vec<ReportEntry>,
}

impl ImportReport {
    /// Creates an empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-sorts the entries by `(file, line_no, kind)`. Called after every
    /// `add_*` so the report is always in canonical order — never relies on
    /// insertion or `HashMap` order.
    fn resort(&mut self) {
        self.entries.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    }

    /// Folds every repair from a [`CnsOverlay`] into the report, attributing each
    /// to `file`.
    pub fn add_cns(&mut self, file: &str, overlay: &CnsOverlay) {
        for r in &overlay.repairs {
            self.entries.push(ReportEntry {
                file: file.to_string(),
                line_no: Some(r.line_no),
                // Every CNS text repair is a provably-safe rewrite.
                tier: Tier::Repaired,
                kind: cns_category(r.kind).to_string(),
                original: r.original.trim().to_string(),
                replacement: None,
            });
        }
        self.resort();
    }

    /// Folds every repair/flag from an [`AirOverlay`] into the report, attributing
    /// each to `file`.
    pub fn add_air(&mut self, file: &str, overlay: &AirOverlay) {
        for r in &overlay.repairs {
            self.entries.push(ReportEntry {
                file: file.to_string(),
                line_no: Some(r.line_no),
                tier: air_tier(r.kind),
                kind: air_category(r.kind).to_string(),
                original: r.original.trim().to_string(),
                replacement: None,
            });
        }
        self.resort();
    }

    /// All entries belonging to `tier`, in canonical sorted order.
    fn entries_in(&self, tier: Tier) -> impl Iterator<Item = &ReportEntry> {
        self.entries.iter().filter(move |e| e.tier == tier)
    }

    /// Returns `true` when the report carries at least one [`Tier::Flagged`] entry
    /// — the condition `--strict` exits non-zero on.
    #[must_use]
    pub fn has_flags(&self) -> bool {
        self.entries.iter().any(|e| e.tier == Tier::Flagged)
    }

    /// Returns `true` when the report carries no entries at all (nothing repaired,
    /// nothing flagged) — the input was clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// Per-category counts within a tier, in `(category, count)` pairs sorted by
    /// category, so the human face is deterministic.
    fn category_counts(&self, tier: Tier) -> Vec<(&'static str, usize)> {
        // Use a fixed category order rather than a HashMap so output is stable.
        const CATS: &[&str] = &[
            "StrayLine",
            "MalformedHeader",
            "EmptyKey",
            "EmptyExpr",
            "TruncatedExpr",
            "JunkColumn",
            "ColonHeader",
            "DeadFrame",
            "ZeroDimSprite",
            "MissingSpriteRef",
            "PartialSff",
            "PartialSnd",
            "Transcoded",
            "AiVarHint",
        ];
        CATS.iter()
            .filter_map(|cat| {
                let n = self.entries_in(tier).filter(|e| e.kind == *cat).count();
                (n > 0).then_some((*cat, n))
            })
            .collect()
    }

    /// Renders the human-readable report: a per-tier section (each with its
    /// per-category counts and `file:line` lines), the clean-room license reminder
    /// (printed **every** run), and — for clean input — `PASS — no repairs needed`.
    ///
    /// Deterministic for a given report: the entry list is kept sorted, and the
    /// tiers/categories are emitted in a fixed order.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("Import report\n");

        if self.is_clean() {
            out.push_str("\nPASS — no repairs needed\n");
        } else {
            out.push_str(&format!(
                "\n{} repair(s)/flag(s) across {} file(s).\n",
                self.entries.len(),
                self.distinct_files()
            ));
            for tier in [Tier::Repaired, Tier::Flagged, Tier::Advisory] {
                let counts = self.category_counts(tier);
                let total: usize = counts.iter().map(|(_, n)| n).sum();
                if total == 0 {
                    continue;
                }
                let breakdown = counts
                    .iter()
                    .map(|(c, n)| format!("{c} x{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!(
                    "\n{} ({total}): {breakdown}\n",
                    tier.label().to_uppercase()
                ));
                for e in self.entries_in(tier) {
                    let loc = match e.line_no {
                        Some(n) => format!("{}:{n}", e.file),
                        None => e.file.clone(),
                    };
                    out.push_str(&format!("  - {loc} {} — {}\n", e.kind, e.original));
                }
            }
        }

        out.push('\n');
        out.push_str(crate::validate::LICENSE_REMINDER);
        out.push('\n');
        out
    }

    /// The number of distinct source files referenced by the report.
    fn distinct_files(&self) -> usize {
        let mut files: Vec<&str> = self.entries.iter().map(|e| e.file.as_str()).collect();
        files.sort_unstable();
        files.dedup();
        files.len()
    }

    /// Serializes the report to **stable, sorted JSON** (pretty-printed).
    ///
    /// The entry list is kept sorted by `(file, line_no, kind)`, and the JSON is
    /// produced over that ordered vec, so two runs over identical input emit a
    /// **byte-identical** document. Never relies on `HashMap` iteration order.
    ///
    /// # Errors
    ///
    /// Returns [`FpError::Other`] if serialization fails (should not happen for
    /// this all-owned, no-NaN model — surfaced rather than panicking).
    pub fn to_json(&self) -> FpResult<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| FpError::Other(format!("import report JSON serialization failed: {e}")))
    }

    /// Writes the stable JSON to `dest`, refusing any destination inside an
    /// `assets/` tree (the same clean-room write guard as the overlays).
    ///
    /// # Errors
    ///
    /// - [`FpError::Other`] if `dest` lies inside an `assets/` directory, or if
    ///   serialization fails — the file is **not** written.
    /// - [`FpError::Io`] if the parent cannot be created or the file written.
    pub fn write_json(&self, dest: &Path) -> FpResult<()> {
        let json = self.to_json()?;
        write_overlay_text(&json, dest)
    }

    /// `true` when the report carries **no flagged entries** — the
    /// import-core "clean" predicate of T082 (a report may still hold
    /// [`Tier::Repaired`] rewrites and [`Tier::Advisory`] notes and be clean).
    ///
    /// This is the inverse of [`ImportReport::has_flags`]; the shipped
    /// `assets/trainingdummy` imports with zero flags, so this returns `true` for
    /// it. (Distinct from [`ImportReport::is_clean`], which is the stricter
    /// "no entries at all".)
    #[must_use]
    pub fn is_flag_free(&self) -> bool {
        !self.has_flags()
    }

    /// Number of entries of a given [`RepairKind`] (matched on the stable
    /// category label). Used by the import-core tests to assert exact tallies.
    #[must_use]
    pub fn count_kind(&self, kind: RepairKind) -> usize {
        let cat = kind.category();
        self.entries.iter().filter(|e| e.kind == cat).count()
    }

    /// Folds a single [`CoreRepair`] into the report (its tier comes from the
    /// [`RepairKind`]). Re-sorts so the report stays canonical.
    ///
    /// Part of the import-core ingestion API; later F034 passes (SFF/SND partial
    /// parse, transcode notes) feed repairs through here.
    #[allow(dead_code)]
    pub fn add_repair(&mut self, repair: CoreRepair) {
        self.entries.push(ReportEntry {
            file: repair.file,
            line_no: repair.line_no,
            tier: repair.kind.tier(),
            kind: repair.kind.category().to_string(),
            original: repair.original,
            replacement: repair.replacement,
        });
        self.resort();
    }

    /// Builds a fresh import report for a loaded character, attributing every
    /// repair to `file` (the user-facing `.def` path).
    ///
    /// Walks the compiled state graph and the asset set the live match would use:
    ///
    /// - Every `is_fallback` trigger / parameter / state-header expression splits
    ///   on whether its source was empty: an empty source is an
    ///   [`RepairKind::EmptyExpr`] ([`Tier::Repaired`], dropped); a non-empty
    ///   source is an [`RepairKind::TruncatedExpr`] ([`Tier::Flagged`], the source
    ///   preserved as `original`). Multi-value parameters iterate per **component**
    ///   so `damage = 20, 5` is never double-counted.
    /// - Every AIR frame whose `(group, image)` is absent from the SFF is an
    ///   [`RepairKind::MissingSpriteRef`] ([`Tier::Flagged`]).
    /// - Every SFF sprite with degenerate, non-linked `0×0` dimensions is a
    ///   [`RepairKind::ZeroDimSprite`] advisory ([`Tier::Advisory`]).
    #[must_use]
    pub fn from_character(file: &str, loaded: &fp_character::LoadedCharacter) -> Self {
        let mut report = Self::new();
        report.add_character(file, loaded);
        report
    }

    /// Folds a loaded character's repairs into this report. See
    /// [`ImportReport::from_character`] for the classification rules.
    pub fn add_character(&mut self, file: &str, loaded: &fp_character::LoadedCharacter) {
        // --- failed-compile expressions over the compiled state graph ----
        // Reuse the validator's static analysis: it already walks every state
        // header, trigger, and per-component parameter for `is_fallback`.
        let analysis = crate::validate::analyze(loaded);
        for failed in &analysis.failed_exprs {
            let src = failed.source.trim();
            let kind = if src.is_empty() {
                RepairKind::EmptyExpr
            } else {
                RepairKind::TruncatedExpr
            };
            self.entries.push(ReportEntry {
                file: file.to_string(),
                line_no: None,
                tier: kind.tier(),
                kind: kind.category().to_string(),
                // An EmptyExpr has no meaningful source; describe its site instead
                // so the report still localizes it.
                original: if src.is_empty() {
                    format!(
                        "state {} {} (empty expression)",
                        failed.from_state, failed.site
                    )
                } else {
                    format!("state {} {}: {}", failed.from_state, failed.site, src)
                },
                // EmptyExpr is dropped (no replacement); TruncatedExpr is flagged
                // for a human (no auto-rewrite either).
                replacement: None,
            });
        }

        // --- AIR frames referencing sprites absent from the SFF ----------
        for missing in &analysis.missing_sprites {
            self.entries.push(ReportEntry {
                file: file.to_string(),
                line_no: None,
                tier: RepairKind::MissingSpriteRef.tier(),
                kind: RepairKind::MissingSpriteRef.category().to_string(),
                original: format!(
                    "action {} frame {} -> sprite ({}, {}) not in SFF",
                    missing.action, missing.frame, missing.group, missing.image
                ),
                replacement: None,
            });
        }

        // --- degenerate (non-linked 0x0) sprites: advisory ---------------
        for (index, sprite) in loaded.sff.sprites.iter().enumerate() {
            let linked = sprite.linked_index as usize != index;
            if !linked && sprite.width == 0 && sprite.height == 0 {
                self.entries.push(ReportEntry {
                    file: file.to_string(),
                    line_no: None,
                    tier: RepairKind::ZeroDimSprite.tier(),
                    kind: RepairKind::ZeroDimSprite.category().to_string(),
                    original: format!(
                        "sprite ({}, {}) is 0x0 and not linked — renders nothing",
                        sprite.group, sprite.image
                    ),
                    replacement: None,
                });
            }
        }

        self.resort();
    }
}

/// A single repair the import collected, in the import-core (`T082`) shape.
///
/// This is the input form for [`ImportReport::add_repair`]; the report stores it
/// internally as a [`ReportEntry`] (whose `tier` is derived from the kind). It is
/// the documented model shape: `file`, an optional source `line_no`, the
/// [`RepairKind`], the `original` text, and an optional `replacement`.
///
/// Part of the import-core public model; populated by later F034 passes and by
/// callers feeding [`ImportReport::add_repair`].
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreRepair {
    /// The source file the repair was found in (as the user named it on the CLI).
    pub file: String,
    /// 1-based source line number, or `None` for a finding with no single owning
    /// line (e.g. a whole-character graph-walk finding).
    pub line_no: Option<usize>,
    /// The repair category.
    pub kind: RepairKind,
    /// The original text / a description of what was found.
    pub original: String,
    /// The replacement text, when the repair substituted one; `None` for a drop,
    /// a flag, or an advisory.
    pub replacement: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber;
    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that appends all formatted log output to a shared buffer,
    /// so a test can count the `CNS:` warnings a re-parse emits.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Parses `text` through the real CNS parser while capturing logs, and
    /// returns the number of emitted lines mentioning `CNS:` at any level.
    fn count_cns_warnings(text: &str) -> usize {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        subscriber::with_default(subscriber, || {
            let _ = fp_formats::cns::CnsFile::from_str(text);
        });
        let captured = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        captured.lines().filter(|l| l.contains("CNS:")).count()
    }

    /// The synthetic CNS from the acceptance criteria. A `[Statedef]` wraps the
    /// stray content so the parser has an owner for the (repaired) controller,
    /// and the controller carries a `type` so the repaired overlay re-parses
    /// without the parser's "untyped controller" warning.
    const DIRTY_CNS: &str = "\
[Statedef -1]
Special cancelling
t
= 5
[State 9999: Foo]
type = Null
[GarbageHeader
";

    #[test]
    fn overlay_cns_repairs_each_shape_with_expected_counts() {
        let overlay = repair_cns_text(DIRTY_CNS);
        assert_eq!(
            overlay.count(RepairKind::StrayLine),
            2,
            "`Special cancelling` and `t` are stray lines"
        );
        assert_eq!(
            overlay.count(RepairKind::EmptyKey),
            1,
            "`= 5` has an empty key"
        );
        assert_eq!(
            overlay.count(RepairKind::ColonHeader),
            1,
            "`[State 9999: Foo]` uses a colon separator"
        );
        assert_eq!(
            overlay.count(RepairKind::MalformedHeader),
            1,
            "`[GarbageHeader` is an unclosed header"
        );
        assert_eq!(overlay.repairs.len(), 5, "exactly five repairs total");
    }

    #[test]
    fn overlay_cns_reparses_with_zero_warnings() {
        // Sanity: the DIRTY input itself produces warnings.
        assert!(
            count_cns_warnings(DIRTY_CNS) > 0,
            "the dirty input must warn (negative control)"
        );
        let overlay = repair_cns_text(DIRTY_CNS);
        assert_eq!(
            count_cns_warnings(&overlay.text),
            0,
            "the repaired overlay must re-parse with zero CNS: warnings; overlay was:\n{}",
            overlay.text
        );
    }

    #[test]
    fn overlay_cns_colon_header_becomes_comma() {
        let overlay = repair_cns_text("[State 9999: Foo]\n");
        assert_eq!(overlay.text, "[State 9999, Foo]\n");
        // Re-parse: the colon header now parses as a normal controller header.
        assert_eq!(overlay.count(RepairKind::ColonHeader), 1);
    }

    #[test]
    fn overlay_cns_never_touches_a_real_key() {
        // A real `key = value` line — even with a colon in the VALUE — is kept
        // verbatim and produces no repair.
        let input = "[Statedef 0]\ntype = ChangeState\nvalue = time := 5\n";
        let overlay = repair_cns_text(input);
        assert!(overlay.is_clean(), "clean content must report no repairs");
        assert_eq!(
            overlay.text, input,
            "real keys must round-trip byte-identical"
        );
    }

    #[test]
    fn overlay_cns_clean_file_roundtrips_byte_identical() {
        let clean = "; a comment\r\n[Statedef 200]\r\ntype = S\r\n\r\n[State 200, 1]\r\ntype = Null\r\ntrigger1 = 1\r\n";
        let overlay = repair_cns_text(clean);
        assert!(overlay.is_clean());
        assert_eq!(
            overlay.text, clean,
            "CRLF clean file must be byte-identical"
        );

        // No trailing newline must also round-trip.
        let no_nl = "[Statedef 0]\ntype = S";
        let overlay = repair_cns_text(no_nl);
        assert!(overlay.is_clean());
        assert_eq!(overlay.text, no_nl);
    }

    #[test]
    fn overlay_cns_preserves_bom_and_indentation() {
        let input = "\u{feff}[Statedef 0]\n    type = S\n  Special cancelling\n";
        let overlay = repair_cns_text(input);
        // BOM preserved, indentation preserved on the kept line, and the indented
        // stray line is commented (keeping its original text after `; `).
        assert!(overlay.text.starts_with('\u{feff}'));
        assert!(overlay.text.contains("    type = S\n"));
        assert!(overlay.text.contains(";   Special cancelling\n"));
        assert_eq!(overlay.count(RepairKind::StrayLine), 1);
    }

    #[test]
    fn import_write_guard_refuses_assets_dir() {
        let overlay = repair_cns_text("Special cancelling\n");
        let dir = std::env::temp_dir().join("fp-import-guard-test");
        // A path inside an `assets/` component is refused, regardless of casing.
        for p in [
            dir.join("assets/kfm/kfm.cns"),
            dir.join("Assets/kfm/kfm.cns"),
            dir.join("ASSETS/x.cns"),
        ] {
            let err = write_overlay(&overlay, &p).expect_err("must refuse assets/ path");
            assert!(matches!(err, FpError::Other(_)));
            assert!(!p.exists(), "must not create the file when refused");
        }
    }

    #[test]
    fn import_write_guard_allows_cache_dir_and_writes() {
        let overlay = repair_cns_text("Special cancelling\n");
        let dir = std::env::temp_dir().join("fp-import-guard-ok");
        let dest = dir.join(".fp-cache/overlays/kfm.cns");
        // Clean any stale fixture from a prior run.
        let _ = std::fs::remove_file(&dest);
        write_overlay(&overlay, &dest).expect("cache-dir write must succeed");
        let written = std::fs::read_to_string(&dest).expect("overlay file exists");
        assert_eq!(written, overlay.text);
        let _ = std::fs::remove_file(&dest);
    }

    // -------------------------------------------------------------------
    // AIR overlay (T084)
    // -------------------------------------------------------------------

    /// A synthetic AIR whose action 0 references sprites (0,0), (0,1), (0,2) and
    /// carries a `2..A` junk column on its second frame. The caller's
    /// `sprite_present` predicate decides which `(group, image)` are renderable.
    const DIRTY_AIR: &str = "\
; idle
[Begin Action 0]
0, 0, 0,0, 7
0, 1, 0,0, 2..A
0, 2, 0,0, 7
";

    #[test]
    fn overlay_air_salvages_junk_column() {
        // Everything present -> no dead frames; the `2..A` column is salvaged.
        let overlay = repair_air_text(DIRTY_AIR, false, |_g, _i| true);
        assert_eq!(
            overlay.count(AirRepairKind::JunkColumn),
            1,
            "the `2..A` column is the one junk repair"
        );
        assert_eq!(overlay.count(AirRepairKind::DeadFrame), 0);
        assert_eq!(overlay.count(AirRepairKind::MissingSpriteRef), 0);
        // The salvaged line is in the overlay text; the junk is gone.
        assert!(overlay.text.contains("0, 1, 0,0, 2\n"));
        assert!(!overlay.text.contains("2..A"));
        // The clean frames are byte-preserved.
        assert!(overlay.text.contains("0, 0, 0,0, 7\n"));
    }

    #[test]
    fn overlay_air_clean_file_roundtrips_byte_identical() {
        let clean = "[Begin Action 0]\r\n0, 0, 0,0, 7\r\n0, 1, 0,0, 7\r\n";
        let overlay = repair_air_text(clean, true, |_g, _i| true);
        assert!(overlay.is_clean(), "a clean AIR raises no repairs");
        assert_eq!(
            overlay.text, clean,
            "a clean AIR round-trips byte-identical (incl. CRLF)"
        );
    }

    #[test]
    fn overlay_air_flags_missing_without_prune() {
        // (0,2) is absent; without --prune it is only flagged, line kept.
        let overlay = repair_air_text(DIRTY_AIR, false, |g, i| !(g == 0 && i == 2));
        assert_eq!(
            overlay.count(AirRepairKind::MissingSpriteRef),
            1,
            "the absent (0,2) frame is flagged"
        );
        assert_eq!(
            overlay.count(AirRepairKind::DeadFrame),
            0,
            "without --prune nothing is removed"
        );
        // The dead frame's (salvaged) line is still present in the text.
        assert!(overlay.text.contains("0, 2, 0,0, 7\n"));
    }

    #[test]
    fn overlay_air_prunes_dead_frame() {
        // (0,2) is absent; with --prune the frame is removed and reported.
        let overlay = repair_air_text(DIRTY_AIR, true, |g, i| !(g == 0 && i == 2));
        assert_eq!(
            overlay.count(AirRepairKind::DeadFrame),
            1,
            "the absent (0,2) frame is pruned"
        );
        assert_eq!(
            overlay.count(AirRepairKind::MissingSpriteRef),
            0,
            "a pruned frame is reported as DeadFrame, not just flagged"
        );
        // The dead frame line is gone; the surviving frames + junk salvage remain.
        assert!(!overlay.text.contains("0, 2, 0,0, 7"));
        assert!(overlay.text.contains("0, 0, 0,0, 7\n"));
        assert!(overlay.text.contains("0, 1, 0,0, 2\n"));
        // And the still-present overlay must still parse as a valid AIR.
        let reparsed = fp_formats::air::AirFile::from_str(&overlay.text)
            .expect("pruned overlay must re-parse");
        let action = reparsed.action(0).expect("action 0 survives");
        assert_eq!(action.frames.len(), 2, "two frames survive the prune");
    }

    #[test]
    fn overlay_air_linked_zero_dim_sprite_survives_prune() {
        // A 0x0-by-design sprite resolves to real pixels via its link, so the
        // presence oracle reports it present and --prune leaves it alone.
        // Model that here: (0,1) is "linked" so present == true even though we
        // pretend it is a 0x0 entry.
        let air = "[Begin Action 0]\n0, 0, 0,0, 7\n0, 1, 0,0, 7\n";
        let present = |g: u16, i: u16| {
            // (0,0) real, (0,1) linked-0x0 -> both renderable.
            matches!((g, i), (0, 0) | (0, 1))
        };
        let overlay = repair_air_text(air, true, present);
        assert!(
            overlay.is_clean(),
            "a linked/by-design 0x0 sprite is not dead and is not pruned"
        );
        assert_eq!(overlay.text, air, "nothing pruned -> byte-identical");
    }

    #[test]
    fn overlay_air_prune_never_empties_last_frame_of_action() {
        // Every frame of action 5 references an absent sprite. With --prune we
        // must keep the LAST one so the action never empties (AIR errors on zero
        // actions / an action with no frames is meaningless).
        let air = "[Begin Action 5]\n0, 0, 0,0, 7\n0, 1, 0,0, 7\n";
        let overlay = repair_air_text(air, true, |_g, _i| false);
        // First frame pruned, last frame downgraded to a flag.
        assert_eq!(
            overlay.count(AirRepairKind::DeadFrame),
            1,
            "all-but-last pruned"
        );
        assert_eq!(
            overlay.count(AirRepairKind::MissingSpriteRef),
            1,
            "the last surviving frame is flagged, not pruned"
        );
        // The overlay still parses and action 5 keeps exactly one frame.
        let reparsed = fp_formats::air::AirFile::from_str(&overlay.text).expect("must re-parse");
        let action = reparsed.action(5).expect("action 5 survives");
        assert_eq!(action.frames.len(), 1, "the action retains its last frame");
    }

    #[test]
    fn overlay_air_write_guard_refuses_assets_dir() {
        let overlay = repair_air_text(DIRTY_AIR, false, |_g, _i| true);
        let dir = std::env::temp_dir().join("fp-import-air-guard");
        let p = dir.join("assets/kfm/kfm.air");
        let err = write_air_overlay(&overlay, &p).expect_err("must refuse assets/ path");
        assert!(matches!(err, FpError::Other(_)));
        assert!(!p.exists(), "must not create the file when refused");
    }

    // -------------------------------------------------------------------
    // Import report: tiers, human face, stable JSON, --strict gate (T085)
    // -------------------------------------------------------------------

    /// Builds a report over the dirty CNS + a dirty AIR with one flagged
    /// (un-pruned) missing-sprite reference, so it exercises both Repaired and
    /// Flagged tiers across two files.
    fn dirty_report() -> ImportReport {
        let mut report = ImportReport::new();
        report.add_cns("kfm.cns", &repair_cns_text(DIRTY_CNS));
        // (0,2) absent, no --prune -> a flagged MissingSpriteRef + a Repaired
        // JunkColumn.
        let air = repair_air_text(DIRTY_AIR, false, |g, i| !(g == 0 && i == 2));
        report.add_air("kfm.air", &air);
        report
    }

    #[test]
    fn import_report_groups_by_tier_with_counts_and_file_line() {
        let report = dirty_report();
        let text = report.render();
        // Tier headings present, in fixed order, with totals.
        let repaired_at = text.find("REPAIRED").expect("repaired section");
        let flagged_at = text.find("FLAGGED").expect("flagged section");
        assert!(repaired_at < flagged_at, "REPAIRED before FLAGGED:\n{text}");
        // Per-category counts: 2 stray lines + the rest from DIRTY_CNS, plus the
        // AIR junk column under Repaired; the missing-sprite under Flagged.
        assert!(text.contains("StrayLine x2"), "{text}");
        assert!(text.contains("EmptyKey x1"), "{text}");
        assert!(text.contains("ColonHeader x1"), "{text}");
        assert!(text.contains("MalformedHeader x1"), "{text}");
        assert!(text.contains("JunkColumn x1"), "{text}");
        assert!(text.contains("MissingSpriteRef x1"), "{text}");
        // file:line attribution appears.
        assert!(text.contains("kfm.cns:"), "{text}");
        assert!(text.contains("kfm.air:"), "{text}");
        // License reminder prints on a non-clean run too.
        assert!(text.contains(crate::validate::LICENSE_REMINDER), "{text}");
    }

    #[test]
    fn import_report_clean_prints_pass_and_license() {
        let mut report = ImportReport::new();
        report.add_cns("clean.cns", &repair_cns_text("[Statedef 0]\ntype = S\n"));
        assert!(report.is_clean(), "no repairs -> clean");
        let text = report.render();
        assert!(
            text.contains("PASS — no repairs needed"),
            "clean content must print the PASS line:\n{text}"
        );
        assert!(
            text.contains(crate::validate::LICENSE_REMINDER),
            "the license reminder prints every run, including clean:\n{text}"
        );
    }

    #[test]
    fn import_report_json_is_stable_across_two_runs() {
        let report = dirty_report();
        let a = report.to_json().expect("json a");
        let b = report.to_json().expect("json b");
        assert_eq!(a, b, "two encodes of the same report are byte-identical");

        // A freshly rebuilt report from identical input also matches (no
        // HashMap-order leakage across construction).
        let c = dirty_report().to_json().expect("json c");
        assert_eq!(a, c, "rebuilt-from-identical-input JSON is byte-identical");

        // Sanity: the JSON actually carries the data, sorted (kfm.air entries
        // before kfm.cns because 'a' < 'c').
        let air_at = c.find("kfm.air").expect("air in json");
        let cns_at = c.find("kfm.cns").expect("cns in json");
        assert!(air_at < cns_at, "entries sorted by file:\n{c}");
        assert!(c.contains("\"flagged\""), "flagged tier serialized:\n{c}");
    }

    #[test]
    fn import_report_strict_gate_keys_off_flagged_only() {
        // A report with only Repaired entries must NOT trip --strict.
        let mut repaired_only = ImportReport::new();
        repaired_only.add_cns("a.cns", &repair_cns_text(DIRTY_CNS));
        assert!(
            !repaired_only.has_flags(),
            "all-repaired report has no flags"
        );
        assert!(!repaired_only.is_clean(), "but it is not clean either");

        // A report with a flagged missing-sprite trips --strict.
        let flagged = dirty_report();
        assert!(flagged.has_flags(), "missing-sprite flag trips --strict");

        // A clean report has neither.
        let clean = ImportReport::new();
        assert!(!clean.has_flags());
        assert!(clean.is_clean());
    }

    #[test]
    fn import_report_json_write_guard_refuses_assets_dir() {
        let report = dirty_report();
        let dir = std::env::temp_dir().join("fp-import-report-guard");
        let p = dir.join("assets/report.json");
        let err = report.write_json(&p).expect_err("must refuse assets/ path");
        assert!(matches!(err, FpError::Other(_)));
        assert!(!p.exists(), "must not create the file when refused");
    }

    #[test]
    fn import_report_json_write_to_cache_succeeds_and_matches() {
        let report = dirty_report();
        let dir = std::env::temp_dir().join("fp-import-report-ok");
        let dest = dir.join(".fp-cache/report.json");
        let _ = std::fs::remove_file(&dest);
        report
            .write_json(&dest)
            .expect("cache-dir write must succeed");
        let written = std::fs::read_to_string(&dest).expect("report file exists");
        assert_eq!(written, report.to_json().unwrap());
        let _ = std::fs::remove_file(&dest);
    }

    // -------------------------------------------------------------------
    // Import core: character `.def` graph-walk + tier model (T082)
    // -------------------------------------------------------------------

    use fp_character::loader::{
        CompiledController, CompiledExpr, CompiledState, CompiledTriggerGroup,
    };
    use fp_character::{CharacterConstants, LoadedCharacter};
    use fp_core::SpriteId;
    use fp_formats::air::{AirFile, AnimAction, AnimFrame};
    use fp_formats::sff::SffFile;
    use std::collections::HashMap;

    /// Builds a synthetic SFF whose sprites are described by
    /// `(group, image, width, height)`. A `0×0` non-linked sprite is the
    /// `ZeroDimSprite` shape; a non-zero one is renderable. All sprites link to
    /// themselves (so a `0×0` entry owns no pixels and is degenerate).
    fn sff_with(coords: &[(u16, u16, u16, u16)]) -> SffFile {
        let n = coords.len();
        let sprite_off = 512usize;
        let palette_off = sprite_off + 28 * n;
        let ldata_off = palette_off + 16;
        let ldata_len = 768 + n;
        let total = ldata_off + ldata_len;
        let mut buf = vec![0u8; total];

        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 2; // major = v2
        buf[36..40].copy_from_slice(&(sprite_off as u32).to_le_bytes());
        buf[40..44].copy_from_slice(&(n as u32).to_le_bytes());
        buf[44..48].copy_from_slice(&(palette_off as u32).to_le_bytes());
        buf[48..52].copy_from_slice(&1u32.to_le_bytes());
        buf[52..56].copy_from_slice(&(ldata_off as u32).to_le_bytes());
        buf[56..60].copy_from_slice(&(ldata_len as u32).to_le_bytes());
        buf[60..64].copy_from_slice(&(total as u32).to_le_bytes());
        buf[64..68].copy_from_slice(&0u32.to_le_bytes());

        for (i, (g, im, w, h)) in coords.iter().enumerate() {
            let o = sprite_off + i * 28;
            buf[o..o + 2].copy_from_slice(&g.to_le_bytes());
            buf[o + 2..o + 4].copy_from_slice(&im.to_le_bytes());
            buf[o + 4..o + 6].copy_from_slice(&w.to_le_bytes());
            buf[o + 6..o + 8].copy_from_slice(&h.to_le_bytes());
            buf[o + 12..o + 14].copy_from_slice(&(i as u16).to_le_bytes()); // linked=self
            buf[o + 14] = 0; // raw
            buf[o + 15] = 8; // depth
            let px_off = 768 + i;
            buf[o + 16..o + 20].copy_from_slice(&(px_off as u32).to_le_bytes());
            buf[o + 20..o + 24].copy_from_slice(&1u32.to_le_bytes());
        }
        let p = palette_off;
        buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes());
        buf[p + 12..p + 16].copy_from_slice(&768u32.to_le_bytes());

        SffFile::from_bytes(&buf).expect("synthetic SFF parses")
    }

    fn air_with(action_no: i32, sprites: &[(u16, u16)]) -> AirFile {
        let frames = sprites
            .iter()
            .map(|(g, i)| AnimFrame {
                sprite: SpriteId::new(*g, *i),
                ticks: 5,
                ..Default::default()
            })
            .collect();
        let mut map = HashMap::new();
        map.insert(
            action_no,
            AnimAction {
                action_number: action_no,
                frames,
                loopstart: 0,
            },
        );
        AirFile { actions: map }
    }

    /// A controller whose sole trigger condition is compiled from `trigger_src`
    /// (pass `""` to force an empty-expression fallback).
    fn ctrl_with_trigger(kind: &str, trigger_src: &str) -> CompiledController {
        CompiledController {
            state_number: 0,
            label: kind.to_string(),
            controller_type: Some(kind.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile(trigger_src)],
            }],
            persistent: None,
            ignorehitpause: None,
            params: HashMap::new(),
        }
    }

    fn loaded_char(
        sff: SffFile,
        air: AirFile,
        controllers: Vec<CompiledController>,
    ) -> LoadedCharacter {
        let mut states = HashMap::new();
        states.insert(
            0,
            CompiledState {
                number: 0,
                controllers,
                ..Default::default()
            },
        );
        LoadedCharacter {
            name: "Synthetic".to_string(),
            displayname: "Synthetic".to_string(),
            author: String::new(),
            localcoord: (320, 240),
            constants: CharacterConstants::default(),
            states,
            sff,
            air,
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        }
    }

    #[test]
    fn import_core_character_walk_tallies_expr_and_sprite_repairs() {
        // SFF: one real sprite (0,0) + one degenerate 0x0 sprite (10,0).
        let sff = sff_with(&[(0, 0, 8, 8), (10, 0, 0, 0)]);
        // AIR references (0,0) (present) and (99,9) (absent -> MissingSpriteRef).
        let air = air_with(0, &[(0, 0), (99, 9)]);
        // Two controllers: one empty trigger (EmptyExpr), one truncated (TruncatedExpr).
        let controllers = vec![
            ctrl_with_trigger("Null", ""),
            ctrl_with_trigger("Null", "var("),
        ];
        let loaded = loaded_char(sff, air, controllers);

        let report = ImportReport::from_character("synthetic.def", &loaded);

        assert_eq!(
            report.count_kind(RepairKind::EmptyExpr),
            1,
            "the empty trigger is an EmptyExpr (Repaired)"
        );
        assert_eq!(
            report.count_kind(RepairKind::TruncatedExpr),
            1,
            "the `var(` trigger is a TruncatedExpr (Flagged)"
        );
        assert_eq!(
            report.count_kind(RepairKind::ZeroDimSprite),
            1,
            "the 0x0 non-linked sprite is one ZeroDimSprite advisory"
        );
        assert_eq!(
            report.count_kind(RepairKind::MissingSpriteRef),
            1,
            "the (99,9) frame is one MissingSpriteRef flag"
        );

        // Tier placement: EmptyExpr + ZeroDimSprite do NOT flag; TruncatedExpr +
        // MissingSpriteRef do.
        assert!(report.has_flags(), "Truncated/Missing trip the flag gate");
        assert!(
            !report.is_flag_free(),
            "a report with flags is not flag-free"
        );
    }

    #[test]
    fn import_core_empty_expr_is_repaired_truncated_is_flagged() {
        assert_eq!(RepairKind::EmptyExpr.tier(), Tier::Repaired);
        assert_eq!(RepairKind::TruncatedExpr.tier(), Tier::Flagged);
        assert_eq!(RepairKind::ZeroDimSprite.tier(), Tier::Advisory);
        assert_eq!(RepairKind::MissingSpriteRef.tier(), Tier::Flagged);
    }

    #[test]
    fn import_core_synthetic_fixture_has_each_required_tier_and_kind() {
        // The acceptance fixture: a malformed CNS *text* (stray lines + empty key)
        // overlaid, PLUS a character graph walk that yields the empty trigger
        // (EmptyExpr, Repaired) and the zero-dim sprite advisory — exactly what
        // `import --report <char.def>` assembles internally.
        let mut report = ImportReport::new();
        report.add_cns("synthetic.cns", &repair_cns_text(DIRTY_CNS));

        let sff = sff_with(&[(0, 0, 8, 8), (10, 0, 0, 0)]);
        let air = air_with(0, &[(0, 0)]);
        let controllers = vec![ctrl_with_trigger("Null", "")];
        report.add_character("synthetic.def", &loaded_char(sff, air, controllers));

        // ≥1 StrayLine (from the CNS text overlay, DIRTY_CNS has 2).
        assert!(
            report.count_kind(RepairKind::StrayLine) >= 1,
            "≥1 StrayLine expected, got {}",
            report.count_kind(RepairKind::StrayLine)
        );
        // ≥1 EmptyExpr, recorded as Repaired.
        assert!(
            report.count_kind(RepairKind::EmptyExpr) >= 1,
            "≥1 EmptyExpr expected"
        );
        // Exactly one ZeroDimSprite advisory.
        assert_eq!(
            report.count_kind(RepairKind::ZeroDimSprite),
            1,
            "the ZeroDimSprite advisory must be present exactly once"
        );

        // The rendered human report carries each tier heading it should.
        let text = report.render();
        assert!(text.contains("StrayLine"), "{text}");
        assert!(text.contains("EmptyExpr"), "{text}");
        assert!(text.contains("ZeroDimSprite"), "{text}");
    }

    #[test]
    fn import_core_clean_character_is_flag_free() {
        // A character with only good exprs and only renderable sprites flags
        // nothing — the trainingdummy invariant in miniature (no file written).
        let sff = sff_with(&[(0, 0, 8, 8)]);
        let air = air_with(0, &[(0, 0)]);
        let controllers = vec![ctrl_with_trigger("Null", "1")];
        let report = ImportReport::from_character("clean.def", &loaded_char(sff, air, controllers));

        assert!(report.is_clean(), "no repairs at all: {report:?}");
        assert!(report.is_flag_free(), "zero Flagged -> import-core clean");
        assert_eq!(report.count_kind(RepairKind::EmptyExpr), 0);
        assert_eq!(report.count_kind(RepairKind::ZeroDimSprite), 0);
        assert_eq!(report.count_kind(RepairKind::MissingSpriteRef), 0);
    }

    #[test]
    fn import_core_repair_kind_model_is_complete_and_stable() {
        // Every RepairKind in the T082 model maps to a stable category label and a
        // tier. Asserting the full set here both documents the contract and keeps
        // the public enum exercised (no variant is dead in the model).
        let all = [
            (RepairKind::StrayLine, "StrayLine", Tier::Repaired),
            (
                RepairKind::MalformedHeader,
                "MalformedHeader",
                Tier::Repaired,
            ),
            (RepairKind::EmptyKey, "EmptyKey", Tier::Repaired),
            (RepairKind::EmptyExpr, "EmptyExpr", Tier::Repaired),
            (RepairKind::TruncatedExpr, "TruncatedExpr", Tier::Flagged),
            (RepairKind::JunkColumn, "JunkColumn", Tier::Repaired),
            (RepairKind::ColonHeader, "ColonHeader", Tier::Repaired),
            (RepairKind::DeadFrame, "DeadFrame", Tier::Repaired),
            (RepairKind::ZeroDimSprite, "ZeroDimSprite", Tier::Advisory),
            (
                RepairKind::MissingSpriteRef,
                "MissingSpriteRef",
                Tier::Flagged,
            ),
            (RepairKind::PartialSff, "PartialSff", Tier::Flagged),
            (RepairKind::PartialSnd, "PartialSnd", Tier::Flagged),
            (RepairKind::Transcoded, "Transcoded", Tier::Advisory),
            (RepairKind::AiVarHint, "AiVarHint", Tier::Advisory),
        ];
        let mut report = ImportReport::new();
        for (kind, label, tier) in all {
            assert_eq!(kind.category(), label, "stable category label");
            assert_eq!(kind.tier(), tier, "tier mapping for {label}");
            report.add_repair(CoreRepair {
                file: "model.def".to_string(),
                line_no: None,
                kind,
                original: label.to_string(),
                replacement: None,
            });
            assert_eq!(report.count_kind(kind), 1, "{label} counted once");
        }
        // Four kinds are flags (TruncatedExpr, MissingSpriteRef, PartialSff, PartialSnd).
        assert!(report.has_flags());
    }

    #[test]
    fn import_core_add_repair_derives_tier_from_kind() {
        let mut report = ImportReport::new();
        report.add_repair(CoreRepair {
            file: "x.cns".to_string(),
            line_no: Some(3),
            kind: RepairKind::Transcoded,
            original: "shift-jis content".to_string(),
            replacement: None,
        });
        report.add_repair(CoreRepair {
            file: "x.sff".to_string(),
            line_no: None,
            kind: RepairKind::PartialSff,
            original: "truncated trailer".to_string(),
            replacement: None,
        });
        // Transcoded is advisory (no flag); PartialSff is a flag.
        assert!(report.has_flags(), "PartialSff is a flag");
        assert_eq!(report.count_kind(RepairKind::Transcoded), 1);
        assert_eq!(report.count_kind(RepairKind::PartialSff), 1);
    }

    /// Asset-gated: the shipped `assets/trainingdummy` imports with zero Flagged.
    /// Runs only when the fixture is present (it is shipped + CI-tracked, so this
    /// is not gated away on CI). No file is written.
    #[test]
    fn import_core_trainingdummy_is_flag_free() {
        // Locate the workspace `assets/trainingdummy/trainingdummy.def` from the
        // crate dir (CARGO_MANIFEST_DIR == crates/fp-app).
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy/trainingdummy.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("trainingdummy loads");
        let report = ImportReport::from_character("trainingdummy.def", &loaded);
        assert!(
            report.is_flag_free(),
            "trainingdummy must import with zero Flagged; report:\n{}",
            report.render()
        );
    }
}
