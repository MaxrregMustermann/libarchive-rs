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
//!
//! ## Every constant below was read out of the decoder that reads the format
//!
//! An earlier version of this file had signatures for eight of the twelve long-tail formats that
//! were written from the published description rather than from the bytes, and not one of the eight
//! matched a real archive. The table below says where each constant comes from; every one is a
//! literal in `unarc-rs` 0.7.2, the crate that actually decodes these formats, with the file and
//! line it was copied from. `tests/detect_signatures.rs` checks each one against the vendored
//! fixture, so a constant that drifts from the bytes fails there rather than in the field.
//!
//! ## Two of the twelve have no bytes to identify them with
//!
//! ICE begins with the uncompressed size in little endian and ARC begins with `0x1A`, the DOS
//! end-of-file marker, which appears in files that are not archives at all. Inventing a signature
//! for either would turn a text file into a Zoo. Those two are reachable by name instead, through
//! [`detect_header_with_name`], which is consulted only after the bytes have said nothing.
//!
//! ## Compressed TAR sniffing
//!
//! A `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` or `.tar.lz4` file has the same leading bytes as
//! a raw `.gz`, `.bz2`, `.xz`, `.zst` or `.lz4` file. The only way to tell them apart is to
//! decompress far enough to reach the TAR magic (`ustar` at offset 257). This module does that
//! sniff when the leading bytes match a known compressor, reading at most one TAR block (512
//! bytes) of decompressed data. The sniff is bounded by [`Limits::max_metadata_size`] so that a
//! hostile stream cannot force unbounded decompression during detection.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::compression::decode;
use crate::error::{Error, Result};
use crate::format::{Compression, Format};
use crate::limits::Limits;
use crate::reader::RawReader;

/// How many bytes detection reads.
///
/// 512 is a TAR block and covers every signature here, including the TAR one at offset 257 and the
/// ACE marker wherever it lands. Reading more than this on every open costs time and, on a
/// channel-backed source, a JNI round trip.
pub const HEADER_SIZE: usize = 512;

/// How many decompressed bytes to read when sniffing for a compressed TAR.
///
/// One TAR block (512 bytes) is enough to reach the ustar magic at offset 257 and confirm the
/// format. Reading more would cost time without adding certainty.
const SNIFF_SIZE: usize = 512;

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

/// `unarc-rs-0.7.2/src/ha/header.rs:6` — `HA_MAGIC`.
const HA: &[u8] = b"HA";
/// `unarc-rs-0.7.2/src/zoo/zoo_header.rs:6` — `TEXT`. Seventeen bytes, and the version in it is
/// 2.10. The constant this replaced said `ZOO 200`, which is a different archive format.
const ZOO: &[u8] = b"ZOO 2.10 Archive.";
/// `unarc-rs-0.7.2/src/sq/header.rs` — `SQ_SIG`.
const SQ1: &[u8] = &[0x76, 0xFF];
/// `unarc-rs-0.7.2/src/sq/header.rs` — `SQ2_SIG`.
const SQ2: &[u8] = &[0xFA, 0xFF];
/// `unarc-rs-0.7.2/src/hyp/header.rs:5` — `COMPRESSED: u16 = 0x5048`. That is the number, and these
/// are the three bytes the file has: the `0x1A` end-of-file marker, then `"HP"`.
const HYP_COMPRESSED: &[u8] = &[0x1A, b'H', b'P'];
/// `unarc-rs-0.7.2/src/hyp/header.rs:6` — `STORED: u16 = 0x5453`, which is `0x1A` then `"ST"`.
const HYP_STORED: &[u8] = &[0x1A, b'S', b'T'];
/// `unarc-rs-0.7.2/src/uc2/uc2_archive.rs:40` — `ID: u32 = 0x1A324355`, which is these four bytes
/// read as a little-endian `u32`. The fifth byte of a UC2 file is the algorithm, not part of the
/// signature: the four vendored fixtures carry 0xa4, 0x1a, 0x16 and 0x08 there.
const UC2: &[u8] = b"UC2\x1A";
/// `unarc-rs-0.7.2/src/ace/ace_archive.rs` — `ACE_MAGIC`. Not at a fixed offset: the decoder
/// *searches* for it and then treats whatever is seven bytes before it as the start of the main
/// header, so it is searched for here too rather than compared at offset 0.
const ACE: &[u8] = b"**ACE**";
/// Where `**ACE**` may sit, relative to the start of the file. The decoder rejects a marker found
/// before this, because the seven bytes before it are the main header's CRC and size.
const ACE_MINIMUM_OFFSET: usize = 7;

