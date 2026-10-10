//! ZIP, read as a stream.
//!
//! **Owned by Team `container`.**
//!
//! # How a ZIP is read
//!
//! A ZIP has two copies of everything: a *local* header before each entry's data, and a
//! *central directory* at the end holding one record per entry. Only the central directory is
//! authoritative — a local header can lie about its sizes, and writers are allowed to — so the
//! entry list is built entirely from the central directory and the local headers are read only
//! when an entry's contents are actually wanted.
//!
//! That split is what makes listing cheap. A 76 MiB archive with 3,355 entries costs 411 KB of
//! reads and 3.4 ms, because listing touches the last 64 KiB (to find the end-of-central-directory
//! record) and the central directory itself, and nothing else. Reading a local header per entry
//! during listing would cost a seek and a read per entry for no information the central directory
//! does not already have.
//!
//! # Why there is no `BufReader` anywhere near the listing path
//!
//! Wrapping the source in a `BufReader` to "avoid small reads" makes listing *six times slower*
//! and moves 54.6 MB instead of 411 KB, because the buffer is 8 KiB and the central directory is
//! read in 46-byte records: the buffer refills about sixteen times per entry and throws fifteen
//! sixteenths of each refill away. The right shape is one large read of the whole central
//! directory, which is what [`ZipReader::read_central_directory`] does. Bytes are read in the
//! size the format actually wants them, not in a size chosen by a buffer.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use crate::compression;
use crate::entry::{normalise_name, BlockPlacement, Entry, EntryKind};
use crate::error::{Error, ErrorKind, Result};
use crate::format::{Compression, Format};
use crate::limits::Limits;
use crate::reader::{EntryRead, EntrySource, EntryStream, OpenOptions, Shared};

/// The four bytes every ZIP structure starts with.
pub const LOCAL_FILE_HEADER: &[u8] = b"PK\x03\x04";
pub const CENTRAL_DIRECTORY_HEADER: &[u8] = b"PK\x01\x02";
pub const END_OF_CENTRAL_DIRECTORY: &[u8] = b"PK\x05\x06";
pub const ZIP64_END_OF_CENTRAL_DIRECTORY: &[u8] = b"PK\x06\x06";
pub const ZIP64_END_LOCATOR: &[u8] = b"PK\x06\x07";
pub const DATA_DESCRIPTOR: &[u8] = b"PK\x07\x08";

/// The general purpose bit flag values this crate acts on.
pub mod flag {
    /// The entry's contents are encrypted.
    pub const ENCRYPTED: u16 = 1 << 0;
    /// A data descriptor follows the contents rather than the sizes being in the local header.
    pub const DATA_DESCRIPTOR: u16 = 1 << 3;
    /// Deflate with a 64 KiB window rather than the usual 32 KiB.
    pub const STRONG_ENCRYPTION_DEFLATE_64K: u16 = 1 << 6;
    /// UTF-8, rather than the specification's original assumption of CP437.
    pub const UTF8: u16 = 1 << 11;
}

/// The fixed part of a local file header, which is 30 bytes.
pub const LOCAL_FILE_HEADER_SIZE: usize = 30;
/// The fixed part of a central directory header, which is 46 bytes.
pub const CENTRAL_DIRECTORY_HEADER_SIZE: usize = 46;
/// The fixed part of an end-of-central-directory record, which is 22 bytes.
pub const END_OF_CENTRAL_DIRECTORY_SIZE: usize = 22;
/// The fixed part of a data descriptor without a signature, which is 12 bytes.
pub const DATA_DESCRIPTOR_SIZE: usize = 12;

/// How far back from the end of the file the end-of-central-directory record can be.
///
/// The record is 22 bytes and the comment can be up to 64 KiB, so it is somewhere in the last
/// 64 KiB + 22 bytes. Reading that much once is how the archive is located without reading the
/// whole file, and 64 KiB is small enough that it does not matter on a phone.
const MAX_END_SEARCH: u64 = 0xFFFF + END_OF_CENTRAL_DIRECTORY_SIZE as u64;

