//! 7Z, and the solid-block machinery that goes with it.
//!
//! **Owned by Team 2.**
//!
//! # Why this module is the hard one
//!
//! A 7Z archive is a list of *folders*. Each folder is a run of entries compressed together with
//! one codec chain, and each entry in it is a byte range of the folder's output. Decoding any entry
//! therefore means decoding its whole folder from the start.
//!
//! The consequence is arithmetic. Reading 400 entries from one solid folder by calling a
//! name-based `read_file` per entry decodes the folder once per entry: 400 decodes of a block
//! meant to be decoded once. Measured, that is 15.5 s against 204 ms for the same data stored
//! non-solid. It is not a slow codec; it is a quadratic loop in how the entries are addressed.
//!
//! # What fixes it
//!
//! Three things, and they only work together:
//!
//! 1. **Index-addressed access.** [`Entry::block`] records which folder an entry is in, so two
//!    entries in the same folder are known to be in the same folder without decoding anything.
//! 2. **A persistent handle.** The archive stays open between reads, so the decoder for the
//!    current folder is still there when the next entry is asked for.
//! 3. **A forward-only folder cursor.** The decoder advances through the folder and hands out
//!    byte ranges as it goes. Asking for the next entry continues; asking for an earlier one
//!    restarts the folder, which is the format's own cost and cannot be avoided.
//!
//! An earlier attempt at this kept a single-forward-pass cache keyed by entry *name* inside the
//! native reader. It worked — 60 solid entries became one 2.65 s pass plus 5 us cache hits — and it
//! was still slower in the real app, because the app opens a fresh archive per file, so every read
//! started with an empty cache and the pass then decoded ~32 MiB ahead that nothing consumed. That
//! is why item 2 above is not optional, and why [`crate::ArchiveReader`] is designed to be held
//! open rather than reopened per entry.
//!
//! # Key order warning
//!
//! `sevenz-rust2`'s `for_each_entries` yields folder entries *first*, then entries with no stream
//! of their own. Its order is therefore not the same as `archive.files`. Anything keyed by position
//! taken from one and compared against the other is a bug, and `solid.zip` in this repository's
//! test corpus is the case that catches it.

use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::entry::Entry;
use crate::reader::{EntrySource, OpenOptions};
use crate::writer::{EntryWriter, WriteOptions};

/// The six bytes every 7Z file starts with.
pub const SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// How many bytes a folder may expand to before the reader gives up.
///
/// A 7Z folder's output size is recorded, so this can be checked from the header alone, before any
/// memory is committed. A file claiming a 4 EiB folder is refused rather than attempted.
pub const DEFAULT_MAX_FOLDER_SIZE: u64 = 4 * 1024 * 1024 * 1024;

pub struct SevenZReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek> SevenZReader<R> {
    pub fn open(_source: R, _options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("7Z reading is Team 2's work in progress"))
    }
}

pub struct SevenZWriter<W> {
    _sink: std::marker::PhantomData<W>,
}

impl<W: Write + Seek> SevenZWriter<W> {
    pub fn create(_sink: W, _options: &WriteOptions) -> Result<Box<dyn EntryWriter<W>>> {
        Err(Error::unimplemented("7Z writing is Team 2's work in progress"))
    }
}

/// Whether two entries are in the same folder, which is what decides whether a decode can be
/// reused.
///
/// Kept here rather than only in the reader because this is the rule every 7Z path has to obey,
/// and getting it wrong is the bug that made the first attempt at this slower than what it replaced.
pub fn same_folder(first: &Entry, second: &Entry) -> bool {
    match (first.block, second.block) {
        (Some(a), Some(b)) => a.block_index == b.block_index,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{BlockPlacement, EntryKind};

    fn entry(block: Option<usize>, offset: u64) -> Entry {
        Entry {
            name: format!("e{block:?}"),
            kind: EntryKind::File,
            size: None,
            compressed_size: None,
            last_modified: None,
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: block.map(|block_index| BlockPlacement {
                block_index,
                offset_in_block: offset,
                block_size: 1024,
            }),
        }
    }

    #[test]
    fn the_signature_is_the_six_bytes_the_format_specifies() {
        assert_eq!(SIGNATURE, [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]);
    }

    #[test]
    fn two_entries_in_one_folder_are_recognised_so_the_decode_is_shared() {
        assert!(same_folder(&entry(Some(0), 0), &entry(Some(0), 512)));
        assert!(!same_folder(&entry(Some(0), 0), &entry(Some(1), 0)));
    }

    #[test]
    fn an_entry_with_no_folder_is_never_treated_as_sharing_one_with_anything() {
        // The bug this guards against: keying by position across two different orderings, so an
        // entry with no stream of its own lines up with an arbitrary folder and silently reads
        // the wrong bytes.
        assert!(!same_folder(&entry(None, 0), &entry(None, 0)));
        assert!(!same_folder(&entry(None, 0), &entry(Some(0), 0)));
    }
}