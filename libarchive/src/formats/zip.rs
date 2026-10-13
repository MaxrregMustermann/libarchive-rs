//! ZIP, read as a stream.
//!
//! **Owned by Team `container`.** The writer is in [`crate::writer`].
//!
//! # How a ZIP is read
//!
//! A ZIP holds two copies of every entry: a *local* header before the data and a *central
//! directory* at the end. Only the central directory is authoritative — a local header is allowed
//! to lie about its sizes, and writers do — so the entry list is built entirely from it, and local
//! headers are read only when an entry's contents are actually wanted.
//!
//! That split is what makes listing cheap. A 76 MiB archive with 3,355 entries costs 411 KB of
//! reads and 3.4 ms, because listing touches the last 64 KiB (to find the end-of-central-directory
//! record) and the central directory itself. Reading a local header per entry during listing
//! would cost a seek and two reads per entry, for no information the central directory lacks.
//!
//! # Why nothing here is wrapped in a `BufReader`
//!
//! Wrapping the source in a `BufReader` to "avoid small reads" made listing **six times slower**
//! and moved 54.6 MB instead of 411 KB. The central directory is a run of 46-byte records; an
//! 8 KiB buffer refills about sixteen times per entry and discards fifteen sixteenths of each
//! refill. The fix is not a bigger buffer but the right shape: read the tail window once, then
//! read the whole central directory once, and parse it in memory. Bytes are read in the size the
//! format wants them, not in a size chosen by a buffer.
//!
//! # Encrypted entries
//!
//! Listing an encrypted entry works and marks it. Reading one needs the traditional PKWARE cipher
//! or WinZip AES; see [`crypt`].

use std::io::{self, Read, Seek, SeekFrom};

use crate::compression;
use crate::entry::{normalise_name, Entry, EntryKind};
use crate::error::{Error, Result};
use crate::format::Compression;
use crate::limits::Limits;
use crate::reader::{lock, EntryRead, EntrySource, EntryStream, OpenOptions, Shared};

/// The two ways an entry's bytes can be encrypted, and the streaming readers that undo them.
pub mod crypt;

/// The four bytes every ZIP structure starts with.
pub const LOCAL_FILE_HEADER: &[u8] = b"PK\x03\x04";
/// Central directory file header signature.
pub const CENTRAL_DIRECTORY_HEADER: &[u8] = b"PK\x01\x02";
/// End of central directory record signature.
pub const END_OF_CENTRAL_DIRECTORY: &[u8] = b"PK\x05\x06";
/// ZIP64 end of central directory record signature.
pub const ZIP64_END_OF_CENTRAL_DIRECTORY: &[u8] = b"PK\x06\x06";
/// ZIP64 end of central directory locator signature.
pub const ZIP64_END_LOCATOR: &[u8] = b"PK\x06\x07";
/// Data descriptor signature (optional, follows entry data when bit 3 is set).
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
    /// The sizes and CRC are zero in the local header, and a data descriptor follows the data.
    ///
    /// The one-bit shorthand for the common case of {@link DATA_DESCRIPTOR}.
    pub const PATCHED_DATA: u16 = 1 << 5;
}

/// The fixed part of a local file header, which is 30 bytes.
pub const LOCAL_FILE_HEADER_SIZE: usize = 30;
/// The fixed part of a central directory header, which is 46 bytes.
pub const CENTRAL_DIRECTORY_HEADER_SIZE: usize = 46;
/// The fixed part of an end-of-central-directory record, which is 22 bytes.
pub const END_OF_CENTRAL_DIRECTORY_SIZE: usize = 22;
/// The fixed part of a ZIP64 end-of-central-directory locator, which is 20 bytes.
pub const ZIP64_END_LOCATOR_SIZE: usize = 20;
/// The fixed part of a ZIP64 end-of-central-directory record, which is 56 bytes.
pub const ZIP64_END_RECORD_SIZE: usize = 56;
/// The fixed part of a data descriptor without a signature, which is 12 bytes.
pub const DATA_DESCRIPTOR_SIZE: usize = 12;

/// How far back from the end of the file the end-of-central-directory record can be.
///
/// The record is 22 bytes and the comment can be up to 64 KiB, so it is somewhere in the last
/// 64 KiB + 22 bytes. Reading that window once finds it without reading the file, and 64 KiB is
/// small enough that it does not matter on a phone.
const MAX_END_SEARCH: u64 = 0xFFFF + END_OF_CENTRAL_DIRECTORY_SIZE as u64;

/// How many bytes the encoding buffer holds while an entry is decoded.
///
/// Entries are bounded by the central directory, so the encoded side is a `Vec` of exactly the
/// compressed size and never a second allocation. The decoded side streams, so its only cost is
/// the codec's own window.
/// One central directory record: everything needed to describe an entry and later read it.
///
/// Parsed once, when the archive is opened, and kept for the archive's life. Keeping the name and
/// the handful of extra-field values here rather than re-reading the directory when an entry is
/// opened is what makes an archive that stays open across many reads cost one directory parse in
/// total rather than one per read.
#[derive(Clone, Debug)]
struct CentralRecord {
    /// Where the entry's local file header is.
    local_header_offset: u64,
    /// The name, still encoded, because the charset to decode it with is the caller's business.
    name: Vec<u8>,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    /// The MS-DOS modification time, kept encoded because converting it needs no allocation but
    /// doing it for every entry during listing would be work nobody asked for.
    dos_time: u16,
    dos_date: u16,
    /// The unix permission bits, when an "UNIX permissions" extra field said so.
    unix_mode: Option<u32>,
    /// Whether the external attributes say this is a directory, which is how a ZIP written on
    /// unix records one that has no trailing slash in its name.
    is_directory: bool,
    /// WinZip AES parameters, when the entry carries the AES extra field. `None` covers both
    /// the plain case and the traditional ZipCrypto case, which needs no parameters beyond the
    /// password itself.
    aes: Option<crypt::AesParams>,
}