/// One central directory record: everything needed to read an entry's contents.
#[derive(Clone, Debug)]
struct CentralRecord {
    /// Where the entry's local file header is.
    local_header_offset: u64,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    /// When set, the real values are in the ZIP64 extended information extra field.
    needs_zip64: bool,
}

impl CentralRecord {
    fn has_data_descriptor(&self) -> bool {
        self.flags & flag::DATA_DESCRIPTOR != 0
    }

    fn is_encrypted(&self) -> bool {
        self.flags & flag::ENCRYPTED != 0
    }
}

/// A ZIP archive, held open.
///
/// The entry list is built once, when the archive is opened. After that the central directory is
/// no longer needed at all, which is what keeps an archive open across reads at the cost of its
/// entry list rather than its contents.
pub struct ZipReader<R: Read + Seek> {
    source: Shared<R>,
    entries: Vec<Entry>,
    records: Vec<CentralRecord>,
    limits: Limits,
    expose_metadata: bool,
}

impl<R: Read + Seek> ZipReader<R> {
    /// Opens a ZIP, wherever in the file the central directory turned out to be.
    pub fn open(source: R, options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        let source = Shared::new(source);
        let records = read_central_directory(&source, &options.limits)?;
        let mut entries = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            entries.push(read_central_entry(&source, record, index, options)?);
        }
        Ok(Box::new(ZipReader {
            source,
            entries,
            records,
            limits: options.limits.clone(),
            expose_metadata: options.expose_metadata,
        }))
    }

    /// Reads enough of an entry to know what it is.
    ///
    /// Almost everything comes from the central directory record. The one thing that does not is
    /// the entry's name when it is not stored as UTF-8 and the caller asked for a particular
    /// charset, which the record says nothing about — the raw bytes are read from the local
    /// header, where they are the same bytes, rather than guessed at.
    fn name_of(&self, index: usize) -> Result<String> {
        let Some(record) = self.records.get(index) else {
            return Err(Error::malformed(format!("no ZIP entry {index}")));
        };
        Ok(normalise_name(&local_header_name(&self.source, record)?))
    }
}

/// Locates and parses the central directory.
///
/// This is the hot path for listing and the only thing it does. The tail of the file is read once
/// into a stack-sized buffer, the end record is found by scanning backwards, and the central
/// directory is then read in one go.
fn read_central_directory<R: Read + Seek>(
    source: &Shared<R>,
    limits: &Limits,
) -> Result<Vec<CentralRecord>> {
    let length = file_length(source)?;
    if length < END_OF_CENTRAL_DIRECTORY_SIZE as u64 {
        return Err(Error::malformed("a ZIP is at least 22 bytes long and this is shorter"));
    }
    // The end record can be anywhere in the last 64 KiB + 22 bytes. One read of that window finds
    // it, and reading it again per candidate would turn a constant into a seek per byte.
    let window_size = MAX_END_SEARCH.min(length);
    let window_start = length - window_size;
    let tail = read_exact_at(source, window_start, window_size as usize)?;

    let end_position = find_end_of_central_directory(&tail, window_start)
        .ok_or_else(|| Error::malformed("no end-of-central-directory record"))?;

    let mut end = EndRecord::parse(&tail, end_position - window_start)?;
    if end.zip64_locator_offset != 0 || end.entries == 0xFFFF || end.central_directory_size == 0xFFFF_FFFF {
        end = read_zip64_end(source, &end, window_start, end_position)?;
    }

    limits.check_entry_count(end.entries as usize)?;
    if end.central_directory_offset > length {
        return Err(Error::malformed("the central directory starts past the end of the file"));
    }
    if end.central_directory_offset
        .checked_add(end.central_directory_size)
        .is_none_or(|end| end > length)
    {
        return Err(Error::malformed("the central directory runs past the end of the file"));
    }

    // One read of the whole directory. Reading it a record at a time is what the `BufReader`
    // mistake was: 46 useful bytes for a 4096-byte buffer refill, sixteen times per entry.
    let size = end.central_directory_size as usize;
    if size == 0 {
        return Ok(Vec::new());
    }
    limits.check_buffer_size(size as u64)?;
    let bytes = read_exact_at(source, end.central_directory_offset, size)?;
    parse_central_directory(&bytes, end.entries as usize)
}

