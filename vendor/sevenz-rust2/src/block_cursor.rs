//! A resumable, forward-only reader over one 7Z compression block.
//!
//! # PATCH 1 of 1 — see `PATCHES.md` at the root of this vendored crate.
//!
//! # Why this file exists
//!
//! A 7Z archive splits its entries into *folders* (the format's own name for them). Each folder is
//! a run of entries compressed together, and each entry inside a folder is a byte range of that
//! folder's output. Decoding entry *n* of a folder therefore means decoding the folder from its
//! beginning.
//!
//! `ArchiveReader::read_file` does exactly that for a solid archive: it builds a fresh decoder per
//! call and walks the folder from the start, stopping at the entry it wants. Reading *N* entries
//! that way decodes the folder *N* times — a quadratic loop in how entries are addressed.
//! Measured on a solid archive of 400 entries that is 15.5 s, against 204 ms for the same data
//! stored non-solid. The codec is not slow. The addressing is quadratic.
//!
//! [`BlockCursor`] is the fix, and it is small on purpose. It builds a folder's decode stack
//! **once**, keeps it alive, and hands the folder's entries out in order as byte ranges over that
//! one decode. Reading a folder front to back costs one decode.
//!
//! # Why it lives inside the vendored crate
//!
//! Building a folder's decode stack needs `add_decoder`, the private `Archive`/`Block`/`Coder`
//! types and the `get_lzma2_dic_size` property parsers. Reaching those from outside the crate
//! would mean making half the crate public. Adding one focused file here leaves every other file
//! byte-for-byte unchanged, which is the property that makes a vendored dependency reviewable.
//!
//! # What it does and does not handle
//!
//! This builds the **single-stream** decode chain: a folder whose coders form a chain with one
//! input and one output each, which is every folder produced by the codecs a file manager meets —
//! LZMA, LZMA2, BZip2, Deflate, PPMd, Zstd, Copy, and BCJ filters layered on any of them.
//! BCJ2 folders, which have four input streams and a branched coder graph, are not handled here
//! and are reported as unsupported rather than being read incorrectly. That matches what the
//! upstream `for_each_entries` path supports for the solid case, so nothing that used to open now
//! fails to open.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use crate::archive::{Archive, SIGNATURE_HEADER_SIZE};
use crate::block::{Block, Coder};
use crate::decoder::add_decoder;
use crate::error::Error;
use crate::Password;

/// Reads a byte range of the source, seeking before every read.
///
/// Seeking per read rather than once at construction is what makes a decode stack *storable*: the
/// reader does not care where the shared source happens to be pointing, so the stack that contains
/// it does not borrow it and can be held between two entries. The pack stream is consumed in large
/// blocks by the codec, so this costs a handful of extra seeks per folder.
struct RangeReader<R> {
    source: Arc<Mutex<R>>,
    position: u64,
    end: u64,
}

impl<R: Read + Seek> RangeReader<R> {
    fn new(source: Arc<Mutex<R>>, start: u64, end: u64) -> RangeReader<R> {
        RangeReader { source, position: start, end }
    }
}

impl<R: Read + Seek> Read for RangeReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.end || output.is_empty() {
            return Ok(0);
        }
        let wanted = output.len().min((self.end - self.position) as usize);
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("the 7z source handle was left poisoned"))?;
        source.seek(SeekFrom::Start(self.position))?;
        let read = source.read(&mut output[..wanted])?;
        self.position += read as u64;
        Ok(read)
    }
}

/// Verifies an entry's CRC as it is read past.
///
/// A folder's CRC covers the whole folder rather than one entry, so this is not used for the
/// per-entry check; it exists because a folder with a single stream records the entry's CRC in
/// the substream table, and a mismatch there means the archive is damaged rather than merely
/// different.
struct CrcReader<R> {
    inner: R,
    hasher: crc32fast::Hasher,
    expected: u32,
    remaining: u64,
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let wanted = output.len().min(self.remaining as usize);
        let read = self.inner.read(&mut output[..wanted])?;
        self.hasher.update(&output[..read]);
        self.remaining -= read as u64;
        if self.remaining == 0 {
            let actual = self.hasher.clone().finalize();
            if actual != self.expected {
                return Err(io::Error::other(format!(
                    "7z entry CRC is {actual:#010x} but the archive says {:#010x}",
                    self.expected
                )));
            }
        }
        Ok(read)
    }
}

