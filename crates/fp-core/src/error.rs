//! Unified error handling for the Fighters Paradise engine.
//!
//! The engine follows a "never crash on bad content" philosophy. MUGEN community
//! content is notoriously inconsistent, so parsers should collect warnings and
//! substitute safe defaults rather than failing hard. This error type covers
//! cases where recovery is not possible or where the caller needs to know
//! something went wrong.

/// Unified error type for the Fighters Paradise engine.
///
/// All crates in the workspace use this error type (via [`FpResult`]) for
/// recoverable errors. For content warnings that don't prevent loading,
/// use `tracing::warn!` instead.
#[derive(Debug, thiserror::Error)]
pub enum FpError {
    /// An I/O error occurred while reading a file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A file format could not be parsed.
    ///
    /// Includes the format name and a description of what went wrong.
    #[error("parse error in {format}: {message}")]
    Parse {
        /// The file format being parsed (e.g., "SFF", "AIR", "CNS").
        format: &'static str,
        /// Description of the parsing failure.
        message: String,
    },

    /// A required resource was not found.
    ///
    /// For example, a sprite or animation referenced by a character state
    /// does not exist in the loaded files.
    #[error("{kind} not found: {description}")]
    NotFound {
        /// What kind of resource is missing (e.g., "sprite", "animation", "state").
        kind: &'static str,
        /// Additional details about which resource was expected.
        description: String,
    },

    /// The requested feature or format version is not yet implemented.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A rendering or GPU error occurred.
    #[error("render error: {0}")]
    Render(String),

    /// A generic error with a descriptive message.
    ///
    /// Use sparingly; prefer more specific variants when possible.
    #[error("{0}")]
    Other(String),
}

impl FpError {
    /// Creates a parse error for the given format.
    pub fn parse(format: &'static str, message: impl Into<String>) -> Self {
        Self::Parse {
            format,
            message: message.into(),
        }
    }

    /// Creates a not-found error for the given resource kind.
    pub fn not_found(kind: &'static str, description: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            description: description.into(),
        }
    }
}

/// A convenience type alias for `Result<T, FpError>`.
pub type FpResult<T> = Result<T, FpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_display() {
        let err = FpError::parse("SFF", "invalid signature");
        assert_eq!(err.to_string(), "parse error in SFF: invalid signature");
    }

    #[test]
    fn not_found_error_display() {
        let err = FpError::not_found("sprite", "(0, 0)");
        assert_eq!(err.to_string(), "sprite not found: (0, 0)");
    }

    #[test]
    fn io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let fp_err: FpError = io_err.into();
        assert!(fp_err.to_string().contains("file missing"));
    }
}