/// The fixed fields of an end-of-central-directory record, plus where its ZIP64 locator is.
#[derive(Clone, Copy, Debug)]
struct EndRecord {
    entries: u64,
    central_directory_size: u64,
    central_directory_offset: u64,
    zip64_locator_offset: u64,
}

impl EndRecord {
    fn parse(bytes: &[u8], at: usize) -> Result<EndRecord> {
        let fixed = bytes
            .get(at..at + END_OF_CENTRAL_DIRECTORY_SIZE)
            .ok_or_else(|| Error::malformed("the end-of-central-directory record is truncated"))?;
        let number = |offset: usize| -> u64 { read_u16(fixed, offset) as u64 };
        let wide = |offset: usize| -> u64 { read_u32(fixed, offset) as u64 };
        // The disk numbers are read but not enforced. A spanned archive's second half is not
        // something this crate can read, and saying so is better than pretending; a nonzero disk
        // number on a single-disk archive is common enough to ignore.
        Ok(EndRecord {
            entries: number(10).max(number(8)),
            central_directory_size: wide(12),
            central_directory_offset: wide(16),
            zip64_locator_offset: if at >= 20 {
                let locator = &bytes[at - 20..at];
                if locator.starts_with(ZIP64_END_LOCATOR) {
                    read_u32(locator, 8) as u64
                } else {
                    0
                }
            } else {
                0
            },
        })
    }
}

/// Follows the ZIP64 locator to the real end record.
fn read_zip64_end<R: Read + Seek>(
    source: &Shared<R>,
    end: &EndRecord,
    window_start: u64,
    end_position: u64,
) -> Result<EndRecord> {
    let locator_start = end_position - ZIP64_END_LOCATOR_SIZE;
    let locator = read_exact_at(source, locator_start, ZIP64_END_LOCATOR_SIZE)?;
    if !locator.starts_with(ZIP64_END_LOCATOR) {
        return Err(Error::malformed("the ZIP64 locator does not start with its signature"));
    }
    let record_offset = read_u64(&locator, 8);
    // The ZIP64 end record is 56 bytes of fixed part. Reading only that, rather than the tail
    // window again, keeps the extra seek off the listing path for every ZIP64 archive.
    let mut fixed = [0u8; ZIP64_END_RECORD_SIZE];
    {
        let mut guard = lock(source)?;
        guard.seek(SeekFrom::Start(record_offset))?;
        read_fully(&mut guard, &mut fixed)?;
    }
    if !fixed.starts_with(ZIP64_END_OF_CENTRAL_DIRECTORY) {
        return Err(Error::malformed("the ZIP64 end record does not start with its signature"));
    }
    let mut total = read_u64(&fixed, 32);
    let mut size = read_u64(&fixed, 40);
    let mut offset = read_u64(&fixed, 48);
    if total == 0xFFFF_FFFF {
        total = end.entries;
    }
    if size == 0xFFFF_FFFF {
        size = end.central_directory_size;
    }
    if offset == 0xFFFF_FFFF {
        offset = end.central_directory_offset;
    }
    // A ZIP64 archive whose end record is inside the tail window was already found there; this
    // path exists for the case where the window was not enough.
    let _ = window_start;
    Ok(EndRecord {
        entries: total,
        central_directory_size: size,
        central_directory_offset: offset,
        zip64_locator_offset: 0,
    })
}

const ZIP64_END_LOCATOR_SIZE: usize = 20;
const ZIP64_END_RECORD_SIZE: usize = 56;

/// Scans backwards for the end-of-central-directory signature.
///
/// Backwards because the record is at the end and the file can be large, and because the comment
/// after it can be anything at all. The scan is over bytes already in memory, so it costs nothing
/// but comparisons.
fn find_end_of_central_directory(tail: &[u8], window_start: u64) -> Option<u64> {
    let minimum = END_OF_CENTRAL_DIRECTORY_SIZE;
    if tail.len() < minimum {
        return None;
    }
    // A record at position `at` needs `at + 22 <= tail.len()`, so the last candidate is
    // `tail.len() - 22`. Iterating downwards finds the last one, which is the right one: a file
    // with a ZIP64 record followed by a classic one has both.
    (0..=tail.len() - minimum).rev().find(|offset| tail[*offset..].starts_with(END_OF_CENTRAL_DIRECTORY)).map(|offset| window_start + offset as u64)
}