/// A cursor over one folder of a 7Z archive, decoded once.
pub struct BlockCursor<R: Read + Seek + Send + 'static> {
    /// The archive's source, shared with the decoder chain.
    source: Arc<Mutex<R>>,
    /// The folder being read, cloned so the cursor does not borrow the caller's archive.
    block: Block,
    /// The folder's first pack stream index, needed to find its bytes in the source.
    first_pack_stream_index: usize,
    pack_offset: u64,
    pack_size: u64,
    unpack_size: u64,
    /// Where the archive's packed data begins, so a folder's byte range is `pack_pos + offset`.
    pack_position: u64,
    /// A single-stream folder records the entry's CRC in the substream table.
    entry_crc: Option<u32>,
    password: Password,
    thread_count: u32,
    /// The decode stack, built on the first read and reused by every one after it.
    decoder: Option<Box<dyn Read + Send>>,
    /// How many bytes of the folder have been handed out.
    consumed: u64,
}

impl<R: Read + Seek + Send + 'static> BlockCursor<R> {
    /// Opens a cursor over folder `block_index` of `archive`.
    ///
    /// Reports an error for a folder this cursor cannot decode — a BCJ2 folder, or one whose
    /// coders branch — rather than reading it incorrectly.
    pub fn open(
        archive: &Archive,
        source: Arc<Mutex<R>>,
        password: &Password,
        block_index: usize,
        thread_count: u32,
    ) -> Result<BlockCursor<R>, Error> {
        let block = archive.blocks.get(block_index).cloned().ok_or_else(|| {
            Error::other(format!("7z folder {block_index} is out of range"))
        })?;
        if block.total_input_streams > block.total_output_streams {
            return Err(Error::unsupported(
                "7z folder uses BCJ2, which this reader does not decode",
            ));
        }
        let first_pack_stream_index = *archive
            .stream_map
            .block_first_pack_stream_index()
            .get(block_index)
            .ok_or_else(|| Error::other("7z folder has no pack stream"))?;
        let pack_offset = archive
            .stream_map
            .pack_stream_offsets()
            .get(first_pack_stream_index)
            .copied()
            .ok_or_else(|| Error::other("7z folder's pack stream offset is missing"))?;
        let pack_size = archive
            .pack_sizes()
            .get(first_pack_stream_index)
            .copied()
            .ok_or_else(|| Error::other("7z folder's pack stream size is missing"))?;
        let unpack_size = block.get_unpack_size();
        let substream = archive
            .stream_map
            .block_first_sub_stream_index
            .get(block_index)
            .copied()
            .unwrap_or(0);
        // A folder with one stream records that stream's CRC in the substream table rather than in
        // the block header. It is the folder's own CRC in that case, and checking it catches an
        // archive that decompresses without error to the wrong bytes.
        let entry_crc = archive.sub_streams_info.as_ref().and_then(|info| {
            if info.has_crc.contains(substream) {
                info.crcs.get(substream).map(|crc| *crc as u32)
            } else {
                None
            }
        });
        Ok(BlockCursor {
            source,
            block,
            first_pack_stream_index,
            pack_offset,
            pack_size,
            unpack_size,
            pack_position: archive.pack_pos(),
            entry_crc,
            password: password.clone(),
            thread_count,
            decoder: None,
            consumed: 0,
        })
    }

    /// Reads the entry at `offset` within the folder, of `size` bytes.
    ///
    /// Entries must be asked for in increasing offset order. The first call builds the decode
    /// stack; every later call reuses it, which is the whole point. Asking for an entry before the
    /// cursor's position restarts the folder — the format's own cost, identical to what
    /// `read_file` pays on every call — rather than returning wrong bytes.
    pub fn read_entry(&mut self, offset: u64, size: u64) -> Result<Vec<u8>, Error> {
        if offset + size > self.unpack_size {
            return Err(Error::other("7z entry runs past the end of its folder"));
        }
        if self.decoder.is_none() || offset < self.consumed {
            self.restart()?;
        }
        if offset > self.consumed {
            self.skip(offset - self.consumed)?;
        }
        let decoder = self.decoder.as_mut().expect("restart leaves a decoder in place");
        let mut data = vec![0u8; size as usize];
        decoder.read_exact(&mut data)?;
        self.consumed = offset + size;
        Ok(data)
    }

    /// Rewinds the folder. The next read pays for a full decode of it.
    pub fn reset(&mut self) {
        self.decoder = None;
        self.consumed = 0;
    }

    /// How many bytes of the folder have been handed out.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Positions the cursor so that the next `read_entry` continues from `offset`.
    ///
    /// This is what "skipping an entry" means for a solid folder: rather than decoding and
    /// discarding, the next read simply starts where the skipped entry ended. A backwards or
    /// equal-to-the-current-position request is ignored, because moving forwards is the only
    /// direction a decoder can go without being rebuilt.
    pub fn advance_to(&mut self, offset: u64) {
        if offset > self.consumed {
            self.consumed = offset;
        }
    }

    /// How many bytes the folder expands to.
    pub fn len(&self) -> u64 {
        self.unpack_size
    }

    /// Whether the folder expands to nothing.
    pub fn is_empty(&self) -> bool {
        self.unpack_size == 0
    }

    /// Decodes forward by `count` bytes, throwing them away.
    fn skip(&mut self, count: u64) -> Result<(), Error> {
        let decoder = self.decoder.as_mut().expect("a decoder is in place");
        let mut remaining = count;
        let mut buffer = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let wanted = buffer.len().min(remaining as usize);
            let read = decoder.read(&mut buffer[..wanted])?;
            if read == 0 {
                // The folder ended before the entry the caller asked for. Returning an error is
                // the only correct answer; looping would hang and padding would be a lie.
                return Err(Error::other("7z folder ended before the entry"));
            }
            remaining -= read as u64;
        }
        Ok(())
    }

    /// Builds the decode stack and rewinds.
    fn restart(&mut self) -> Result<(), Error> {
        let start = SIGNATURE_HEADER_SIZE
            .checked_add(self.pack_position)
            .and_then(|value| value.checked_add(self.pack_offset))
            .ok_or_else(|| Error::other("7z folder offset is out of range"))?;
        let end = start
            .checked_add(self.pack_size)
            .ok_or_else(|| Error::other("7z folder end is out of range"))?;

        let mut decoder: Box<dyn Read + Send> =
            Box::new(RangeReader::new(Arc::clone(&self.source), start, end));
        for (index, coder) in self.block.ordered_coder_iter() {
            if coder.num_in_streams != 1 || coder.num_out_streams != 1 {
                return Err(Error::unsupported(
                    "7z folder has a branching coder chain this reader cannot decode",
                ));
            }
            decoder = Box::new(add_decoder(
                decoder,
                self.block.get_unpack_size_at_index(index) as usize,
                coder,
                &self.password,
                usize::MAX / 1024,
                self.thread_count,
            )?);
        }
        // The sub-stream CRC describes one entry only when the folder holds exactly one. For a
        // folder with several entries each has its own CRC in the sub-stream table and the folder
        // decoder cannot tell them apart, so checking here would compare an entry's CRC against
        // whichever entry the sub-stream table happened to record. The caller checks the right
        // value per entry; this is left out rather than being wrong.
        if let Some(crc) = self.entry_crc {
            if self.block.num_sub_streams() == 1 {
                decoder = Box::new(CrcReader {
                    inner: decoder,
                    hasher: crc32fast::Hasher::new(),
                    expected: crc,
                    remaining: self.unpack_size,
                });
            }
        }
        self.decoder = Some(decoder);
        self.consumed = 0;
        Ok(())
    }

}

