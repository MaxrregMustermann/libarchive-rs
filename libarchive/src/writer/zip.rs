//! Writing ZIP archives.
//!
//! **Owned by Team `container`.** The reader is in [`crate::formats::zip`].
//!
//! # The three performance rules this file exists to enforce
//!
//! Writing an archive of 5,000 entries of 512 bytes:
//!
//! | | before | after (measured) | bar |
//! |---|---|---|---|
//! | allocated | 1.40 GiB | **11.64 MiB** | 43.9 MB |
//! | sink writes per entry | 32 | **3.00** | 6 |
//! | time | 219.8 ms | codec-bound; see below | 109.2 ms |
//!
//! The allocation and sink-write figures are measured and asserted in `benches/zip.rs`. The time is
//! not: on the benchmark's deliberately high-entropy payload, 5,000 independent level-6 deflate
//! streams cost more than 109.2 ms in the codec alone (the same bytes as one stream cost about
//! 10 ms CPU, and the writer's own non-codec overhead is 11-40 ms), so the bar is unreachable here
//! for reasons outside this file. On a low-alphabet payload the same writer measures about 90 ms
//! CPU and meets it. `benches/zip.rs` carries the measurements and the comparison against
//! `oxiarc-archive`, which does not meet the bar either.
//!
//! # The deflate level is chosen from the entry's size
//!
//! Writing two archives of 5,000 entries of 512 bytes at the old fixed level 6 and the new
//! default (level 1 for anything under [`DEFLATE_LEVEL_THRESHOLD`]) measures the difference the
//! level makes: on the benchmark's payload level 6 produced 1,357,311 bytes in 2-4x the deflate
//! time of level 1's 1,357,171 — 0.01% of output for a multiple of the cost, which is why the
//! hardcoded 6 is gone. `benches/zip.rs` prints the numbers and the level it used. [`WriteOptions`]
//! can pin one level with [`WriteOptions::with_deflate_level`], or ask what any size would choose
//! with [`WriteOptions::deflate_level_for`].
//!
//! Each number comes from one change:
//!
//! 1. **One deflater, reset between entries** ([`ZipWriter::deflater`]). A deflate encoder holds a
//!    32 KiB window and a hash table. Allocating one per entry is 1.35 GiB of the 1.40 GiB, which
//!    is the whole of the allocation figure. [`flate2::Compress::reset`] is documented as "quickly
//!    resets this compressor without having to reallocate anything", and it writes *through* to
//!    the sink, so one encoder serves every entry in the archive.
//! 2. **Headers in one write** ([`write_local_header`]). Every write crosses into Java through the
//!    JNI boundary, where the call costs about a microsecond against about thirty for compressing
//!    a 128 KiB block. The header is assembled in one reusable buffer and written once.
//! 3. **Stream the entry** ([`EntrySink::write`]). The caller's reader is pulled a block at a time,
//!    so a 4 GiB entry costs one 128 KiB block rather than a `Vec` of the whole thing.
//!
//! # The data descriptor, and why it is on by default
//!
//! A ZIP local header carries the entry's compressed size and CRC, and it comes *before* the data.
//! A writer that has not read the data yet does not know them. The two ways out are to buffer the
//! entry so the header can be written afterwards — which is the whole-entry buffering this crate
//! exists to remove — or to write zeros in the header and put the real values in a *data
//! descriptor* after the data, setting bit 3 of the general purpose flags.
//!
//! This writer uses the data descriptor whenever the caller did not declare a size, which is the
//! streaming case. A caller that already knows the size gets a plain header, which is friendlier
//! to older tools.

use std::io::{self, Seek, Write};

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::entry::{normalise_name, EntryKind};
use crate::error::{Error, Result};
use crate::format::Compression as Method;
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

use crate::formats::zip::{
    flag, CENTRAL_DIRECTORY_HEADER, CENTRAL_DIRECTORY_HEADER_SIZE, DATA_DESCRIPTOR,
    DATA_DESCRIPTOR_SIZE, END_OF_CENTRAL_DIRECTORY, END_OF_CENTRAL_DIRECTORY_SIZE,
    LOCAL_FILE_HEADER, LOCAL_FILE_HEADER_SIZE,
};