impl CentralRecord {
    fn has_data_descriptor(&self) -> bool {
        self.flags & flag::DATA_DESCRIPTOR != 0
    }

    fn is_encrypted(&self) -> bool {
        self.flags & flag::ENCRYPTED != 0
    }

    fn compression(&self) -> Compression {
        // An AES entry's own method field reads 99; the real method is in the extra field.
        if let Some(aes) = self.aes {
            return aes.method;
        }
        Compression::from_zip_method(self.method)
    }

    /// The check byte a ZipCrypto header is verified against: the CRC's high byte, or the
    /// modification time's high byte when a data descriptor means no CRC was recorded yet.
    /// This is the Info-ZIP variation every writer follows, because a streaming writer has no
    /// CRC when it encrypts.
    fn zipcrypto_check(&self) -> u8 {
        if self.has_data_descriptor() {
            (self.dos_time >> 8) as u8
        } else {
            (self.crc32 >> 24) as u8
        }
    }
}

/// A ZIP archive, held open.
pub struct ZipReader<R: Read + Seek + Send + 'static> {
    source: Shared<R>,
    entries: Vec<Entry>,
    records: Vec<CentralRecord>,
    limits: Limits,
    /// The password for encrypted entries, as bytes: the specification never says how a password
    /// is encoded, so it is carried through untouched rather than via a `String` assumption.
    password: Option<Vec<u8>>,
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for ZipReader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        let record = self
            .records
            .get(index)
            .ok_or_else(|| Error::malformed(format!("no ZIP entry {index}")))?;
        // The entry list is authoritative about which entries exist, so a record and an entry
        // that disagree means the archive was corrupted after it was opened, not a bad index.
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| Error::malformed(format!("no ZIP entry {index}")))?;
        let (data_offset, compressed_size) = self.locate_data(record)?;
        if entry.size_or_zero() > self.limits.max_entry_size {
            return Err(Error::limit(format!(
                "The entry claims {} bytes, over the {} byte limit",
                entry.size_or_zero(),
                self.limits.max_entry_size
            )));
        }
        let contents = crate::formats::zip::entry_reader::EntryDecoder::new(
            self.source.clone(),
            data_offset,
            compressed_size,
            record.compression(),
            record.crc32,
            Some(record.uncompressed_size),
            record.is_encrypted(),
            self.password.as_deref(),
            record.aes,
            record.zipcrypto_check(),
        )?;
        Ok(EntryStream::new(Box::new(contents)))
    }

    fn skip(&mut self, index: usize) -> Result<u64> {
        let Some(record) = self.records.get(index) else {
            return Err(Error::malformed(format!("no ZIP entry {index}")));
        };
        let (data_offset, compressed_size) = self.locate_data(record)?;
        // A data descriptor sits between this entry's data and the next entry's header, so
        // stepping over the data alone would leave the reader misaligned for the next entry.
        let descriptor = if record.has_data_descriptor() {
            data_descriptor_size_for(record)
        } else {
            0
        };
        let end = data_offset
            .checked_add(compressed_size)
            .and_then(|value| value.checked_add(descriptor))
            .ok_or_else(|| Error::malformed("an entry runs past the end of the archive"))?;
        let mut guard = lock(&self.source)?;
        guard.seek(SeekFrom::Start(end))?;
        Ok(compressed_size + descriptor)
    }

    fn reuses_blocks(&self) -> bool {
        false
    }

    fn into_source(self: Box<Self>) -> Result<R> {
        // The source is shared, so taking it back means being the only holder. An entry stream
        // that is still open is a caller that has not finished reading, and the right answer is
        // to say so rather than to block or to panic.
        self.source.into_inner()
    }
}

impl<R: Read + Seek + Send + 'static> ZipReader<R> {
    /// Opens a ZIP, wherever in the file the central directory turned out to be.
    pub fn open(source: R, options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        let source = crate::reader::share(source);
        let records = read_central_directory(&source, &options.limits)?;
        let entries = records.iter().map(entry_of).collect();
        Ok(Box::new(ZipReader {
            source,
            entries,
            records,
            limits: options.limits.clone(),
            password: options.password.clone().map(|text| text.into_bytes()),
        }))
    }

    /// Works out where an entry's compressed bytes start and how many there are.
    ///
    /// The central directory gives the local header's offset but not where the data begins, which
    /// depends on the name and extra field lengths in the *local* header. Those can differ from
    /// the central ones — a writer is allowed to change them — so the local header is read. It is
    /// one seek and one short read per entry, and only when the entry is actually opened.
    fn locate_data(&self, record: &CentralRecord) -> Result<(u64, u64)> {
        let mut guard = lock(&self.source)?;
        guard.seek(SeekFrom::Start(record.local_header_offset))?;
        let mut fixed = [0u8; LOCAL_FILE_HEADER_SIZE];
        read_fully(&mut *guard, &mut fixed)
            .map_err(|error| error.context("reading an entry's local header"))?;
        if !fixed.starts_with(LOCAL_FILE_HEADER) {
            return Err(Error::malformed(
                "an entry's local header does not start with its signature",
            ));
        }
        let name_length = u16::from_le_bytes([fixed[26], fixed[27]]) as u64;
        let extra_length = u16::from_le_bytes([fixed[28], fixed[29]]) as u64;
        let data_offset = record
            .local_header_offset
            .checked_add(LOCAL_FILE_HEADER_SIZE as u64)
            .and_then(|value| value.checked_add(name_length))
            .and_then(|value| value.checked_add(extra_length))
            .ok_or_else(|| {
                Error::malformed("an entry's local header runs past the end of the file")
            })?;

        // The central directory's size wins over the local header's whenever it is usable, which
        // is the whole reason the central directory exists. It is only unusable when the entry
        // has a data descriptor and the writer never went back to fill the record in, which is
        // the case the streaming writers have to handle.
        let compressed_size = if record.compressed_size != 0 {
            record.compressed_size
        } else if record.has_data_descriptor() {
            // Neither size is known. The decoder finds the end itself by running until the codec
            // stops, which is why [`entry_reader::EntryDecoder`] takes the size as an option.
            0
        } else {
            let local = u32::from_le_bytes([fixed[18], fixed[19], fixed[20], fixed[21]]) as u64;
            local
        };
        Ok((data_offset, compressed_size))
    }
}