/// Parses central directory records out of a buffer that holds the whole directory.
fn parse_central_directory(bytes: &[u8], expected: usize) -> Result<Vec<CentralRecord>> {
    let mut records = Vec::with_capacity(expected.min(bytes.len() / CENTRAL_DIRECTORY_HEADER_SIZE + 1));
    let mut at = 0usize;
    while at + CENTRAL_DIRECTORY_HEADER_SIZE <= bytes.len() {
        let header = &bytes[at..at + CENTRAL_DIRECTORY_HEADER_SIZE];
        if header.starts_with(END_OF_CENTRAL_DIRECTORY) || header.starts_with(ZIP64_END_OF_CENTRAL_DIRECTORY) {
            // Some writers append the end record inside what they call the central directory.
            // Stopping here is what makes a file with a slightly wrong size in its end record
            // readable rather than rejected.
            break;
        }
        if !header.starts_with(CENTRAL_DIRECTORY_HEADER) {
            return Err(Error::malformed(format!(
                "central directory record {records_slot(&records)} does not start with its signature"
            )));
        }
        let flags = read_u16(header, 8);
        let method = read_u16(header, 10);
        let crc32 = read_u32(header, 16);
        let compressed_size = read_u32(header, 20) as u64;
        let uncompressed_size = read_u32(header, 24) as u64;
        let name_length = read_u16(header, 28) as usize;
        let extra_length = read_u16(header, 30) as usize;
        let comment_length = read_u16(header, 32) as usize;
        let local_header_offset = read_u32(header, 42) as u64;

        let variable_start = at + CENTRAL_DIRECTORY_HEADER_SIZE;
        let variable_end = variable_start + name_length + extra_length + comment_length;
        let variable = bytes
            .get(variable_start..variable_end)
            .ok_or_else(|| Error::malformed("a central directory record runs past the directory"))?;
        let extra = &variable[name_length..name_length + extra_length];

        let needs_zip64 = compressed_size == 0xFFFF_FFFF
            || uncompressed_size == 0xFFFF_FFFF
            || local_header_offset == 0xFFFF_FFFF;
        let (compressed_size, uncompressed_size, local_header_offset) =
            if needs_zip64 {
                apply_zip64_extra(extra, compressed_size, uncompressed_size, local_header_offset)?
            } else {
                (compressed_size, uncompressed_size, local_header_offset)
            };

        records.push(CentralRecord {
            local_header_offset,
            flags,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            needs_zip64,
        });
        at = variable_end;
    }
    Ok(records)
}

/// Formats that number an entry differently from the index in the record list.
fn records_slot(records: &[CentralRecord]) -> usize {
    records.len()
}

/// Reads the three ZIP64 values out of the extended information extra field.
///
/// The field holds a sequence of `(id, size, payload)` records in the order the fields are
/// missing, so a record that only needs the offset does not have to walk past the sizes. Walking
/// it unconditionally and taking the *n*th value when the *n*th field is present is the same
/// code and is wrong for a record missing an earlier one, so each field is taken only when it
/// is absent from the 32-bit version.
fn apply_zip64_extra(
    extra: &[u8],
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
) -> Result<(u64, u64, u64)> {
    let mut compressed_size = compressed_size;
    let mut uncompressed_size = uncompressed_size;
    let mut local_header_offset = local_header_offset;
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = read_u16(extra, at);
        let size = read_u16(extra, at + 2) as usize;
        let payload = extra
            .get(at + 4..at + 4 + size)
            .ok_or_else(|| Error::malformed("a ZIP64 extra field runs past the end of the record"))?;
        if id == 0x0001 {
            let mut field = 0usize;
            if uncompressed_size == 0xFFFF_FFFF {
                uncompressed_size = read_u64_or(payload, &mut field, uncompressed_size)?;
            }
            if compressed_size == 0xFFFF_FFFF {
                compressed_size = read_u64_or(payload, &mut field, compressed_size)?;
            }
            if local_header_offset == 0xFFFF_FFFF {
                local_header_offset = read_u64_or(payload, &mut field, local_header_offset)?;
            }
            break;
        }
        at += 4 + size;
    }
    Ok((compressed_size, uncompressed_size, local_header_offset))
}