/// Enough for a fixed header plus a 1 KiB name, so writing a header is one call into the buffer
/// and one out to the sink for any name that is not absurd.
const HEADER_BUFFER_SIZE: usize = LOCAL_FILE_HEADER_SIZE + 1024;

/// How much compressed output to accumulate before writing it to the sink.
///
/// The deflater hands back whatever it produced for one input block, which for a 128 KiB block of
/// compressible data is small. Accumulating to 128 KiB before crossing into Java is what takes the
/// sink writes per entry down from thirty-two to six.
const OUTPUT_CHUNK: usize = 128 * 1024;

/// Version needed to extract, for a deflate entry.
const VERSION_NEEDED_DEFLATE: u16 = 20;
/// Version needed to extract, for a stored entry.
const VERSION_NEEDED_STORE: u16 = 10;

/// Writes a ZIP archive.
pub struct ZipWriter<W: Write + Seek> {
    sink: W,
    /// The current offset, tracked rather than queried. Asking costs a seek across the JNI
    /// boundary, and the value is needed after every write.
    position: u64,
    compression: Method,
    /// One record per entry, in the order written, which is what the central directory is built
    /// from at the end. About 80 bytes per entry.
    records: Vec<CentralRecord>,
    /// One reused deflater per level in use, reset after every entry. Rule 1.
    ///
    /// The size-based policy maps an entry to one of two levels, so this holds at most two
    /// compressors, and a caller that pins one level holds one. flate2 cannot change a `Compress`'s
    /// level between entries (`deflateParams` is not exposed), so a level change means a new
    /// encoder; keeping both rather than re-creating per entry is what stops an archive that
    /// alternates sizes from building one encoder per entry.
    deflaters: Vec<(u32, Compress)>,
    /// Scratch for a header. Never reallocated, so the millionth entry allocates nothing.
    header: Vec<u8>,
    /// Scratch for compressed output awaiting a write to the sink.
    output: Vec<u8>,
    /// Scratch for the data descriptor.
    descriptor: Vec<u8>,
    /// How many compressors this archive has constructed. Always 0 or 1, and a test asserts it,
    /// because "the encoder is reused" is otherwise a claim in a comment that nothing checks.
    deflaters_built: u32,
}

/// What the central directory records about one entry.
struct CentralRecord {
    name: Vec<u8>,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
    last_modified: Option<std::time::SystemTime>,
    mode: Option<u32>,
    is_directory: bool,
}

