//! Errors.
//!
//! Every failure in this crate is an [`Error`], and the type says what kind of failure it was so
//! that a caller can tell "this file is not an archive" apart from "this entry needs a password"
//! without matching on a message.
//!
//! # Panics
//!
//! Nothing in this crate is allowed to panic on input it did not create. A panic that reaches the
//! JNI boundary aborts the process instead of unwinding into a Java exception, which turns a
//! corrupt archive into a crashed file manager. Every fallible parse returns an `Err` instead, and
//! the `zip`, `sevenz` and `tar` modules are covered by a fuzz target and a truncation sweep that
//! assert this holds. Indexing that could go out of range uses `get`, never `[]`.

use std::fmt;

/// What a format is, when reading goes wrong.
///
/// These are deliberately coarse. The point is to let a caller decide whether to try another
/// implementation, which is what the app does when the native side refuses a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The bytes are not an archive of any format this crate knows, or the signature is wrong.
    NotAnArchive,
    /// The signature is right but the structure inside it is malformed: a truncated central
    /// directory, a header field that overflows, an entry that runs past the end of the file.
    Malformed,
    /// The entry is encrypted and no password, or the wrong password, was supplied.
    Password,
    /// The format is recognised but this crate does not implement it. Distinct from `Malformed` so
    /// the app can tell "unsupported" from "corrupt" and stop trying.
    Unsupported,
    /// The entry's contents decompress to more bytes than [`Limits`] allows.
    LimitExceeded,
    /// The archive uses a feature this crate does not support, such as a method this build was not
    /// compiled with.
    Unimplemented,
    /// The underlying reader or writer failed. The original error is kept.
    Io,
}

impl ErrorKind {
    /// Whether trying a different implementation could plausibly succeed.
    ///
    /// The app uses this to decide whether to fall back to `libarchive`: a file this crate called
    /// corrupt is worth a second opinion, and a file it called unsupported is not.
    pub fn is_worth_retrying(self) -> bool {
        matches!(self, ErrorKind::NotAnArchive | ErrorKind::Malformed)
    }
}

/// Anything that can go wrong while reading or writing an archive.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// Creates a new error with the given kind and message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// An error caused by the underlying reader or writer.
    pub fn io(error: std::io::Error) -> Error {
        let kind = match error.kind() {
            // An unexpected end of file is the single most common way an archive turns out to be
            // malformed, and it reads much better if it is called that.
            std::io::ErrorKind::UnexpectedEof => ErrorKind::Malformed,
            _ => ErrorKind::Io,
        };
        Error {
            kind,
            message: error.to_string(),
            source: Some(Box::new(error)),
        }
    }

    /// Wraps this error with more context, keeping the kind.
    ///
    /// Used to say *where* in a structure something went wrong, since "invalid field" is useless
    /// on its own.
    pub fn context(mut self, message: impl fmt::Display) -> Error {
        self.message = format!("{message}: {}", self.message);
        self
    }

    /// Returns the kind of this error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Creates a `Malformed` error.
    pub fn malformed(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Malformed, message)
    }

    /// Creates an `Unsupported` error.
    pub fn unsupported(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Unsupported, message)
    }

    /// Creates an `Unimplemented` error.
    pub fn unimplemented(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Unimplemented, message)
    }

    /// Creates a `Password` error.
    pub fn password(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Password, message)
    }

    /// Creates a `LimitExceeded` error.
    pub fn limit(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::LimitExceeded, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| &**error as _)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Error {
        Error::io(error)
    }
}

/// Every `unarc-rs` error becomes one of this crate's kinds.
///
/// This lives here rather than in each legacy format module because all twelve formats need it and
/// the mapping has to be the *same* for all of them: the app decides whether to retry with
/// `libarchive` by switching on [`ErrorKind::is_worth_retrying`], so a RAR that reports
/// corruption as `Io` and an ARC that reports it as `Malformed` would send the user down two
/// different paths for the same kind of damage.
///
/// The mapping is deliberately conservative. Where `unarc-rs` wraps an external library
/// (`rars` for RAR, `delharc` for LZH, `sevenz-rust2` for 7Z) and only reports a string, there is
/// nothing to classify on, so those become [`ErrorKind::Malformed`]: an archive a third-party
/// decoder gave up on is far more often damaged than merely unfamiliar.
impl From<unarc_rs::error::ArchiveError> for Error {
    fn from(error: unarc_rs::error::ArchiveError) -> Error {
        use unarc_rs::error::ArchiveError as Source;
        let message = error.to_string();
        match error {
            // Already ours: keep the kind, and keep the cause, rather than flattening it.
            Source::Io(io) => Error::io(io),
            Source::UnsupportedFormat(_) | Source::UnsupportedMethod { .. } => {
                Error::unsupported(message)
            }
            Source::InvalidHeader { .. }
            | Source::IndexMismatch(_)
            | Source::CrcMismatch { .. }
            | Source::DecompressionFailed { .. }
            | Source::ExternalLibrary { .. } => Error::malformed(message),
            Source::CorruptedEntry { .. } => Error::malformed(message),
            Source::PasswordRequired { .. }
            | Source::EncryptionRequired { .. }
            | Source::InvalidPassword { .. } => Error::password(message),
            // A size limit is a limit whichever crate raised it, and it is not worth retrying
            // with another implementation: the same limit will be hit again.
            Source::SizeLimitExceeded { .. } => Error::limit(message),
        }
    }
}