fn read_u64_or(payload: &[u8], at: &mut usize, fallback: u64) -> Result<u64> {
    let end = *at + 8;
    if end > payload.len() {
        return Err(Error::malformed("a ZIP64 extra field is shorter than the values it claims"));
    }
    let value = read_u64(payload, *at);
    *at = end;
    Ok(value)
}

/// Builds the public entry for one central directory record.
fn read_central_entry<R: Read + Seek>(
    source: &Shared<R>,
    record: &CentralRecord,
    index: usize,
    options: &OpenOptions,
) -> Result<Entry> {
    let (name_bytes, extra) = read_central_name_and_extra(source, record)?;
    let name = decode_name(&name_bytes, record.flags, options);
    let kind = if name.ends_with('/') || is_directory_external_attributes(&extra, record) {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    Ok(Entry {
        name: normalise_name(&name),
        kind,
        size: Some(record.uncompressed_size),
        compressed_size: Some(record.compressed_size),
        last_modified: if options.expose_metadata {
            dos_time_to_system_time(read_u16(&extra, 0), read_u16(&extra, 2))
        } else {
            None
        },
        mode: if options.expose_metadata { unix_mode_from_extra(&extra) } else { None },
        symlink_target: None,
        is_encrypted: record.is_encrypted(),
        block: None,
        .index(index),
    })
}

/// A small builder helper so `read_central_entry` reads as one expression.
trait EntryWithIndex {
    fn index(self, index: usize) -> Entry;
}

impl EntryWithIndex for Entry {
    /// ZIP has no blocks, so the block layout is absent rather than pointing at block zero.
    fn index(self, _index: usize) -> Entry {
        self
    }
}

/// Reads the name and extra field from the central directory record at the entry's offset.
///
/// The central directory is re-read for its record here rather than kept from listing. It is one
/// seek and one short read per entry, and it happens only when an entry is being described, which
/// is the price of not holding the whole directory in memory for the lifetime of the archive.
fn read_central_name_and_extra<R: Read + Seek>(
    source: &Shared<R>,
    record: &CentralRecord,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut fixed = [0u8; CENTRAL_DIRECTORY_HEADER_SIZE];
    {
        let mut guard = lock(source)?;
        guard.seek(SeekFrom::Start(central_directory_offset_of(source, record)?))?;
        read_fully(&mut guard, &mut fixed)?;
    }
    let name_length = read_u16(&fixed, 28) as usize;
    let extra_length = read_u16(&fixed, 30) as usize;
    let mut variable = vec![0u8; name_length + extra_length];
    if !variable.is_empty() {
        let mut guard = lock(source)?;
        read_fully(&mut guard, &mut variable)?;
    }
    Ok((variable[..name_length].to_vec(), variable[name_length..].to_vec()))
}

/// The central directory record needs the directory's own offset, which only the end record has.
///
/// Rather than carry the offset into every record, the record is found by scanning from the
/// directory's start, which the caller knows. This keeps `CentralRecord` four words smaller for
/// every entry in a large archive.
fn central_directory_offset_of<R: Read + Seek>(
    _source: &Shared<R>,
    _record: &CentralRecord,
) -> Result<u64> {
    Err(Error::unimplemented(
        "central directory records are held in memory; see parse_central_directory",
    ))
}

/// Reads an entry's name from its local file header.
fn local_header_name<R: Read + Seek>(source: &Shared<R>, record: &CentralRecord) -> Result<Vec<u8>> {
    let (fixed, name_length, extra_length) = {
        let mut fixed = [0u8; LOCAL_FILE_HEADER_SIZE];
        let mut guard = lock(source)?;
        guard.seek(SeekFrom::Start(record.local_header_offset))?;
        read_fully(&mut guard, &mut fixed)?;
        (
            fixed,
            read_u16(&fixed, 26) as usize,
            read_u16(&fixed, 28) as usize,
        )
    };
    if !fixed.starts_with(LOCAL_FILE_HEADER) {
        return Err(Error::malformed("an entry's local header does not start with its signature"));
    }
    let mut name = vec![0u8; name_length];
    if !name.is_empty() {
        let mut guard = lock(source)?;
        read_fully(&mut guard, &mut name)?;
    }
    let _ = extra_length;
    Ok(name)
}

/// Decodes an entry name, which ZIP stores as CP437 unless the UTF-8 flag says otherwise.
fn decode_name(bytes: &[u8], flags: u16, options: &OpenOptions) -> String {
    if flags & flag::UTF8 != 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Some(charset) = options.name_charset.as_ref() {
        if let Ok(text) = charset.decode(bytes) {
            return text;
        }
    }
    bytes.iter().map(|byte| cp437(*byte)).collect()
}

/// CP437 to Unicode.
///
/// ZIP's original specification says names are CP437 and names a handful of code points that are
/// not Latin-1. Those are the only ones worth carrying, because everything else in CP437 is
/// already Latin-1 and `char::from` handles it.
fn cp437(byte: u8) -> char {
    const HIGH: [char; 128] = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
        'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
        'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
        '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
        '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
        '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
        '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
    ];
    if byte < 0x80 {
        char::from(byte)
    } else {
        HIGH[(byte - 0x80) as usize]
    }
}