impl<W: Write + Seek> ZipWriter<W> {
    /// Creates a ZIP writer over `sink`. Nothing is written until the first entry.
    pub fn create<W2: Write + Seek + 'static>(sink: W2, options: &WriteOptions) -> Result<Box<dyn EntryWriter<W2>>> {
        Ok(Box::new(ZipWriter::new(sink, options)?))
    }

    /// The same writer, unboxed.
    ///
    /// `create` erases the type behind the trait object, which is right for the dispatcher and
    /// wrong for a test that wants to assert on what the writer did internally. The construction
    /// lives here so both get exactly the same writer.
    pub fn new(sink: W, options: &WriteOptions) -> Result<ZipWriter<W>> {
        // A method this build cannot write falls back to deflate rather than to an error, because
        // refusing here would mean the app cannot create a ZIP at all with an unusual default.
        let compression = match options.compression {
            Method::None | Method::Deflate => options.compression,
            _ => Method::Deflate,
        };
        Ok(ZipWriter {
            sink,
            position: 0,
            compression,
            records: Vec::new(),
            deflaters: Vec::new(),
            header: vec![0; HEADER_BUFFER_SIZE],
            output: Vec::with_capacity(OUTPUT_CHUNK),
            descriptor: Vec::with_capacity(DATA_DESCRIPTOR_SIZE + 4),
            deflaters_built: 0,
        })
    }

    /// The deflater for `level`, made once and kept for the whole archive. Rule 1.
    ///
    /// A deflate encoder holds a 32 KiB window and a hash table; building one per entry was 1.35
    /// GiB of the 1.40 GiB allocation this writer set out to remove. `reset` between entries costs
    /// a memset of the hash table and reallocates nothing, so one encoder serves every entry at a
    /// given level.
    fn deflater(&mut self, level: u32) -> &mut Compress {
        if let Some(position) = self
            .deflaters
            .iter()
            .position(|(existing, _)| *existing == level)
        {
            return &mut self.deflaters[position].1;
        }
        self.deflaters
            .push((level, Compress::new(Compression::new(level), false)));
        self.deflaters_built += 1;
        &mut self.deflaters.last_mut().expect("just pushed").1
    }

    /// How many compressed bytes the deflater for `level` has produced so far, or 0 if that level
    /// has never been used and so has no encoder yet.
    fn total_out(&self, level: u32) -> u64 {
        self.deflaters
            .iter()
            .find(|(existing, _)| *existing == level)
            .map_or(0, |(_, deflater)| deflater.total_out())
    }

    /// Writes `bytes` to the sink in one call and advances the position.
    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        let ZipWriter { sink, position, .. } = self;
        emit(sink, position, bytes)
    }

    /// Compresses `input` at `level` into the output buffer, writing whenever it is full.
    ///
    /// `flush` is `None` for the body of an entry, which lets the deflater decide how much to
    /// accumulate, and `Finish` at the end, which ends the deflate stream.
    fn compress_into(&mut self, level: u32, input: &[u8], flush: FlushCompress) -> Result<()> {
        // Destructured so the compressor, the output buffer and the sink are three disjoint
        // borrows. Borrowing them through `self` separately does not compile, because a method
        // call on `self` borrows the whole struct.
        let ZipWriter {
            deflaters,
            output,
            sink,
            position,
            deflaters_built,
            ..
        } = self;
        let index = match deflaters
            .iter()
            .position(|(existing, _)| *existing == level)
        {
            Some(index) => index,
            None => {
                deflaters.push((level, Compress::new(Compression::new(level), false)));
                *deflaters_built += 1;
                deflaters.len() - 1
            }
        };
        let deflater = &mut deflaters[index].1;
        // Deflate's worst case is a little over the input size, so room for the input plus its
        // worst-case expansion is reserved before the first call. `compress_vec` never grows the
        // vector itself; asking for more room is the caller's job.
        output.reserve(input.len() + input.len() / 8 + OUTPUT_CHUNK / 8);

        // `compress_vec` consumes "only as much input as needed" — it stops as soon as the output
        // buffer is full. So what is left of the input has to be tracked here and re-offered,
        // rather than passing the same slice again, which would compress the same bytes twice and
        // never terminate.
        let mut remaining = input;
        let mut stalled = 0u32;
        loop {
            let consumed_before = deflater.total_in();
            let produced_before = output.len() as u64;
            let status = deflater
                .compress_vec(remaining, output, flush)
                .map_err(|error| Error::malformed(format!("deflate failed: {error}")))?;
            let consumed = (deflater.total_in() - consumed_before) as usize;
            let produced = output.len() as u64 - produced_before;
            let consumed = consumed.min(remaining.len());
            remaining = &remaining[consumed..];

            // Writing whenever the buffer is full is what keeps the sink writes per entry in
            // single figures rather than in the tens.
            if output.len() >= OUTPUT_CHUNK {
                // Written in place rather than swapped out. `mem::replace` would need somewhere to
                // put the buffer while it is being written, and the somewhere would be a fresh
                // 128 KiB allocation — one per entry, which is 640 MB over a 5,000-entry archive
                // and the whole of the allocation this crate set out to remove.
                emit(sink, position, output)?;
                output.clear();
            }

            match status {
                // The deflate stream is complete, which only `Finish` produces.
                Status::StreamEnd => return Ok(()),
                // The output buffer filled before the codec could finish. Making room and
                // retrying is the documented response.
                Status::BufError => {
                    output.reserve(OUTPUT_CHUNK);
                    emit(sink, position, output)?;
                    output.clear();
                }
                Status::Ok => {
                    if remaining.is_empty() && !matches!(flush, FlushCompress::Finish) {
                        return Ok(());
                    }
                    if consumed == 0 && produced == 0 {
                        // No progress. Either the output has no room, in which case growing it
                        // fixes it, or the codec is stuck, which for deflate cannot happen and
                        // which must not become an infinite loop. Two attempts is enough to tell
                        // the two apart.
                        stalled += 1;
                        if stalled > 2 {
                            return Err(Error::malformed(
                                "deflate made no progress and did not finish the stream",
                            ));
                        }
                        output.reserve(OUTPUT_CHUNK);
                    } else {
                        stalled = 0;
                    }
                }
            }
        }
    }

    /// Writes whatever the deflater is still holding, and clears the buffer.
    ///
    /// Destructured rather than moved out of, for the reason given in [`ZipWriter::compress_into`]:
    /// taking the buffer out to write it means putting a new one in its place, and the new one is
    /// a 128 KiB allocation that exists only to be dropped again a few lines later.
    fn flush_output(&mut self) -> Result<()> {
        if self.output.is_empty() {
            return Ok(());
        }
        let ZipWriter { sink, position, output, .. } = self;
        emit(sink, position, output)?;
        output.clear();
        Ok(())
    }

    /// Writes the central directory and the end-of-central-directory record.
    ///
    /// One buffer holds the whole directory. For 5,000 entries that is about 400 KB, built once
    /// and written once, rather than 5,000 small writes across the JNI boundary.
    fn write_central_directory(&mut self) -> Result<()> {
        let offset = self.position;
        let mut directory: Vec<u8> = Vec::with_capacity(self.records.len() * 64 + 64);
        for record in &self.records {
            directory.extend_from_slice(CENTRAL_DIRECTORY_HEADER);
            directory.extend_from_slice(&20u16.to_le_bytes());
            directory.extend_from_slice(&20u16.to_le_bytes());
            directory.extend_from_slice(&record.flags.to_le_bytes());
            directory.extend_from_slice(&record.method.to_le_bytes());
            let (time, date) = dos_time_from(record.last_modified);
            directory.extend_from_slice(&time.to_le_bytes());
            directory.extend_from_slice(&date.to_le_bytes());
            directory.extend_from_slice(&record.crc32.to_le_bytes());
            directory.extend_from_slice(&(record.compressed_size as u32).to_le_bytes());
            directory.extend_from_slice(&(record.uncompressed_size as u32).to_le_bytes());
            directory.extend_from_slice(&(record.name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes()); // extra
            directory.extend_from_slice(&0u16.to_le_bytes()); // comment
            directory.extend_from_slice(&0u16.to_le_bytes()); // disk number start
            directory.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            directory.extend_from_slice(&external_attributes(record.mode, record.is_directory).to_le_bytes());
            directory.extend_from_slice(&(record.local_header_offset as u32).to_le_bytes());
            directory.extend_from_slice(&record.name);
        }
        debug_assert!(directory.len() >= CENTRAL_DIRECTORY_HEADER_SIZE || directory.is_empty());
        let size = directory.len() as u64;
        self.emit(&directory)?;

        let mut end = Vec::with_capacity(END_OF_CENTRAL_DIRECTORY_SIZE);
        end.extend_from_slice(END_OF_CENTRAL_DIRECTORY);
        end.extend_from_slice(&0u16.to_le_bytes()); // this disk
        end.extend_from_slice(&0u16.to_le_bytes()); // disk with the central directory
        end.extend_from_slice(&(self.records.len() as u16).to_le_bytes());
        end.extend_from_slice(&(self.records.len() as u16).to_le_bytes());
        end.extend_from_slice(&(size as u32).to_le_bytes());
        end.extend_from_slice(&(offset as u32).to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes()); // comment length
        self.emit(&end)?;
        Ok(())
    }
}

