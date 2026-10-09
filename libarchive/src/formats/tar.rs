//! TAR, and the compressors that wrap a whole TAR.
//!
//! **Owned by Team 3.**

use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::format::Compression;
use crate::reader::{EntrySource, OpenOptions};
use crate::writer::{EntryWriter, WriteOptions};

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
    pub const REGULAR: u8 = b'0';
    pub const HARD_LINK: u8 = b'1';
    pub const SYMBOLIC_LINK: u8 = b'2';
    pub const CHARACTER_DEVICE: u8 = b'3';
    pub const BLOCK_DEVICE: u8 = b'4';
    pub const DIRECTORY: u8 = b'5';
    pub const FIFO: u8 = b'6';
    pub const CONTIGUOUS: u8 = b'7';
    pub const GNU_SPARSE: u8 = b'S';
    pub const PAX_EXTENDED: u8 = b'x';
    pub const PAX_GLOBAL: u8 = b'g';
    pub const GNU_LONG_NAME: u8 = b'L';
    pub const GNU_LONG_LINK: u8 = b'K';
}

pub struct TarReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek> TarReader<R> {
    pub fn open(_source: R, _options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("TAR reading is Team 3's work in progress"))
    }
}

/// A TAR behind one whole-archive compression stream: `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst`.
pub struct CompressedTarReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek> CompressedTarReader<R> {
    pub fn open(
        _source: R,
        _compression: Compression,
        _options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("compressed TAR reading is Team 3's work in progress"))
    }
}

pub struct TarWriter<W> {
    _sink: std::marker::PhantomData<W>,
}

impl<W: Write + Seek> TarWriter<W> {
    pub fn create(_sink: W, _options: &WriteOptions) -> Result<Box<dyn EntryWriter<W>>> {
        Err(Error::unimplemented("TAR writing is Team 3's work in progress"))
    }
}

pub struct CompressedTarWriter<W> {
    _sink: std::marker::PhantomData<W>,
}

impl<W: Write + Seek> CompressedTarWriter<W> {
    pub fn create(
        _sink: W,
        _compression: Compression,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWriter<W>>> {
        Err(Error::unimplemented("compressed TAR writing is Team 3's work in progress"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}