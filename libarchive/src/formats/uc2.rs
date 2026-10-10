//! UC2 (UltraCrypt 2) archive format adapter.
//! Wraps unarc-rs to provide EntrySource for .uc2 archives.
//!
//! # This does not stream yet, and must not be mistaken for working
//!
//! Two of the three properties this crate exists for are here:
//!
//! - **The archive stays open.** The `UnifiedArchive` backend is built once and held, so the
//!   central directory is parsed once rather than once per entry. An earlier version of this file
//!   called `open_with_format` inside `open`, which re-parsed the directory for every entry read.
//!   That is invisible in a test that reads one entry and quadratic in the app's real access
//!   pattern, which is one archive and many files from it.
//! - **Entries are addressed by index** into an archive held open, not looked up by name.
//!
//! The third is missing. `open` calls `unarc-rs`'s `read`, which returns a `Vec<u8>` of the whole
//! decompressed entry, and wraps it in a `Cursor`. So an entry is still materialised before the
//! caller sees a byte — the thing this crate was built to remove, inherited from the crate it
//! replaces. `Uc2EntryRead` is named as though it streams and does not; renaming it is part of the
//! work, so that the type and the behaviour cannot drift apart again.
//!
//! # Why streaming is reachable here
//!
//! `unarc-rs`'s UC2 decompressor is already a loop over blocks. `uc2/decompress.rs:392` holds
//! `bits`, `circ_buf` (the LZSS dictionary) and `symprev` as locals and calls `decompress_block`
//! once per block; the only reason it returns a `Vec` is that `output` is one. Turning that
//! function into a struct whose state survives across `read()` calls is the whole of the change.
//!
//! Reading the *compressed* bytes of one entry into a `Vec` is fine and should stay: UC2 gives a
//! single offset and a compressed length, so the entry has to be seeked to, and compressed is
//! normally much smaller than decompressed. The promise is about the decompressed entry.

use std::io::{Cursor, Read, Seek};

use crate::error::{Error, Result};
use crate::entry::{Entry, EntryKind, normalise_name};
use crate::format::Format;
use crate::reader::{OpenOptions, EntryRead, EntrySource, EntryStream, Shared};
use unarc_rs::unified::{ArchiveEntry, ArchiveFormat};

// A simple entry reader that wraps a Cursor<Vec<u8>>
struct Uc2EntryRead {
    inner: Cursor<Vec<u8>>,
    expected: Option<u64>,
    produced: u64,
}

impl Read for Uc2EntryRead {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let wanted = match self.expected {
            Some(expected) => {
                let left = expected.saturating_sub(self.produced);
                if left == 0 {
                    return Ok(0);
                }
                output.len().min(left as usize)
            }
            None => output.len(),
        };
        let read = self.inner.read(&mut output[..wanted])?;
        self.produced += read as u64;
        Ok(read)
    }
}

impl EntryRead for Uc2EntryRead {
    fn size(&self) -> Option<u64> {
        self.expected
    }
}

/// A UC2 archive reader, wrapping unarc-rs.
///
/// The backend is **held**, not rebuilt. `open_with_format` parses the central directory, so an
/// adapter that calls it inside `open` re-parses the whole directory for every entry read. That is
/// invisible in a test that reads one entry and quadratic in the one thing this crate exists for,
/// which is an app that opens an archive once and reads many files from it.
pub struct Uc2Reader<R: Read + Seek + Send + 'static> {
    /// The unarc-rs backend, opened once and kept for the life of the archive.
    backend: unarc_rs::unified::UnifiedArchive<Shared<R>>,
    /// Stored ArchiveEntry clones for random-access reading via unarc-rs read().
    archive_entries: Vec<ArchiveEntry>,
    /// Converted Entry objects for metadata listing.
    entries: Vec<Entry>,
    source: Shared<R>,
}

