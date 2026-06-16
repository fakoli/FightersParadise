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
use fp_formats::cns::{section_header, strip_comment, SectionKind};

/// The kind of repair applied to a single CNS/CMD line.
///
/// Each variant corresponds to exactly one of the parser's recoverable
/// `CNS:`-warning shapes; the overlay rewrites the line so that shape no longer
/// warns on re-parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairKind {
    /// A non-blank line with no `=` that is not a section header (e.g. a stray
    /// keyword like `Special cancelling` or a bare token `t`). Commented out.
    StrayLine,
    /// A `key = value` line whose key (the text before the first `=`) is empty
    /// (e.g. `= 5`). Commented out.
    EmptyKey,
    /// A well-formed, parseable header that uses a colon as its number/label
    /// separator (e.g. `[State 9999: Foo]`). The colon is rewritten to a comma
    /// **in the header only**.
    ColonHeader,
    /// A line that opens a section (`[`) but does not parse as a recoverable
    /// `[Statedef N]` / `[State N, label]` header — either it is not closed with
    /// `]` (`[GarbageHeader`) or its state number is non-numeric. Commented out
    /// with an `[unparsed]` marker.
    MalformedHeader,
}

impl RepairKind {
    /// A short, stable human label for the category (used in reports).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RepairKind::StrayLine => "StrayLine",
            RepairKind::EmptyKey => "EmptyKey",
            RepairKind::ColonHeader => "ColonHeader",
            RepairKind::MalformedHeader => "MalformedHeader",
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
    std::fs::write(dest, overlay.text.as_bytes())?;
    Ok(())
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
}
