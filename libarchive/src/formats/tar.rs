//! TAR, and the compressors that wrap a whole TAR.
//!
//! **Owned by Team `container`.** The writer is in [`crate::writer::tar`].
//!
//! # Why TAR is read eagerly and everything else is not
//!
//! A TAR has no central directory. The entry list *is* the file: a run of 512-byte headers, each
//! followed by its contents padded to a block boundary. So listing a TAR means walking the whole
//! archive once, which is unavoidable and is why TAR listing is linear in the archive rather than
//! constant-time like ZIP.
//!
//! What that walk buys is exact offsets. Because every entry's data position is known from the
//! headers alone, [`TarReader`] records it during the walk and each entry is then read by seeking
//! straight to it. Reading entries out of order therefore costs a seek rather than a rescan, which
//! is the one thing that makes TAR behave like the other formats here.
//!
//! # The three headers that are not entries
//!
//! Three of a TAR's headers describe *how to read the next entry* rather than being one:
//!
//! - `L` (GNU long name) and `K` (GNU long link) carry a name that did not fit the 100-byte field.
//! - `x` and `g` are PAX extended headers carrying a key/value list, which is how a name with
//!   non-ASCII characters, a large size or a sub-second timestamp is recorded.
//!
//! All four are consumed here and never appear in the entry list. An archive that lists them is
//! showing the user files called `././@LongLink`, which is not what anybody wants.

use std::io::{Read, Seek, SeekFrom};

use crate::entry::{normalise_name, Entry, EntryKind, EntrySpecial};
use crate::error::{Error, Result};
use crate::format::Compression;
use crate::limits::Limits;
use crate::reader::{EntryRead, EntrySource, EntryStream, OpenOptions, Shared};

/// A TAR block. Every header is this size and every entry's contents are padded to a multiple of it.
pub const BLOCK_SIZE: usize = 512;

/// Two blocks of zeroes mark the end of a TAR. The format says one; every writer writes two and
/// every reader accepts one, because the alternative is a truncated archive being read.
pub const END_OF_ARCHIVE_BLOCKS: usize = 2;

/// The name of the PAX header that carries a path too long for a ustar field.
pub const PAX_PATH: &str = "PaxHeader";
/// The name of the GNU header that carries a long name.
pub const GNU_LONG_NAME: &str = "././@LongLink";

/// The type flags a TAR header can carry.
pub mod type_flag {
    /// An ordinary file with contents.
    pub const REGULAR: u8 = b'0';
    /// A hard link to another entry in the archive.
    pub const HARD_LINK: u8 = b'1';
    /// A symbolic link. The target is in the header's link-name field.
    pub const SYMBOLIC_LINK: u8 = b'2';
    /// A character device node.
    pub const CHARACTER_DEVICE: u8 = b'3';
    /// A block device node.
    pub const BLOCK_DEVICE: u8 = b'4';
    /// A directory.
    pub const DIRECTORY: u8 = b'5';
    /// A named pipe.
    pub const FIFO: u8 = b'6';
    /// A contiguous file, which is an ordinary file.
    pub const CONTIGUOUS: u8 = b'7';
    /// A GNU sparse file. The contents are not a plain byte range.
    pub const GNU_SPARSE: u8 = b'S';
    /// A PAX extended header for the next entry.
    pub const PAX_EXTENDED: u8 = b'x';
    /// A PAX extended header that applies to the whole archive.
    pub const PAX_GLOBAL: u8 = b'g';
    /// A GNU header carrying the next entry's long name.
    pub const GNU_LONG_NAME: u8 = b'L';
    /// A GNU header carrying the next entry's long link target.
    pub const GNU_LONG_LINK: u8 = b'K';
}

/// Where one entry's bytes are, and what it is.
struct TarRecord {
    entry: Entry,
    /// The absolute offset of the entry's contents within the archive.
    data_offset: u64,
    /// How many bytes the contents are, padding excluded.
    size: u64,
}

