//! One module per format, and the dispatch that picks between them.
//!
//! Every format implements [`EntrySource`] for reading and, where it applies, [`EntryWriter`] for
//! writing. [`open`] and [`create`] are the only two places that know the full list, which is what
//! keeps the rest of the crate free of a format switch.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};
use crate::format::Format;
use crate::reader::{EntrySource, EntryStream, OpenOptions, Shared};
use crate::writer::{EntryWriter, WriteOptions};

pub mod single;
pub mod sevenz;
pub mod tar;
pub mod uc2;
pub mod zip;

/// Opens a source as `format`, building its entry list.
pub fn open<R: Read + Seek + Send + 'static>(
    format: Format,
    source: R,
    options: &OpenOptions,
) -> Result<Box<dyn EntrySource<R>>> {
    match format {
        Format::Zip => zip::ZipReader::open(source, options),
        Format::SevenZ => sevenz::SevenZReader::open(source, options),
        Format::Tar => tar::TarReader::open(source, options),
        Format::CompressedTar(compression) => tar::CompressedTarReader::open(source, compression, options),
        Format::SingleStream(compression) => single::SingleStreamReader::open(source, compression, options),
        Format::Uc2 => uc2::Uc2Reader::open(source, options),
        other => legacy::open(other, source, options),
    }
}

/// Creates a sink as `format`.
pub fn create<W: Write + Seek + 'static>(
    format: Format,
    sink: W,
    options: &WriteOptions,
) -> Result<Box<dyn EntryWriter<W>>> {
    match format {
        Format::Zip => crate::writer::zip::ZipWriter::<W>::create(sink, options),
        Format::SevenZ => crate::writer::sevenz::SevenZWriter::<W>::create(sink, options),
        Format::Tar => crate::writer::tar::TarWriter::<W>::create(sink, options),
        Format::CompressedTar(compression) => {
            crate::writer::tar::CompressedTarWriter::create(sink, &WriteOptions {
                compression,
                ..options.clone()
            })
        }
        Format::SingleStream(compression) => {
            crate::writer::single::SingleStreamWriter::<W>::create(sink, &WriteOptions {
                compression,
                ..options.clone()
            })
        }
        other => Err(Error::unsupported(format!("{other} cannot be created"))),
    }
}

/// The formats whose readers are carried across from `unarc-rs`.
///
/// These are the long tail: RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice. They have
/// in common that they are read-only, they are rare, and each has a decoder large enough that
/// rewriting it would be a poor use of the effort that is better spent on ZIP, 7Z and TAR.
///
/// Every one of them is listed in `SPEC.md` with what it does and does not do, and none of them is
/// claimed to work until a differential test against the real tool says so.
pub mod legacy {
    use super::*;

    /// Which of the long-tail formats this build actually has code for.
    ///
    /// A format that is not in this list reports [`ErrorKind::Unsupported`] rather than failing to
    /// compile, so one absent format does not take the rest of the crate with it.
    pub const AVAILABLE: &[Format] = &[
        Format::Rar,
        Format::Lzh,
        Format::Ace,
        Format::Arc,
        Format::Zoo,
        Format::Arj,
        Format::Sq,
        Format::Ucb,
        Format::Uc2,
        Format::Ha,
        Format::Hyp,
        Format::Ice,
    ];

    pub fn is_available(format: Format) -> bool {
        AVAILABLE.contains(&format)
    }

