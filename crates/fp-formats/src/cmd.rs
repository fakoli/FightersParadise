//! # CMD — Input command file parser
//!
//! Parses MUGEN `.cmd` files which define input command sequences for special
//! moves and super moves. Each command has a name, an input sequence string,
//! and timing parameters.
//!
//! # Format
//!
//! ```text
//! [Defaults]
//! command.time = 15
//! command.buffer.time = 1
//!
//! [Command]
//! name = "QCF_x"
//! command = ~D, DF, F, x
//! time = 20
//!
//! [Command]
//! name = "QCF_y"
//! command = ~D, DF, F, y
//! ```
//!
//! CMD files also contain `[Statedef -1]` and `[State -1, ...]` sections
//! which define AI/command triggers. These are ignored by this parser and
//! will be handled by the CNS parser in a later phase.

use std::path::Path;

use fp_core::FpResult;

/// Default timing values for commands.
///
/// These values apply to any [`CmdCommand`] that does not explicitly specify
/// its own `time` or `buffer.time`.
#[derive(Debug, Clone)]
pub struct CmdDefaults {
    /// Default time window for command input (in ticks). Default: 15.
    pub command_time: u32,
    /// Default buffer time after command detection (in ticks). Default: 1.
    pub command_buffer_time: u32,
}

impl Default for CmdDefaults {
    fn default() -> Self {
        Self {
            command_time: 15,
            command_buffer_time: 1,
        }
    }
}

/// A single command definition from a CMD file.
///
/// Each command maps a name (used in state controller triggers) to a raw
/// input sequence string with associated timing parameters.
#[derive(Debug, Clone)]
pub struct CmdCommand {
    /// The command name (e.g., "QCF_x").
    pub name: String,
    /// The raw command string (e.g., "~D, DF, F, x").
    pub command: String,
    /// Time window for input in ticks (overrides default if specified).
    pub time: u32,
    /// Buffer time after detection in ticks (overrides default if specified).
    pub buffer_time: u32,
}

/// A parsed CMD file.
///
/// Contains the default timing values and all command definitions. Statedef
/// and State sections are intentionally skipped — those are handled by the
/// CNS parser.
#[derive(Debug, Clone)]
pub struct CmdFile {
    /// Default timing values.
    pub defaults: CmdDefaults,
    /// All command definitions.
    pub commands: Vec<CmdCommand>,
}

/// Which section the parser is currently inside.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Section {
    /// Not inside any section (before first header).
    None,
    /// Inside `[Defaults]`.
    Defaults,
    /// Inside `[Command]`.
    Command,
    /// Inside a section we skip (`[Statedef ...]`, `[State ...]`, etc.).
    Ignored,
}

/// Accumulates fields while parsing a single `[Command]` block.
struct CommandBuilder {
    name: Option<String>,
    command: Option<String>,
    time: Option<u32>,
    buffer_time: Option<u32>,
}

impl CommandBuilder {
    fn new() -> Self {
        Self {
            name: None,
            command: None,
            time: None,
            buffer_time: None,
        }
    }

    /// Finalize this builder into a [`CmdCommand`], using the given defaults
    /// for any unset timing values. Returns `None` if `name` is missing.
    fn build(self, defaults: &CmdDefaults) -> Option<CmdCommand> {
        let name = self.name?;
        Some(CmdCommand {
            name,
            command: self.command.unwrap_or_default(),
            time: self.time.unwrap_or(defaults.command_time),
            buffer_time: self.buffer_time.unwrap_or(defaults.command_buffer_time),
        })
    }
}