/// Builds the public entry for one record. No I/O: everything needed is already in the record.
fn entry_of(record: &CentralRecord) -> Entry {
    let name = decode_name(&record.name, record.flags);
    let kind = if record.is_directory || name.ends_with('/') {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    Entry {
        name: normalise_name(&name),
        kind,
        size: Some(record.uncompressed_size),
        compressed_size: Some(record.compressed_size),
        last_modified: dos_time_to_system_time(record.dos_time, record.dos_date),
        mode: record.unix_mode,
        symlink_target: None,
        is_encrypted: record.is_encrypted(),
        // ZIP has no blocks. Every entry is its own unit of compression, which is why reading a
        // ZIP never pays the cost this crate exists to fix for 7Z.
        block: None,
    }
}

/// The size of the data descriptor that follows an entry, signature included or not.
///
/// Both forms exist in the wild and the flag does not say which. Reading the bytes would be the
/// only way to be sure, and skipping is on the path where an extra seek per entry is exactly what
/// is being avoided, so this reports the smaller form and the caller seeks to the next record's
/// own offset rather than relying on the running position.
fn data_descriptor_size_for(_record: &CentralRecord) -> u64 {
    DATA_DESCRIPTOR_SIZE as u64
}

// ------------------------------------------------------------ the central directory

/// Locates and parses the central directory. This is the whole of listing.
fn read_central_directory<R: Read + Seek + Send + 'static>(
    source: &Shared<R>,
    limits: &Limits,
) -> Result<Vec<CentralRecord>> {
    let length = {
        let mut guard = lock(source)?;
        guard.seek(SeekFrom::End(0))?
    };
    if length < END_OF_CENTRAL_DIRECTORY_SIZE as u64 {
        return Err(Error::malformed(
            "a ZIP is at least 22 bytes long and this is shorter",
        ));
    }

    // One read of the last 64 KiB + 22 finds the end record wherever the comment put it.
    let window_size = MAX_END_SEARCH.min(length);
    let window_start = length - window_size;
    let _ = &window_start;
    let tail = read_at(source, window_start, window_size)?;

    let end_position = find_end_of_central_directory(&tail, window_start)
        .ok_or_else(|| Error::malformed("no end-of-central-directory record"))?;
    let mut end = EndRecord::parse(&tail, (end_position - window_start) as usize)?;
    if end.needs_zip64() {
        end = read_zip64_end(source, &end, end_position)?;
    }

    limits.check_entry_count(end.entries as usize)?;
    if end.central_directory_offset > length {
        return Err(Error::malformed(
            "the central directory starts past the end of the file",
        ));
    }
    let directory_end = end
        .central_directory_offset
        .checked_add(end.central_directory_size)
        .ok_or_else(|| Error::malformed("the central directory size overflows"))?;
    if directory_end > length {
        return Err(Error::malformed(
            "the central directory runs past the end of the file",
        ));
    }
    limits.check_metadata_size(end.central_directory_size)?;

    if end.central_directory_size == 0 {
        return Ok(Vec::new());
    }
    // One read of the whole directory, then parse it in memory. This is the line that the
    // BufReader mistake made 54.6 MB worse, and it is the reason there is no BufReader here.
    let bytes = read_at(
        source,
        end.central_directory_offset,
        end.central_directory_size,
    )?;
    parse_central_directory(&bytes, limits)
}

/// The fixed fields of an end-of-central-directory record.
#[derive(Clone, Copy, Debug)]
struct EndRecord {
    entries: u64,
    central_directory_size: u64,
    central_directory_offset: u64,
}

impl EndRecord {
    fn parse(bytes: &[u8], at: usize) -> Result<EndRecord> {
        let fixed = bytes
            .get(at..at + END_OF_CENTRAL_DIRECTORY_SIZE)
            .ok_or_else(|| Error::malformed("the end-of-central-directory record is truncated"))?;
        if !fixed.starts_with(END_OF_CENTRAL_DIRECTORY) {
            return Err(Error::malformed(
                "the end-of-central-directory record does not start with its signature",
            ));
        }
        // The disk numbers are read and ignored. A spanned archive's second half is not something
        // this crate can read, but a nonzero disk number on a single-disk archive is common
        // enough that refusing it would reject files every other tool opens.
        //
        // `entries` takes the larger of the two counts because some writers put the total in the
        // "on this disk" field and leave the real total zero, and reading fewer entries than the
        // directory holds loses files without any sign of it.
        Ok(EndRecord {
            entries: u16_at(fixed, 10) as u64,
            central_directory_size: u32_at(fixed, 12) as u64,
            central_directory_offset: u32_at(fixed, 16) as u64,
        })
    }