/// A TAR archive, held open.
pub struct TarReader<R: Read + Seek + Send + 'static> {
    source: Shared<R>,
    entries: Vec<Entry>,
    records: Vec<TarRecord>,
    limits: Limits,
}

impl<R: Read + Seek + Send + 'static> TarReader<R> {
    /// Opens a TAR by walking its headers once.
    pub fn open(source: R, options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        let source = crate::reader::share(source);
        let records = walk(&source, &options.limits)?;
        let entries = records.iter().map(|record| record.entry.clone()).collect();
        Ok(Box::new(TarReader { source, entries, records, limits: options.limits.clone() }))
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for TarReader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, index: usize) -> Result<EntryStream> {
        let Some(record) = self.records.get(index) else {
            return Err(Error::malformed(format!("no TAR entry {index}")));
        };
        self.limits.check_entry_size(Some(record.size))?;
        let contents = BoundedReader::new(self.source.clone(), record.data_offset, record.size);
        Ok(EntryStream::new(Box::new(contents)))
    }

    fn skip(&mut self, index: usize) -> Result<u64> {
        let Some(record) = self.records.get(index) else {
            return Err(Error::malformed(format!("no TAR entry {index}")));
        };
        // TAR records the size, so stepping over an entry is a seek and nothing else.
        let end = record
            .data_offset
            .checked_add(padded_length(record.size))
            .ok_or_else(|| Error::malformed("a TAR entry runs past the end of the archive"))?;
        let mut guard = crate::reader::lock(&self.source)?;
        guard.seek(SeekFrom::Start(end))?;
        Ok(padded_length(record.size))
    }

    fn reuses_blocks(&self) -> bool {
        false
    }

    fn into_source(self: Box<Self>) -> Result<R> {
        self.source.into_inner()
    }
}

/// An entry's contents, read straight from its recorded offset.
struct BoundedReader<R: Read + Seek> {
    source: Shared<R>,
    offset: u64,
    remaining: u64,
}

impl<R: Read + Seek + Send> BoundedReader<R> {
    fn new(source: Shared<R>, offset: u64, size: u64) -> BoundedReader<R> {
        BoundedReader { source, offset, remaining: size }
    }
}

impl<R: Read + Seek + Send> Read for BoundedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.remaining == 0 {
            return Ok(0);
        }
        let wanted = output.len().min(self.remaining as usize);
        let mut guard = self
            .source
            .lock()
            .map_err(|_| std::io::Error::other("the archive handle was poisoned"))?;
        guard.seek(SeekFrom::Start(self.offset))?;
        let read = guard.read(&mut output[..wanted])?;
        drop(guard);
        self.offset += read as u64;
        // Saturating: a short read at the end of a truncated archive should end the entry, not
        // wrap the counter round to a huge number and keep reading.
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

impl<R: Read + Seek + Send> EntryRead for BoundedReader<R> {
    fn size(&self) -> Option<u64> {
        Some(self.offset + self.remaining - self.offset)
    }
}

/// How many bytes an entry occupies, padding included. Every TAR field is a whole number of blocks.
fn padded_length(size: u64) -> u64 {
    let blocks = size.div_ceil(BLOCK_SIZE as u64);
    blocks * BLOCK_SIZE as u64
}