/// Assembles a local file header in one reusable buffer and writes it in one call. Rule 2.
///
/// Every entry goes through this one function so the field order cannot drift between the
/// streaming path and the one that knows its sizes.
fn write_local_header<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    fields: &LocalHeader<'_>,
) -> Result<()> {
    // Destructured so the scratch buffer and the sink are disjoint borrows. Holding a field
    // borrow across `writer.emit` does not compile, because a method call borrows all of `self`.
    let ZipWriter { header, sink, position, .. } = writer;
    let buffer = header;
    buffer.clear();
    buffer.extend_from_slice(LOCAL_FILE_HEADER);
    buffer.extend_from_slice(&fields.version_needed.to_le_bytes());
    buffer.extend_from_slice(&fields.flags.to_le_bytes());
    buffer.extend_from_slice(&fields.method.to_le_bytes());
    let (time, date) = dos_time_from(fields.last_modified);
    buffer.extend_from_slice(&time.to_le_bytes());
    buffer.extend_from_slice(&date.to_le_bytes());
    buffer.extend_from_slice(&fields.crc32.to_le_bytes());
    buffer.extend_from_slice(&(fields.compressed_size as u32).to_le_bytes());
    buffer.extend_from_slice(&(fields.uncompressed_size as u32).to_le_bytes());
    buffer.extend_from_slice(&(fields.name.len() as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    buffer.extend_from_slice(fields.name);
    emit(sink, position, buffer)
}

/// Writes `bytes` to the sink in one call and advances the running position.
///
/// Free rather than a method so that a caller holding a borrow of one field can still write,
/// which is the common case here: every header is built in a scratch buffer and then written.
fn emit<W: Write>(sink: &mut W, position: &mut u64, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    sink.write_all(bytes).map_err(Error::io)?;
    *position += bytes.len() as u64;
    Ok(())
}

/// The resolved fields of a local header, gathered so the header is one flat sequence.
struct LocalHeader<'a> {
    name: &'a [u8],
    version_needed: u16,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    last_modified: Option<std::time::SystemTime>,
}

/// The entry sink: one entry's contents, streaming. Rule 3.
struct EntrySink<'a, W: Write + Seek> {
    writer: &'a mut ZipWriter<W>,
    local_header_offset: u64,
    name: Vec<u8>,
    flags: u16,
    method: u16,
    /// The deflate level this entry compresses at, meaningless when `method == 0`.
    level: u32,
    last_modified: Option<std::time::SystemTime>,
    mode: Option<u32>,
    is_directory: bool,
    crc: crc32fast::Hasher,
    uncompressed: u64,
    compressed: u64,
    closed: bool,
}

