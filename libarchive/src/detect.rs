//! Working out what an archive is from the bytes rather than the name.
//!
//! The name says what somebody called the file. The bytes say what it is. An archive gets renamed,
//! an `.zip` turns out to be a JAR, and a file with no extension at all is still an archive if its
//! first six bytes say so.
//!
//! Detection here reads a bounded amount and never seeks, because a source that is a pipe or a
//! network socket can be read once. It also never *fails*: anything unrecognised comes back as
//! [`Format::Zip`], because ZIP is the only format whose central directory is found by scanning
//! back from the end, which makes it the one that can produce a useful error for a file that is not
//! an archive at all.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::format::{Compression, Format};

/// How many bytes detection reads.
///
/// 512 is a TAR block and covers every fixed signature, including the one at offset 257. Reading
/// more than this on every open costs time and, on a channel-backed source, a JNI round trip.
pub const HEADER_SIZE: usize = 512;

const SEVEN_Z: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const XZ: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];
const GZIP: &[u8] = &[0x1F, 0x8B, 0x08];
const BZIP2: &[u8] = &[b'B', b'Z', b'h'];
const ZSTD: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
const LZ4: &[u8] = &[0x04, 0x22, 0x4D, 0x18];
const ZIP_LOCAL: &[u8] = &[b'P', b'K', 0x03, 0x04];
const ZIP_EMPTY: &[u8] = &[b'P', b'K', 0x05, 0x06];
const ZIP_SPANNED: &[u8] = &[b'P', b'K', 0x07, 0x08];
const RAR4: &[u8] = &[b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x00];
const RAR5: &[u8] = &[b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x01, 0x00];
const ARJ: &[u8] = &[0x60, 0xEA];
const LZH_ONE: &[u8] = &[0x2D, 0x6C, 0x68];
const LZH_TWO: &[u8] = &[0x2D, 0x6C, 0x7A];
const ACE: &[u8] = &[b'*', b'*', b'A', b'C', b'E', b'*', b'*'];
const ZOO: &[u8] = &[0x5A, 0x4F, 0x4F, 0x20, 0x32, 0x30, 0x30];
const SQ: &[u8] = &[b'S', b'Q', 0x1A];
const UCB: &[u8] = &[b'U', b'C', b'h', 0x1A, 0x00];
const HYP: &[u8] = &[b'Z', b'Z', b'X', 0x00];
const ICE: &[u8] = &[0x1F, 0xA0];

/// The byte order mark that begins a `compress`ed stream.
const UNIX_COMPRESS: &[u8] = &[0x1F, 0x9D];

/// Reads a header and rewinds, then works out the format.
///
/// The read is a loop rather than one `read_exact`, because a source backed by a channel hands
/// back short reads and a single call would miss the signature.
pub fn detect<R: Read + Seek>(source: &mut R) -> Result<Format> {
    let start = source.stream_position()?;
    let mut header = [0u8; HEADER_SIZE];
    let mut filled = 0;
    while filled < header.len() {
        match source.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::io(error).context("reading the archive header")),
        }
    }
    // Rewinding matters: every format's reader starts from the beginning, and a source that cannot
    // rewind cannot be opened at all. The check is what turns that into a clear message.
    if let Err(error) = source.seek(SeekFrom::Start(start)) {
        return Err(Error::io(error).context("the archive source is not seekable"));
    }
    Ok(detect_header(&header[..filled]))
}

/// Works out the format from bytes already in hand.
pub fn detect_header(header: &[u8]) -> Format {
    // A TAR has no signature at offset zero. Its magic is the word "ustar" 257 bytes in, inside
    // what is otherwise a block of mostly zeroes, so it is checked last: it is the weakest signal
    // here and the only one that needs the header to be long enough to reach.
    if let Some(format) = detect_signed(header) {
        return format;
    }
    if header.len() > 262 && header[257..262] == *b"ustar" {
        return Format::Tar;
    }
    // Nothing recognised. ZIP is the fallback because it can report a meaningful error for a file
    // that is not an archive, rather than "unknown format".
    Format::Zip
}

/// The formats that start with bytes at offset zero.
fn detect_signed(header: &[u8]) -> Option<Format> {
    let starts = |magic: &[u8]| header.len() >= magic.len() && &header[..magic.len()] == magic;
    if starts(SEVEN_Z) {
        return Some(Format::SevenZ);
    }
    if starts(XZ) {
        // An XZ stream is either a single compressed file or a whole compressed tar. Which one can
        // only be known by decompressing it, so detection says "XZ" and the reader decides.
        return Some(Format::SingleStream(Compression::Xz));
    }
    if starts(ZIP_LOCAL) || starts(ZIP_EMPTY) || starts(ZIP_SPANNED) {
        return Some(Format::Zip);
    }
    if starts(RAR5) {
        return Some(Format::Rar);
    }
    if starts(RAR4) {
        return Some(Format::Rar);
    }
    if starts(GZIP) {
        return Some(Format::SingleStream(Compression::Gzip));
    }
    if starts(BZIP2) {
        return Some(Format::SingleStream(Compression::Bzip2));
    }
    if starts(ZSTD) {
        return Some(Format::SingleStream(Compression::Zstd));
    }
    if starts(LZ4) {
        return Some(Format::SingleStream(Compression::Lz4));
    }
    if starts(UNIX_COMPRESS) {
        return Some(Format::SingleStream(Compression::Z));
    }
    if starts(ACE) {
        return Some(Format::Ace);
    }
    if starts(ZOO) {
        return Some(Format::Zoo);
    }
    if starts(ARJ) {
        return Some(Format::Arj);
    }
    if starts(SQ) {
        return Some(Format::Sq);
    }
    if starts(UCB) {
        return Some(Format::Ucb);
    }
    if starts(HYP) {
        return Some(Format::Hyp);
    }
    if starts(ICE) {
        return Some(Format::Ice);
    }
    // LZH and its two levels of compression are told apart by the compression method byte at
    // offset 20, which is a digit rather than a letter for method zero and a letter otherwise.
    if (starts(LZH_ONE) || starts(LZH_TWO)) && header.len() > 20 {
        let method = header[20];
        let known = matches!(
            method,
            b'0'..=b'9' | b'l' | b'p' | b'z' | b'b' | b'-' | b'h' | b's'
        );
        if known {
            return Some(Format::Lzh);
        }
    }
    None
}