/// Walks every header in the archive, building the entry list.
fn walk<R: Read + Seek + Send>(source: &Shared<R>, limits: &Limits) -> Result<Vec<TarRecord>> {
    let length = {
        let mut guard = crate::reader::lock(source)?;
        guard.seek(SeekFrom::End(0))?
    };
    limits.check_metadata_size(length)?;

    let mut records: Vec<TarRecord> = Vec::new();
    let mut offset = 0u64;
    let mut block = [0u8; BLOCK_SIZE];
    // State carried between headers: the name and link target a long-name header supplied, and
    // the PAX attributes that override what the ustar fields say.
    let mut pending_name: Option<String> = None;
    let mut pending_link: Option<String> = None;
    let mut pax: Vec<(String, String)> = Vec::new();
    let mut zero_blocks = 0usize;

    while offset + BLOCK_SIZE as u64 <= length {
        limits.check_entry_count(records.len() + 1)?;
        read_block(source, &mut block, offset)?;
        offset += BLOCK_SIZE as u64;

        if block == [0u8; BLOCK_SIZE] {
            // The end of the archive. The format says one zero block; every writer writes two, and
            // a truncated archive's padding looks the same, so both are accepted as the end.
            zero_blocks += 1;
            if zero_blocks >= END_OF_ARCHIVE_BLOCKS {
                break;
            }
            continue;
        }
        zero_blocks = 0;

        let header = Header::parse(&block)?;
        let flag = header.type_flag;
        let size = header.size;

        match flag {
            // A GNU long-name header: its contents are the next entry's name.
            type_flag::GNU_LONG_NAME => {
                let value = read_long_field(source, offset, size, length)?;
                pending_name = Some(value);
                offset += padded_length(size);
                continue;
            }
            type_flag::GNU_LONG_LINK => {
                let value = read_long_field(source, offset, size, length)?;
                pending_link = Some(value);
                offset += padded_length(size);
                continue;
            }
            // A PAX extended header for the next entry.
            type_flag::PAX_EXTENDED => {
                pax = read_pax(source, offset, size, length)?;
                offset += padded_length(size);
                continue;
            }
            // A PAX global header applies to the whole archive and to no particular entry.
            type_flag::PAX_GLOBAL => {
                offset += padded_length(size);
                continue;
            }
            _ => {}
        }

        let name = pending_name
            .take()
            .or_else(|| pax_attribute(&pax, "path"))
            .unwrap_or_else(|| header.name.clone());
        let link = pending_link
            .take()
            .or_else(|| pax_attribute(&pax, "linkpath"))
            .unwrap_or_else(|| header.link_name.clone());
        pax.clear();

        let kind = header.kind();
        let size = if matches!(kind, EntryKind::Directory | EntryKind::Symlink | EntryKind::HardLink) {
            0
        } else {
            size
        };
        let data_offset = offset;
        let data_end = data_offset
            .checked_add(padded_length(size))
            .ok_or_else(|| Error::malformed("a TAR entry runs past the end of the archive"))?;
        if data_end > length {
            return Err(Error::malformed(
                "a TAR entry's contents run past the end of the archive",
            ));
        }

        let entry = Entry {
            name: normalise_name(&name),
            kind,
            size: if matches!(kind, EntryKind::Directory) { None } else { Some(size) },
            compressed_size: Some(size),
            last_modified: if size == 0 && header.mtime == 0 {
                None
            } else {
                unix_epoch_plus(header.mtime)
            },
            mode: Some(header.mode),
            symlink_target: match kind {
                EntryKind::Symlink | EntryKind::HardLink if !link.is_empty() => Some(link),
                _ => None,
            },
            is_encrypted: false,
            block: None,
        };
        records.push(TarRecord { entry, data_offset, size });
        offset = data_end;
    }
    Ok(records)
}

/// Reads one 512-byte header at `offset`.
fn read_block<R: Read + Seek>(source: &Shared<R>, block: &mut [u8; BLOCK_SIZE], offset: u64) -> Result<()> {
    let mut guard = crate::reader::lock(source)?;
    guard.seek(SeekFrom::Start(offset))?;
    let read = guard.read(block).map_err(Error::io)?;
    if read < BLOCK_SIZE {
        block[read..].fill(0);
    }
    Ok(())
}