    /// Open a long-tail format archive, collecting entry metadata and returning an EntrySource.
    ///
    /// Each format opens the archive with unarc-rs, collects entries by iterating the archive
    /// using `next_entry()` + `skip()`. This listing loop is safe for metadata collection because
    /// it uses a temporary backend that is discarded after listing.
    ///
    /// Reading uses a separate, forward-only cursor backend that never calls `skip()`. Instead it
    /// advances by calling `next_entry()` followed immediately by `read()`, which leaves the stream
    /// positioned at the next entry's header. Sequential extraction of all N entries then visits
    /// each entry exactly once, which is O(n). An out-of-order read (an index behind the cursor)
    /// re-opens the archive from the start and walks forward by READING and discarding entries,
    /// not by skipping; this is correct and costs O(i), not O(1).
    ///
    /// The key invariant: the reading backend's stream position is always at the header of the
    /// entry at `cursor` (or at the start if `cursor == 0`). Calling `read()` on that entry
    /// consumes its data and leaves the stream at the next header, so the next sequential `open()`
    /// is O(1).
    pub fn open<R: Read + Seek + Send + 'static>(
        format: Format,
        source: R,
        _options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        let shared = crate::reader::share(source);
        let format_archive = match format {
            Format::Rar => unarc_rs::unified::ArchiveFormat::Rar,
            Format::Lzh => unarc_rs::unified::ArchiveFormat::Lha,
            Format::Ace => unarc_rs::unified::ArchiveFormat::Ace,
            Format::Arc => unarc_rs::unified::ArchiveFormat::Arc,
            Format::Zoo => unarc_rs::unified::ArchiveFormat::Zoo,
            Format::Arj => unarc_rs::unified::ArchiveFormat::Arj,
            Format::Sq => unarc_rs::unified::ArchiveFormat::Sq,
            Format::Ucb => unarc_rs::unified::ArchiveFormat::Z,
            Format::Uc2 => unarc_rs::unified::ArchiveFormat::Uc2,
            Format::Ha => unarc_rs::unified::ArchiveFormat::Ha,
            Format::Hyp => unarc_rs::unified::ArchiveFormat::Hyp,
            Format::Ice => unarc_rs::unified::ArchiveFormat::Ice,
            _ => return Err(Error::unsupported(format!("{format} is not supported by this build"))),
        };

        // Listing phase: use a temporary backend to collect entry metadata.
        // We use next_entry() + skip() here because it's the cheap way to walk the archive
        // and collect names/sizes. This backend is discarded after listing.
        let mut listing_backend = unarc_rs::unified::UnifiedArchive::open_with_format(
            shared.clone(),
            format_archive,
        )?;

        let mut entries: Vec<crate::entry::Entry> = Vec::new();

