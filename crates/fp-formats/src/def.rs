//! # DEF — INI-like configuration file parser
//!
//! Parses MUGEN `.def` files which use a simple INI-style format with sections
//! and key-value pairs. Used for character definitions, stage definitions, and
//! system configuration.
//!
//! # Format
//!
//! ```text
//! ; comment
//! [Info]
//! name = "Kung Fu Man"
//! author = "Elecbyte"
//!
//! [Files]
//! sprite = kfm.sff
//! anim = kfm.air
//! ```

use std::collections::HashMap;
use std::path::Path;

use fp_core::FpResult;

/// A parsed DEF file containing sections of key-value pairs.
///
/// MUGEN DEF files follow a simple INI-like format. Sections are identified
/// by `[SectionName]` headers, and each section contains `key = value` pairs.
/// Keys and section names are case-insensitive.
#[derive(Debug, Clone)]
pub struct DefFile {
    /// Map from lowercase section name to key-value pairs.
    pub sections: HashMap<String, HashMap<String, String>>,
}

impl DefFile {
    /// Loads and parses a DEF file from the given path.
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_str(&text)
    }

    /// Parses a DEF file from a string.
    ///
    /// Tolerates a leading UTF-8 BOM (real MUGEN `.def` files are sometimes saved
    /// UTF-8-with-BOM) and CRLF line endings.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> FpResult<Self> {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current_section: Option<String> = None;

        // Strip a leading UTF-8 BOM if present so the first line parses cleanly.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        for raw_line in text.lines() {
            // `lines()` already strips the trailing `\r` of CRLF endings.
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Check for section header [Name]
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].trim().to_ascii_lowercase();
                current_section = Some(name.clone());
                sections.entry(name).or_default();
                continue;
            }

            // Parse key = value
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim().to_ascii_lowercase();
                let value = line[eq_pos + 1..].trim().to_string();
                // Strip surrounding quotes from value if present
                let value = strip_quotes(&value);

                if let Some(ref section) = current_section {
                    sections
                        .entry(section.clone())
                        .or_default()
                        .insert(key, value);
                }
            }
        }

        Ok(Self { sections })
    }

    /// Gets a value from the given section and key.
    ///
    /// Both section and key are matched case-insensitively.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(&section.to_ascii_lowercase())
            .and_then(|s| s.get(&key.to_ascii_lowercase()))
            .map(|s| s.as_str())
    }

    /// Gets a value and parses it as the given type.
    pub fn get_parsed<T: std::str::FromStr>(&self, section: &str, key: &str) -> Option<T> {
        self.get(section, key).and_then(|v| v.parse().ok())
    }

    /// Resolves a file path relative to the DEF file's directory.
    ///
    /// MUGEN file references in DEF files are relative to the DEF file's location.
    pub fn resolve_path(def_path: &Path, relative: &str) -> std::path::PathBuf {
        let dir = def_path.parent().unwrap_or(Path::new("."));
        dir.join(relative)
    }
}

/// Strip `;` comments from a line.
fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

/// Strip surrounding double quotes from a value.
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DEF: &str = r#"
; Character definition
[Info]
name = "Kung Fu Man"
displayname = "Kung Fu Man"
author = "Elecbyte"

[Files]
sprite = kfm.sff
anim = kfm.air
sound = kfm.snd
cmd = kfm.cmd
cns = kfm.cns

[Arcade]
intro.storyboard =
ending.storyboard =
"#;

    #[test]
    fn parse_def_sections() {
        let def = DefFile::from_str(SAMPLE_DEF).unwrap();
        assert!(def.sections.contains_key("info"));
        assert!(def.sections.contains_key("files"));
        assert!(def.sections.contains_key("arcade"));
    }

    #[test]
    fn get_values() {
        let def = DefFile::from_str(SAMPLE_DEF).unwrap();
        assert_eq!(def.get("Info", "name"), Some("Kung Fu Man"));
        assert_eq!(def.get("Files", "sprite"), Some("kfm.sff"));
        assert_eq!(def.get("Files", "anim"), Some("kfm.air"));
    }

    #[test]
    fn case_insensitive() {
        let def = DefFile::from_str(SAMPLE_DEF).unwrap();
        assert_eq!(def.get("INFO", "NAME"), Some("Kung Fu Man"));
        assert_eq!(def.get("files", "SPRITE"), Some("kfm.sff"));
    }

    #[test]
    fn missing_key_returns_none() {
        let def = DefFile::from_str(SAMPLE_DEF).unwrap();
        assert_eq!(def.get("Info", "nonexistent"), None);
        assert_eq!(def.get("nosection", "name"), None);
    }

    #[test]
    fn empty_values() {
        let def = DefFile::from_str(SAMPLE_DEF).unwrap();
        assert_eq!(def.get("Arcade", "intro.storyboard"), Some(""));
    }

    #[test]
    fn resolve_path() {
        let def_path = Path::new("/chars/kfm/kfm.def");
        let resolved = DefFile::resolve_path(def_path, "kfm.sff");
        assert_eq!(resolved, Path::new("/chars/kfm/kfm.sff"));
    }

    #[test]
    fn leading_bom_and_crlf_tolerated() {
        // Some MUGEN `.def` files are saved UTF-8-with-BOM and CRLF-terminated;
        // the BOM can land directly on the first `[Section]` header.
        let text = "\u{feff}[Info]\r\nname = \"Kung Fu Man\"\r\nauthor = \"Elecbyte\"\r\n";
        let def = DefFile::from_str(text).unwrap();
        assert_eq!(
            def.get("Info", "name"),
            Some("Kung Fu Man"),
            "[Info] name must parse despite BOM/CRLF"
        );
        assert_eq!(def.get("Info", "author"), Some("Elecbyte"));
    }
}