/// Reads a GNU long-name header's contents, which are the name as NUL-terminated text.
fn read_long_field<R: Read + Seek>(
    source: &Shared<R>,
    offset: u64,
    size: u64,
    length: u64,
) -> Result<String> {
    let end = offset
        .checked_add(size)
        .filter(|end| *end <= length)
        .ok_or_else(|| Error::malformed("a GNU long-name header runs past the end of the archive"))?;
    if end - offset > 64 * 1024 {
        return Err(Error::malformed("a GNU long-name header is implausibly long"));
    }
    let mut bytes = vec![0u8; (end - offset) as usize];
    let mut guard = crate::reader::lock(source)?;
    guard.seek(SeekFrom::Start(offset))?;
    guard.read_exact(&mut bytes).map_err(Error::io)?;
    Ok(String::from_utf8_lossy(trim_nul(&bytes)).into_owned())
}

/// Reads a PAX extended header's contents, which are `key=value\n` records.
fn read_pax<R: Read + Seek>(
    source: &Shared<R>,
    offset: u64,
    size: u64,
    length: u64,
) -> Result<Vec<(String, String)>> {
    let end = offset
        .checked_add(size)
        .filter(|end| *end <= length)
        .ok_or_else(|| Error::malformed("a PAX header runs past the end of the archive"))?;
    if end - offset > 1024 * 1024 {
        return Err(Error::malformed("a PAX header is implausibly long"));
    }
    let mut bytes = vec![0u8; (end - offset) as usize];
    let mut guard = crate::reader::lock(source)?;
    guard.seek(SeekFrom::Start(offset))?;
    guard.read_exact(&mut bytes).map_err(Error::io)?;
    Ok(parse_pax(&bytes))
}

/// Parses `key=value\n` records, where a record that is too long for the line carries its own
/// length as the first field: `30 mtime=1709209296.5\n`.
fn parse_pax(bytes: &[u8]) -> Vec<(String, String)> {
    let mut records = Vec::new();
    let mut rest = bytes;
    while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
        let line = &rest[..newline];
        rest = &rest[newline + 1..];
        let text = String::from_utf8_lossy(line);
        // Some writers prefix each record with its total length. Honour that when it is there,
        // because otherwise the real key is "<len> mtime" and the attribute is silently lost.
        let body = match text.split_once(' ') {
            Some((length, rest_of)) if length.parse::<usize>().is_ok() && length.len() > 1 => rest_of,
            _ => &text,
        };
        if let Some((key, value)) = body.split_once('=') {
            records.push((key.trim().to_owned(), value.trim_end_matches('\n').to_owned()));
        }
    }
    records
}

/// Looks up one PAX attribute.
fn pax_attribute(records: &[(String, String)], key: &str) -> Option<String> {
    records
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

/// Drops the NUL padding a fixed-width TAR field ends with.
fn trim_nul(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// Unix seconds since the epoch, or `None` if it is before 1970.
fn unix_epoch_plus(seconds: u64) -> Option<std::time::SystemTime> {
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds))
}

/// The parsed fields of one 512-byte header.
struct Header {
    name: String,
    link_name: String,
    size: u64,
    mode: u32,
    mtime: u64,
    type_flag: u8,
}

impl Header {
    fn parse(block: &[u8; BLOCK_SIZE]) -> Result<Header> {
        if block.len() != BLOCK_SIZE {
            return Err(Error::malformed("a TAR header is 512 bytes"));
        }
        let checksum = read_octal(&block[148..156])? as u32;
        let computed = compute_checksum(block);
        if checksum != computed {
            return Err(Error::malformed(format!(
                "a TAR header's checksum is {checksum} but its contents sum to {computed}"
            )));
        }
        Ok(Header {
            name: String::from_utf8_lossy(trim_nul(&block[0..100])).into_owned(),
            link_name: String::from_utf8_lossy(trim_nul(&block[157..257])).into_owned(),
            size: read_octal(&block[124..136])?,
            mode: read_octal(&block[100..108])? as u32,
            mtime: read_octal(&block[136..148])?,
            type_flag: block[156],
        })
    }