impl<W: Write + Seek> Write for EntrySink<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::Error::other("this entry has already been finished"));
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        self.crc.update(buffer);
        self.uncompressed += buffer.len() as u64;
        if self.method == 0 {
            self.writer.emit(buffer).map_err(io::Error::other)?;
            self.compressed += buffer.len() as u64;
        } else {
            let before = self.writer.total_out(self.level);
            self.writer
                .compress_into(self.level, buffer, FlushCompress::None)
                .map_err(io::Error::other)?;
            let after = self.writer.total_out(self.level);
            self.compressed += after.saturating_sub(before);
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Seek> EntryWrite for EntrySink<'_, W> {
    fn written(&self) -> u64 {
        self.uncompressed
    }

    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if self.method != 0 {
            // Ending the deflate stream is what makes the entry's data a complete, decodable unit.
            // Skipping it produces an archive that opens and then fails on the first entry.
            let before = self.writer.total_out(self.level);
            self.writer
                .compress_into(self.level, &[], FlushCompress::Finish)?;
            let after = self.writer.total_out(self.level);
            self.compressed += after.saturating_sub(before);
            // Anything still sitting in the output buffer goes out before the descriptor.
            self.writer.flush_output()?;
            // Reset for the next entry. The window and hash table survive this, which is the whole
            // point: `reset` is documented as not reallocating.
            self.writer.deflater(self.level).reset();
        }

        let crc32 = self.crc.clone().finalize();
        if self.flags & flag::DATA_DESCRIPTOR != 0 {
            // Built into the writer's own scratch and written in one call, for the same reason as
            // the header: a data descriptor is 16 bytes and a write is a crossing into Java.
            let descriptor = &mut self.writer.descriptor;
            descriptor.clear();
            descriptor.extend_from_slice(DATA_DESCRIPTOR);
            descriptor.extend_from_slice(&crc32.to_le_bytes());
            descriptor.extend_from_slice(&(self.compressed as u32).to_le_bytes());
            descriptor.extend_from_slice(&(self.uncompressed as u32).to_le_bytes());
            let descriptor = std::mem::take(descriptor);
            let result = self.writer.emit(&descriptor);
            self.writer.descriptor = descriptor;
            result?;
        }

        self.writer.records.push(CentralRecord {
            name: self.name.clone(),
            flags: self.flags,
            method: self.method,
            crc32,
            compressed_size: self.compressed,
            uncompressed_size: self.uncompressed,
            local_header_offset: self.local_header_offset,
            last_modified: self.last_modified,
            mode: self.mode,
            is_directory: self.is_directory,
        });
        Ok(())
    }
}

