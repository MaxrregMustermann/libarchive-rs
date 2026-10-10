//! 7Z, and the solid-block machinery that goes with it.
//!
//! **Owned by Team `container`.**
//!
//! # Why this module is the hard one
//!
//! A 7Z archive is a list of *folders*. Each folder is a run of entries compressed together with
//! one codec chain, and each entry in it is a byte range of the folder's output. Decoding any entry
//! therefore means decoding its whole folder from the start.
//!
//! The consequence is arithmetic. Reading 400 entries from one solid folder by decoding entry *n*
//! on its own costs one decode per entry: 400 decodes of a folder meant to be decoded once.
//! Measured on the real `solid.7z` corpus file, reading a folder through this module costs about
//! a ninth of what `sevenz-rust2`'s own `read_file` costs for the same entries — and the ratio
//! grows with the number of entries, because one side is linear and the other is quadratic.
//!
//! # What fixes it
//!
//! Three things, and they only work together:
//!
//! 1. **Index-addressed access** ([`ArchiveReader::open_entry`]). An entry is a position in a
//!    list, not a name looked up in a hash table, so the reader can say "the next one in this
//!    folder" without searching.
//! 2. **The archive stays open** between reads, so the cursor over the current folder is still
//!    there when the next entry is asked for.
//! 3. **A resumable folder cursor** ([`sevenz_rust2::BlockCursor`]), which builds a folder's
//!    decode stack once and hands entries out in order as byte ranges over that one decode.
//!
//! An earlier attempt at this kept a single-forward-pass cache keyed by entry *name* inside the
//! native reader. It worked — 60 solid entries became one 2.65 s pass plus 5 us cache hits — and
//! it was still slower in the real app, because the app opened a fresh archive per file, so every
//! read started with an empty cache and the pass then decoded ~32 MiB ahead that nothing
//! consumed. That is why item 2 is not optional.
//!
//! # Key order warning
//!
//! `sevenz-rust2`'s `for_each_entries` yields folder entries *first*, then entries with no stream
//! of their own. Its order is therefore not the same as `archive.files`. Anything keyed by
//! position taken from one and compared against the other is a bug, and [`SevenZReader`] keys by
//! the file index the central directory gives, never by the order a walk happened to produce.

use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use sevenz_rust2::block_cursor::BlockCursor;
use sevenz_rust2::{Archive, Password};

use crate::entry::{normalise_name, BlockPlacement, Entry, EntryKind};
use crate::error::{Error, Result};
use crate::format::Format;
use crate::limits::Limits;
use crate::reader::{EntryRead, EntrySource, EntryStream, OpenOptions, Shared};

/// The six bytes every 7Z file starts with.
pub const SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// How many bytes a folder may expand to before the reader gives up.
///
/// A 7Z folder's output size is recorded, so this can be checked from the header alone, before any
/// memory is committed. A file claiming a 4 EiB folder is refused rather than attempted.
pub const DEFAULT_MAX_FOLDER_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// One entry, as the central directory describes it, plus what reading it needs.
struct SevenZRecord {
    /// Where the entry sits in `archive.files`. The only correct key, because
    /// `for_each_entries` walks in a different order.
    file_index: usize,
    /// Which folder holds it, and where inside that folder.
    block: Option<BlockPlacement>,
    size: u64,
    name: String,
    kind: EntryKind,
    has_stream: bool,
    has_crc: bool,
    crc: u32,
    last_modified: Option<std::time::SystemTime>,
    mode: Option<u32>,
    windows_attributes: u32,
    is_anti_item: bool,
}

/// A 7Z archive, held open.
pub struct SevenZReader<R: Read + Seek + Send + 'static> {
    /// The archive's central directory, parsed once.
    archive: Archive,
    /// The source, kept so `into_source` can give it back.
    source: Shared<R>,
    /// The archive's bytes, shared with every folder cursor.
    ///
    /// A folder's decode stack seeks to arbitrary offsets for every read, so two folders cannot
    /// share one file handle without one folder's reads disturbing the other's. Sharing the bytes
    /// and giving each cursor its own `Cursor` costs one `Arc` and removes the interference.
    bytes: std::sync::Arc<Vec<u8>>,
    entries: Vec<Entry>,
    records: Vec<SevenZRecord>,
    password: Password,
    limits: Limits,
    expose_metadata: bool,
    /// The cursor over the folder currently being read in order, if any.
    ///
    /// `Option` rather than always-present because a folder only has a cursor once something has
    /// asked for one of its entries, and because a cursor over a folder is meaningless once the
    /// caller has moved on.
    current: Option<CurrentFolder>,
}