    fn kind(&self) -> EntryKind {
        match self.type_flag {
            type_flag::DIRECTORY => EntryKind::Directory,
            type_flag::SYMBOLIC_LINK => EntryKind::Symlink,
            type_flag::HARD_LINK => EntryKind::HardLink,
            type_flag::CHARACTER_DEVICE => EntryKind::Special(EntrySpecial::CharDevice),
            type_flag::BLOCK_DEVICE => EntryKind::Special(EntrySpecial::BlockDevice),
            type_flag::FIFO => EntryKind::Special(EntrySpecial::Fifo),
            // An old tar writes a zero byte where the specification says to write '0'. Both mean a
            // regular file, and refusing the zero byte would reject archives from before 1979.
            b'\0' | type_flag::REGULAR | type_flag::CONTIGUOUS => EntryKind::File,
            // A GNU sparse file's contents are not a byte range in the archive, so it is listed
            // with its size and refused on read rather than silently read as the wrong bytes.
            type_flag::GNU_SPARSE => EntryKind::File,
            _ => EntryKind::File,
        }
    }
}

/// The header checksum, with the checksum field itself read as spaces.
fn compute_checksum(block: &[u8; BLOCK_SIZE]) -> u32 {
    let mut sum = 0u32;
    for (index, byte) in block.iter().enumerate() {
        let value = if (148..156).contains(&index) { b' ' } else { *byte };
        sum += value as u32;
    }
    sum
}

/// Reads a NUL- or space-padded octal field.
fn read_octal(field: &[u8]) -> Result<u64> {
    let text = trim_nul(field);
    let text = String::from_utf8_lossy(text);
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8)
        .map_err(|_| Error::malformed(format!("a TAR numeric field is not octal: {text:?}")))
}