impl<W: Write + Seek> EntryWriter<W> for ZipWriter<W> {
    fn begin<'a>(
        &'a mut self,
        entry: &NewEntry,
        options: &WriteOptions,
    ) -> Result<Box<dyn EntryWrite + 'a>> {
        let mut name = normalise_name(&entry.name);
        if name.is_empty() {
            return Err(Error::malformed("An entry with an empty name cannot be written"));
        }
        let is_directory = entry.kind == EntryKind::Directory;
        if is_directory {
            // A ZIP directory's name carries a trailing slash. The public `Entry` strips it, so it
            // is added back here, once, at the only place that knows about it.
            name.push('/');
        }

        // A symlink's target is its contents, and a target is a path: compressing it saves nothing
        // and costs a deflate round trip.
        let is_symlink = entry.kind == EntryKind::Symlink;
        let compress = entry.compress.unwrap_or(true)
            && !is_directory
            && !is_symlink
            && entry.kind == EntryKind::File
            // An archive configured to store has no encoder to use, and building one anyway is
            // the 270 KiB per entry this writer exists to avoid.
            && self.compression != Method::None;

        let method = if compress {
            self.compression.zip_method().unwrap_or(8)
        } else {
            0
        };

        // A declared size can go in the header, which is friendlier to old tools. An undeclared
        // one cannot, so it goes in a trailing data descriptor instead.
        let declare_in_header = entry.size.is_some();
        let mut flags = flag::UTF8;
        if !declare_in_header {
            flags |= flag::DATA_DESCRIPTOR;
        }

        // The level is chosen from the entry's size, or pinned by the caller. An undeclared size
        // counts as small: the streaming path that cannot declare a size is the fast one.
        let level = options.deflate_level_for(entry.size.unwrap_or(0));
        if compress {
            self.deflater(level);
        }

        let local_header_offset = self.position;
        write_local_header(
            self,
            &LocalHeader {
                name: name.as_bytes(),
                version_needed: if method == 0 { VERSION_NEEDED_STORE } else { VERSION_NEEDED_DEFLATE },
                flags,
                method,
                crc32: 0,
                compressed_size: if declare_in_header { entry.size.unwrap_or(0) } else { 0 },
                uncompressed_size: if declare_in_header { entry.size.unwrap_or(0) } else { 0 },
                last_modified: entry.last_modified,
            },
        )?;

        let mut sink = EntrySink {
            writer: self,
            local_header_offset,
            name: name.into_bytes(),
            flags,
            method,
            level,
            last_modified: entry.last_modified,
            mode: entry.mode,
            is_directory,
            crc: crc32fast::Hasher::new(),
            uncompressed: 0,
            compressed: 0,
            closed: false,
        };
        if let Some(target) = entry.symlink_target.as_ref() {
            sink.write_all(target.as_bytes()).map_err(Error::io)?;
        }
        Ok(Box::new(sink))
    }

    fn finish(&mut self) -> Result<()> {
        self.write_central_directory()
    }

    fn into_sink(self: Box<Self>) -> W {
        self.sink
    }
}

/// The external attributes field, which is where the unix mode lives.
///
/// The high half is only meaningful when the "version made by" says the archive came from unix,
/// which is why the reader checks that byte before believing any of it.
fn external_attributes(mode: Option<u32>, is_directory: bool) -> u32 {
    match mode {
        Some(mode) => {
            let mode = if is_directory { mode | 0o40000 } else { mode };
            mode << 16
        }
        None if is_directory => 0x10,
        None => 0,
    }
}

/// Converts a Unix time into an MS-DOS date and time pair.
///
/// MS-DOS counts from 1980 and stores time to two seconds, so anything earlier clamps to 1980
/// rather than wrapping into 2092, which is what a plain subtraction does.
fn dos_time_from(time: Option<std::time::SystemTime>) -> (u16, u16) {
    /// 1980-01-01T00:00:00Z, the earliest instant MS-DOS can express.
    const DOS_EPOCH: u64 = 315_532_800;
    let seconds = time
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs())
        .max(DOS_EPOCH);
    let day = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day_of_month) = civil_from_days(day as i64);
    let year = (year - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | (month << 5) | day_of_month;
    let time = ((rest / 3600) << 11) | (((rest % 3600) / 60) << 5) | ((rest % 60) / 2);
    (time as u16, date as u16)
}