/// The byte order mark that begins a `compress`ed stream.
const UNIX_COMPRESS: &[u8] = &[0x1F, 0x9D];

/// The extensions a name can settle when the bytes cannot.
///
/// Reachable only through [`detect_header_with_name`], and only after every signature above has
/// failed, so a name never overrides a format the file actually is. Two entries here do real work:
/// `arc` and `ice` have no signature at all and would otherwise be unreachable, because their first
/// bytes are the DOS end-of-file marker and a length respectively. The rest are here so that a
/// long-tail archive with a large stub in front of it — which puts its marker past the 512 bytes
/// detection reads — is still reachable when the caller knows what it is called.
///
/// `ucb` is deliberately absent: there is no UCB decoder in this crate, so naming a format that
/// cannot be read would only move the failure.
const NAME_HINTS: &[(&str, Format)] = &[
    ("rar", Format::Rar),
    ("lzh", Format::Lzh),
    ("lha", Format::Lzh),
    ("ace", Format::Ace),
    ("arc", Format::Arc),
    ("zoo", Format::Zoo),
    ("arj", Format::Arj),
    ("sq", Format::Sq),
    ("sq2", Format::Sq),
    ("uc2", Format::Uc2),
    ("ha", Format::Ha),
    ("hyp", Format::Hyp),
    ("ice", Format::Ice),
];

/// Reads a header and rewinds, then works out the format.
///
/// The read is a loop rather than one `read_exact`, because a source backed by a channel hands
/// back short reads and a single call would miss the signature.
pub fn detect<R: Read + Seek + Send>(source: &mut R, limits: &Limits) -> Result<Format> {
    detect_with_name(source, None, limits)
}

/// [`detect`], with the name the caller already has as a fallback, and limits to bound any
/// decompression sniffing.
///
/// The name is consulted only when the bytes identified nothing, so it can make a format with no
/// signature reachable without ever overriding a format the file really is. See
/// [`detect_header_with_name`].
pub fn detect_with_name<R: Read + Seek + Send>(
    source: &mut R,
    name: Option<&Path>,
    limits: &Limits,
) -> Result<Format> {
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
    Ok(detect_header_with_name(
        &header[..filled],
        name,
        source,
        start,
        limits,
    ))
}

/// Works out the format from bytes already in hand.
pub fn detect_header(header: &[u8]) -> Format {
    detect_header_with_name(header, None, &mut std::io::empty(), 0, &Limits::default())
}

/// [`detect_header`], falling back to `name` when the bytes identified nothing.
///
/// `name` is the path the app already has in its hand; only its extension is read, lowercased, so
/// `LICENSE.ICE` and `license.ice` are the same answer. It is a fallback and not an override: a
/// Zoo file called `photo.jpg` is a Zoo, and this returns [`Format::Zoo`].
///
/// The `source` and `start` parameters are used for compressed TAR sniffing: if the header matches
/// a known compressor (gzip, bzip2, xz, zstd, lz4), the source is rewound to `start` and up to
/// `SNIFF_SIZE` decompressed bytes are read to check for the TAR magic at offset 257. If `source`
/// is empty (e.g., `std::io::empty()`), sniffing is skipped.
pub fn detect_header_with_name<R: Read + Seek + Send>(
    header: &[u8],
    name: Option<&Path>,
    source: &mut R,
    start: u64,
    limits: &Limits,
) -> Format {
    // A TAR has no signature at offset zero. Its magic is the word "ustar" 257 bytes in, inside
    // what is otherwise a block of mostly zeroes, so it is checked last: it is the weakest signal
    // here and the only one that needs the header to be long enough to reach.
    if let Some(format) = detect_signed(header) {
        // For compressors that can wrap a TAR, sniff the decompressed content.
        if let Format::SingleStream(compression) = format {
            if matches!(
                compression,
                Compression::Gzip
                    | Compression::Bzip2
                    | Compression::Xz
                    | Compression::Zstd
                    | Compression::Lz4
            ) {
                if let Ok(true) = sniff_compressed_tar(source, start, compression, limits) {
                    return Format::CompressedTar(compression);
                }
            }
        }
        return format;
    }
    if header.len() > 262 && header[257..262] == *b"ustar" {
        return Format::Tar;
    }
    // Nothing recognised by content. A name is the only thing left that can say, and for ARC and
    // ICE it is the only thing that ever can.
    if let Some(format) = name.and_then(format_from_name) {
        return format;
    }
    // Nothing recognised. ZIP is the fallback because it can report a meaningful error for a file
    // that is not an archive, rather than "unknown format".
    Format::Zip
}

