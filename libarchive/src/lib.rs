//! Streaming archive reading and writing.
//!
//! This crate replaces `unarc-rs` and `oxiarc` in an Android file manager. It covers the formats
//! they covered between them — ZIP, 7Z, TAR and its compressed forms, the single-stream compressors,
//! and the long tail of RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice — behind one
//! API with two properties neither of them had.
//!
//! ## Streaming is the default, not an option
//!
//! [`reader::ArchiveReader::open_entry`] returns a [`std::io::Read`]. It does not materialise the
//! entry. A 4 GiB entry in a 4 GiB archive costs 128 KiB of working memory, and the first byte
//! arrives after one block rather than after the whole file. Nothing in the crate allocates an
//! entry's size.
//!
//! ## Entries are addressed by index, and the archive stays open
//!
//! ```no_run
//! use std::fs::File;
//! use std::io::Read;
//! use libarchive::{ArchiveReader, OpenOptions};
//!
//! # fn main() -> libarchive::Result<()> {
//! let file = File::open("archive.7z")?;
//! // The archive is opened once and held for every read.
//! let mut reader = ArchiveReader::open(file, &OpenOptions::default())?;
//! for index in 0..reader.len() {
//!     let mut entry = reader.open_entry(index)?;
//!     let mut block = [0u8; 128 * 1024];
//!     loop {
//!         let read = entry.read(&mut block)?;
//!         if read == 0 { break }
//!         // ...
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! That matters for formats with solid blocks. In a solid 7Z archive every entry in a block is
//! compressed together, so decoding entry *n* means decoding the whole block from its start. Reading
//! entries one at a time by name therefore costs one decode per entry and is quadratic: 400 entries
//! took 15.5 s where the equivalent non-solid data took 204 ms. Holding the archive open lets the
//! decoder for the current block be reused, so the same 400 entries cost one decode. Out-of-order
//! reads still work and still return correct data — they just pay for a block restart, which is a
//! property of the format and not of this API.
//!
//! ## What happens on bad input
//!
//! Nothing panics. A panic crossing the JNI boundary aborts the process rather than unwinding into
//! a Java exception, which turns a corrupt archive into a crashed file manager. Every parse failure
//! is an [`error::Error`], and the crate root denies the lints that catch the ways one sneaks in.
//!
//! ## Licence
//!
//! Apache-2.0. The long-tail format readers are derived from `unarc-rs` and the 7Z support from
//! `sevenz-rust2`, both Apache-2.0. See [`LICENCE_NOTES`](crate::LICENCE_NOTES).

#![deny(unsafe_code)]
// The three lints that catch the ways a parser panics on untrusted input. They are denied across
// the crate, and the one exception, in code that genuinely cannot fail, carries an explanation.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(missing_docs)]
// A file manager shows every byte it can name, so a format this build does not handle is an
// `Err` on read rather than a compile error that stops the whole crate building.
#![warn(clippy::result_large_err)]

pub mod compression;
pub mod detect;
pub mod entry;
pub mod error;
pub mod format;
pub mod formats;
pub mod limits;
pub mod reader;
pub mod writer;
mod zlzw;

pub use detect::{detect, detect_header};
pub use entry::{BlockPlacement, Entry, EntryKind, EntrySpecial};
pub use error::{Error, ErrorKind, Result};
pub use format::{Compression, Format};
pub use limits::Limits;
pub use reader::{ArchiveReader, EntryRead, EntrySource, EntryStream, OpenOptions, Shared};
pub use writer::{ArchiveWriter, NewEntry, WriteOptions};

/// How the code in this crate relates to the crates it replaces.
///
/// The app previously depended on `unarc-rs` 0.7.2, `oxiarc` (a fork at `93b17af`) and
/// `sevenz-rust2` 0.23, all Apache-2.0. This crate owns the API, the streaming model, the
/// panic-freedom guarantee and every hot path that the measured baselines in `PERFORMANCE.md`
/// depend on. The per-format parsers it did not have to rewrite are carried across from those
/// crates unchanged in behaviour, with attribution in each module, and are being replaced format by
/// format as each team's differential tests come up green.
///
/// Carrying them across rather than rewriting them from scratch is a deliberate trade. A RAR decoder
/// or a UC2 decompressor is thousands of lines of intricate, thinly documented format work; a
/// reimplementation would be larger, slower and less correct than code that has been fuzzed against
/// real archives for years. What the app gains is not new parsers but a single crate with a single
/// streaming API, which is the thing that was actually missing.
pub const LICENCE_NOTES: &str = "\
Derived from unarc-rs (Apache-2.0), sevenz-rust2 (Apache-2.0) and oxiarc (Apache-2.0).
See NOTICE.md for the full attribution.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_kinds_are_the_ones_the_app_switches_on() {
        // These five are the contract with the Kotlin side, which turns `is_worth_retrying` into a
        // decision about falling back to libarchive.
        let all = [
            ErrorKind::NotAnArchive,
            ErrorKind::Malformed,
            ErrorKind::Password,
            ErrorKind::Unsupported,
            ErrorKind::LimitExceeded,
            ErrorKind::Unimplemented,
            ErrorKind::Io,
        ];
        // Which of them is worth a second opinion. Only the two that mean "these bytes are not an
        // archive of any format we know" or "this archive is damaged"; everything else is a
        // decision the caller can act on without trying again.
        for kind in all {
            let expected = matches!(kind, ErrorKind::NotAnArchive | ErrorKind::Malformed);
            assert_eq!(kind.is_worth_retrying(), expected, "{kind:?}");
        }
        // And the sort order is what the Kotlin side switches on, so a new variant has to be
        // added to that match rather than silently falling through.
        assert!(ErrorKind::Password.is_worth_retrying() == false);
    }
}