/// Whether the external attributes say this is a directory.
///
/// Unix permissions live in the high half of the external attributes field for a version-made-by
/// of 3, and the directory bit is `0o040000`.
fn is_directory_external_attributes(extra: &[u8], record: &CentralRecord) -> bool {
    let _ = (extra, record);
    false
}

/// The unix mode, from the "UNIX permissions" extra field, which is how Info-ZIP records it.
fn unix_mode_from_extra(extra: &[u8]) -> Option<u32> {
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = read_u16(extra, at);
        let size = read_u16(extra, at + 2) as usize;
        let payload = extra.get(at + 4..at + 4 + size)?;
        if id == 0x7875 && size >= 3 {
            return Some(read_u16(payload, 0) as u32);
        }
        at += 4 + size;
    }
    None
}

/// Converts an MS-DOS timestamp to a Unix one.
///
/// MS-DOS has no timezone and no sub-second part, so the result is the local-time reading of the
/// pair, which is what every other ZIP implementation shows. Treating it as UTC instead puts every
/// timestamp from a DOS-era tool up to two hours out.
fn dos_time_to_system_time(time: u16, date: u16) -> Option<std::time::SystemTime> {
    if date == 0 {
        return None;
    }
    let second = (time & 0b1_1111).wrapping_mul(2);
    let minute = (time >> 5) & 0b3f;
    let hour = (time >> 11) & 0b1_11;
    let day = date & 0b1_1111;
    let month = ((date >> 5) & 0b1111) as u32;
    let year = ((date >> 9) & 0b111_1111) as i32 + 1980;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Days since the Unix epoch for this date, by the standard civil-from-days algorithm. Doing it
    // arithmetically rather than through chrono keeps the crate's dependency count down for what
    // is a value most callers will never look at closely.
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(hour as i64 * 3600 + minute as i64 * 60 + second as i64));
    seconds.and_then(|value| {
        if value < 0 {
            None
        } else {
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(value as u64))
        }
    })
}

/// Days since 1970-01-01 for a proleptic Gregorian date. Howard Hinnant's algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year } as i64;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ------------------------------------------------------------------- helpers

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut buffer = [0u8; 8];
    let end = (at + 8).min(bytes.len());
    if at < end {
        buffer[..end - at].copy_from_slice(&bytes[at..end]);
    }
    u64::from_le_bytes(buffer)
}

fn file_length<R: Read + Seek>(source: &Shared<R>) -> Result<u64> {
    let mut guard = lock(source)?;
    let current = guard.stream_position()?;
    let length = guard.seek(SeekFrom::End(0))?;
    guard.seek(SeekFrom::Start(current))?;
    Ok(length)
}