/// Result type using this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unexpected_eof_reads_as_malformed_rather_than_as_a_plain_io_failure() {
        let error = Error::io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short",
        ));
        assert_eq!(error.kind(), ErrorKind::Malformed);
        assert!(error.kind().is_worth_retrying());
    }

    #[test]
    fn an_unsupported_format_is_not_worth_retrying() {
        assert!(!ErrorKind::Unsupported.is_worth_retrying());
        assert!(!ErrorKind::Password.is_worth_retrying());
        assert!(!ErrorKind::LimitExceeded.is_worth_retrying());
    }

    #[test]
    fn context_keeps_the_kind_and_prefixes_the_message() {
        let error = Error::malformed("bad magic").context("central directory");
        assert_eq!(error.kind(), ErrorKind::Malformed);
        assert_eq!(error.message(), "central directory: bad magic");
    }

    /// Every `unarc-rs` variant has to land on the kind this crate means by it.
    ///
    /// The app switches on `is_worth_retrying` to decide whether to fall back to `libarchive`, so
    /// a misclassified variant sends the user down the wrong path for the same kind of damage.
    /// Twelve formats share this mapping, which is exactly why it is pinned here rather than
    /// tested once per format.
    #[test]
    fn unarc_errors_map_to_the_kind_the_app_expects() {
        use unarc_rs::error::ArchiveError as Source;

        let cases: Vec<(Source, ErrorKind)> = vec![
            (
                Source::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short",
                )),
                ErrorKind::Malformed,
            ),
            (
                Source::UnsupportedFormat("weird".into()),
                ErrorKind::Unsupported,
            ),
            (
                Source::UnsupportedMethod {
                    format: "ARC".into(),
                    method: "9".into(),
                },
                ErrorKind::Unsupported,
            ),
            (
                Source::InvalidHeader {
                    format: "ARJ".into(),
                },
                ErrorKind::Malformed,
            ),
            (Source::IndexMismatch("nope".into()), ErrorKind::Malformed),
            (
                Source::CrcMismatch {
                    entry: "a".into(),
                    expected: 1,
                    actual: 2,
                },
                ErrorKind::Malformed,
            ),
            (
                Source::DecompressionFailed {
                    entry: "a".into(),
                    reason: "bad".into(),
                },
                ErrorKind::Malformed,
            ),
            (
                Source::CorruptedEntry {
                    format: "ACE".into(),
                    entry_name: "a".into(),
                    reason: "bad".into(),
                },
                ErrorKind::Malformed,
            ),
            (
                Source::ExternalLibrary {
                    library: "rars".into(),
                    message: "no".into(),
                },
                ErrorKind::Malformed,
            ),
            (
                Source::PasswordRequired {
                    path: "/tmp/a.rar".into(),
                },
                ErrorKind::Password,
            ),
            (
                Source::EncryptionRequired {
                    entry: "a".into(),
                    format: "ACE".into(),
                },
                ErrorKind::Password,
            ),
            (
                Source::InvalidPassword {
                    entry: "a".into(),
                    format: "ACE".into(),
                },
                ErrorKind::Password,
            ),
            (
                Source::SizeLimitExceeded {
                    entry: "a".into(),
                    limit: 10,
                },
                ErrorKind::LimitExceeded,
            ),
        ];

        for (source, expected) in cases {
            let error = Error::from(source);
            assert_eq!(error.kind(), expected, "{error}");
            assert!(
                !error.message().is_empty(),
                "a mapped error must still say what went wrong"
            );
        }
    }

    /// An `Io` error keeps its cause, so the app's own channel failure is still visible.
    #[test]
    fn an_unarc_io_error_keeps_the_original_cause() {
        let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the channel went away");
        let error = Error::from(unarc_rs::error::ArchiveError::Io(io));
        assert!(
            std::error::Error::source(&error).is_some(),
            "the original error must not be discarded"
        );
    }
}
