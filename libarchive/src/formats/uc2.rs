//! UC2 (UltraCrypt 2) archive format adapter.
//! Wraps unarc-rs to provide EntrySource for .uc2 archives.

use std::io::{Read, Cursor};

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
pub struct Uc2Reader<R: Read + Seek + Send + 'static> {
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
            name: normalise_name(&ae.name),
            kind: EntryKind::File,
            size: Some(ae.original_size as u64),
            compressed_size: Some(ae.compressed_size as u64),
            last_modified: None,
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: None,
        }).collect();

        Ok(Box::new(Uc2Reader {
            archive_entries,
            entries,
            source,
        }))
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for Uc2Reader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        let archive_entry = self.archive_entries.get(index).ok_or_else(|| {
            Error::malformed(format!(
                "Entry {index} is out of range, the archive has {} entries",
                self.entries.len()
            ))
        })?;

        // Use unarc-rs to read this entry's data.
        // Create a new backend and read the entry using the stored ArchiveEntry.
        // UnifiedArchive::read() seeks to the entry's offset internally.
        let backend = unarc_rs::unified::UnifiedArchive::open_with_format(
            self.source.clone(),
            ArchiveFormat::Uc2,
        )?;
        let decoded_data = backend.read(archive_entry)?;

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