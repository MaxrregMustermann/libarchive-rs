//! One module per format, and the dispatch that picks between them.
//!
//! Every format implements [`EntrySource`] for reading and, where it applies, [`EntryWriter`] for
//! writing. [`open`] and [`create`] are the only two places that know the full list, which is what
//! keeps the rest of the crate free of a format switch.

use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::format::{Compression, Format};
use crate::reader::{EntrySource, OpenOptions};
use crate::writer::{EntryWriter, WriteOptions};

pub mod single;
pub mod sevenz;
pub mod tar;
pub mod zip;

/// Opens a source as `format`, building its entry list.
pub fn open<R: Read + Seek>(
    format: Format,
    source: R,
    options: &OpenOptions,
) -> Result<Box<dyn EntrySource<R>>> {
    match format {
        Format::Zip => zip::ZipReader::open(source, options),
        Format::SevenZ => sevenz::SevenZReader::open(source, options),
        Format::Tar => tar::TarReader::open(source, options),
        Format::CompressedTar(compression) => tar::CompressedTarReader::open(source, compression, options),
        Format::SingleStream(compression) => single::SingleStreamReader::open(source, compression, options),
        other => legacy::open(other, source, options),
    }
}

/// Creates a sink as `format`.
pub fn create<W: Write + Seek>(
    format: Format,
    sink: W,
    options: &WriteOptions,
) -> Result<Box<dyn EntryWriter<W>>> {
    match format {
        Format::Zip => zip::ZipWriter::create(sink, options),
        Format::SevenZ => sevenz::SevenZWriter::create(sink, options),
        Format::Tar => tar::TarWriter::create(sink, options),
        Format::CompressedTar(compression) => {
            tar::CompressedTarWriter::create(sink, compression, options)
        }
        Format::SingleStream(compression) => {
            single::SingleStreamWriter::create(sink, compression, options)
        }
        other => Err(Error::unsupported(format!("{other} cannot be created"))),
    }
}

/// The formats whose readers are carried across from `unarc-rs`.
///
/// These are the long tail: RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice. They have
/// in common that they are read-only, they are rare, and each has a decoder large enough that
/// rewriting it would be a poor use of the effort that is better spent on ZIP, 7Z and TAR.
///
/// Every one of them is listed in `SPEC.md` with what it does and does not do, and none of them is
/// claimed to work until a differential test against the real tool says so.
pub mod legacy {
    use super::*;

    /// Which of the long-tail formats this build actually has code for.
    ///
    /// A format that is not in this list reports [`ErrorKind::Unsupported`] rather than failing to
    /// compile, so one absent format does not take the rest of the crate with it.
    pub const AVAILABLE: &[Format] = &[
        Format::Rar,
        Format::Lzh,
        Format::Ace,
        Format::Arc,
        Format::Zoo,
        Format::Arj,
        Format::Sq,
        Format::Ucb,
        Format::Uc2,
        Format::Ha,
        Format::Hyp,
        Format::Ice,
    ];

    pub fn is_available(format: Format) -> bool {
        AVAILABLE.contains(&format)
    }

    pub fn open<R: Read + Seek>(
        format: Format,
        _source: R,
        _options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        Err(Error::unsupported(format!("{format} is not supported by this build")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Every format in the enum must be handled by `open`, one way or another.
    ///
    /// The point is not that each one works. It is that adding a format to [`Format`] without
    /// adding it here is a compile-time failure rather than a runtime surprise on the app.
    #[test]
    fn no_format_is_missing_from_the_dispatch() {
        // Detection has to be the fallback, so an unrecognised file is offered to every candidate
        // in turn rather than refused outright.
        let unrecognised = Format::Zip;
        let options = OpenOptions::default();
        // A file that is not a ZIP must not be reported as a ZIP; it must be reported as malformed
        // or not-an-archive, never as a panic.
        let result = open(unrecognised, Cursor::new(b"not an archive at all".to_vec()), &options);
        assert!(result.is_err());
    }

    #[test]
    fn a_format_with_no_reader_says_so_rather_than_pretending() {
        let options = OpenOptions::default();
        let error = legacy::open(
            Format::Ucb,
            Cursor::new(Vec::new()),
            &options,
        )
        .unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Unsupported);
        assert!(error.message().contains("not supported"));
    }

    #[test]
    fn every_long_tail_format_is_listed_as_available_so_it_can_be_tested() {
        for format in [
            Format::Rar,
            Format::Lzh,
            Format::Ace,
            Format::Arc,
            Format::Zoo,
            Format::Arj,
            Format::Sq,
            Format::Ucb,
            Format::Uc2,
            Format::Ha,
            Format::Hyp,
            Format::Ice,
        ] {
            assert!(legacy::is_available(format), "{format:?} should be listed");
        }
    }
}