/// Rewinds a source, or says why it could not.
pub fn rewind<R: Seek>(source: &mut R) -> Result<()> {
    source
        .seek(SeekFrom::Start(0))
        .map(|_| ())
        .map_err(|error| Error::io(error).context("the archive source is not seekable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn every_format_is_recognised_from_its_own_signature() {
        let cases: &[(&[u8], Format)] = &[
            (SEVEN_Z, Format::SevenZ),
            (XZ, Format::SingleStream(Compression::Xz)),
            (ZIP_LOCAL, Format::Zip),
            (ZIP_EMPTY, Format::Zip),
            (ZIP_SPANNED, Format::Zip),
            (RAR4, Format::Rar),
            (RAR5, Format::Rar),
            (GZIP, Format::SingleStream(Compression::Gzip)),
            (BZIP2, Format::SingleStream(Compression::Bzip2)),
            (ZSTD, Format::SingleStream(Compression::Zstd)),
            (LZ4, Format::SingleStream(Compression::Lz4)),
            (UNIX_COMPRESS, Format::SingleStream(Compression::Z)),
            (ACE, Format::Ace),
            (ZOO, Format::Zoo),
            (ARJ, Format::Arj),
            (SQ, Format::Sq),
            (UCB, Format::Ucb),
            (HYP, Format::Hyp),
            (ICE, Format::Ice),
        ];
        for (magic, expected) in cases {
            let mut header = vec![0u8; HEADER_SIZE];
            header[..magic.len()].copy_from_slice(magic);
            assert_eq!(detect_header(&header), *expected, "magic {magic:02x?}");
        }
    }

    #[test]
    fn a_tar_is_found_by_its_magic_at_offset_257_rather_than_at_the_start() {
        let mut header = vec![0u8; HEADER_SIZE];
        header[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_header(&header), Format::Tar);
        // Too short to reach the magic, so it cannot be told apart from an empty file.
        assert_eq!(detect_header(&header[..200]), Format::Zip);
    }

    #[test]
    fn an_lzh_is_only_lzh_when_its_method_byte_is_one_a_compressor_actually_uses() {
        let mut header = vec![0u8; HEADER_SIZE];
        header[..5].copy_from_slice(b"-lh5-");
        assert_eq!(detect_header(&header), Format::Lzh);
        // A file that merely starts "-lh" but has nonsense where the method goes is not an LZH.
        header[20] = b'\xff';
        assert_eq!(detect_header(&header), Format::Zip);
    }

    #[test]
    fn a_file_with_nothing_recognisable_falls_back_to_zip_rather_than_failing() {
        // ZIP is the fallback because it can report a real error for a non-archive, rather than
        // "unknown format", which tells the caller nothing.
        assert_eq!(detect_header(b"this is a text file"), Format::Zip);
        assert_eq!(detect_header(&[]), Format::Zip);
    }

    #[test]
    fn detection_leaves_the_source_where_it_found_it() {
        let bytes = [0x37u8, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04];
        let mut source = Cursor::new(bytes.to_vec());
        source.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(detect(&mut source).unwrap(), Format::SevenZ);
        assert_eq!(source.stream_position().unwrap(), 0, "the source must be rewound");
    }

    #[test]
    fn detection_handles_a_source_that_returns_fewer_bytes_than_asked() {
        // A channel-backed source does this constantly. A single read call would miss everything
        // after the first short chunk.
        struct Chunky {
            data: Vec<u8>,
            position: usize,
        }
        impl Read for Chunky {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.position >= self.data.len() {
                    return Ok(0);
                }
                // Seven bytes at a time, which is smaller than any signature.
                let wanted = output.len().min(7).min(self.data.len() - self.position);
                output[..wanted].copy_from_slice(&self.data[self.position..self.position + wanted]);
                self.position += wanted;
                Ok(wanted)
            }
        }
        impl Seek for Chunky {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                let target = match position {
                    SeekFrom::Start(offset) => offset as i64,
                    SeekFrom::Current(delta) => self.position as i64 + delta,
                    SeekFrom::End(delta) => self.data.len() as i64 + delta,
                };
                self.position = target.max(0) as usize;
                Ok(self.position as u64)
            }
        }
        let mut source = Chunky { data: SEVEN_Z.to_vec(), position: 0 };
        assert_eq!(detect(&mut source).unwrap(), Format::SevenZ);
        assert_eq!(source.position, 0);
    }
}