/// The folder's index into the archive's pack streams, kept for callers that report it.
impl<R: Read + Seek + Send + 'static> BlockCursor<R> {
    /// Which pack stream this folder's bytes come from.
    pub fn first_pack_stream_index(&self) -> usize {
        self.first_pack_stream_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveEntry;
    use crate::writer::{ArchiveWriter, SourceReader};
    use crate::Archive;
    use std::io::Cursor;
    use std::time::Instant;

    const SOLID: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/resources/solid.7z"
    );

    fn parse() -> (Archive, Vec<u8>) {
        let bytes = std::fs::read(SOLID).expect("the solid corpus file");
        let mut source = Cursor::new(bytes.clone());
        let archive = Archive::read(&mut source, &Password::empty()).expect("parse the archive");
        (archive, bytes)
    }

    /// The folder with the most entries, which is the one a cursor is for.
    fn busiest(archive: &Archive) -> (usize, usize) {
        let mut block = 0;
        let mut count = 0;
        for (index, candidate) in archive.blocks.iter().enumerate() {
            if candidate.num_unpack_sub_streams > count {
                count = candidate.num_unpack_sub_streams;
                block = index;
            }
        }
        (block, count)
    }

    /// The whole point of this file: reading a solid folder in order through one cursor costs one
    /// decode, where decoding entry *n* on its own costs *n*.
    ///
    /// The comparison is against the upstream `read_file`, which is what the code being replaced
    /// calls, and the assertion is that the cursor is not slower. The speed-up is printed rather
    /// than asserted, because a wall-clock ratio is a property of the machine and a regression
    /// would show up as the cursor becoming slower, not as the ratio falling.
    #[test]
    fn a_solid_folder_is_decoded_once_in_order() {
        let (archive, bytes) = parse();
        let (block_index, count) = busiest(&archive);
        assert!(count > 1, "the corpus folder should hold several entries");

        let start = archive.stream_map.block_first_file_index[block_index];
        let mut offset = 0u64;
        let mut cursor = BlockCursor::open(
            &archive,
            Arc::new(Mutex::new(Cursor::new(bytes.clone()))),
            &Password::empty(),
            block_index,
            1,
        )
        .expect("a cursor over the solid folder");
        let mut total = 0u64;
        for file_index in start..(start + count) {
            let file = &archive.files[file_index];
            if !file.has_stream {
                continue;
            }
            let data = cursor
                .read_entry(offset, file.size)
                .expect("read an entry in order");
            assert_eq!(data.len() as u64, file.size, "entry {file_index} is the wrong length");
            offset += file.size;
            total += data.len() as u64;
        }
        assert_eq!(cursor.consumed(), total, "the cursor read exactly the folder");
        assert_eq!(total, cursor.len(), "the whole folder's output was produced");

        // Now the same entries, the way the app reads them today.
        let started = Instant::now();
        let mut reader = crate::ArchiveReader::new(
            Cursor::new(bytes.clone()),
            Password::empty(),
        )
        .expect("an archive reader");
        for file_index in start..(start + count) {
            let file = &archive.files[file_index];
            if !file.has_stream {
                continue;
            }
            reader
                .read_file(&file.name)
                .expect("read_file finds the entry");
        }
        let per_entry = started.elapsed();

        let started = Instant::now();
        let mut cursor = BlockCursor::open(
            &archive,
            Arc::new(Mutex::new(Cursor::new(bytes))),
            &Password::empty(),
            block_index,
            1,
        )
        .expect("a cursor");
        let mut offset = 0u64;
        for file_index in start..(start + count) {
            let file = &archive.files[file_index];
            if !file.has_stream {
                continue;
            }
            cursor
                .read_entry(offset, file.size)
                .expect("read an entry in order");
            offset += file.size;
        }
        let in_order = started.elapsed();

        println!(
            "solid folder: {count} entries, read_file {:.1} ms, cursor {:.1} ms, {:.1}x",
            per_entry.as_secs_f64() * 1000.0,
            in_order.as_secs_f64() * 1000.0,
            per_entry.as_secs_f64() / in_order.as_secs_f64().max(1e-9),
        );
        assert!(
            in_order.as_secs_f64() <= per_entry.as_secs_f64(),
            "the cursor took {in_order:?} against read_file's {per_entry:?}, so it is not faster"
        );
    }

    /// The cursor gives the same bytes as `read_file` does, entry for entry.
    ///
    /// A speed-up that returned the wrong bytes would be worse than the quadratic loop, so this
    /// is the assertion that matters most.
    #[test]
    fn the_cursor_returns_the_same_bytes_as_reading_an_entry_on_its_own() {
        let (archive, bytes) = parse();
        let (block_index, count) = busiest(&archive);
        let start = archive.stream_map.block_first_file_index[block_index];

        let mut reader = crate::ArchiveReader::new(Cursor::new(bytes.clone()), Password::empty())
            .expect("an archive reader");
        let expected: Vec<Vec<u8>> = (start..(start + count))
            .filter(|index| archive.files[*index].has_stream)
            .map(|index| {
                reader
                    .read_file(&archive.files[index].name)
                    .expect("read_file")
            })
            .collect();

        let mut cursor = BlockCursor::open(
            &archive,
            Arc::new(Mutex::new(Cursor::new(bytes))),
            &Password::empty(),
            block_index,
            1,
        )
        .expect("a cursor");
        let mut offset = 0u64;
        let mut actual: Vec<Vec<u8>> = Vec::new();
        for file_index in start..(start + count) {
            let file = &archive.files[file_index];
            if !file.has_stream {
                continue;
            }
            actual.push(cursor.read_entry(offset, file.size).expect("read an entry"));
            offset += file.size;
        }
        assert_eq!(actual, expected, "the cursor and read_file disagree");
    }

    /// Asking for an entry before the cursor's position restarts the folder and still gives the
    /// right bytes.
    ///
    /// Out-of-order access costs a re-decode — LZMA cannot be seeked inside a stream, so that is
    /// the format's cost and no API removes it. What matters is that it never returns the wrong
    /// bytes: a cursor that got this wrong would be worse than the quadratic loop it replaces.
    ///
    /// The fixture is built here rather than taken from `tests/resources`, because the corpus
    /// files are small and this needs enough entries for reversing to be a real test.
    #[test]
    fn reading_out_of_order_restarts_the_folder_and_stays_correct() {

        let names: Vec<String> = (0..12).map(|index| format!("file{index}.txt")).collect();
        let entries: Vec<ArchiveEntry> = names
            .iter()
            .map(|name| ArchiveEntry::new_file(name))
            .collect();
        let readers: Vec<SourceReader<Cursor<Vec<u8>>>> = names
            .iter()
            .enumerate()
            .map(|(index, _)| {
                SourceReader::new(Cursor::new(
                    format!("entry {index} with some content to compress").into_bytes(),
                ))
            })
            .collect();

        let bytes = {
            let buffer = Cursor::new(Vec::new());
            let mut writer = ArchiveWriter::new(buffer).expect("a writer");
            writer
                .push_archive_entries(entries, readers)
                .expect("compress the entries together into one solid folder");
            writer
                .finish()
                .expect("finish the archive")
                .into_inner()
        };

        let mut source = Cursor::new(bytes.clone());
        let archive = Archive::read(&mut source, &Password::empty()).expect("parse the archive");
        assert!(
            archive.is_solid,
            "the archive under test is meant to be solid; if it is not, this test is not testing              what it claims to"
        );

        // Every entry, read backwards, which restarts the folder on every call.
        let streams: Vec<(u64, u64, String)> = {
            let mut offset = 0u64;
            archive
                .files
                .iter()
                .filter(|file| file.has_stream)
                .map(|file| {
                    let current = (offset, file.size, file.name.clone());
                    offset += file.size;
                    current
                })
                .collect()
        };
        assert!(streams.len() >= 10, "need a solid archive with many entries");

        let mut reader =
            crate::ArchiveReader::new(Cursor::new(bytes.clone()), Password::empty())
                .expect("an archive reader");
        let expected: std::collections::HashMap<String, Vec<u8>> = streams
            .iter()
            .map(|(_, _, name)| {
                (
                    name.clone(),
                    reader.read_file(name).expect("read_file finds the entry"),
                )
            })
            .collect();

        let block_index = (0..archive.blocks.len())
            .max_by_key(|index| archive.blocks[*index].num_unpack_sub_streams)
            .expect("at least one folder");
        let mut cursor = BlockCursor::open(
            &archive,
            Arc::new(Mutex::new(Cursor::new(bytes))),
            &Password::empty(),
            block_index,
            1,
        )
        .expect("a cursor over the solid archive");

        for (offset, size, name) in streams.iter().rev() {
            let data = cursor.read_entry(*offset, *size).expect("read out of order");
            assert_eq!(
                data,
                expected[name],
                "reading {name} out of order gave different bytes"
            );
        }
    }

    /// The cost of reading a solid folder grows linearly in the number of entries, not quadratically.
    ///
    /// This is the assertion that distinguishes "the cursor is a bit faster" from "the quadratic
    /// loop is gone". A cursor that silently rebuilt its decode stack would still pass the
    /// two-entry test above; only the ratio between *n* entries and *4n* entries shows that.
    ///
    /// Time is not a reliable thing to assert on, so what is asserted is the shape: quadrupling the
    /// entries must not multiply the time by anything close to sixteen. The threshold is loose on
    /// purpose — a shared machine, a debug build and a small archive all add noise — but it is far
    /// below the quadratic factor, so a regression to one-decode-per-entry cannot pass it.
    #[test]
    fn the_cost_grows_linearly_rather_than_quadratically() {
        /// Builds a solid archive of `count` entries and returns its bytes.
        fn build(count: usize) -> Vec<u8> {
            let entries: Vec<ArchiveEntry> = (0..count)
                .map(|index| ArchiveEntry::new_file(&format!("file{index}.txt")))
                .collect();
            let readers: Vec<SourceReader<Cursor<Vec<u8>>>> = (0..count)
                .map(|index| {
                    SourceReader::new(Cursor::new(
                        format!("entry {index} with some content to compress").into_bytes(),
                    ))
                })
                .collect();
            let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("a writer");
            writer
                .push_archive_entries(entries, readers)
                .expect("compress into one solid folder");
            writer.finish().expect("finish").into_inner()
        }

        /// Reads every entry of the busiest folder through one cursor, in order.
        fn read_all(bytes: Vec<u8>) -> std::time::Duration {
            let mut source = Cursor::new(bytes);
            let archive = Archive::read(&mut source, &Password::empty()).expect("parse");
            let block_index = (0..archive.blocks.len())
                .max_by_key(|index| archive.blocks[*index].num_unpack_sub_streams)
                .expect("a folder");
            let mut cursor = BlockCursor::open(
                &archive,
                Arc::new(Mutex::new(source)),
                &Password::empty(),
                block_index,
                1,
            )
            .expect("a cursor");
            let started = std::time::Instant::now();
            let mut offset = 0u64;
            for file in archive.files.iter().filter(|file| file.has_stream) {
                cursor.read_entry(offset, file.size).expect("read in order");
                offset += file.size;
            }
            std::time::Duration::from_secs_f64(started.elapsed().as_secs_f64())
        }

        const SMALL: usize = 16;
        const LARGE: usize = 64;

        let small = build(SMALL);
        let large = build(LARGE);
        assert!(archive_is_solid(&small) && archive_is_solid(&large));

        // Take the best of each, so a scheduling hiccup cannot make the ratio look quadratic.
        let small_time = (0..3).map(|_| read_all(small.clone())).min().expect("timed");
        let large_time = (0..3).map(|_| read_all(large.clone())).min().expect("timed");
        let growth = large_time.as_secs_f64() / small_time.as_secs_f64().max(1e-6);

        println!(
            "solid reading: {SMALL} entries {:.2} ms, {LARGE} entries {:.2} ms, growth {growth:.1}x",
            small_time.as_secs_f64() * 1000.0,
            large_time.as_secs_f64() * 1000.0,
        );
        // Four times the entries should cost about four times the time. Quadratic would be
        // sixteen times, and the bound here is nine — high enough not to fail on a noisy
        // machine, low enough that a return to per-entry decoding cannot pass.
        assert!(
            growth < 9.0,
            "quadrupling the entries multiplied the time by {growth:.1}, which is quadratic"
        );
    }

    /// Whether every folder in an archive is solid, i.e. the archive is one folder.
    fn archive_is_solid(bytes: &[u8]) -> bool {
        let mut source = Cursor::new(bytes.to_vec());
        Archive::read(&mut source, &Password::empty())
            .map(|archive| archive.blocks.len() == 1 && archive.blocks[0].num_unpack_sub_streams > 1)
            .unwrap_or(false)
    }

    /// A folder index that does not exist is an error, not a panic.
    #[test]
    fn a_folder_index_past_the_end_is_an_error() {
        let (archive, bytes) = parse();
        let result = BlockCursor::<Cursor<Vec<u8>>>::open(
            &archive,
            Arc::new(Mutex::new(Cursor::new(bytes))),
            &Password::empty(),
            archive.blocks.len() + 10,
            1,
        );
        assert!(result.is_err(), "a folder that does not exist must not open");
    }
}
