//! Writing TAR archives.
//!
//! **Owned by Team `container`.**
//!
//! # Why TAR is the easy format and still worth care
//!
//! A TAR entry is a 512-byte header followed by the contents padded to a multiple of 512. There
//! is no compression and no central directory, so an entry's size is in the header before the
//! data — which is exactly the case the streaming ZIP writer cannot use, because the header
//! carries the CRC too and TAR has no CRC.
//!
//! That means a TAR entry can be written with its size known in advance and no data descriptor,
//! and the writer streams straight through. It also means TAR cannot write an entry whose size is
//! not known, which is the honest limitation: [`NewEntry::size`] must be `Some`, and the writer
//! says so rather than buffering to find out.
//!
//! # The block size is not optional
//!
//! TAR is a stream of 512-byte blocks. An entry's contents that do not fill a block are padded
//! with zeroes, and the archive ends with two zero blocks. Getting either wrong produces a file
//! that some tools read and others do not, so both are written explicitly rather than left to
//! whatever the sink's length happens to be.

use std::io::{self, Seek, Write};

use crate::entry::{normalise_name, EntryKind};
use crate::error::{Error, Result};
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

/// The size of every TAR block, and of every header.
pub const BLOCK_SIZE: usize = 512;

/// A TAR header, already resolved.
struct Header {
    name: [u8; 100],
    mode: u32,
    size: u64,
    mtime: u64,
    checksum: u32,
    type_flag: u8,
    link_name: [u8; 100],
    magic: &'static [u8],
}

impl Header {
    /// Serialises the header into exactly one block.
    ///
    /// One block, written in one call, because every write crosses into Java and a TAR header
    /// written a field at a time is eleven writes for 512 bytes.
    fn write_to(&self, block: &mut [u8; BLOCK_SIZE]) {
        block.fill(0);
        block[..self.name.len()].copy_from_slice(&self.name);
        write_octal(&mut block[100..108], self.mode as u64, 7);
        write_octal(&mut block[108..116], self.uid(), 7);
        write_octal(&mut block[116..124], self.gid(), 7);
        write_octal(&mut block[124..136], self.size, 11);
        write_octal(&mut block[136..148], self.mtime, 11);
        // The checksum is six octal digits, a NUL and a space. Writing it as octal rather than as
        // the decimal text the format's own field width implies is what makes GNU tar accept it.
        write_octal(&mut block[148..154], self.checksum as u64, 6);
        block[154] = 0;
        block[155] = b' ';
        block[156] = self.type_flag;
        block[157..157 + self.link_name.len()].copy_from_slice(&self.link_name);
        block[257..263].copy_from_slice(self.magic);
        block[263..265].copy_from_slice(b"00"); // version
        block[265..297].copy_from_slice(&[b' '; 32]); // uname
        block[297..329].copy_from_slice(&[b' '; 32]); // gname
        // devmajor/devminor left as "unset", which is what an empty octal field means.
    }

    fn uid(&self) -> u64 {
        0
    }

    fn gid(&self) -> u64 {
        0
    }
}

/// Writes a TAR whose whole stream is then compressed: a `.tar.gz` or `.tar.xz`.
///
/// Two writers rather than one, because the compressed form is a compressor around an
/// uncompressed TAR and the block alignment the TAR needs is not visible to the compressor. The
/// inner writer is finished first, so the TAR's end-of-archive blocks are compressed too, which
/// is what every tar-compatible tool expects.
pub struct CompressedTarWriter<W: Write + Seek> {
    sink: Option<W>,
    compression: crate::format::Compression,
}

impl<W: Write + Seek> CompressedTarWriter<W> {
    pub fn create(sink: W, options: &WriteOptions) -> Result<Box<dyn EntryWriter<W>>> {
        let _ = &sink;
        Err(Error::unimplemented(format!(
            "writing a TAR compressed with {} is Team container's work in progress",
            options.compression
        )))
    }
}

impl<W: Write + Seek> EntryWriter<W> for CompressedTarWriter<W> {
    fn begin<'a>(&'a mut self, _entry: &NewEntry, _options: &WriteOptions) -> Result<Box<dyn EntryWrite + 'a>> {
        Err(Error::unimplemented("pending"))
    }
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    fn into_sink(self: Box<Self>) -> W {
        self.sink.expect("a writer that never began still owns its sink")
    }
}

/// Writes an uncompressed TAR archive.
pub struct TarWriter<W: Write + Seek> {
    sink: W,
    position: u64,
    /// Scratch for one header block, so no entry allocates.
    block: [u8; BLOCK_SIZE],
    /// Scratch for padding, reused across every entry.
    padding: [u8; BLOCK_SIZE],
}

