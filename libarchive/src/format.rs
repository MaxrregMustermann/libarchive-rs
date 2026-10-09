//! Formats and the compression methods inside them.

use std::fmt;

/// An archive format this crate can read or write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    Zip,
    SevenZ,
    Tar,
    /// A tar wrapped in one whole-archive compression stream.
    CompressedTar(Compression),
    /// A single compressed stream, which is one entry by definition.
    SingleStream(Compression),
    Rar,
    Lzh,
    Ace,
    Arc,
    Zoo,
    Arj,
    Sq,
    Ucb,
    Uc2,
    Ha,
    Hyp,
    Ice,
}

impl Format {
    /// The extension the app appends when creating an archive of this format.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Tar => "tar",
            Format::CompressedTar(compression) => match compression {
                Compression::Gzip => "tar.gz",
                Compression::Bzip2 => "tar.bz2",
                Compression::Xz => "tar.xz",
                Compression::Zstd => "tar.zst",
                Compression::Lz4 => "tar.lz4",
                _ => "tar",
            },
            Format::SingleStream(compression) => match compression {
                Compression::Gzip => "gz",
                Compression::Bzip2 => "bz2",
                Compression::Xz => "xz",
                Compression::Zstd => "zst",
                Compression::Lz4 => "lz4",
                Compression::Brotli => "br",
                Compression::Z => "Z",
                _ => "bin",
            },
            Format::Rar => "rar",
            Format::Lzh => "lzh",
            Format::Ace => "ace",
            Format::Arc => "arc",
            Format::Zoo => "zoo",
            Format::Arj => "arj",
            Format::Sq => "sq",
            Format::Ucb => "ucb",
            Format::Uc2 => "uc2",
            Format::Ha => "ha",
            Format::Hyp => "hyp",
            Format::Ice => "ice",
        }
    }

    /// The formats this crate can create. Reading is deliberately wider than writing: a file
    /// manager that cannot open a format must still list and extract it.
    pub fn is_writable(self) -> bool {
        matches!(
            self,
            Format::Zip
                | Format::SevenZ
                | Format::Tar
                | Format::CompressedTar(_)
                | Format::SingleStream(_)
        )
    }

    /// Whether the format stores entries in random order or requires a full pass to list.
    ///
    /// This matters for the reader: a format that has to be read front to back cannot be opened
    /// once and indexed, so entries have to be collected as they are found.
    pub fn needs_seek_to_list(self) -> bool {
        !matches!(
            self,
            Format::Tar
                | Format::CompressedTar(_)
                | Format::SingleStream(_)
                | Format::Hyp
                | Format::Ice
                | Format::Ha
                | Format::Arc
                | Format::Zoo
        )
    }
}

/// A compression method, either for a whole archive or for one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Compression {
    /// No compression. Called "store" in ZIP.
    None,
    Deflate,
    Deflate64,
    Bzip2,
    Lzma,
    Lzma2,
    /// The PPMd variant used by 7Z.
    Ppmd,
    Zstd,
    Lz4,
    /// XZ, which is LZMA2 inside a container with its own index and integrity checks.
    Xz,
    /// Gzip, which is deflate inside a container with its own header and trailer.
    Gzip,
    Brotli,
    /// The Unix `compress` format, which is LZW with a twist.
    Z,
    /// A method this build cannot handle. Carried rather than dropped so that listing an archive
    /// still shows every entry, even ones whose contents cannot be produced.
    Unknown(u16),
}

impl Compression {
    /// The ZIP general purpose bit flag value, or `None` when the method is not a ZIP one.
    pub fn zip_method(self) -> Option<u16> {
        Some(match self {
            Compression::None => 0,
            Compression::Deflate => 8,
            Compression::Bzip2 => 12,
            Compression::Lzma => 14,
            Compression::Zstd => 93,
            Compression::Xz => 95,
            Compression::Deflate64 => 9,
            _ => return None,
        })
    }

    /// Whether this crate can decompress this method.
    pub fn is_readable(self) -> bool {
        !matches!(self, Compression::Unknown(_))
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Compression::None => "store",
            Compression::Deflate => "deflate",
            Compression::Deflate64 => "deflate64",
            Compression::Bzip2 => "bzip2",
            Compression::Lzma => "lzma",
            Compression::Lzma2 => "lzma2",
            Compression::Ppmd => "ppmd",
            Compression::Zstd => "zstd",
            Compression::Lz4 => "lz4",
            Compression::Xz => "xz",
            Compression::Gzip => "gzip",
            Compression::Brotli => "brotli",
            Compression::Z => "Z",
            Compression::Unknown(id) => return write!(f, "unknown method {id}"),
        };
        f.write_str(name)
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extensions_the_app_appends_are_the_ones_it_shows() {
        assert_eq!(Format::Zip.extension(), "zip");
        assert_eq!(Format::SevenZ.extension(), "7z");
        assert_eq!(Format::Tar.extension(), "tar");
        assert_eq!(Format::CompressedTar(Compression::Xz).extension(), "tar.xz");
        assert_eq!(Format::CompressedTar(Compression::Gzip).extension(), "tar.gz");
        assert_eq!(Format::CompressedTar(Compression::Bzip2).extension(), "tar.bz2");
        assert_eq!(Format::SingleStream(Compression::Xz).extension(), "xz");
    }

    #[test]
    fn every_zip_method_id_round_trips_against_the_specification() {
        assert_eq!(Compression::None.zip_method(), Some(0));
        assert_eq!(Compression::Deflate.zip_method(), Some(8));
        assert_eq!(Compression::Deflate64.zip_method(), Some(9));
        assert_eq!(Compression::Bzip2.zip_method(), Some(12));
        assert_eq!(Compression::Lzma.zip_method(), Some(14));
        assert_eq!(Compression::Zstd.zip_method(), Some(93));
        assert_eq!(Compression::None.zip_method().unwrap(), 0, "store is method 0");
    }

    #[test]
    fn an_unknown_method_is_carried_rather_than_dropped_so_listing_still_works() {
        // This is what lets an archive with one unsupported entry still list every other entry.
        assert!(!Compression::Unknown(99).is_readable());
        assert_eq!(Compression::Unknown(99).zip_method(), None);
    }

    #[test]
    fn reading_covers_strictly_more_than_writing() {
        let all = [
            Format::Zip,
            Format::SevenZ,
            Format::Tar,
            Format::CompressedTar(Compression::Xz),
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
        let writable: Vec<_> = all.iter().filter(|format| format.is_writable()).collect();
        assert!(writable.len() < all.len(), "some formats must be read-only");
    }
}