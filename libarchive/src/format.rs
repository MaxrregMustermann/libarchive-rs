//! Formats and the compression methods inside them.

use std::fmt;

/// An archive format this crate can read or write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// ZIP archive (.zip). Supports Deflate, Deflate64, Bzip2, LZMA, Zstd. Read and write.
    Zip,
    /// 7-Zip archive (.7z). Supports LZMA2, Bzip2, Zstd, PPMd, BCJ2. Read and write.
    SevenZ,
    /// Uncompressed TAR archive (.tar). Read and write.
    Tar,
    /// A tar wrapped in one whole-archive compression stream (.tar.gz, .tar.bz2, etc.). Read and write for gzip, bzip2, xz, zstd, lz4.
    CompressedTar(Compression),
    /// A single compressed stream, which is one entry by definition (.gz, .bz2, .xz, .zst, .lz4, .br, .Z). Read and write.
    SingleStream(Compression),
    /// RAR archive (.rar). Read only; multi-volume requires a VolumeProvider not yet wired.
    Rar,
    /// LHA/LZH archive (.lzh). Read only; extraction blocked by listing-loop defect.
    Lzh,
    /// ACE archive (.ace). Read only; single-volume works, multi-volume requires VolumeProvider.
    Ace,
    /// ARC archive (.arc). Read only; extraction blocked by listing-loop defect.
    Arc,
    /// Zoo archive (.zoo). Read only; works for most fixtures.
    Zoo,
    /// ARJ archive (.arj). Read only; extraction blocked by listing-loop defect.
    Arj,
    /// SQ archive (.sq, .sq2). Read only; fails silently (streams 0 bytes).
    Sq,
    /// UCB archive (.ucb). Not supported; no decoder available.
    Ucb,
    /// UC2 archive (.uc2). Read only; this crate's own implementation, works correctly.
    Uc2,
    /// Ha archive (.ha). Read only; extraction blocked by listing-loop defect.
    Ha,
    /// Hyp archive (.hyp). Read only; extraction blocked by listing-loop defect.
    Hyp,
    /// Ice archive (.ice). Read only; works correctly for single-volume.
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
    /// Deflate (ZIP method 8, gzip, zlib). Read and write.
    Deflate,
    /// Deflate64 (ZIP method 9). Read only.
    Deflate64,
    /// Bzip2 (ZIP method 12, .bz2, 7Z, TAR). Read and write.
    Bzip2,
    /// Raw LZMA (ZIP method 14). Read only.
    Lzma,
    /// LZMA2 (7Z folder, XZ container). Not a raw codec; driven by 7Z/XZ readers.
    Lzma2,
    /// The PPMd variant used by 7Z. Not wired.
    Ppmd,
    /// Zstd (7Z folder, .zst, TAR). Read and write.
    Zstd,
    /// LZ4 frame (7Z folder, .lz4, TAR). Frame decoder only; read only.
    Lz4,
    /// XZ, which is LZMA2 inside a container with its own index and integrity checks. Read and write.
    Xz,
    /// Gzip, which is deflate inside a container with its own header and trailer. Read and write.
    Gzip,
    /// Brotli (.br, 7Z folder). Read only; no magic, trial-decode.
    Brotli,
    /// The Unix `compress` format, which is LZW with a twist. No decoder crate; carried from `unarc-rs`.
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

    /// The ZIP method number for a compression, or the unknown method carrying the number.
    ///
    /// The reverse of [`Compression::zip_method`], and the only place a ZIP method number becomes
    /// a [`Compression`]. An unrecognised number becomes [`Compression::Unknown`] rather than an
    /// error, so that listing an archive still shows entries this build cannot extract — which is
    /// what a file manager should do, since the entry's name and size are still worth showing.
    pub fn from_zip_method(method: u16) -> Compression {
        match method {
            0 => Compression::None,
            8 => Compression::Deflate,
            9 => Compression::Deflate64,
            12 => Compression::Bzip2,
            14 => Compression::Lzma,
            93 => Compression::Zstd,
            95 => Compression::Xz,
            // 1 is shrink, 6 is implode, and 2 through 5 are reduce variants. None of them has a
            // crate here and all of them are old enough that a file using one is already a
            // curiosity. They fall through to Unknown rather than being special-cased.
            other => Compression::Unknown(other),
        }
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
        assert_eq!(
            Format::CompressedTar(Compression::Gzip).extension(),
            "tar.gz"
        );
        assert_eq!(
            Format::CompressedTar(Compression::Bzip2).extension(),
            "tar.bz2"
        );
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
        assert_eq!(
            Compression::None.zip_method().unwrap(),
            0,
            "store is method 0"
        );
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