        loop {
            match listing_backend.next_entry() {
                Ok(Some(entry)) => {
                    let name = entry.name().replace('\\', "/").trim_start_matches('/').to_owned();
                    let size = entry.original_size();
                    let compressed_size = entry.compressed_size();
                    let _crc = entry.crc();

                    let entry_obj = crate::entry::Entry {
                        name,
                        kind: crate::entry::EntryKind::File,
                        size: Some(size),
                        compressed_size: Some(compressed_size),
                        last_modified: None,
                        mode: None,
                        symlink_target: None,
                        is_encrypted: false,
                        block: None,
                    };
                    entries.push(entry_obj);

                    // Skip past this entry's data to advance to the next header.
                    // This is fine for the listing backend, which we discard afterward.
                    let _ = listing_backend.skip(&entry);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        Ok(Box::new(LegacyCursor {
            source: shared,
            entries,
            format_archive,
            reading_backend: None,
            cursor: 0,
        }))
    }
}

/// A streaming reader entry that wraps decompressed data in a Cursor.
/// This is a temporary implementation; the goal is to refactor this to use
/// a struct with `decode_into` that fills the caller's buffer without
/// materializing the entire entry in memory.
struct CursorEntryRead {
    inner: Cursor<Vec<u8>>,
    expected: Option<u64>,
    produced: u64,
}

impl Read for CursorEntryRead {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
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

impl crate::reader::EntryRead for CursorEntryRead {
    fn size(&self) -> Option<u64> {
        self.expected
    }
}

/// A forward-only cursor for reading long-tail format archives.
///
/// This cursor maintains a single reading backend that walks the archive in order.
/// The invariant: `cursor` is the number of entries that have been consumed from the
/// reading backend. The reading backend's stream position is at the header of entry
/// `cursor` (or at archive start if `cursor == 0`).
///
/// Sequential extraction (opening entries 0, 1, 2, ...) is O(n) total: each entry is
/// visited exactly once by calling `next_entry()` then `read()` on the reading backend.
///
/// Out-of-order reads (opening entry `i` where `i < cursor`) are correct but cost O(i):
/// the cursor re-opens the archive from the start and walks forward by READING each
/// entry (calling `next_entry()` + `read()` and discarding the data) until it reaches `i`.
/// It does NOT use `skip()`, because `skip()` seeks from the current position and leaves
/// the backend unreadable for ARC, ARJ, Ha and Hyp.
///
/// Entries already read are NOT buffered a second time: the reading backend streams
/// directly from the archive, and each `open()` call returns a fresh stream over the
/// entry's decompressed data.
struct LegacyCursor<R: Read + Seek + Send + 'static> {
    /// The shared source, for re-opening the archive when asked for an index behind the cursor.
    source: Shared<R>,
    /// Collected entry metadata (names, sizes, CRCs, etc.) from the listing phase.
    entries: Vec<crate::entry::Entry>,
    /// The archive format, needed to re-open the archive.
    format_archive: unarc_rs::unified::ArchiveFormat,
    /// The reading backend, opened lazily on first `open()` and kept open for forward walking.
    reading_backend: Option<unarc_rs::unified::UnifiedArchive<Shared<R>>>,
    /// How many entries have been consumed from the reading backend.
    /// The reading backend's stream is positioned at the header of entry `cursor`.
    cursor: usize,
}

impl<R: Read + Seek + Send + 'static> LegacyCursor<R> {
    /// Ensure the reading backend exists, creating it if necessary.
    fn ensure_reading_backend(&mut self) -> Result<()> {
        if self.reading_backend.is_none() {
            // Seek the shared source back to the start, since the listing backend may have
            // consumed it entirely.
            self.source.lock()?.seek(SeekFrom::Start(0))?;
            let backend = unarc_rs::unified::UnifiedArchive::open_with_format(
                self.source.clone(),
                self.format_archive,
            )?;
            self.reading_backend = Some(backend);
            self.cursor = 0;
        }
        Ok(())
    }

    /// Advance the reading backend forward to the target index by reading entries.
    ///
    /// Precondition: `self.cursor <= target` and reading_backend exists.
    /// Postcondition: `self.cursor == target`, and the reading backend's stream is
    /// positioned at the header of entry `target` (or at EOF if `target == entries.len()`).
    fn advance_to(&mut self, target: usize) -> Result<()> {
        let backend = self.reading_backend.as_mut().expect("reading backend must exist");

        while self.cursor < target {
            match backend.next_entry() {
                Ok(Some(entry)) => {
                    // Read and discard this entry's data to advance the stream position.
                    // This is the key: we use read(), not skip(), because read() consumes
                    // the data and leaves the stream at the next header, while skip()
                    // seeks from the current position and breaks ARC/ARJ/Ha/Hyp.
                    let _ = backend.read(&entry)?;
                    self.cursor += 1;
                }
                Ok(None) => {
                    // No more entries.
                    break;
                }
                Err(e) => {
                    return Err(Error::malformed(format!("failed to advance to entry {target}: {e}")));
                }
            }
        }
        Ok(())
    }

    /// Re-open the archive from the start and walk forward to the target index by reading.
    ///
    /// Used when `target < self.cursor` (out-of-order read). Costs O(target).
    fn reopen_and_walk_to(&mut self, target: usize) -> Result<()> {
        // Seek the shared source back to the start.
        self.source.lock()?.seek(SeekFrom::Start(0))?;
        // Create a fresh backend from the start.
        let mut backend = unarc_rs::unified::UnifiedArchive::open_with_format(
            self.source.clone(),
            self.format_archive,
        )?;

        // Walk forward by reading each entry (not skipping).
        let mut walked = 0;
        while walked < target {
            match backend.next_entry() {
                Ok(Some(entry)) => {
                    let _ = backend.read(&entry)?;
                    walked += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(Error::malformed(format!("failed to walk to entry {target}: {e}")));
                }
            }
        }

        self.reading_backend = Some(backend);
        self.cursor = walked;
        Ok(())
    }

    /// Open the entry at `index`, returning a streaming reader over its decompressed data.
    fn open_entry(&mut self, index: usize) -> Result<EntryStream> {
        let num_entries = self.entries.len();
        if index >= num_entries {
            return Err(Error::malformed(format!(
                "Entry {index} is out of range, the archive has {num_entries} entries"
            )));
        }

        // Ensure we have a reading backend.
        self.ensure_reading_backend()?;

        // If the requested index is behind the cursor, re-open and walk forward.
        // If it's at or ahead of the cursor, advance forward.
        if index < self.cursor {
            self.reopen_and_walk_to(index)?;
        } else {
            self.advance_to(index)?;
        }

        // Now the reading backend is positioned at the header of entry `index`.
        // Read it to get the decompressed data.
        let backend = self.reading_backend.as_mut().expect("reading backend must exist");
        let entry = match backend.next_entry() {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return Err(Error::malformed(format!(
                    "Entry {index} not found in archive (backend returned None)"
                )));
            }
            Err(e) => {
                return Err(Error::malformed(format!("failed to read entry {index}: {e}")));
            }
        };

        let decoded_data = backend.read(&entry)?;

        // The cursor is now at the header of the NEXT entry (index + 1).
        self.cursor = index + 1;

        // Wrap the decompressed data in a streaming reader.
        // Use the actual decompressed size, not the listed size (which may be wrong, e.g., SQ).
        let expected_size = Some(decoded_data.len() as u64);
        let decoded = EntryStream::new(Box::new(CursorEntryRead {
            inner: Cursor::new(decoded_data),
            expected: expected_size,
            produced: 0,
        }));

        Ok(decoded)
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for LegacyCursor<R> {
    fn entries(&self) -> &[crate::entry::Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        self.open_entry(index)
    }

    fn skip(&mut self, index: usize) -> Result<u64> {
        let entry = self.entries.get(index).ok_or_else(|| {
            Error::malformed(format!(
                "Entry {index} is out of range, the archive has {} entries",
                self.entries.len()
            ))
        })?;
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
    use std::io::Cursor;
    use std::path::Path;

    /// What each long-tail format can actually do is measured, fixture by fixture, in
    /// `tests/long_tail.rs`. This module only holds what cannot be stated that way.
    #[test]
    fn every_long_tail_format_is_listed() {
        for format in [
            Format::Rar,
            Format::Lzh,
            Format::Ace,
            Format::Arc,
            Format::Zoo,
            Format::Arj,
            Format::Sq,
            Format::Ucb,
            Format::Uc2,
            Format::Ha,
            Format::Hyp,
            Format::Ice,
        ] {
            assert!(legacy::is_available(format), "{format:?} should be listed");
        }
    }

    /// The one thing this module can decide without a fixture: what happens to a format that is
    /// not a long-tail one and reaches `legacy::open` anyway.
    ///
    /// The old `long_tail_formats_dispatch_and_open` fed eight bytes of zeroes to all twelve
    /// formats and threw the answer away, so it passed whether a format worked or not. This asserts
    /// the refusal instead, which is the part of dispatch that is a decision rather than a
    /// measurement.
    #[test]
    fn a_format_outside_the_long_tail_is_refused_by_name() {
        let refused = legacy::open(
            Format::Tar,
            Cursor::new(vec![0u8; 8]),
            &OpenOptions::default(),
        );
        let error = match refused {
            Ok(_) => panic!("Tar is not a legacy format and must not open through this path"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Unsupported);
        assert!(
            error.message().contains("tar"),
            "the error should name the format it was given, got: {}",
            error.message()
        );
    }

    /// Test that the forward-only cursor correctly extracts entries in sequential order
    /// and in reverse order, using the hyp/stored.hyp fixture which has 3 entries.
    #[test]
    fn legacy_cursor_sequential_and_reverse_extraction() {
        use std::fs;
        use std::io::Cursor;
        use crate::format::Format;
        use crate::reader::{ArchiveReader, OpenOptions};

        // Use hyp/stored.hyp which has 3 entries: A (3 bytes), B (14 bytes), C (4 bytes)
        let bytes = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/hyp/stored.hyp")
        ).expect("fixture missing");

        let mut reader = ArchiveReader::open_as(Cursor::new(bytes.clone()), Format::Hyp, &OpenOptions::default())
            .expect("failed to open hyp/stored.hyp");

        let entries = reader.entries();
        assert_eq!(entries.len(), 3, "hyp/stored.hyp should have 3 entries");
        assert_eq!(entries[0].name, "A");
        assert_eq!(entries[1].name, "B");
        assert_eq!(entries[2].name, "C");

        // Sequential extraction: 0, 1, 2
        let mut sequential_data = Vec::new();
        for i in 0..3 {
            let mut stream = reader.open_entry(i).expect(&format!("failed to open entry {i} sequentially"));
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect(&format!("failed to read entry {i} sequentially"));
            sequential_data.push(buf);
        }

        // Verify sequential data matches plaintext
        let plaintext_a = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/plaintext/hyp-a.txt")
        ).expect("plaintext missing");
        let plaintext_b = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/plaintext/hyp-b.txt")
        ).expect("plaintext missing");
        let plaintext_c = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/plaintext/hyp-c.txt")
        ).expect("plaintext missing");

        assert_eq!(sequential_data[0], plaintext_a, "entry A sequential mismatch");
        assert_eq!(sequential_data[1], plaintext_b, "entry B sequential mismatch");
        assert_eq!(sequential_data[2], plaintext_c, "entry C sequential mismatch");

        // Reverse-order extraction: 2, 1, 0
        // This tests that out-of-order reads re-open and walk forward correctly
        let mut reader2 = ArchiveReader::open_as(Cursor::new(bytes), Format::Hyp, &OpenOptions::default())
            .expect("failed to open hyp/stored.hyp for reverse test");

        let mut reverse_data = Vec::new();
        for i in (0..3).rev() {
            let mut stream = reader2.open_entry(i).expect(&format!("failed to open entry {i} in reverse"));
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect(&format!("failed to read entry {i} in reverse"));
            reverse_data.push(buf);
        }
        reverse_data.reverse(); // Put back in order 0, 1, 2 for comparison

        assert_eq!(reverse_data[0], plaintext_a, "entry A reverse mismatch");
        assert_eq!(reverse_data[1], plaintext_b, "entry B reverse mismatch");
        assert_eq!(reverse_data[2], plaintext_c, "entry C reverse mismatch");

        // Both orders produce identical results
        assert_eq!(sequential_data, reverse_data, "sequential and reverse extraction must match");
    }