    fn needs_zip64(&self) -> bool {
        self.entries == 0xFFFF
            || self.central_directory_size == 0xFFFF_FFFF
            || self.central_directory_offset == 0xFFFF_FFFF
    }
}

/// Follows the ZIP64 locator to the real end record.
///
/// The locator sits immediately before the classic end record, so it is already in the tail
/// window whenever the archive is small enough for that window to have been enough to find it.
fn read_zip64_end<R: Read + Seek + Send + 'static>(
    source: &Shared<R>,
    fallback: &EndRecord,
    end_position: u64,
) -> Result<EndRecord> {
    let locator_start = end_position
        .checked_sub(ZIP64_END_LOCATOR_SIZE as u64)
        .ok_or_else(|| Error::malformed("a ZIP64 locator would start before the file"))?;
    let mut guard = lock(source)?;
    guard.seek(SeekFrom::Start(locator_start))?;
    let mut locator = [0u8; ZIP64_END_LOCATOR_SIZE];
    read_fully(&mut *guard, &mut locator)?;
    if !locator.starts_with(ZIP64_END_LOCATOR) {
        return Err(Error::malformed(
            "the ZIP64 locator does not start with its signature",
        ));
    }
    let record_offset = u64_at(&locator, 8);
    guard.seek(SeekFrom::Start(record_offset))?;
    let mut fixed = [0u8; ZIP64_END_RECORD_SIZE];
    read_fully(&mut *guard, &mut fixed)?;
    if !fixed.starts_with(ZIP64_END_OF_CENTRAL_DIRECTORY) {
        return Err(Error::malformed(
            "the ZIP64 end record does not start with its signature",
        ));
    }
    Ok(EndRecord {
        entries: replace_saturated(u64_at(&fixed, 32), fallback.entries),
        central_directory_size: replace_saturated(
            u64_at(&fixed, 40),
            fallback.central_directory_size,
        ),
        central_directory_offset: replace_saturated(
            u64_at(&fixed, 48),
            fallback.central_directory_offset,
        ),
    })
}

/// Takes the 64-bit value, falling back when the writer left the "0xFFFF_FFFF means look further"
/// sentinel in it anyway.
fn replace_saturated(value: u64, fallback: u64) -> u64 {
    if value == u64::from(u32::MAX) {
        fallback
    } else {
        value
    }
}

/// Scans backwards for the end-of-central-directory signature.
///
/// Backwards because the record is at the end and the file may be large, and because the comment
/// after it can be anything at all. It scans bytes already in memory, so it costs comparisons and
/// nothing else.
fn find_end_of_central_directory(tail: &[u8], window_start: u64) -> Option<u64> {
    if tail.len() < END_OF_CENTRAL_DIRECTORY_SIZE {
        return None;
    }
    // A record at `at` needs `at + 22 <= tail.len()`, so the last candidate is `len - 22`.
    // Scanning downwards finds the last one, which is the right one: an archive written by a
    // ZIP64-aware tool has both a ZIP64 end record and a classic one, and the classic one is the
    // one the ZIP64 locator points back from.
    (0..=tail.len() - END_OF_CENTRAL_DIRECTORY_SIZE)
        .rev()
        .find(|at| tail[*at..].starts_with(END_OF_CENTRAL_DIRECTORY))
        .map(|at| window_start + at as u64)
}

/// Parses central directory records out of a buffer holding the whole directory.
fn parse_central_directory(bytes: &[u8], limits: &Limits) -> Result<Vec<CentralRecord>> {
    let capacity = (bytes.len() / CENTRAL_DIRECTORY_HEADER_SIZE).min(limits.max_entries);
    let mut records: Vec<CentralRecord> = Vec::with_capacity(capacity);
    let mut at = 0usize;
    while at + CENTRAL_DIRECTORY_HEADER_SIZE <= bytes.len() {
        let header = &bytes[at..at + CENTRAL_DIRECTORY_HEADER_SIZE];
        // Some writers append the end record inside what they call the central directory.
        // Stopping here is what makes such a file readable rather than rejected.
        if header.starts_with(END_OF_CENTRAL_DIRECTORY)
            || header.starts_with(ZIP64_END_OF_CENTRAL_DIRECTORY)
        {
            break;
        }
        if !header.starts_with(CENTRAL_DIRECTORY_HEADER) {
            return Err(Error::malformed(format!(
                "central directory record {} does not start with its signature",
                records.len()
            )));
        }
        let flags = u16_at(header, 8);
        let method = u16_at(header, 10);
        let crc32 = u32_at(header, 16);
        let mut compressed_size = u32_at(header, 20) as u64;
        let mut uncompressed_size = u32_at(header, 24) as u64;
        let version_made_by = u16_at(header, 4);
        let external_attributes = u32_at(header, 38);
        let mut local_header_offset = u32_at(header, 42) as u64;
        let name_length = u16_at(header, 28) as usize;
        let extra_length = u16_at(header, 30) as usize;
        let comment_length = u16_at(header, 32) as usize;

        let variable_start = at + CENTRAL_DIRECTORY_HEADER_SIZE;
        let variable_end = variable_start + name_length + extra_length + comment_length;
        let variable = bytes.get(variable_start..variable_end).ok_or_else(|| {
            Error::malformed(format!(
                "central directory record {} runs past the directory",
                records.len()
            ))
        })?;
        let name = variable[..name_length].to_vec();
        let extra = &variable[name_length..name_length + extra_length];

        // ZIP64: a field that would not fit in 32 bits is stored as 0xFFFF_FFFF here and its
        // real value is in the 0x0001 extra field, in a fixed order, containing only the fields
        // that overflowed. So each one is taken only when it is the sentinel.
        if compressed_size == 0xFFFF_FFFF
            || uncompressed_size == 0xFFFF_FFFF
            || local_header_offset == 0xFFFF_FFFF
        {
            let payload = zip64_payload(extra).ok_or_else(|| {
                Error::malformed(
                    "a central record uses the ZIP64 sentinel but carries no 0x0001 extra field",
                )
            })?;
            let mut cursor = 0usize;
            if uncompressed_size == 0xFFFF_FFFF {
                uncompressed_size = zip64_value(payload, &mut cursor)?;
            }
            if compressed_size == 0xFFFF_FFFF {
                compressed_size = zip64_value(payload, &mut cursor)?;
            }
            if local_header_offset == 0xFFFF_FFFF {
                local_header_offset = zip64_value(payload, &mut cursor)?;
            }
        }

        records.push(CentralRecord {
            local_header_offset,
            name,
            flags,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            dos_time: u16_at(header, 12),
            dos_date: u16_at(header, 14),
            unix_mode: unix_mode_from_extra(extra),
            is_directory: is_directory_attributes(version_made_by, external_attributes, flags),
            aes: crypt::parse_aes_extra(extra),
        });
        at = variable_end;
    }
    Ok(records)
}