impl<R: Read + Seek + Send + 'static> Uc2Reader<R> {
    /// Opens a UC2 archive from a seekable source.
    ///
    /// This reads the archive once to collect the entry list with metadata
    /// (names, original sizes, compressed sizes, CRCs). After opening,
    /// entries are addressed by index for random access, and their contents
    /// are streamed on demand.
    pub fn open(
        source: R,
        options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        let shared = crate::reader::share(source);
        // Use open_with_format since UnifiedArchive::new() is not available
        let mut backend = unarc_rs::unified::UnifiedArchive::open_with_format(
            shared.clone(),
            ArchiveFormat::Uc2,
        )?;

        // Collect entries by iterating through the archive
        let mut archive_entries = Vec::new();
        while let Some(result) = backend.next_entry()? {
            let raw_entry = result;
            // Clone the ArchiveEntry for random-access reading later
            archive_entries.push(raw_entry.clone());
            // Skip the entry data to advance to the next entry
            let _ = backend.skip(&raw_entry);
        }

        // Convert ArchiveEntry metadata to our Entry type
        let entries: Vec<Entry> = archive_entries.iter().map(|ae| Entry {
            name: normalise_name(ae.name()),
            kind: EntryKind::File,
            size: Some(ae.original_size()),
            compressed_size: Some(ae.compressed_size()),
            last_modified: None,
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: None,
        }).collect();

        Ok(Box::new(Uc2Reader {
            backend,
            archive_entries,
            entries,
            source: shared,
        }))
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for Uc2Reader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        let archive_entry = self
            .archive_entries
            .get(index)
            .cloned()
            .ok_or_else(|| {
            Error::malformed(format!(
                "Entry {index} is out of range, the archive has {} entries",
                self.entries.len()
            ))
        })?;

        // `read` seeks to the entry's offset itself, so the backend does not need to be rewound
        // between entries; what it does need is to exist only once.
        let decoded_data = self.backend.read(&archive_entry)?;

        // Wrap the decompressed data in a streaming reader using our Uc2EntryRead
        let decoded = EntryStream::new(Box::new(Uc2EntryRead {
            inner: Cursor::new(decoded_data),
            expected: self.entries[index].size,
            produced: 0,
        }));

        Ok(decoded)
    }

    fn skip(&mut self, index: usize) -> Result<u64> {
        let entry = self.entries.get(index).ok_or_else(|| {
            Error::malformed(format!(
                "Entry {index} is out of range, the archive has {} entries",
                self.entries.len()
            ))
        })?;

        // Return the compressed size as the conceptual bytes skipped.
        Ok(entry.compressed_size.unwrap_or(0))
    }

    fn reuses_blocks(&self) -> bool {
        false
    }

    fn into_source(self: Box<Self>) -> Result<R> {
        self.source.into_inner()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// UC2 is dispatched rather than refused, so `detect` and `open` have to agree about it.
    ///
    /// The list in `legacy::AVAILABLE` and the match arm in `formats::open` are two lists of the
    /// same thing, and they have already drifted once: this format was added to the dispatch
    /// without the list saying so.
    #[test]
    fn uc2_is_dispatched_rather_than_refused() {
        assert!(
            crate::formats::legacy::is_available(crate::Format::Uc2),
            "a format that opens must also be listed as available"
        );
    }

    /// What the adapter actually does today, so that changing it is a deliberate act.
    ///
    /// The tripwire is a type annotation, so it is checked by the compiler rather than at run
    /// time: this stops compiling the moment `Uc2EntryRead` stops being a `Cursor<Vec<u8>>`. When
    /// that happens someone is making UC2 actually stream, and this test and the module doc both
    /// have to change in the same commit.
    #[test]
    fn the_entry_is_still_materialised_and_this_test_says_so() {
        let mut reader = Uc2EntryRead {
            inner: Cursor::new(vec![7u8; 64]),
            expected: Some(64),
            produced: 0,
        };
        let mut one = [0u8; 1];
        reader.read(&mut one).expect("a byte");
        assert_eq!(reader.produced, 1);
        let whole_entry: Cursor<Vec<u8>> = reader.inner;
        assert_eq!(
            whole_entry.get_ref().len(),
            64,
            "the entry is held whole, which is the thing to remove"
        );
    }

    /// Reading past the recorded size stops rather than handing back whatever the codec produced.
    #[test]
    fn a_reader_stops_at_the_size_the_archive_recorded() {
        let mut reader = Uc2EntryRead {
            inner: Cursor::new(vec![1, 2, 3, 4]),
            expected: Some(2),
            produced: 0,
        };
        let mut out = [0u8; 8];
        let read = reader.read(&mut out).expect("reads");
        assert_eq!(read, 2, "must not produce more than the archive said");
        assert_eq!(reader.read(&mut out).expect("reads"), 0, "and must then stop");
    }

    #[test]
    fn an_entry_with_no_recorded_size_is_not_given_one() {
        let mut reader = Uc2EntryRead {
            inner: Cursor::new(vec![1, 2, 3]),
            expected: None,
            produced: 0,
        };
        assert_eq!(reader.size(), None, "an unknown size must stay unknown, not become 0");
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).expect("reads"), 3);
    }
}