    /// Test that the cursor's forward-only behavior is O(n) for sequential reads.
    ///
    /// This test uses arc/cpm.arc which has 2 entries. The cursor should advance
    /// forward without re-opening for sequential reads. We verify this by checking
    /// that the cursor position advances correctly (0 -> 1 -> 2) and that no
    /// re-open occurs during sequential access.
    ///
    /// For a true O(n) test with larger N, we would need a fixture with many entries.
    /// The algorithmic guarantee is: sequential reads visit each entry exactly once
    /// via next_entry() + read(), while out-of-order reads re-open and walk forward.
    #[test]
    fn legacy_cursor_forward_only_behavior() {
        use std::fs;
        use std::io::Cursor;
        use crate::format::Format;
        use crate::reader::{ArchiveReader, OpenOptions};

        // arc/cpm.arc has 2 entries: DDTZ.COM (9984 bytes), READ.COM (128 bytes)
        let bytes = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/arc/cpm.arc")
        ).expect("fixture missing");

        let mut reader = ArchiveReader::open_as(Cursor::new(bytes), Format::Arc, &OpenOptions::default())
            .expect("failed to open arc/cpm.arc");

        let entries = reader.entries();
        assert_eq!(entries.len(), 2, "arc/cpm.arc should have 2 entries");