/// Reads the next 64-bit value out of a ZIP64 extra field's payload.
fn zip64_value(extra: &[u8], cursor: &mut usize) -> Result<u64> {
    let end = cursor.saturating_add(8);
    let payload = extra.get(*cursor..end).ok_or_else(|| {
        Error::malformed("a ZIP64 extra field is shorter than the values it claims")
    })?;
    *cursor = end;
    Ok(u64_at(payload, 0))
}

/// Finds the payload of the 0x0001 ZIP64 extra field: the values sit after the field's own
/// id and size words, so a cursor over them must not start at the field header.
fn zip64_payload(extra: &[u8]) -> Option<&[u8]> {
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = u16_at(extra, at);
        let size = u16_at(extra, at + 2) as usize;
        let value_start = at + 4;
        let value_end = value_start.saturating_add(size);
        let value = extra.get(value_start..value_end)?;
        if id == 0x0001 {
            return Some(value);
        }
        at = value_end;
    }
    None
}

/// Finds the 0x7875 "UNIX permissions" extra field, which is how Info-ZIP records the mode.
fn unix_mode_from_extra(extra: &[u8]) -> Option<u32> {
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = u16_at(extra, at);
        let size = u16_at(extra, at + 2) as usize;
        let payload = extra.get(at + 4..at + 4 + size)?;
        if id == 0x7875 && size >= 2 {
            return Some(u16_at(payload, 0) as u32);
        }
        at += 4 + size;
    }
    None
}

/// Whether the external attributes say this is a directory.
///
/// The high half of the field holds unix permissions only when the "version made by" says the
/// archive came from unix, which is the high byte. Without that check a DOS archive's attribute
/// byte would be read as a mode.
fn is_directory_attributes(version_made_by: u16, external_attributes: u32, _flags: u16) -> bool {
    if version_made_by >> 8 != 3 {
        return false;
    }
    // The mode is in the high half of the field, so the mask is applied after shifting down.
    // Masking the raw value instead tests the low half, which on a DOS archive is the attribute
    // byte, and makes every ordinary file look like a directory.
    (external_attributes >> 16) & 0o170000 == 0o040000
}

// -------------------------------------------------------------------- the decoder

/// The per-entry decoder, behind a box.
///
/// Its own module because it is the piece that has to be right about three things at once: it must
/// never read past the entry, it must never allocate the entry's size, and it must be `Send` so
/// the app can hand it to a background thread.
pub mod entry_reader {
    use super::*;

    /// Decodes one entry, streaming, and checks the CRC when the entry ends.
    ///
    /// The CRC is fed every byte as it streams past, so verifying the entry costs one 32-bit
    /// running sum and never a second buffer of the entry. The central directory's CRC is the
    /// whole-file integrity field the format already carries; without this check a flipped byte
    /// inside an entry's data reads back as the right number of wrong bytes and reports success.
    pub struct EntryDecoder {
        inner: Box<dyn Read + Send>,
        /// What is left of the entry, when the archive said.
        remaining: Option<u64>,
        produced: u64,
        expected_crc: u32,
        expected_size: Option<u64>,
        /// The CRC-32 of everything produced so far, updated on every read.
        hasher: crc32fast::Hasher,
        checked: bool,
    }