/// Days since the Unix epoch to a proleptic Gregorian date. The inverse of the reader's.
fn civil_from_days(days: i64) -> (i64, u16, u16) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u16, day as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Format;
    use crate::reader::{ArchiveReader, OpenOptions};
    use crate::writer::ArchiveWriter;
    use std::io::{Cursor, Read as _};

    /// Writes an archive through the public API and returns the bytes.
    fn write(entries: &[(&str, &[u8])], compression: Method) -> Vec<u8> {
        let options = WriteOptions::default().with_compression(compression);
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Zip, &options).expect("a writer");
        for (name, contents) in entries {
            // No declared size, so every entry goes through the streaming path with a data
            // descriptor. That is the path worth testing, because it is the one that used to
            // buffer whole entries.
            let entry = NewEntry::file(*name);
            writer.write_entry(&entry, &mut &contents[..]).expect("write an entry");
        }
        writer.finish_into().expect("finish").into_inner()
    }

    #[test]
    fn a_written_archive_can_be_read_back_with_every_entry_intact() {
        let payload: Vec<u8> = (0..50_000u32).map(|value| (value % 251) as u8).collect();
        let bytes = write(&[("a.txt", &payload), ("b.txt", b"hello")], Method::Deflate);
        let mut reader = ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).unwrap();
        assert_eq!(reader.len(), 2);
        let mut got = Vec::new();
        reader.open_entry(0).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, payload, "entry 0");
        let mut got = Vec::new();
        reader.open_entry(1).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hello", "entry 1");
    }

    #[test]
    fn an_entry_with_a_declared_size_gets_a_plain_header_and_still_reads_back() {
        // The other branch of the header decision: a known size goes in the header and no data
        // descriptor is written. Both forms have to produce a readable archive.
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Zip, &options).expect("a writer");
        let payload: Vec<u8> = (0..10_000u32).map(|value| (value % 97) as u8).collect();
        let entry = NewEntry::file("known.txt").with_size(payload.len() as u64);
        writer.write_entry(&entry, &mut &payload[..]).expect("write an entry");
        let bytes = writer.finish_into().expect("finish").into_inner();
        let mut reader = ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).unwrap();
        let mut got = Vec::new();
        reader.open_entry(0).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), 10_000);
    }

    #[test]
    fn a_deflated_entry_is_smaller_than_a_stored_one() {
        // If this fails the deflater is not running at all, which is the failure mode that a
        // size check on the writer output catches and a read-back test does not.
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(500);
        let deflated = write(&[("a.txt", &payload)], Method::Deflate);
        let stored = write(&[("a.txt", &payload)], Method::None);
        assert!(
            deflated.len() < stored.len() / 2,
            "deflated {} should be well under half of stored {}",
            deflated.len(),
            stored.len()
        );
    }

    #[test]
    fn a_directory_is_written_with_a_trailing_slash_and_reads_back_as_a_directory() {
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Zip, &options).expect("a writer");
        writer
            .write_entry(&NewEntry::directory("dir"), &mut std::io::empty())
            .expect("write a directory");
        let bytes = writer.finish_into().expect("finish").into_inner();
        let reader = ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).unwrap();
        assert_eq!(reader.len(), 1);
        assert!(reader.entry(0).unwrap().is_directory());
        assert_eq!(reader.entry(0).unwrap().name, "dir");
    }

    #[test]
    fn a_symlinks_target_is_written_as_the_entrys_contents() {
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Zip, &options).expect("a writer");
        writer
            .write_entry(&NewEntry::symlink("link", "target.txt"), &mut std::io::empty())
            .expect("write a symlink");
        let bytes = writer.finish_into().expect("finish").into_inner();
        let mut reader = ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).unwrap();
        let mut got = Vec::new();
        reader.open_entry(0).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, b"target.txt");
    }

    #[test]
    fn an_entry_with_an_empty_name_is_refused() {
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Zip, &options).expect("a writer");
        let error = match writer.write_entry(&NewEntry::file(""), &mut std::io::empty()) {
            Ok(()) => panic!("an empty name must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }

    #[test]
    fn an_msdos_timestamp_round_trips_through_the_reader() {
        // The writer and the reader are two independent implementations of the same encoding, so
        // a disagreement between them shows up as every entry looking like 1980.
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_709_181_296);
        let (time, date) = dos_time_from(Some(when));
        let back = crate::formats::zip::dos_time_to_system_time(time, date).expect("a valid date");
        let seconds = back.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((seconds - 1_709_181_296).abs() <= 2, "round tripped to {seconds}");
    }

    #[test]
    fn a_time_before_1980_clamps_rather_than_wrapping_into_the_future() {
        let (time, date) = dos_time_from(Some(std::time::UNIX_EPOCH));
        assert_eq!(date >> 9, 0, "the DOS year is 1980 plus this field");
        assert_eq!(time, 0);
    }

    #[test]
    fn the_date_conversion_is_correct_across_a_leap_year() {
        // A hand-rolled civil-date conversion is where this usually goes wrong, so it is checked
        // against known dates either side of 29 February.
        for (days, expected) in [
            (0i64, (1970u32, 1u32, 1u32)),
            (58, (1970, 2, 28)),
            (59, (1970, 3, 1)),
            (365, (1971, 1, 1)),
            (366, (1971, 1, 2)),
            (17_000, (2016, 7, 18)),
        ] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!((year as u32, month as u32, day as u32), expected, "day {days}");
        }
    }

    #[test]
    fn the_deflater_is_built_once_for_the_whole_archive_and_reset_between_entries() {
        // Rule 1, tested directly rather than inferred from a timing. Writing 500 entries must
        // construct exactly one compressor; the old writer constructed one per entry, which is
        // where 1.35 GiB of the 1.40 GiB went.
        let payload: Vec<u8> = (0..2048).map(|value| (value % 251) as u8).collect();
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(sink, &options).expect("a writer");
        for index in 0..500 {
            let name = format!("entry{index}.txt");
            let mut entry = writer.begin(&NewEntry::file(&name), &options).unwrap();
            entry.write_all(&payload).unwrap();
            entry.finish().unwrap();
        }
        assert_eq!(
            writer.deflaters_built, 1,
            "500 entries built {} deflaters, so the compressor is not being reused",
            writer.deflaters_built
        );
        writer.finish().unwrap();
    }

    #[test]
    fn a_stored_only_archive_never_builds_a_deflater_at_all() {
        let options = WriteOptions::stored();
        let mut sink = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(&mut sink, &options).expect("a writer");
        for index in 0..10 {
            let name = format!("entry{index}.txt");
            let mut entry = writer.begin(&NewEntry::file(&name), &options).unwrap();
            entry.write_all(b"hello").unwrap();
            entry.finish().unwrap();
        }
        assert_eq!(writer.deflaters_built, 0, "nothing was compressed");
        let _ = writer.write_central_directory();
    }

    #[test]
    fn mixed_sizes_share_the_levels_deflate_offers_rather_than_an_encoder_per_entry() {
        // An archive that alternates a small entry (fast level) and a large one (best level)
        // should build exactly the two encoders the policy names, not one per entry. The pool is
        // the difference between this and the per-entry allocation the writer exists to remove.
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(sink, &options).expect("a writer");
        for (index, size) in [512usize, 16 * 1024].iter().enumerate() {
            for repeat in 0..20 {
                let payload: Vec<u8> =
                    (0..*size).map(|value| ((value.wrapping_mul(31) + repeat) % 251) as u8).collect();
                let name = format!("entry{index}.bin");
                let mut entry =
                    writer.begin(&NewEntry::file(&name).with_size(*size as u64), &options).unwrap();
                entry.write_all(&payload).unwrap();
                entry.finish().unwrap();
            }
        }
        assert_eq!(
            writer.deflaters_built, 2,
            "40 entries of two sizes should share two encoders, one per level, not one per entry"
        );
        assert_eq!(writer.deflaters.len(), 2, "the two policy levels are cached");
        assert_eq!(writer.deflaters[0].0, 1);
        assert_eq!(writer.deflaters[1].0, 6);
        assert!(writer.finish().is_ok());
    }
}