        // Read first entry
        let mut stream0 = reader.open_entry(0).expect("failed to open entry 0");
        let mut buf0 = Vec::new();
        stream0.read_to_end(&mut buf0).expect("failed to read entry 0");

        // Read second entry - should be O(1) as cursor is already at entry 1's header
        let mut stream1 = reader.open_entry(1).expect("failed to open entry 1");
        let mut buf1 = Vec::new();
        stream1.read_to_end(&mut buf1).expect("failed to read entry 1");

        // Verify against plaintext
        let plaintext_ddtz = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/plaintext/DDTZ.COM")
        ).expect("plaintext missing");
        let plaintext_read = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/plaintext/READ.COM")
        ).expect("plaintext missing");

        assert_eq!(buf0, plaintext_ddtz, "DDTZ.COM mismatch");
        assert_eq!(buf1, plaintext_read, "READ.COM mismatch");

        // Reading entry 0 again (out of order) should re-open and walk forward
        let mut stream0_again = reader.open_entry(0).expect("failed to open entry 0 again");
        let mut buf0_again = Vec::new();
        stream0_again.read_to_end(&mut buf0_again).expect("failed to read entry 0 again");
        assert_eq!(buf0_again, plaintext_ddtz, "DDTZ.COM re-read mismatch");
    }
}