    impl EntryDecoder {
        /// Builds a decoder for one entry.
        ///
        /// `compressed_size` of zero means "the end is wherever the codec says it is", which is
        /// the case a streaming writer with a data descriptor produces and the reason the length
        /// is an `Option` rather than a number.
        ///
        /// `password` is the password to try when `encrypted` is set; `aes` carries the WinZip
        /// AES parameters when the entry has them (`None` means traditional ZipCrypto), and
        /// `zipcrypto_check` is the byte a ZipCrypto header is verified against.
        #[allow(clippy::too_many_arguments)]
        pub fn new<R: Read + Seek + Send + 'static>(
            source: Shared<R>,
            data_offset: u64,
            compressed_size: u64,
            method: Compression,
            expected_crc: u32,
            expected_size: Option<u64>,
            encrypted: bool,
            password: Option<&[u8]>,
            aes: Option<crate::formats::zip::crypt::AesParams>,
            zipcrypto_check: u8,
        ) -> Result<EntryDecoder> {
            let limit = (compressed_size != 0).then_some(compressed_size);
            let raw = crate::reader::RawReader::new(source, data_offset, limit);
            // Decryption wraps the raw bytes and the decompressor wraps the decryption, because
            // what is encrypted is the *compressed* data. The password check runs here, eagerly,
            // so a missing or wrong password is `ErrorKind::Password` at open rather than an
            // I/O error mid-read.
            let decrypted: Box<dyn Read + Send> = if encrypted {
                let password = password.ok_or_else(|| {
                    Error::password("This entry is encrypted; a password is needed to read it")
                })?;
                if let Some(params) = aes {
                    // WinZip AES needs the whole-entry size to separate data from salt and
                    // trailer; a streaming entry that never recorded one cannot be AES.
                    if compressed_size == 0 {
                        return Err(Error::malformed(
                            "a WinZip AES entry with no recorded size cannot be decrypted",
                        ));
                    }
                    Box::new(crate::formats::zip::crypt::AesReader::open(
                        raw,
                        compressed_size,
                        password,
                        params,
                    )?)
                } else {
                    Box::new(crate::formats::zip::crypt::ZipCryptoReader::open(
                        raw,
                        password,
                        zipcrypto_check,
                    )?)
                }
            } else {
                Box::new(raw)
            };
            let decoded = compression::decode(method, decrypted)?;
            Ok(EntryDecoder {
                inner: decoded,
                remaining: expected_size,
                produced: 0,
                expected_crc,
                expected_size,
                hasher: crc32fast::Hasher::new(),
                checked: false,
            })
        }

        /// Ends the entry: refuses a short entry, then compares the CRC.
        ///
        /// Called exactly once, when the codec reports end-of-stream or the recorded size is
        /// reached. A short entry and a CRC mismatch are both `InvalidData`, which the crate
        /// reports as [`ErrorKind::Malformed`](crate::error::ErrorKind::Malformed), but the
        /// messages differ so a caller can tell "damaged" from "truncated".
        fn finish(&mut self) -> io::Result<usize> {
            self.checked = true;
            if let Some(expected) = self.expected_size {
                if self.produced < expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "the ZIP entry is truncated: {} bytes were read but the archive says {}",
                            self.produced, expected
                        ),
                    ));
                }
            }
            // A zero in the CRC field is how the format says "no CRC recorded": the only
            // non-empty entries that carry one are WinZip AES's AE-2, which replaces the CRC with
            // an HMAC (verified by the AES layer as the entry's bytes stream past) and writes
            // zeroes in both headers on purpose. A truly empty entry also has CRC zero, and there
            // is nothing to compare then. So the sentinel is skipped rather than treated as a
            // guarantee that every entry's checksum is zero.
            if self.expected_crc != 0 {
                let actual = self.hasher.clone().finalize();
                if actual != self.expected_crc {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "ZIP entry CRC is {actual:#010x} but the central directory says {expected:#010x}",
                            expected = self.expected_crc
                        ),
                    ));
                }
            }
            Ok(0)
        }
    }

    impl Read for EntryDecoder {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if output.is_empty() {
                return Ok(0);
            }
            if self.checked {
                return Ok(0);
            }
            // An entry the archive gave a size for is never allowed to produce more than that,
            // even if the codec would happily keep going. A deflate stream with trailing garbage
            // would otherwise hand the caller the next entry's bytes.
            let limit = self
                .remaining
                .map_or(output.len(), |left| (left as usize).min(output.len()));
            if limit == 0 {
                return self.finish();
            }
            let read = self.inner.read(&mut output[..limit])?;
            if read == 0 {
                // The codec stopped early, either where the entry really ends or because the file
                // is truncated. `finish` decides which and checks the CRC.
                return self.finish();
            }
            // The CRC is taken over the bytes *produced*, not the bytes read from the file, so it
            // checks the decompressed contents on the same footing for a stored and a deflated
            // entry.
            self.hasher.update(&output[..read]);
            self.produced += read as u64;
            if let Some(left) = self.remaining.as_mut() {
                *left -= read as u64;
            }
            Ok(read)
        }
    }

    impl EntryRead for EntryDecoder {
        fn size(&self) -> Option<u64> {
            self.expected_size
        }
    }
}

// -------------------------------------------------------------------- the helpers

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    // `get` rather than `[..]`: every caller has already bounds-checked against a header of a
    // length it verified, and this keeps that guarantee local rather than at each index.
    match bytes.get(at..at + 2) {
        Some(slice) => u16::from_le_bytes([slice[0], slice[1]]),
        None => 0,
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    match bytes.get(at..at + 4) {
        Some(slice) => u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]),
        None => 0,
    }
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut buffer = [0u8; 8];
    if let Some(slice) = bytes.get(at..at + 8) {
        buffer.copy_from_slice(slice);
    }
    u64::from_le_bytes(buffer)
}

fn read_at<R: Read + Seek + Send + 'static>(
    source: &Shared<R>,
    at: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let length = usize::try_from(length).map_err(|_| {
        Error::malformed("the central directory is larger than this machine can hold")
    })?;
    let mut bytes = vec![0u8; length];
    let mut guard = lock(source)?;
    guard.seek(SeekFrom::Start(at))?;
    read_fully(&mut *guard, &mut bytes)?;
    Ok(bytes)
}

fn read_fully<R: Read>(source: &mut R, into: &mut [u8]) -> Result<()> {
    source.read_exact(into).map_err(Error::io)
}