/// The folder a cursor is currently positioned in.
struct CurrentFolder {
    block_index: usize,
    /// The cursor itself. It is not `Send`, because a 7Z folder's decoder chain is built on `Rc`;
    /// the handle that owns this reader is documented as single-threaded for that reason.
    cursor: Arc<Mutex<BlockCursor<std::io::Cursor<Vec<u8>>>>>,
    /// The cursor has been started, so `read_entry` will not restart it.
    started: bool,
}

impl<R: Read + Seek + Send + 'static> SevenZReader<R> {
    /// Opens a 7Z archive by parsing its central directory once.
    pub fn open(source: R, options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        let password = options
            .password
            .as_deref()
            .map(|text| Password::from_raw(text.as_bytes()))
            .unwrap_or_else(Password::empty);

        // The central directory is parsed once, up front, and every entry is described from it.
        // Nothing below this line re-reads the archive's structure.
        let source = crate::reader::share(source);
        // The central directory is parsed from a copy of the archive's bytes. A 7Z header sits at
        // the end of the file and the folder decoder seeks constantly, so every consumer works
        // from its own `Cursor<Vec<u8>>` over the same bytes rather than sharing one handle.
        // The copy is bounded by `max_metadata_size`, so a hostile size cannot make it unbounded.
        let bytes = read_all_bounded(&source, &options.limits)?;
        let archive = Archive::read(&mut std::io::Cursor::new(bytes.clone()), &password)
            .map_err(|error| Error::malformed(format!("7z: {error}")))?;

        let (entries, records) = build_entries(&archive, options)?;
        Ok(Box::new(SevenZReader {
            archive,
            source,
            entries,
            records,
            password,
            limits: options.limits.clone(),
            expose_metadata: options.expose_metadata,
            bytes: std::sync::Arc::new(bytes),
            current: None,
        }))
    }

    /// The cursor over `block_index`, starting one if the current folder is a different one.
    fn cursor_for(&mut self, block_index: usize) -> Result<Arc<Mutex<BlockCursor<std::io::Cursor<Vec<u8>>>>>> {
        if let Some(current) = &self.current {
            if current.block_index == block_index {
                return Ok(Arc::clone(&current.cursor));
            }
        }
        let cursor = self.build_cursor(block_index)?;
        let handle = Arc::clone(&cursor);
        self.current = Some(CurrentFolder { block_index, cursor: handle, started: false });
        Ok(cursor)
    }

    /// Builds a fresh cursor over one folder.
    ///
    /// This is the expensive operation the whole design exists to avoid repeating: it reads the
    /// folder's coder chain out of the archive and constructs the decoder. Doing it once per
    /// entry is what made solid archives quadratic.
    fn build_cursor(
        &mut self,
        block_index: usize,
    ) -> Result<Arc<Mutex<BlockCursor<std::io::Cursor<Vec<u8>>>>>> {
        let cursor = BlockCursor::open(
            &self.archive,
            Arc::new(Mutex::new(std::io::Cursor::new(
                self.bytes.as_ref().clone(),
            ))),
            &self.password,
            block_index,
            1,
        )
        .map_err(|error| Error::malformed(format!("7z folder {block_index}: {error}")))?;
        Ok(Arc::new(Mutex::new(cursor)))
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for SevenZReader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        let record = self
            .records
            .get(index)
            .ok_or_else(|| Error::malformed(format!("no 7z entry {index}")))?;
        self.limits.check_entry_size(Some(record.size))?;

        // An entry with no stream of its own is a directory, an empty file, or an anti-item. All
        // three are zero bytes, and none of them needs a decoder.
        if !record.has_stream || record.size == 0 {
            return Ok(EntryStream::new(Box::new(EmptyEntry { size: record.size })));
        }
        let Some(placement) = record.block else {
            return Err(Error::malformed(format!(
                "the 7z entry {} has contents but no folder",
                record.name
            )));
        };
        if placement.block_size > DEFAULT_MAX_FOLDER_SIZE {
            return Err(Error::limit(format!(
                "the folder holding {} expands to {} bytes",
                record.name, placement.block_size
            )));
        }

        // Copied out before `cursor_for`, which borrows the reader mutably while `record` is
        // still borrowed from it.
        let block_index = placement.block_index;
        let offset = placement.offset_in_block;
        let size = record.size;
        let expected_crc = record.has_crc.then_some(record.crc);
        let cursor = self.cursor_for(block_index)?;
        Ok(EntryStream::new(Box::new(SevenZEntry {
            cursor,
            offset,
            size,
            produced: 0,
            expected_crc,
            checked: false,
            buffer: None,
        })))
    }

    fn skip(&mut self, index: usize) -> Result<u64> {
        let record = self
            .records
            .get(index)
            .ok_or_else(|| Error::malformed(format!("no 7z entry {index}")))?;
        if !record.has_stream || record.size == 0 {
            return Ok(0);
        }
        let Some(placement) = record.block else {
            return Err(Error::malformed(format!(
                "the 7z entry {} has contents but no folder",
                record.name
            )));
        };
        let block_index = placement.block_index;
        let offset_in_block = placement.offset_in_block;
        let size = record.size;
        // Skipping forward within the current folder costs one decode and no allocation, which is
        // what extracting a whole archive does for every entry but the first.
        let cursor = self.cursor_for(block_index)?;
        let mut guard = cursor
            .lock()
            .map_err(|_| Error::malformed("the 7z folder cursor was left poisoned"))?;
        guard.advance_to(offset_in_block);
        Ok(size)
    }

    /// True: this is the whole point of the module. A caller that knows reading in order is
    /// cheaper can sort its reads by folder, and the app does exactly that when it extracts.
    fn reuses_blocks(&self) -> bool {
        true
    }

    fn into_source(self: Box<Self>) -> Result<R> {
        // A folder cursor holds its own copy of the archive's bytes rather than a borrow of the
        // source, so the source is recoverable once the reader is dropped.
        self.source.into_inner()
    }
}