/// Sniffs a compressed stream to check if it contains a TAR archive.
///
/// Reads up to `max_compressed` bytes from `source` starting at `start`, creates a decoder for
/// `compression`, and reads up to `SNIFF_SIZE` decompressed bytes. Returns `Ok(true)` if the TAR
/// magic (`ustar` at offset 257) is found, `Ok(false)` otherwise. Returns an error if the source
/// cannot be rewound or if the decompression fails in a way that is not EOF.
///
/// The read is bounded by `limits.max_metadata_size` to prevent a hostile stream from forcing
/// unbounded decompression during detection.
fn sniff_compressed_tar<R: Read + Seek>(
    source: &mut R,
    start: u64,
    compression: Compression,
    limits: &Limits,
) -> Result<bool> {
    // Rewind to the start of the compressed stream.
    source.seek(SeekFrom::Start(start))?;

    // Read up to 64 KiB of compressed data (enough to produce 512 decompressed bytes for any
    // realistic compression ratio, and bounded for hostile inputs).
    const MAX_COMPRESSED: u64 = 64 * 1024;
    let mut compressed = Vec::new();
    let mut limited = source.take(MAX_COMPRESSED);
    std::io::copy(&mut limited, &mut compressed)?;

    // Create a decoder over the compressed buffer.
    let cursor = std::io::Cursor::new(compressed);
    let mut decoder = match decode(compression, Box::new(cursor)) {
        Ok(d) => d,
        Err(_) => {
            source.seek(SeekFrom::Start(start))?;
            return Ok(false);
        }
    };

    // Read up to SNIFF_SIZE decompressed bytes, charging against the metadata limit.
    let mut buf = [0u8; SNIFF_SIZE];
    let mut total_read = 0usize;
    while total_read < SNIFF_SIZE {
        let want = SNIFF_SIZE - total_read;
        match decoder.read(&mut buf[total_read..total_read + want]) {
            Ok(0) => break,
            Ok(n) => {
                total_read += n;
                // Charge against the metadata limit.
                if let Err(_) = limits.check_metadata_size(total_read as u64) {
                    source.seek(SeekFrom::Start(start))?;
                    return Ok(false);
                }
            }
            Err(_) => {
                source.seek(SeekFrom::Start(start))?;
                return Ok(false);
            }
        }
    }

    // Check for TAR magic at offset 257.
    let is_tar = total_read >= 262 && &buf[257..262] == b"ustar";

    // Rewind the source again for the actual reader.
    source.seek(SeekFrom::Start(start))?;

    Ok(is_tar)
}

/// The long-tail format an extension names, if it names one.
fn format_from_name(name: &Path) -> Option<Format> {
    let extension = name.extension()?.to_str()?.to_ascii_lowercase();
    NAME_HINTS
        .iter()
        .find(|(hint, _)| *hint == extension)
        .map(|(_, format)| *format)
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
    if is_lzh(header) {
        return Some(Format::Lzh);
    }
    if has_ace_marker(header) {
        return Some(Format::Ace);
    }
    if starts(ZOO) {
        return Some(Format::Zoo);
    }
    if starts(HYP_COMPRESSED) || starts(HYP_STORED) {
        return Some(Format::Hyp);
    }
    if starts(SQ1) || starts(SQ2) {
        return Some(Format::Sq);
    }
    if starts(UC2) {
        return Some(Format::Uc2);
    }
    if starts(ARJ) {
        return Some(Format::Arj);
    }
    if starts(HA) {
        // The weakest signature in this file, and the last one checked. `HA` is two bytes of
        // upper case at offset zero, which is not much to go on — it is here because it is what
        // the Ha decoder accepts (`ha/header.rs:6`) and no stronger signal exists.
        return Some(Format::Ha);
    }
    None
}