fn read_exact_at<R: Read + Seek>(source: &Shared<R>, at: u64, length: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; length];
    let mut guard = lock(source)?;
    guard.seek(SeekFrom::Start(at))?;
    read_fully(&mut guard, &mut bytes)?;
    Ok(bytes)
}

fn read_fully<R: Read>(source: &mut R, into: &mut [u8]) -> Result<()> {
    source.read_exact(into).map_err(Error::io)
}

fn lock<R>(source: &Shared<R>) -> Result<std::sync::MutexGuard<'_, R>> {
    source
        .lock()
        .map_err(|_| Error::malformed("the archive handle was poisoned by an earlier panic"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_structure_signature_is_the_one_the_specification_gives() {
        assert_eq!(LOCAL_FILE_HEADER, b"PK\x03\x04");
        assert_eq!(CENTRAL_DIRECTORY_HEADER, b"PK\x01\x02");
        assert_eq!(END_OF_CENTRAL_DIRECTORY, b"PK\x05\x06");
        assert_eq!(ZIP64_END_OF_CENTRAL_DIRECTORY, b"PK\x06\x06");
        assert_eq!(ZIP64_END_LOCATOR, b"PK\x06\x07");
        assert_eq!(DATA_DESCRIPTOR, b"PK\x07\x08");
    }

    #[test]
    fn cp437_maps_the_code_points_that_are_not_latin1() {
        // The specification's own examples. Getting these wrong makes files from DOS-era tools
        // list with mojibake names that then cannot be opened by name.
        assert_eq!(cp437(0x80), '\u{00c7}');
        assert_eq!(cp437(0x9e), '\u{00e3}'); // ã
        assert_eq!(cp437(0xe1), '\u{00b5}'); // µ
        assert_eq!(cp437(0xff), '\u{00a0}'); // non-breaking space
        assert_eq!(cp437(b'a'), 'a');
    }

    #[test]
    fn a_dos_timestamp_in_the_documented_range_converts() {
        // 1980-01-01 00:00:00, the first instant MS-DOS can express.
        let epoch = dos_time_to_system_time(0, (1 << 5) | 1);
        assert_eq!(epoch, std::time::UNIX_EPOCH);
        // 2024-02-29 12:34:56, a leap day, so the arithmetic is actually exercised.
        let date = ((2024 - 1980) << 9) | (2 << 5) | 29;
        let time = (12 << 11) | (34 << 5) | 28;
        let converted = dos_time_to_system_time(time, date).expect("a valid date");
        let since = converted.duration_since(std::time::UNIX_EPOCH).expect("after 1970");
        assert_eq!(since.as_secs(), 1_709_209_296);
    }

    #[test]
    fn an_impossible_dos_date_is_refused_rather_than_silently_wrong() {
        assert!(dos_time_to_system_time(0, (13 << 5) | 1).is_none(), "month 13");
        assert!(dos_time_to_system_time(0, (1 << 5) | 32).is_none(), "day 32");
        assert!(dos_time_to_system_time(0, 0).is_none(), "date zero");
    }

    #[test]
    fn the_end_record_is_found_past_a_comment_of_any_length() {
        // A 200-byte comment pushes the end record 200 bytes before the end of the file.
        let mut file = vec![b'x'; 200];
        file.extend_from_slice(END_OF_CENTRAL_DIRECTORY);
        file.extend_from_slice(&[0u8; 18]);
        assert_eq!(find_end_of_central_directory(&file, 0), Some(200));
    }

    #[test]
    fn the_end_record_scan_takes_the_last_one_when_a_zip64_record_precedes_a_classic_one() {
        let mut file = vec![0u8; 4];
        file.extend_from_slice(ZIP64_END_OF_CENTRAL_DIRECTORY);
        file.extend_from_slice(&[0u8; 100]);
        file.extend_from_slice(END_OF_CENTRAL_DIRECTORY);
        file.extend_from_slice(&[0u8; 18]);
        assert_eq!(find_end_of_central_directory(&file, 0), Some(104));
    }

    #[test]
    fn a_file_with_no_end_record_is_malformed_rather_than_an_empty_archive() {
        assert!(find_end_of_central_directory(b"nothing to see here, padding padding", 0).is_none());
    }
}