impl<W: Write + Seek> TarWriter<W> {
    /// Creates a TAR writer over `sink`.
    pub fn create<W2: Write + Seek + 'static>(sink: W2, _options: &WriteOptions) -> Result<Box<dyn EntryWriter<W2>>> {
        Ok(Box::new(TarWriter {
            sink,
            position: 0,
            block: [0; BLOCK_SIZE],
            padding: [0; BLOCK_SIZE],
        }))
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        let TarWriter { sink, position, .. } = self;
        emit(sink, position, bytes)
    }

    /// Pads out to the next block boundary, which is what every TAR entry must end with.
    fn pad_to_block(&mut self) -> Result<()> {
        let remainder = (self.position % BLOCK_SIZE as u64) as usize;
        if remainder != 0 {
            let needed = BLOCK_SIZE - remainder;
            let TarWriter { sink, position, padding, .. } = self;
            emit(sink, position, &padding[..needed])?;
        }
        Ok(())
    }
}

/// The entry sink: one entry's contents, streaming.
struct EntrySink<'a, W: Write + Seek> {
    writer: &'a mut TarWriter<W>,
    size: u64,
    written: u64,
    closed: bool,
}

impl<W: Write + Seek> Write for EntrySink<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::Error::other("this entry has already been finished"));
        }
        // TAR headers declare the size, so more than that is a caller bug and writing it anyway
        // would desynchronise the whole archive.
        if self.written + buffer.len() as u64 > self.size {
            return Err(io::Error::other(format!(
                "this entry declared {} bytes but more were supplied",
                self.size
            )));
        }
        self.writer.emit(buffer).map_err(io::Error::other)?;
        self.written += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Seek> EntryWrite for EntrySink<'_, W> {
    fn written(&self) -> u64 {
        self.written
    }

    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if self.written != self.size {
            return Err(Error::malformed(format!(
                "The entry declared {} bytes but {} were supplied",
                self.size, self.written
            )));
        }
        self.writer.pad_to_block()
    }
}

impl<W: Write + Seek> EntryWriter<W> for TarWriter<W> {
    fn begin<'a>(
        &'a mut self,
        entry: &NewEntry,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWrite + 'a>> {
        // TAR has no CRC and no data descriptor, so an entry whose size is not known cannot be
        // written without buffering it whole. Saying so is better than writing a header with a
        // zero size and an archive nobody can read past.
        let size = entry.size.ok_or_else(|| {
            Error::unimplemented(
                "A TAR entry's size must be known before it is written, because the TAR header \
                 carries the size and there is no data descriptor to correct it afterwards",
            )
        })?;

        let is_directory = entry.kind == EntryKind::Directory;
        let is_symlink = entry.kind == EntryKind::Symlink;
        let type_flag = match entry.kind {
            EntryKind::Directory => b'5',
            EntryKind::Symlink => b'2',
            EntryKind::HardLink => b'1',
            _ => b'0',
        };
        let size = if is_directory || is_symlink { 0 } else { size };

        let mut name = normalise_name(&entry.name);
        if is_directory {
            name.push('/');
        }
        let mut name_field = [0u8; 100];
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(100);
        name_field[..name_len].copy_from_slice(&name_bytes[..name_len]);
        if name_bytes.len() > 100 {
            return Err(Error::malformed(format!(
                "The entry name {:?} is {} bytes, over TAR's 100 byte field",
                name,
                name_bytes.len()
            )));
        }

        let mut link_field = [0u8; 100];
        if let Some(target) = entry.symlink_target.as_ref() {
            let target_bytes = target.as_bytes();
            if target_bytes.len() > 100 {
                return Err(Error::malformed(format!(
                    "The link target {:?} is {} bytes, over TAR's 100 byte field",
                    target,
                    target_bytes.len()
                )));
            }
            link_field[..target_bytes.len()].copy_from_slice(target_bytes);
        }

        let mtime = entry
            .last_modified
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_secs());

        let mut header = Header {
            name: name_field,
            mode: entry.mode.unwrap_or(if is_directory { 0o755 } else { 0o644 }),
            size,
            mtime,
            // Filled in below, once every other field is set: the checksum covers the header
            // itself with the checksum field read as spaces.
            checksum: 0,
            type_flag,
            link_name: link_field,
            magic: b"ustar\0",
        };
        let mut block = [0u8; BLOCK_SIZE];
        header.write_to(&mut block);
        header.checksum = checksum(&block);
        header.write_to(&mut block);
        // Copied into the writer's own scratch so the header is one write and nothing is
        // allocated per entry.
        self.block.copy_from_slice(&block);
        let TarWriter { sink, position, block, .. } = self;
        emit(sink, position, block)?;

        Ok(Box::new(EntrySink { writer: self, size, written: 0, closed: false }))
    }

    fn finish(&mut self) -> Result<()> {
        self.pad_to_block()?;
        // Two zero blocks mark the end of a TAR. One is not enough: some readers treat the first
        // zero block as padding and keep going.
        let TarWriter { sink, position, padding, .. } = self;
        emit(sink, position, padding)?;
        emit(sink, position, padding)?;
        Ok(())
    }

    fn into_sink(self: Box<Self>) -> W {
        self.sink
    }
}