/// Whether the header opens with an LHA/LZH level 0 or level 1 header.
///
/// The `-lh5-` marker is at **offset 2**, not offset 0, because the first two bytes of such a
/// header are the header's own length and checksum — which is also why the LZH fixtures here do not
/// carry an SFX stub in front of anything. The five-byte method identifier sits at offsets 2..7,
/// so the method itself is the byte at offset 5. This is checked rather than compared whole
/// because a header the decoder cannot use is not an LZH, and the caller is better served by the
/// "not an archive" error than by an LHA error.
///
/// The identifier list is `delharc` 0.8.0's own (`src/header/compression.rs`): `-lh0-` through
/// `-lh7-`, and `-lhx-`, `-lhd-`, `-lzs-`, `-lz4-`, `-lz5-`.
fn is_lzh(header: &[u8]) -> bool {
    const MARKER_AT: usize = 2;
    let end = MARKER_AT + 5;
    if header.len() < end || &header[MARKER_AT..MARKER_AT + 3] != b"-lh" {
        return false;
    }
    let method = header[MARKER_AT + 3];
    let method = matches!(method, b'0'..=b'9' | b'd' | b's' | b'x' | b'z');
    method && header[end - 1] == b'-'
}

/// Whether the header carries an ACE main header anywhere the decoder would accept one.
///
/// The marker is searched for rather than compared at a fixed offset, because ACE archives are
/// routinely prefixed with a self-extracting stub and `unarc-rs` searches the first 512 KiB for
/// it. Detection reads [`HEADER_SIZE`] bytes, so this finds a marker in the first 512 and not
/// beyond; the name hint in [`detect_header_with_name`] covers the rest.
fn has_ace_marker(header: &[u8]) -> bool {
    header.len() >= ACE.len() + ACE_MINIMUM_OFFSET
        && header[ACE_MINIMUM_OFFSET..]
            .windows(ACE.len())
            .any(|window| window == ACE)
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
            (ZOO, Format::Zoo),
            (ARJ, Format::Arj),
            (SQ1, Format::Sq),
            (SQ2, Format::Sq),
            (UC2, Format::Uc2),
            (HYP_COMPRESSED, Format::Hyp),
            (HYP_STORED, Format::Hyp),
            (HA, Format::Ha),
        ];
        for (magic, expected) in cases {
            let mut header = vec![0u8; HEADER_SIZE];
            header[..magic.len()].copy_from_slice(magic);
            assert_eq!(detect_header(&header), *expected, "magic {magic:02x?}");
        }
    }

    /// The signatures that used to be in this table, and what they actually matched.
    ///
    /// Every one of these is a constant that was removed because it identified nothing. If any of
    /// them ever becomes real it should become real because a decoder accepts it, not because a
    /// byte pattern looked plausible.
    #[test]
    fn no_signature_is_recognised_from_a_constant_that_matched_nothing() {
        let invented: &[(&str, &[u8])] = &[
            ("ZOO 200", b"ZOO 200"),
            ("SQ\\x1a", b"SQ\x1a"),
            ("HYP ZZX\\0", b"ZZX\x00"),
            ("ICE 1f a0", &[0x1F, 0xA0]),
            ("UCB UCh\\x1a\\0", b"UCh\x1a\x00"),
            ("ACE at offset 0", ACE),
        ];
        for (name, bytes) in invented {
            let mut header = vec![0u8; HEADER_SIZE];
            header[..bytes.len()].copy_from_slice(bytes);
            assert_eq!(
                detect_header(&header),
                Format::Zip,
                "{name} was removed because it identified nothing; if it is real again, say so here"
            );
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
    fn an_lzh_is_recognised_by_its_method_marker_at_offset_2_rather_than_at_the_start() {
        // A real level 1 header: two bytes of header length and checksum, then `-lh5-`.
        let mut header = vec![0u8; HEADER_SIZE];
        header[..7].copy_from_slice(&[0x1e, 0xbb, b'-', b'l', b'h', b'5', b'-']);
        assert_eq!(detect_header(&header), Format::Lzh);

        // Every method `delharc` knows, at the byte that carries it.
        for method in [b'0', b'1', b'4', b'5', b'7', b'x', b'd', b's', b'z'] {
            header[5] = method;
            assert_eq!(
                detect_header(&header),
                Format::Lzh,
                "-lh{method}- is an LZH"
            );
        }
        // A method no decoder has is not an LZH, and saying so gets the caller "not an archive"
        // rather than an LHA error about a method.
        header[5] = b'\xff';
        assert_eq!(detect_header(&header), Format::Zip);
        header[5] = b'5';
        // And the trailing '-' of the identifier is not optional.
        header[6] = b'!';
        assert_eq!(detect_header(&header), Format::Zip);
    }

    #[test]
    fn an_ace_is_found_wherever_its_marker_is_rather_than_at_a_fixed_offset() {
        // `-lh5-` at offset 2 of a real ACE file: a self-extracting stub in front of the header.
        let mut header = vec![0u8; HEADER_SIZE];
        header[400..407].copy_from_slice(ACE);
        assert_eq!(detect_header(&header), Format::Ace);

        // Too close to the start to be a main header: the seven bytes before the marker are the
        // header's CRC and size, and `unarc-rs` refuses a marker found before that.
        header.fill(0);
        header[3..10].copy_from_slice(ACE);
        assert_eq!(detect_header(&header), Format::Zip);
    }

    #[test]
    fn a_name_settles_only_what_the_bytes_cannot() {
        let ice = [0x5Du8, 0x1C, 0x00, 0x00, 0xAA, 0x00];
        let mut empty = std::io::empty();
        let limits = Limits::default();
        // Content first, and then the name, which is how an ARC and an ICE become reachable at all.
        assert_eq!(
            detect_header_with_name(&ice, Some(Path::new("a.ice")), &mut empty, 0, &limits),
            Format::Ice
        );
        assert_eq!(
            detect_header_with_name(&ice, Some(Path::new("a.zip")), &mut empty, 0, &limits),
            Format::Zip
        );
        assert_eq!(
            detect_header_with_name(&ice, None, &mut empty, 0, &limits),
            Format::Zip
        );
        // Case and a second dot do not matter; a name with no extension does not either.
        assert_eq!(
            detect_header_with_name(&ice, Some(Path::new("a.ICE")), &mut empty, 0, &limits),
            Format::Ice
        );
        assert_eq!(
            detect_header_with_name(
                &ice,
                Some(Path::new("archive.tar.ice")),
                &mut empty,
                0,
                &limits
            ),
            Format::Ice
        );
        assert_eq!(
            detect_header_with_name(&ice, Some(Path::new("ice")), &mut empty, 0, &limits),
            Format::Zip
        );
        // A real archive keeps its real format whatever it is called.
        let mut zoo = vec![0u8; HEADER_SIZE];
        zoo[..ZOO.len()].copy_from_slice(ZOO);
        assert_eq!(
            detect_header_with_name(&zoo, Some(Path::new("photo.jpg")), &mut empty, 0, &limits),
            Format::Zoo
        );
        // And there is no name that says UCB, because nothing here can read one.
        assert_eq!(
            detect_header_with_name(&ice, Some(Path::new("a.ucb")), &mut empty, 0, &limits),
            Format::Zip
        );
    }

    #[test]
    fn a_file_with_nothing_recognisable_falls_back_to_zip_rather_than_failing() {
        // ZIP is the fallback because it can report a real error for a non-archive, rather than
        // "unknown format", which tells the caller nothing.
        let mut empty = std::io::empty();
        let limits = Limits::default();
        assert_eq!(
            detect_header_with_name(b"this is a text file", None, &mut empty, 0, &limits),
            Format::Zip
        );
        assert_eq!(
            detect_header_with_name(&[], None, &mut empty, 0, &limits),
            Format::Zip
        );
    }

    #[test]
    fn detection_leaves_the_source_where_it_found_it() {
        let bytes = [0x37u8, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04];
        let mut source = Cursor::new(bytes.to_vec());
        source.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(detect(&mut source, &Limits::default()).unwrap(), Format::SevenZ);
        assert_eq!(
            source.stream_position().unwrap(),
            0,
            "the source must be rewound"
        );
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
        let mut source = Chunky {
            data: SEVEN_Z.to_vec(),
            position: 0,
        };
        assert_eq!(detect(&mut source, &Limits::default()).unwrap(), Format::SevenZ);
        assert_eq!(source.position, 0);
    }
}