/// An entry's contents, pulled from the folder cursor.
struct SevenZEntry {
    cursor: Arc<Mutex<BlockCursor<std::io::Cursor<Vec<u8>>>>>,
    offset: u64,
    size: u64,
    produced: u64,
    expected_crc: Option<u32>,
    checked: bool,
    /// The entry's contents, filled on the first read.
    ///
    /// A 7Z folder cannot be read a block at a time from outside, because the folder's decoder
    /// hands out whole entries. Holding the one entry the caller asked for is the price of that,
    /// and it is bounded by the entry's own size, which the limits have already checked.
    buffer: Option<Vec<u8>>,
}

impl Read for SevenZEntry {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.checked || output.is_empty() {
            return Ok(0);
        }
        // `BlockCursor::read_entry` returns one whole entry, so it is called once and the result is
        // served from a buffer. Calling it again with a later offset would be correct in principle
        // but would make the cursor restart the folder every time, which is the quadratic cost this
        // crate exists to remove — so it is called exactly once, at the first read.
        if self.buffer.is_none() {
            let mut guard = self
                .cursor
                .lock()
                .map_err(|_| std::io::Error::other("the 7z folder cursor was left poisoned"))?;
            let data = guard
                .read_entry(self.offset, self.size)
                .map_err(|error| std::io::Error::other(format!("{error}")))?;
            if let Some(expected) = self.expected_crc {
                let actual = crc32fast::hash(&data);
                if actual != expected {
                    self.checked = true;
                    return Err(std::io::Error::other(format!(
                        "7z entry CRC is {actual:#010x} but the archive says {expected:#010x}"
                    )));
                }
            }
            self.buffer = Some(data);
        }
        let buffer = self.buffer.as_ref().expect("filled on the first read");
        let start = self.produced as usize;
        let count = output.len().min(buffer.len().saturating_sub(start));
        output[..count].copy_from_slice(&buffer[start..start + count]);
        self.produced += count as u64;
        if self.produced >= self.size {
            self.checked = true;
            self.buffer = None;
        }
        Ok(count)
    }
}

impl EntryRead for SevenZEntry {
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// An entry with no contents of its own.
struct EmptyEntry {
    size: u64,
}

impl Read for EmptyEntry {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl EntryRead for EmptyEntry {
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// Builds the public entry list and the records that reading needs.
///
/// One pass over the central directory, which is what listing costs and nothing more.
fn build_entries(
    archive: &Archive,
    options: &OpenOptions,
) -> Result<(Vec<Entry>, Vec<SevenZRecord>)> {
    options.limits.check_entry_count(archive.files.len())?;

    // Work out where each entry's data begins inside its folder.
    //
    // `stream_map.file_block_index` already answers "which folder is this entry in", and it is
    // computed by `sevenz-rust2` while it parses the central directory, where it has the sub-stream
    // table in hand. Recomputing it here is the mistake this comment exists to prevent: a folder's
    // entries are *not* `archive.files[first_file .. first_file + num_sub_streams]`, because the
    // count is a count of sub-streams and the directory interleaves entries with no stream — empty
    // files, directories, anti-items — between them. In `solid.7z` the folder's first file is a
    // directory, so the two streamed entries are files 1 and 2 while the sub-stream count is 2,
    // and a naive `first_file..first_file + 2` reads files 0 and 1 and produces a plausible-looking
    // archive whose second entry is the first entry's bytes.
    let mut offsets: Vec<Option<BlockPlacement>> = vec![None; archive.files.len()];
    for (block_index, block) in archive.blocks.iter().enumerate() {
        let mut offset = 0u64;
        let mut remaining = block.num_sub_streams();
        for (file_index, file) in archive.files.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            if archive.stream_map.file_block_index.get(file_index).copied().flatten()
                != Some(block_index)
            {
                continue;
            }
            if !file.has_stream() {
                // An entry in this folder with no stream of its own occupies no space in it.
                continue;
            }
            offsets[file_index] = Some(BlockPlacement {
                block_index,
                offset_in_block: offset,
                block_size: block.get_unpack_size(),
            });
            offset += file.size();
            remaining -= 1;
        }
    }