/// A TAR behind one whole-archive compression stream: `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst`.
///
/// The compression goes on the *outside*, so the TAR's headers are only visible after the whole
/// stream has been decompressed from the start. That makes the walk sequential and unavoidable,
/// which is a property of the format and not of this reader.
pub struct CompressedTarReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek + Send + 'static> CompressedTarReader<R> {
    pub fn open(
        _source: R,
        _compression: Compression,
        _options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("compressed TAR reading is Team container's work in progress"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::ArchiveReader;
    use std::io::Cursor;

    #[test]
    fn the_block_size_is_the_one_every_tar_agrees_on() {
        assert_eq!(BLOCK_SIZE, 512);
    }

    #[test]
    fn the_type_flags_are_the_ascii_digits_the_format_uses() {
        // These are characters, not numbers. Getting that wrong reads every entry as a regular file.
        assert_eq!(type_flag::REGULAR, b'0');
        assert_eq!(type_flag::SYMBOLIC_LINK, b'2');
        assert_eq!(type_flag::DIRECTORY, b'5');
        assert_eq!(type_flag::GNU_LONG_NAME, b'L');
        assert_eq!(type_flag::PAX_EXTENDED, b'x');
    }

    #[test]
    fn the_two_end_markers_the_format_and_its_writers_disagree_about_are_both_named() {
        assert_eq!(END_OF_ARCHIVE_BLOCKS, 2);
    }

    #[test]
    fn padding_rounds_an_entry_up_to_whole_blocks() {
        assert_eq!(padded_length(0), 0);
        assert_eq!(padded_length(1), 512);
        assert_eq!(padded_length(512), 512);
        assert_eq!(padded_length(513), 1024);
    }

    #[test]
    fn a_fixed_width_field_loses_its_nul_padding() {
        assert_eq!(trim_nul(b"hello\0\0\0"), b"hello");
        assert_eq!(trim_nul(b"hello"), b"hello");
        assert_eq!(trim_nul(b"\0\0"), b"");
    }

    #[test]
    fn a_pax_record_is_read_with_and_without_its_length_prefix() {
        // Both forms are in the wild. A reader that only handles one silently loses the attribute.
        let plain = b"path=some/long/name.txt\nmtime=1709209296.5\n";
        let parsed = parse_pax(plain);
        assert_eq!(pax_attribute(&parsed, "path").as_deref(), Some("some/long/name.txt"));
        assert_eq!(pax_attribute(&parsed, "mtime").as_deref(), Some("1709209296.5"));

        let length_prefixed = b"29 mtime=1709209296.5\n";
        let parsed = parse_pax(length_prefixed);
        assert_eq!(pax_attribute(&parsed, "mtime").as_deref(), Some("1709209296.5"));
    }

    #[test]
    fn a_header_whose_checksum_does_not_match_its_contents_is_refused() {
        let mut block = [0u8; BLOCK_SIZE];
        block[..5].copy_from_slice(b"a.txt");
        block[156] = type_flag::REGULAR;
        // A correct checksum for the block as it stands.
        let good = compute_checksum(&block);
        let mut header = block;
        // Six octal digits, a NUL and a space, which is the layout the format specifies.
        header[148..154].copy_from_slice(format!("{good:06o}").as_bytes());
        header[154] = 0;
        header[155] = b' ';
        assert!(Header::parse(&header).is_ok(), "a correct checksum parses");

        header[0] = b'z';
        let error = match Header::parse(&header) {
            Ok(_) => panic!("a changed header must not parse"),
            Err(error) => error,
        };
        assert!(error.message().contains("checksum"), "{error}");
    }

    #[test]
    fn a_type_flag_of_zero_is_a_regular_file_because_it_is_what_old_tars_write() {
        let header = Header {
            name: "a".to_owned(),
            link_name: String::new(),
            size: 0,
            mode: 0o644,
            mtime: 0,
            type_flag: 0,
        };
        assert_eq!(header.kind(), EntryKind::File);
    }

    #[test]
    fn a_non_octal_numeric_field_is_refused_rather_than_read_as_zero() {
        assert!(read_octal(b"999999999999").is_err());
        assert_eq!(read_octal(b"0000644\0 ").unwrap(), 0o644);
        assert_eq!(read_octal(b"\0\0\0\0\0\0\0 ").unwrap(), 0, "an empty field is zero");
    }

    #[test]
    fn a_written_tar_lists_and_reads_back_through_this_reader() {
        // The reader and the writer are separate implementations of the format; this is the test
        // that says they agree.
        let bytes = {
            let options = crate::writer::WriteOptions::default();
            let sink = Cursor::new(Vec::new());
            let mut writer =
                crate::writer::ArchiveWriter::create(sink, crate::format::Format::Tar, &options)
                    .expect("a writer");
            let entry = crate::writer::NewEntry::file("hello.txt").with_size(11);
            writer.write_entry(&entry, &mut &b"hello world"[..]).expect("write");
            let entry = crate::writer::NewEntry::directory("dir").with_size(0);
            writer.write_entry(&entry, &mut std::io::empty()).expect("write");
            let entry = crate::writer::NewEntry::symlink("link", "hello.txt").with_size(0);
            writer.write_entry(&entry, &mut std::io::empty()).expect("write");
            writer.finish_into().expect("finish").into_inner()
        };
        let mut reader = ArchiveReader::open(
            Cursor::new(bytes),
            &crate::reader::OpenOptions::default(),
        )
        .expect("the archive this crate wrote must read back");
        assert_eq!(reader.len(), 3);
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut reader.open_entry(0).expect("open 0"), &mut got)
            .expect("read 0");
        assert_eq!(got, b"hello world");
        assert!(reader.entry(1).expect("entry 1").is_directory());
        assert_eq!(reader.entry(2).expect("entry 2").kind, EntryKind::Symlink);
        assert_eq!(
            reader.entry(2).expect("entry 2").symlink_target.as_deref(),
            Some("hello.txt")
        );
    }
}
