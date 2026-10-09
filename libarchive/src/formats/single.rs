//! Single-stream compressors: one compressed file, one entry.
//!
//! **Owned by Team 4**, together with the long tail of legacy archive formats.
//!
//! A `.gz`, `.bz2`, `.xz`, `.zst` or `.lz4` file is not an archive. It is one compressed stream,
//! and the whole file is one entry. Two things follow:
//!
//! * The uncompressed size is usually recorded in a footer, but not always. Gzip and xz record it,
//!   bzip2 and zstd often do not, and lz4 only in a frame header that may be absent. So
//!   [`Entry::size`] is `Option`, and a stream that does not know its size still reads.
//! * Reading it is entirely a matter of streaming. There is no second entry and no directory, so
//!   the reader is a decompressor wrapped in [`EntryStream`], with no buffering of the whole thing.
//!
//! The exception is `.tar.gz` and its siblings, which are a TAR inside one of these streams. That is
//! [`crate::formats::tar::CompressedTarReader`], not this module, because a caller that lists it
//! wants the TAR's entries rather than one enormous one.

use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::format::Compression;
use crate::reader::{EntrySource, OpenOptions};
use crate::writer::{EntryWriter, WriteOptions};

/// The name given to the single entry of a raw compressed stream.
///
/// A `.gz` file has no name of its own, and the app needs one to put in a listing.
pub const SINGLE_ENTRY_NAME: &str = "content";

/// The signatures that say which compressor a stream uses.
///
/// The `.br` signature is not here because a brotli stream has no magic of its own: the only way to
/// tell one from an arbitrary file is to try to decode it, which is what
/// [`Compression::Brotli`]'s entry in [`crate::detect::detect_header`] falls through to.
pub fn magic(compression: Compression) -> Option<&'static [u8]> {
    Some(match compression {
        Compression::Gzip => &[0x1F, 0x8B, 0x08],
        Compression::Bzip2 => b"BZh",
        Compression::Zstd => &[0x28, 0xB5, 0x2F, 0xFD],
        Compression::Lz4 => &[0x04, 0x22, 0x4D, 0x18],
        Compression::Z => &[0x1F, 0x9D],
        Compression::Xz => &[0xFD, b'7', b'z', b'X', b'Z', 0x00],
        _ => return None,
    })
}

pub struct SingleStreamReader<R> {
    _source: std::marker::PhantomData<R>,
}

impl<R: Read + Seek> SingleStreamReader<R> {
    pub fn open(
        _source: R,
        _compression: Compression,
        _options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unimplemented("single-stream reading is Team 4's work in progress"))
    }
}

pub struct SingleStreamWriter<W> {
    _sink: std::marker::PhantomData<W>,
}

impl<W: Write + Seek> SingleStreamWriter<W> {
    pub fn create(
        _sink: W,
        _compression: Compression,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWriter<W>>> {
        Err(Error::unimplemented("single-stream writing is Team 4's work in progress"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signatures_are_the_ones_each_format_defines() {
        assert_eq!(magic(Compression::Gzip), Some(&[0x1F, 0x8B, 0x08][..]));
        assert_eq!(magic(Compression::Bzip2), Some(&b"BZh"[..]));
        assert_eq!(magic(Compression::Zstd), Some(&[0x28, 0xB5, 0x2F, 0xFD][..]));
        assert_eq!(magic(Compression::Lz4), Some(&[0x04, 0x22, 0x4D, 0x18][..]));
        assert_eq!(magic(Compression::Xz), Some(&[0xFD, b'7', b'z', b'X', b'Z', 0x00][..]));
        assert_eq!(magic(Compression::Z), Some(&[0x1F, 0x9D][..]));
    }

    #[test]
    fn brotli_has_no_signature_and_says_so_rather_than_inventing_one() {
        // A made-up signature here would make detection reject real `.br` files.
        assert_eq!(magic(Compression::Brotli), None);
    }

    #[test]
    fn the_single_entry_name_is_the_one_the_app_already_shows_for_an_xz_file() {
        assert_eq!(SINGLE_ENTRY_NAME, "content");
    }
}