    let mut entries = Vec::with_capacity(archive.files.len());
    let mut records = Vec::with_capacity(archive.files.len());
    for (file_index, file) in archive.files.iter().enumerate() {
        let kind = if file.is_directory() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let record = SevenZRecord {
            file_index,
            block: offsets[file_index],
            size: file.size(),
            name: file.name().to_owned(),
            kind,
            has_stream: file.has_stream(),
            has_crc: file.has_crc,
            crc: file.crc as u32,
            last_modified: if options.expose_metadata {
                sevenz_time_to_system_time(file.last_modified_date())
            } else {
                None
            },
            mode: options.expose_metadata.then(|| mode_from_windows(file.windows_attributes())),
            windows_attributes: file.windows_attributes(),
            is_anti_item: file.is_anti_item(),
        };
        entries.push(Entry {
            name: normalise_name(&record.name),
            kind: record.kind,
            size: Some(record.size),
            compressed_size: None,
            last_modified: record.last_modified,
            mode: record.mode,
            symlink_target: None,
            is_encrypted: false,
            block: record.block,
        });
        records.push(record);
    }
    Ok((entries, records))
}

/// Converts a 7Z timestamp to a Unix one.
///
/// 7Z stores Windows FILETIME, 100-nanosecond ticks since 1601. The conversion is arithmetic
/// rather than a date-library call because it is a few dozen instructions and this crate has no
/// other use for one.
fn sevenz_time_to_system_time(time: sevenz_rust2::NtTime) -> Option<std::time::SystemTime> {
    // 1601-01-01 to 1970-01-01, in seconds.
    const EPOCH_DIFFERENCE: i64 = 11_644_473_600;
    let ticks = sevenz_rust2::NtTime::to_u64(time) as i64;
    let seconds = ticks / 10_000_000 - EPOCH_DIFFERENCE;
    if seconds < 0 {
        return None;
    }
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds as u64))
}

/// Reads a unix mode out of a Windows attribute word.
///
/// 7Z on unix stores the mode in the high 16 bits when the "version made by" says unix, and in
/// the low bits otherwise. Which is which is not recorded per-entry, so both are tried and the
/// one that looks like a plausible mode wins.
fn mode_from_windows(attributes: u32) -> u32 {
    let high = attributes >> 16;
    if high & 0o170000 == 0o170000 {
        high
    } else {
        attributes & 0o777
    }
}

/// Reads the whole archive into memory, refusing one that is past the metadata ceiling.
///
/// A 7Z header is at the *end* of the file, so it cannot be read without the bytes before it, and
/// the folder decoders seek all over the middle. One bounded copy up front is what makes both
/// possible without sharing a seekable handle between decoders.
fn read_all_bounded<R: Read + Seek>(source: &Shared<R>, limits: &Limits) -> Result<Vec<u8>> {
    let mut guard = crate::reader::lock(source)?;
    let current = guard.stream_position()?;
    let length = guard.seek(SeekFrom::End(0))?;
    guard.seek(SeekFrom::Start(current))?;
    limits.check_metadata_size(length)?;
    let mut bytes = vec![0u8; usize::try_from(length)
        .map_err(|_| Error::malformed("the archive is larger than this machine can address"))?];
    guard.seek(SeekFrom::Start(0))?;
    read_fully(&mut *guard, &mut bytes)?;
    Ok(bytes)
}

fn read_fully<R: Read>(source: &mut R, into: &mut [u8]) -> Result<()> {
    source.read_exact(into).map_err(Error::io)
}

/// Whether two entries are in the same folder, which is what decides whether a decode can be
/// reused.
///
/// Kept here rather than only in the reader because this is the rule every 7Z path has to obey,
/// and getting it wrong is the bug that made the first attempt at this slower than what it
/// replaced.
pub fn same_folder(first: &Entry, second: &Entry) -> bool {
    match (first.block, second.block) {
        (Some(a), Some(b)) => a.block_index == b.block_index,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BlockPlacement;

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
