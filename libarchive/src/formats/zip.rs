//! ZIP.
//!
//! **Owned by Team 1.** This file is the scaffolding: the reader and writer the dispatch in
//! [`crate::formats`] calls, plus the structures every ZIP path agrees on. The parsing and the
//! writing are Team 1's work.

use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::reader::{EntrySource, OpenOptions};
use crate::writer::{EntryWriter, WriteOptions};

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

pub struct ZipReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek> ZipReader<R> {
    pub fn open(_source: R, _options: &OpenOptions) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("ZIP reading is Team 1's work in progress"))
    }
}

pub struct ZipWriter<W> {
    _sink: std::marker::PhantomData<W>,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn create(_sink: W, _options: &WriteOptions) -> Result<Box<dyn EntryWriter<W>>> {
        Err(Error::unimplemented("ZIP writing is Team 1's work in progress"))
    }
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
}