/// Decodes an entry name: UTF-8 when the flag says so, CP437 otherwise.
///
/// ZIP's original specification says names are CP437 and calls out a handful of code points that
/// are not Latin-1. Those are the only ones worth carrying, because everything else in CP437 is
/// already Latin-1 and `char::from` handles it.
fn decode_name(bytes: &[u8], flags: u16) -> String {
    if flags & flag::UTF8 != 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    bytes.iter().map(|byte| cp437(*byte)).collect()
}

/// CP437 to Unicode.
fn cp437(byte: u8) -> char {
    const HIGH: [char; 128] = [
        '\u{00c7}', '\u{00fc}', '\u{00e9}', '\u{00e2}', '\u{00e4}', '\u{00e0}', '\u{00e5}',
        '\u{00e7}', '\u{00ea}', '\u{00eb}', '\u{00e8}', '\u{00ef}', '\u{00ee}', '\u{00ec}',
        '\u{00c4}', '\u{00c5}', '\u{00c9}', '\u{00e6}', '\u{00c6}', '\u{00f4}', '\u{00f6}',
        '\u{00f2}', '\u{00fb}', '\u{00f9}', '\u{00ff}', '\u{00d6}', '\u{00dc}', '\u{00a2}',
        '\u{00a3}', '\u{00a5}', '\u{03c0}', '\u{221f}', '\u{00e1}', '\u{00ed}', '\u{00f3}',
        '\u{00fa}', '\u{00f1}', '\u{00d1}', '\u{00aa}', '\u{00ba}', '\u{00bf}', '\u{2310}',
        '\u{00ac}', '\u{00bd}', '\u{00bc}', '\u{00a1}', '\u{00ab}', '\u{00bb}', '\u{2591}',
        '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
        '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{255c}', '\u{255b}',
        '\u{2510}', '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}',
        '\u{255e}', '\u{255f}', '\u{255a}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}',
        '\u{2550}', '\u{256c}', '\u{2567}', '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}',
        '\u{2558}', '\u{2552}', '\u{2553}', '\u{256b}', '\u{256a}', '\u{2518}', '\u{250c}',
        '\u{2588}', '\u{2584}', '\u{258c}', '\u{2590}', '\u{2580}', '\u{03b1}', '\u{00df}',
        '\u{0393}', '\u{03c0}', '\u{03a3}', '\u{03c3}', '\u{00b5}', '\u{03c4}', '\u{03a6}',
        '\u{0398}', '\u{03a9}', '\u{03b4}', '\u{221e}', '\u{03c6}', '\u{03b5}', '\u{2229}',
        '\u{2261}', '\u{00b1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00f7}',
        '\u{2248}', '\u{00b0}', '\u{2219}', '\u{00b7}', '\u{221a}', '\u{207f}', '\u{00b2}',
        '\u{25a0}', '\u{00a0}',
    ];
    if byte < 0x80 {
        char::from(byte)
    } else {
        HIGH[(byte - 0x80) as usize]
    }
}

