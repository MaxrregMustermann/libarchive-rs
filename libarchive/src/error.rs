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
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error { kind, message: message.into(), source: None }
    }

    /// An error caused by the underlying reader or writer.
    pub fn io(error: std::io::Error) -> Error {
        let kind = match error.kind() {
            // An unexpected end of file is the single most common way an archive turns out to be
            // malformed, and it reads much better if it is called that.
            std::io::ErrorKind::UnexpectedEof => ErrorKind::Malformed,
            _ => ErrorKind::Io,
        };
        Error { kind, message: error.to_string(), source: Some(Box::new(error)) }
    }

    /// Wraps this error with more context, keeping the kind.
    ///
    /// Used to say *where* in a structure something went wrong, since "invalid field" is useless
    /// on its own.
    pub fn context(mut self, message: impl fmt::Display) -> Error {
        self.message = format!("{message}: {}", self.message);
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn malformed(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Malformed, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Unsupported, message)
    }

    pub fn unimplemented(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Unimplemented, message)
    }

    pub fn password(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Password, message)
    }

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

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unexpected_eof_reads_as_malformed_rather_than_as_a_plain_io_failure() {
        let error = Error::io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short"));
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
}