/// The header checksum: every byte of the block, with the checksum field read as spaces.
fn checksum(block: &[u8; BLOCK_SIZE]) -> u32 {
    let mut sum = 0u32;
    for (index, byte) in block.iter().enumerate() {
        // The checksum field is eight bytes at offset 148 and is treated as spaces while summing.
        let value = if (148..156).contains(&index) { b' ' } else { *byte };
        sum += value as u32;
    }
    sum
}

/// Writes `bytes` to the sink in one call and advances the running position.
fn emit<W: Write>(sink: &mut W, position: &mut u64, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    sink.write_all(bytes).map_err(Error::io)?;
    *position += bytes.len() as u64;
    Ok(())
}

/// Writes an octal number, zero-padded, with the field's trailing byte left as a NUL or space.
fn write_octal(field: &mut [u8], value: u64, digits: usize) {
    let text = format!("{value:0digits$o}", digits = digits);
    let bytes = text.as_bytes();
    let take = bytes.len().min(field.len().saturating_sub(1));
    field[..take].copy_from_slice(&bytes[bytes.len() - take..]);
    for byte in &mut field[take..] {
        *byte = b'0';
    }
    // TAR wants a NUL or space after the digits; a trailing zero digit would read as part of the
    // number, so it is turned into the terminator.
    if let Some(last) = field.last_mut() {
        *last = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Format;
    use crate::reader::{ArchiveReader, OpenOptions};
    use crate::writer::ArchiveWriter;
    use std::io::Cursor;

    #[test]
    fn an_entry_without_a_size_is_refused_rather_than_written_wrong() {
        // The whole reason `NewEntry::size` is `Option` but TAR insists on it. Writing a zero-size
        // header and streaming anyway produces an archive that reads back as one empty entry.
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Tar, &options).expect("a writer");
        let error = match writer.write_entry(&NewEntry::file("a.txt"), &mut &b"hello"[..]) {
            Ok(()) => panic!("a TAR entry with no declared size must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Unimplemented);
        assert!(error.message().contains("size"), "{error}");
    }

    #[test]
    fn a_tar_round_trips_through_the_reader() {
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Tar, &options).expect("a writer");
        let entry = NewEntry::file("hello.txt").with_size(11);
        writer.write_entry(&entry, &mut &b"hello world"[..]).expect("write an entry");
        let entry = NewEntry::directory("dir").with_size(0);
        writer.write_entry(&entry, &mut std::io::empty()).expect("write a directory");
        let bytes = writer.finish_into().expect("finish").into_inner();
        assert_eq!(bytes.len() % BLOCK_SIZE, 0, "a TAR is a whole number of blocks");
        let mut reader = ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).unwrap();
        assert_eq!(reader.len(), 2);
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut reader.open_entry(0).unwrap(), &mut got).unwrap();
        assert_eq!(got, b"hello world");
        assert!(reader.entry(1).unwrap().is_directory());
    }

    #[test]
    fn the_checksum_is_computed_with_the_field_read_as_spaces() {
        // This is the detail every hand-rolled TAR writer gets wrong, and a wrong checksum means
        // GNU tar refuses the file while other tools ignore it.
        let mut block = [0u8; BLOCK_SIZE];
        block[0..5].copy_from_slice(b"a.txt");
        block[148..156].fill(b'0');
        let with_nonzero_field = checksum(&block);
        block[148..156].fill(b' ');
        assert_eq!(with_nonzero_field, checksum(&block));
    }

    #[test]
    fn an_octal_field_is_zero_padded_to_the_width_the_format_expects() {
        let mut field = [b' '; 8];
        write_octal(&mut field, 0o644, 7);
        assert_eq!(&field[..7], b"0000644");
        assert_eq!(field[7], 0, "the terminator, not another digit");
    }

    #[test]
    fn a_name_that_does_not_fit_the_field_is_refused_rather_than_truncated() {
        let options = WriteOptions::default();
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::Tar, &options).expect("a writer");
        let long = "x".repeat(200);
        let error = match writer.write_entry(&NewEntry::file(&long).with_size(0), &mut std::io::empty()) {
            Ok(()) => panic!("a name that does not fit must be refused"),
            Err(error) => error,
        };
        assert!(error.message().contains("100 byte"), "{error}");
    }
}