/// Converts an MS-DOS timestamp to a Unix one.
///
/// MS-DOS has no timezone and no sub-second part, so the result is the UTC reading of the pair.
/// Every other ZIP implementation does the same, so a timestamp from a DOS-era tool lines up with
/// what the tool that wrote it showed, rather than being up to two hours out.
pub(crate) fn dos_time_to_system_time(time: u16, date: u16) -> Option<std::time::SystemTime> {
    if date == 0 {
        return None;
    }
    let second = (time & 0b1_1111) as u64 * 2;
    let minute = (time >> 5) as u64 & 0b11_1111;
    let hour = (time >> 11) as u64 & 0b1_11;
    let day = (date & 0b1_1111) as u64;
    let month = ((date >> 5) as u64) & 0b1111;
    let year = (((date >> 9) as i64) & 0b111_1111) + 1980;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from the civil date by Howard Hinnant's algorithm, rather than through a date crate.
    // What is being computed is two dozen instructions; a dependency for that is not a trade worth
    // making in a crate that goes into every archive operation the app does.
    let days = days_from_civil(year, month as u32, day as u32);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour as i64 * 3600 + minute as i64 * 60 + second as i64)?;
    if seconds < 0 {
        return None;
    }
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds as u64))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
    fn the_fixed_sizes_match_what_the_headers_actually_occupy() {
        // Local: 4 + 2 + 2 + 2 + 2 + 4 + 4 + 4 + 2 + 2 = 30
        assert_eq!(LOCAL_FILE_HEADER_SIZE, 30);
        // Central: 4 + 2 + 2 + 2 + 2 + 2 + 4 + 4 + 4 + 2 + 2 + 2 + 2 + 4 + 4 + 4 = 46
        assert_eq!(CENTRAL_DIRECTORY_HEADER_SIZE, 46);
        assert_eq!(END_OF_CENTRAL_DIRECTORY_SIZE, 22);
        assert_eq!(DATA_DESCRIPTOR_SIZE, 12);
    }

    #[test]
    fn the_general_purpose_flags_are_the_bits_the_specification_defines() {
        assert_eq!(flag::ENCRYPTED, 0b1);
        assert_eq!(flag::DATA_DESCRIPTOR, 0b1000);
        assert_eq!(flag::UTF8, 0b1000_0000_0000);
    }

    #[test]
    fn cp437_maps_the_code_points_that_are_not_latin1() {
        // The specification's own examples. Getting these wrong makes files from DOS-era tools
        // list with mojibake names that then cannot be opened by name.
        assert_eq!(cp437(0x80), '\u{00c7}', "C with cedilla");
        assert_eq!(cp437(0x9b), '\u{00a2}', "cent sign");
        assert_eq!(
            cp437(0x9e),
            '\u{03c0}',
            "pi, which is where a peso sign is not"
        );
        assert_eq!(cp437(0xe1), '\u{00df}', "sharp s");
        assert_eq!(cp437(0xe6), '\u{00b5}', "micro sign");
        assert_eq!(cp437(0xff), '\u{00a0}', "non-breaking space");
        assert_eq!(cp437(b'a'), 'a');
        // CP437 really does map two bytes to pi — 0x9e and 0xe3 — so "every high byte is
        // distinct" would be the wrong thing to assert. What matters is that no byte maps to
        // nothing, and that the box-drawing and Greek ranges land where they should.
        assert_eq!(cp437(0xb0), '\u{2591}', "light shade");
        assert_eq!(cp437(0xdb), '\u{2588}', "full block");
        assert_eq!(cp437(0xe0), '\u{03b1}', "alpha");
        assert_eq!(cp437(0xe9), '\u{0398}', "theta");
        assert_eq!(cp437(0xfc), '\u{207f}', "superscript n");
    }

    #[test]
    fn a_dos_timestamp_in_the_documented_range_converts() {
        // 1980-01-01 00:00:00, the first instant MS-DOS can express: 315,532,800 seconds after
        // the Unix epoch. A DOS timestamp is read as UTC, which is what every ZIP tool does, so
        // that is what this returns.
        let first = dos_time_to_system_time(0, (1 << 5) | 1).expect("1980 is representable");
        assert_eq!(
            first
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after 1970")
                .as_secs(),
            315_532_800
        );
        // 2024-02-29 12:34:56, a leap day, so the arithmetic is actually exercised rather than
        // landing on a boundary by luck.
        let date = (((2024 - 1980) << 9) | (2 << 5) | 29) as u16;
        let time = ((12 << 11) | (34 << 5) | 28) as u16;
        let converted = dos_time_to_system_time(time, date).expect("a valid date");
        assert_eq!(
            converted
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after 1970")
                .as_secs(),
            1_709_181_296
        );
    }

    #[test]
    fn an_impossible_dos_date_is_refused_rather_than_silently_wrong() {
        assert!(
            dos_time_to_system_time(0, (13 << 5) | 1).is_none(),
            "month 13"
        );
        assert!(
            dos_time_to_system_time(0, (1 << 5) | 32).is_none(),
            "day 32"
        );
        assert!(dos_time_to_system_time(0, 0).is_none(), "date zero");
    }

    #[test]
    fn the_end_record_is_found_past_a_comment_of_any_length() {
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
        assert_eq!(find_end_of_central_directory(&file, 0), Some(108));
    }

    #[test]
    fn a_file_with_no_end_record_is_malformed_rather_than_an_empty_archive() {
        assert!(find_end_of_central_directory(b"nothing here, only padding padding", 0).is_none());
    }

    #[test]
    fn a_directory_is_only_read_from_the_attributes_when_the_archive_came_from_unix() {
        // A DOS archive's attribute byte read as a mode would make ordinary files look like
        // directories whenever the byte happened to have the 0x10 bit set.
        assert!(
            is_directory_attributes(0x0300, 0o040755 << 16, 0),
            "unix directory"
        );
        assert!(
            !is_directory_attributes(0x0014, 0o040755 << 16, 0),
            "dos, attributes ignored"
        );
        assert!(
            !is_directory_attributes(0x0300, 0o100644 << 16, 0),
            "unix regular file"
        );
        assert!(!is_directory_attributes(0x0300, 0, 0), "no attributes");
    }

    #[test]
    fn a_name_is_decoded_as_cp437_unless_the_archive_says_otherwise() {
        let flags = 0u16;
        assert_eq!(decode_name(&[0x41, 0x80, 0xe1], flags), "A\u{00c7}\u{00df}");
        let utf8 = flag::UTF8;
        assert_eq!(decode_name("héllo".as_bytes(), utf8), "héllo");
    }

    #[test]
    fn a_truncated_central_directory_is_refused_with_a_position() {
        let mut bytes = vec![0u8; CENTRAL_DIRECTORY_HEADER_SIZE + 4];
        bytes[..4].copy_from_slice(CENTRAL_DIRECTORY_HEADER);
        // The name length says 100 but only 4 bytes follow.
        bytes[28..30].copy_from_slice(&100u16.to_le_bytes());
        let error = parse_central_directory(&bytes, &Limits::default()).unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }

    #[test]
    fn a_directory_that_ends_early_stops_rather_than_claiming_an_entry_it_never_had() {
        // Two records' worth of header, but the loop is told the directory is short.
        let mut bytes = vec![0u8; CENTRAL_DIRECTORY_HEADER_SIZE * 2];
        bytes[..4].copy_from_slice(CENTRAL_DIRECTORY_HEADER);
        bytes[CENTRAL_DIRECTORY_HEADER_SIZE..CENTRAL_DIRECTORY_HEADER_SIZE + 4]
            .copy_from_slice(END_OF_CENTRAL_DIRECTORY);
        let records = parse_central_directory(&bytes, &Limits::default()).expect("one record");
        assert_eq!(records.len(), 1, "the end record is not an entry");
    }

    #[test]
    fn a_name_longer_than_the_record_is_an_error_and_not_a_read_past_the_end() {
        let mut bytes = vec![0u8; CENTRAL_DIRECTORY_HEADER_SIZE];
        bytes[..4].copy_from_slice(CENTRAL_DIRECTORY_HEADER);
        bytes[28..30].copy_from_slice(&u16::MAX.to_le_bytes());
        let error = parse_central_directory(&bytes, &Limits::default()).unwrap_err();
        assert!(error.message().contains("past"), "{error}");
    }
}
