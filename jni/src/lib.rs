//! The JNI bridge.
//!
//! **Owned by Team 7.** Scaffolding only: the exports the Kotlin side already declares, so that the
//! existing `RustArchive.kt` keeps working while the library underneath is swapped out.
//!
//! # What belongs here and what does not
//!
//! Everything here is a translation and nothing here is a decision. The formats, the entry order,
//! the error kinds and the streaming behaviour all belong to [`libarchive`]. This crate turns a
//! `jlong` handle into a reader, turns what comes back into the string array `RustArchive.kt`
//! expects, and turns an error into a `java.io.IOException` because that is what every caller on
//! the other side already handles.
//!
//! # The two rules a JNI boundary imposes
//!
//! **A panic here aborts the process.** Rust unwinds into `extern "system"` on Android as an
//! abort, not as an exception, so a `Result` that is unwrapped or an index that is out of range
//! turns a corrupt archive into a crashed file manager. Every fallible call in this crate returns
//! an error to Java rather than unwinding.
//!
//! **Every call crosses into Java.** A `write` of 32 bytes costs the same as a `write` of 128 KiB
//! once the two context switches are counted, which is why the writer coalesces its headers and
//! why reads happen 64 KiB at a time. See `libarchive::writer`.

pub use libarchive::{ArchiveReader, ArchiveWriter, Error, Format, Result};

/// The exception every failure is reported as.
///
/// `java/io/IOException` rather than a crate-specific one because every caller on the Kotlin side
/// already catches it, and introducing a new exception type would mean touching all of them for no
/// gain.
pub const IO_EXCEPTION: &str = "java/io/IOException";

/// The name a raw `.xz`, `.gz` or `.zst` stream's single entry is given.
///
/// A compressed stream is one file with no name of its own, and the listing needs something to put
/// in the name column. This is the string the app already uses, so it is unchanged.
pub const SINGLE_ENTRY_NAME: &str = "content";

/// How many bytes move across JNI per read.
///
/// Large enough that the boundary stops dominating and small enough that the temporary buffer on
/// the native side of each call stays cheap. Matches the Kotlin side's `CHUNK_SIZE`.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// How many fields a metadata array carries, in the order `RustArchive.Reader` reads them.
pub const METADATA_FIELDS: usize = 7;

/// The index of each field in the metadata array.
///
/// The order is part of the contract with the Kotlin side and is asserted by a test on both sides.
/// It is not changed for tidiness; if a field is added, it goes at the end and both sides change
/// together.
pub mod field {
    pub const NAME: usize = 0;
    pub const SIZE: usize = 1;
    pub const LAST_MODIFIED_MILLIS: usize = 2;
    pub const SYMLINK_TARGET: usize = 3;
    pub const IS_ENCRYPTED: usize = 4;
    pub const IS_DIRECTORY: usize = 5;
    pub const IS_SYMLINK: usize = 6;
}

/// The handle a failed open returns.
///
/// Zero, so that a caller that ignores the failure gets "the archive is closed" from the next call
/// rather than dereferencing null. The Kotlin side checks it explicitly and says so.
pub const NULL_HANDLE: i64 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_field_order_is_the_one_the_kotlin_side_reads() {
        // RustArchive.kt: METADATA_NAME=0, SIZE=1, LAST_MODIFIED_MILLIS=2, SYMLINK_TARGET=3,
        // IS_ENCRYPTED=4, IS_DIRECTORY=5, IS_SYMLINK=6.
        assert_eq!(field::NAME, 0);
        assert_eq!(field::SIZE, 1);
        assert_eq!(field::LAST_MODIFIED_MILLIS, 2);
        assert_eq!(field::SYMLINK_TARGET, 3);
        assert_eq!(field::IS_ENCRYPTED, 4);
        assert_eq!(field::IS_DIRECTORY, 5);
        assert_eq!(field::IS_SYMLINK, 6);
        assert_eq!(METADATA_FIELDS, field::IS_SYMLINK + 1);
        assert_eq!(METADATA_FIELDS, 7);
    }

    #[test]
    fn the_chunk_size_agrees_with_the_kotlin_side() {
        // RustArchive.kt: `private const val CHUNK_SIZE = 64 * 1024`. If these drift, every read
        // allocates the wrong amount and no test notices, so it is asserted here.
        assert_eq!(CHUNK_SIZE, 64 * 1024);
    }

    #[test]
    fn a_failed_open_returns_zero_rather_than_null_so_a_later_call_says_closed() {
        assert_eq!(NULL_HANDLE, 0);
    }
}