impl CmdFile {
    /// Loads and parses a CMD file from the given path.
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_str(&text)
    }

    /// Parses a CMD file from a string.
    ///
    /// Tolerates a leading UTF-8 BOM (real MUGEN `.cmd` files are commonly saved
    /// UTF-8-with-BOM) and CRLF line endings.
    ///
    /// # Errors
    ///
    /// Returns [`fp_core::FpError::Parse`] only when truly unrecoverable. Missing
    /// or malformed commands are logged as warnings and skipped.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> FpResult<Self> {
        let mut defaults = CmdDefaults::default();
        let mut commands = Vec::new();
        let mut section = Section::None;
        let mut builder: Option<CommandBuilder> = None;

        // Strip a leading UTF-8 BOM if present so the first line parses cleanly.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        for raw_line in text.lines() {
            // `lines()` already strips the trailing `\r` of CRLF endings.
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Check for section header [...]
            if line.starts_with('[') && line.ends_with(']') {
                // Finalize any in-progress command before switching sections
                if let Some(b) = builder.take() {
                    match b.build(&defaults) {
                        Some(cmd) => commands.push(cmd),
                        None => {
                            tracing::warn!("CMD: [Command] block missing 'name', skipped");
                        }
                    }
                }

                let inner = line[1..line.len() - 1].trim().to_ascii_lowercase();
                if inner == "defaults" {
                    section = Section::Defaults;
                } else if inner == "command" {
                    section = Section::Command;
                    builder = Some(CommandBuilder::new());
                } else {
                    // Statedef, State, Remap, etc. — skip
                    section = Section::Ignored;
                }
                continue;
            }

            // Parse key = value within current section
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim().to_ascii_lowercase();
                let value = line[eq_pos + 1..].trim();

                match section {
                    Section::Defaults => {
                        if key == "command.time" {
                            if let Ok(v) = value.parse::<u32>() {
                                defaults.command_time = v;
                            } else {
                                tracing::warn!("CMD: invalid command.time value: {value}");
                            }
                        } else if key == "command.buffer.time" {
                            if let Ok(v) = value.parse::<u32>() {
                                defaults.command_buffer_time = v;
                            } else {
                                tracing::warn!("CMD: invalid command.buffer.time value: {value}");
                            }
                        }
                    }
                    Section::Command => {
                        if let Some(ref mut b) = builder {
                            if key == "name" {
                                b.name = Some(strip_quotes(value));
                            } else if key == "command" {
                                b.command = Some(value.to_string());
                            } else if key == "time" {
                                b.time = value.parse::<u32>().ok();
                            } else if key == "buffer.time" {
                                b.buffer_time = value.parse::<u32>().ok();
                            }
                        }
                    }
                    Section::None | Section::Ignored => {
                        // Skip lines in unknown/ignored sections
                    }
                }
            }
        }

        // Finalize last command if the file doesn't end with a new section header
        if let Some(b) = builder.take() {
            match b.build(&defaults) {
                Some(cmd) => commands.push(cmd),
                None => {
                    tracing::warn!("CMD: [Command] block missing 'name', skipped");
                }
            }
        }

        tracing::info!("CMD: loaded {} commands", commands.len());
        Ok(Self { defaults, commands })
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

    #[test]
    fn parse_single_command() {
        let text = "[Command]\nname = \"QCF_x\"\ncommand = ~D, DF, F, x\n";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1);
        assert_eq!(cmd.commands[0].name, "QCF_x");
        assert_eq!(cmd.commands[0].command, "~D, DF, F, x");
    }

    #[test]
    fn parse_multiple_commands() {
        let text = "\
[Command]
name = \"QCF_x\"
command = ~D, DF, F, x

[Command]
name = \"QCF_y\"
command = ~D, DF, F, y

[Command]
name = \"QCB_x\"
command = ~D, DB, B, x
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 3);
        assert_eq!(cmd.commands[0].name, "QCF_x");
        assert_eq!(cmd.commands[1].name, "QCF_y");
        assert_eq!(cmd.commands[2].name, "QCB_x");
    }

    #[test]
    fn parse_defaults() {
        let text = "\
[Defaults]
command.time = 20
command.buffer.time = 3

[Command]
name = \"test\"
command = x
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.defaults.command_time, 20);
        assert_eq!(cmd.defaults.command_buffer_time, 3);
        // Command should inherit the new defaults
        assert_eq!(cmd.commands[0].time, 20);
        assert_eq!(cmd.commands[0].buffer_time, 3);
    }

    #[test]
    fn command_time_override() {
        let text = "\
[Defaults]
command.time = 15
command.buffer.time = 1

[Command]
name = \"slow_move\"
command = ~D, DF, F, D, DF, F, x
time = 30
buffer.time = 5
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands[0].time, 30);
        assert_eq!(cmd.commands[0].buffer_time, 5);
    }

    #[test]
    fn default_values() {
        let text = "[Command]\nname = \"test\"\ncommand = x\n";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.defaults.command_time, 15);
        assert_eq!(cmd.defaults.command_buffer_time, 1);
        assert_eq!(cmd.commands[0].time, 15);
        assert_eq!(cmd.commands[0].buffer_time, 1);
    }

    #[test]
    fn comments_ignored() {
        let text = "\
; This is a comment
[Command] ; section comment
name = \"test\" ; inline comment
command = x ; another comment
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1);
        assert_eq!(cmd.commands[0].name, "test");
        assert_eq!(cmd.commands[0].command, "x");
    }

    #[test]
    fn case_insensitive() {
        let text = "\
[COMMAND]
name = \"upper\"
command = x

[defaults]
command.time = 25
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1);
        assert_eq!(cmd.commands[0].name, "upper");
        // Defaults parsed after the command, so command uses original default
        assert_eq!(cmd.defaults.command_time, 25);
    }

    #[test]
    fn statedef_skipped() {
        let text = "\
[Command]
name = \"real_cmd\"
command = x

[Statedef -1]

[State -1, QCF_x]
type = ChangeState
trigger1 = command = \"QCF_x\"
value = 1000
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1);
        assert_eq!(cmd.commands[0].name, "real_cmd");
    }

    #[test]
    fn empty_file() {
        let cmd = CmdFile::from_str("").unwrap();
        assert!(cmd.commands.is_empty());
        assert_eq!(cmd.defaults.command_time, 15);
    }

    #[test]
    fn missing_name_skipped() {
        let text = "\
[Command]
command = x
time = 10

[Command]
name = \"valid\"
command = y
";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1);
        assert_eq!(cmd.commands[0].name, "valid");
    }

    #[test]
    fn leading_bom_and_crlf_tolerated() {
        // Real MUGEN `.cmd` files are UTF-8-with-BOM and CRLF-terminated; the
        // BOM can land directly on a `[Command]` header.
        let text = "\u{feff}[Command]\r\nname = \"QCF_x\"\r\ncommand = ~D, DF, F, x\r\n";
        let cmd = CmdFile::from_str(text).unwrap();
        assert_eq!(cmd.commands.len(), 1, "command must parse despite BOM/CRLF");
        assert_eq!(cmd.commands[0].name, "QCF_x");
        assert_eq!(cmd.commands[0].command, "~D, DF, F, x");
    }
}
