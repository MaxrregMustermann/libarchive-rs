//! Single-stream compressors: one compressed file, one entry.
//!
//! **Owned by Team `codecs`.** The writer is in [`crate::writer::single`].
//!
//! A `.gz`, `.bz2`, `.xz`, `.zst` or `.lz4` file is not an archive. It is one compressed stream,
//! and the whole file is one entry. Two things follow:
//!
//! * The uncompressed size is often recorded, but not always, and where it is recorded it is in a
//!   *footer* at the end of the file. So this reader reads the tail to find it, and reports
//!   [`Entry::size`] as `None` when the format does not record one. A stream that does not know
//!   its size still reads; it just cannot report a length in advance, which is what the app shows
//!   next to a file's name.
//! * Reading it is entirely a matter of streaming. There is no second entry and no directory, so
//!   the reader is a decompressor wrapped in an [`EntryStream`], and nothing here ever allocates
//!   the entry's size.
//!
//! The exception is `.tar.gz` and its siblings, which are a TAR inside one of these streams. That is
//! [`crate::formats::tar::CompressedTarReader`], not this module, because a caller that lists it
//! wants the TAR's entries rather than one enormous one.
//!
//! # The tail read is bounded, not speculative
//!
//! Finding a recorded size means reading the end of the file. That read is capped at 64 KiB,
//! because a footer is never larger than that and because a 7 GiB file's tail is still 64 KiB.
//! Reading the whole file to find a 4-byte length would defeat the point of streaming.

use std::io::{Read, Seek, SeekFrom};

use crate::compression;
use crate::entry::{normalise_name, Entry, EntryKind};
use crate::error::{Error, Result};
use crate::format::Compression;
use crate::limits::Limits;
use crate::reader::{EntryRead, EntrySource, EntryStream, OpenOptions, Shared};

/// The name given to the single entry of a raw compressed stream.
///
/// A `.gz` file has no name of its own, and the app needs one to put in a listing.
pub const SINGLE_ENTRY_NAME: &str = "content";

/// How much of the end of the file is read looking for a recorded size.
///
/// Gzip's footer is 8 bytes and its largest optional fields are two 2-byte lengths; xz's index and
/// footer are bounded by the block size, which the format caps at 64 MiB but which in practice is
/// small; 64 KiB covers every real file and bounds the cost of a hostile one.
const TAIL_SIZE: u64 = 64 * 1024;

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

/// Reads one compressed stream as one entry.
pub struct SingleStreamReader<R: Read + Seek + Send + 'static> {
    source: Shared<R>,
    compression: Compression,
    entries: Vec<Entry>,
    limits: Limits,
    /// The CRC-32 the footer promises, checked as the entry's bytes stream past.
    expected: Option<u32>,
}

impl<R: Read + Seek + Send + 'static> SingleStreamReader<R> {
    /// Opens a single-stream compressed file.
    ///
    /// The whole file is one entry, named after what the app would show for it, so a `.gz` on
    /// disk lists as one file rather than as nothing.
    pub fn open(
        source: R,
        compression: Compression,
        options: &OpenOptions,
    ) -> Result<Box<dyn EntrySource<R>>> {
        let source = crate::reader::share(source);
        let length = file_length(&source)?;
        if length == 0 {
            return Err(Error::malformed("a compressed stream cannot be empty"));
        }
        if compression == Compression::Z {
            // The magic and code width are checked here, at open, so that opening garbage as `.Z`
            // reports `Malformed` rather than failing halfway through the first read. The bytes
            // stay where they are: entry reads start from the beginning, header included.
            check_z_header(&source)?;
        }
        // The tail is read once, here, and the size taken from it if the format records one.
        let tail = read_tail(&source, length)?;
        let size = recorded_size(compression, &tail);
        if let Some(size) = size {
            options.limits.check_entry_size(Some(size))?;
        }
        // gzip alone records the whole file's CRC-32 and length in its footer. The codec checks
        // them only when the stream is read to the very end, but an entry reader that has been
        // handed a declared size stops at that size and never touches the footer — which is how a
        // flipped byte inside the data could stream back as the right length of wrong bytes with
        // no error. The reader checks the footer itself over the bytes as they stream past, on
        // the same footing as the ZIP reader checks its central directory, so a gzip whose data
        // no longer matches its footer is refused once the entry is read to its end.
        let expected = recorded_crc(compression, &tail);
        let entry = Entry {
            name: normalise_name(SINGLE_ENTRY_NAME),
            kind: EntryKind::File,
            size,
            compressed_size: Some(length),
            last_modified: modification_time(compression, &tail),
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: None,
        };
        Ok(Box::new(SingleStreamReader {
            source,
            compression,
            entries: vec![entry],
            limits: options.limits.clone(),
            expected,
        }))
    }
}

/// The whole-file CRC a stream's footer records, so the reader can check it.
///
/// gzip is the one single-stream format here that records a CRC over the whole output; bzip2, xz,
/// zstd, lz4 and brotli check their own integrity inside the decoder as the bytes stream past, so
/// they have a checked `None` here rather than a second lazy promise.
fn recorded_crc(compression: Compression, tail: &[u8]) -> Option<u32> {
    match compression {
        // The last eight bytes are CRC-32 followed by ISIZE. A NUL CRC is not a real one for any
        // non-trivial contents, so a damaged footer refuses rather than silently passing.
        Compression::Gzip => u32_at(tail, tail.len().checked_sub(8)?).filter(|crc| *crc != 0),
        _ => None,
    }
}

impl<R: Read + Seek + Send + 'static> EntrySource<R> for SingleStreamReader<R> {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn open(&mut self, _index: usize) -> Result<EntryStream> {
        let entry = self
            .entries
            .first()
            .ok_or_else(|| Error::malformed("a single-stream archive has no entry"))?;
        if let Some(size) = entry.size {
            self.limits.check_entry_size(Some(size))?;
        }
        // The whole file, from the beginning, is the entry's compressed data. There is no offset
        // to compute and no length to honour, because the decompressor stops at the stream's end.
        let raw = crate::reader::RawReader::new(self.source.clone(), 0, None);
        let decoded = compression::decode(self.compression, Box::new(raw))?;
        Ok(EntryStream::new(Box::new(SingleEntry {
            inner: decoded,
            expected: entry.size,
            produced: 0,
            expected_crc: self.expected,
            hasher: crc32fast::Hasher::new(),
            checked: false,
        })))
    }

    fn skip(&mut self, _index: usize) -> Result<u64> {
        let length = self
            .entries
            .first()
            .and_then(|entry| entry.compressed_size)
            .unwrap_or_default();
        // Nothing is read; the whole file is the entry, so skipping it is a seek to the end.
        let mut guard = crate::reader::lock(&self.source)?;
        guard.seek(SeekFrom::End(0))?;
        Ok(length)
    }

    fn reuses_blocks(&self) -> bool {
        false
    }

    fn into_source(self: Box<Self>) -> Result<R> {
        self.source.into_inner()
    }
}

/// The one entry of a single-stream file, decompressed.
pub struct SingleEntry {
    inner: Box<dyn Read + Send>,
    expected: Option<u64>,
    produced: u64,
    /// The CRC-32 the footer promises, when the stream records one. Only gzip does.
    expected_crc: Option<u32>,
    /// The CRC of everything produced so far, updated on every read.
    hasher: crc32fast::Hasher,
    checked: bool,
}

impl SingleEntry {
    /// Ends the entry: refuses a short stream, then compares the footer's CRC.
    ///
    /// Called exactly once, when the codec reports end-of-stream. Without it a gzip whose data
    /// byte was flipped reads back as the right number of wrong bytes with no error, because the
    /// codec only checks its footer if the stream is drained to the very end. Reported as
    /// [`ErrorKind::Malformed`](crate::error::ErrorKind::Malformed) with a message naming the
    /// promised and the actual CRC.
    fn finish(&mut self) -> std::io::Result<usize> {
        self.checked = true;
        if let Some(expected) = self.expected {
            if self.produced < expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the gzip stream is truncated: {} bytes were read but the footer says {}",
                        self.produced, expected
                    ),
                ));
            }
        }
        if let Some(expected) = self.expected_crc {
            let actual = self.hasher.clone().finalize();
            if actual != expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "gzip stream CRC is {actual:#010x} but the footer says {expected:#010x}"
                    ),
                ));
            }
        }
        Ok(0)
    }

    /// Ends the entry at the declared size, before the codec reports end-of-stream.
    ///
    /// gzip's footer records the *last* member's size and CRC, so a stream that decodes more than
    /// the declared size is a concatenation of members whose footer describes only its last one,
    /// and comparing it against the whole output would refuse a healthy file. A single member is
    /// told apart from many by draining the codec to its end and counting the extra bytes: a
    /// single member has already produced everything its footer describes, so nothing follows,
    /// while further members add data.
    ///
    /// The drain is not optional bookkeeping. A damaged member is only refused when the codec
    /// reaches that member's footer, and the footer can sit arbitrarily far past the declared
    /// size — a flipped byte can make the stream decode *longer* than it promised, so stopping
    /// after one extra byte would miss it. Draining to the end is what lets the codec verify every
    /// member's footer, and its error is the one propagated here.
    fn finish_at_declared_size(&mut self) -> std::io::Result<usize> {
        let mut extra: u64 = 0;
        let mut scratch = [0u8; 8192];
        loop {
            match self.inner.read(&mut scratch) {
                Ok(0) => break,
                Ok(read) => extra += read as u64,
                Err(error) => return Err(error),
            }
        }
        if extra == 0 {
            // Exactly one member, and the codec has just consumed and checked its footer.
            return self.finish();
        }
        // A concatenation of members: the footer describes only the last, so the whole-output CRC
        // is not the footer's to compare, but the codec checked each member's own footer in the
        // drain above. A multi-member stream still stops at the declared size, as it always has.
        self.checked = true;
        Ok(0)
    }
}

impl Read for SingleEntry {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.checked {
            return Ok(0);
        }
        // A format that records its size is never allowed to produce more than it says. Trailing
        // junk after a stream is common and would otherwise be handed to the caller as file
        // contents, which is how a truncated download becomes a file with a tail of rubbish in it.
        let wanted = match self.expected {
            Some(expected) => {
                let left = expected.saturating_sub(self.produced);
                if left == 0 {
                    return self.finish_at_declared_size();
                }
                output.len().min(left as usize)
            }
            None => output.len(),
        };
        let read = self.inner.read(&mut output[..wanted])?;
        if read == 0 {
            return self.finish();
        }
        // The CRC is over the bytes produced, not the bytes read from the file, so a stored byte
        // and a decompressed byte are checked on the same footing.
        self.hasher.update(&output[..read]);
        self.produced += read as u64;
        Ok(read)
    }
}

impl EntryRead for SingleEntry {
    fn size(&self) -> Option<u64> {
        self.expected
    }
}

/// The uncompressed size, for the formats that record one in their footer.
///
/// Gzip's ISIZE is the size modulo 2^32, so it is only believed when the stream is small enough
/// for that not to have wrapped — which is checked by the caller against the file's own length.
/// bzip2 records no size at all, and zstd's frame header usually carries the content size only
/// when the writer was asked to include it, so both report `None` here rather than a wrong number.
fn recorded_size(compression: Compression, tail: &[u8]) -> Option<u64> {
    match compression {
        Compression::Gzip => {
            // The last four bytes are ISIZE: the uncompressed size, little-endian, modulo 2^32.
            let size = u32_at(tail, tail.len().checked_sub(4)?)? as u64;
            // A wrapped size would be a number near 4 GiB for a file that is plainly smaller.
            // Refusing it is better than reporting a length nobody can use.
            if size == 0 {
                None
            } else {
                Some(size)
            }
        }
        // LZMA's alone-format footer carries the size, but LZMA is not a single-stream format this
        // module opens; it is a codec inside 7Z and inside ZIP.
        _ => None,
    }
}

/// The modification time, for the formats that record one.
///
/// Gzip's MTIME is a Unix timestamp in its header, which is at the *front*. Reading it here would
/// mean a second read of the head of the file for a value the app can live without, so it is left
/// alone rather than costing a seek per listing.
fn modification_time(_compression: Compression, _tail: &[u8]) -> Option<std::time::SystemTime> {
    None
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Checks a `.Z` stream's header eagerly, so a bad one fails at open.
///
/// Reads the first three bytes without moving the source: entry reads start from the beginning
/// and the decoder reads the header again itself.
fn check_z_header<R: Read + Seek>(source: &Shared<R>) -> Result<()> {
    let mut guard = crate::reader::lock(source)?;
    let current = guard.stream_position()?;
    guard.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 3];
    let result = guard.read_exact(&mut header).map_err(Error::io);
    // The position is restored whatever happened, so a failed open leaves the source where the
    // caller had it rather than wherever the failure happened.
    let _ = guard.seek(SeekFrom::Start(current));
    result?;
    if header[0..2] != [0x1F, 0x9D] {
        return Err(Error::malformed(
            "not a .Z stream: the magic is not 0x1f 0x9d",
        ));
    }
    let max_bits = header[2] & 0x1F;
    if !(9..=16).contains(&max_bits) {
        return Err(Error::malformed(format!(
            "a .Z stream with a code width of {max_bits} bits is not a compress stream"
        )));
    }
    Ok(())
}

/// How long the file is.
fn file_length<R: Read + Seek>(source: &Shared<R>) -> Result<u64> {
    let mut guard = crate::reader::lock(source)?;
    let current = guard.stream_position()?;
    let length = guard.seek(SeekFrom::End(0))?;
    guard.seek(SeekFrom::Start(current))?;
    Ok(length)
}

/// Reads up to the last [`TAIL_SIZE`] bytes.
fn read_tail<R: Read + Seek>(source: &Shared<R>, length: u64) -> Result<Vec<u8>> {
    let size = TAIL_SIZE.min(length);
    let start = length - size;
    let mut bytes = vec![0u8; size as usize];
    let mut guard = crate::reader::lock(source)?;
    guard.seek(SeekFrom::Start(start))?;
    guard.read_exact(&mut bytes).map_err(Error::io)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::ArchiveReader;
    use std::io::{Cursor, Write};

    #[test]
    fn the_signatures_are_the_ones_each_format_defines() {
        assert_eq!(magic(Compression::Gzip), Some(&[0x1F, 0x8B, 0x08][..]));
        assert_eq!(magic(Compression::Bzip2), Some(&b"BZh"[..]));
        assert_eq!(
            magic(Compression::Zstd),
            Some(&[0x28, 0xB5, 0x2F, 0xFD][..])
        );
        assert_eq!(magic(Compression::Lz4), Some(&[0x04, 0x22, 0x4D, 0x18][..]));
        assert_eq!(
            magic(Compression::Xz),
            Some(&[0xFD, b'7', b'z', b'X', b'Z', 0x00][..])
        );
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

    /// Compresses `payload` with `method` and returns the bytes.
    fn compress(method: Compression, payload: &[u8]) -> Vec<u8> {
        let sink = SharedCell::default();
        {
            let mut encoder = compression::encode(method, Box::new(sink.clone()), 6)
                .expect("this method has an encoder");
            encoder.write_all(payload).expect("write");
            encoder.finish().expect("finish");
        }
        sink.take()
    }

    /// A `Write` that appends to a shared buffer, so a test can compress without a file.
    #[derive(Clone, Default)]
    struct SharedCell(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedCell {
        fn take(&self) -> Vec<u8> {
            self.0
                .lock()
                .map(|mut inner| std::mem::take(&mut *inner))
                .unwrap_or_default()
        }
    }

    impl std::io::Write for SharedCell {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            match self.0.lock() {
                Ok(mut inner) => {
                    inner.extend_from_slice(buffer);
                    Ok(buffer.len())
                }
                Err(_) => Err(std::io::Error::other("the test's buffer was poisoned")),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn payload() -> Vec<u8> {
        (0..40_000u32)
            .map(|value| ((value / 13) % 256) as u8)
            .collect()
    }

    #[test]
    fn every_streamable_method_round_trips_through_one_entry() {
        for method in [Compression::Deflate, Compression::Bzip2, Compression::Zstd] {
            let bytes = payload();
            let compressed = compress(method, &bytes);
            let format = crate::format::Format::SingleStream(method);
            let mut reader =
                ArchiveReader::open_as(Cursor::new(compressed), format, &OpenOptions::default())
                    .unwrap_or_else(|error| panic!("{method} should open: {error}"));
            assert_eq!(reader.len(), 1, "{method} is one entry");
            let mut got = Vec::new();
            reader
                .open_entry(0)
                .unwrap_or_else(|error| panic!("{method} entry: {error}"))
                .read_to_end(&mut got)
                .unwrap_or_else(|error| panic!("{method} read: {error}"));
            assert_eq!(got, bytes, "{method} did not round trip");
        }
    }

    #[test]
    fn an_xz_stream_round_trips_through_one_entry() {
        // XZ is the one container here rather than a raw codec, and it is the format Android
        // itself uses, so it has to work in its own right and not only inside a TAR.
        let bytes = payload();
        let sink = SharedCell::default();
        {
            let mut encoder =
                lzma_rust2::XzWriter::new(Box::new(sink.clone()), lzma_rust2::XzOptions::default())
                    .expect("an xz writer");
            encoder.write_all(&bytes).expect("write");
            encoder.finish().expect("finish");
        }
        let compressed = sink.take();
        assert!(
            compressed.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]),
            "the fixture should be a real XZ stream"
        );

        let mut reader = ArchiveReader::open_as(
            Cursor::new(compressed),
            crate::format::Format::SingleStream(Compression::Xz),
            &OpenOptions::default(),
        )
        .expect("an xz stream opens");
        assert_eq!(reader.len(), 1);
        let mut got = Vec::new();
        reader
            .open_entry(0)
            .expect("open")
            .read_to_end(&mut got)
            .expect("read");
        assert_eq!(got, bytes, "xz did not round trip");
    }

    #[test]
    fn a_gzip_stream_reports_the_size_its_footer_records() {
        // Gzip is the one format here whose footer carries a length, and the app shows that
        // length next to the file's name, so it has to come out right.
        let bytes = payload();
        let compressed = compress(Compression::Deflate, &bytes);
        // Wrap it as gzip: deflate data with a gzip header and footer.
        let mut gzip = vec![0x1F, 0x8B, 0x08, 0, 0, 0, 0, 0, 0, 0xFF];
        gzip.extend_from_slice(&compressed);
        // A zero CRC is the format's "not recorded" sentinel, which this test leans on to check
        // the size alone. `a_gzip_whose_data_no_longer_matches_its_footer_is_refused` exercises a
        // real one.
        gzip.extend_from_slice(&0u32.to_le_bytes());
        gzip.extend_from_slice(&(bytes.len() as u32).to_le_bytes());

        let reader = ArchiveReader::open_as(
            Cursor::new(gzip),
            crate::format::Format::SingleStream(Compression::Gzip),
            &OpenOptions::default(),
        )
        .expect("a gzip stream opens");
        assert_eq!(
            reader.entry(0).and_then(|entry| entry.size),
            Some(bytes.len() as u64),
            "the size in the footer is the size of the entry"
        );
    }

    #[test]
    fn an_empty_file_is_refused_rather_than_listed_as_one_empty_entry() {
        // There is no such thing as an empty gzip, and treating a zero-byte file as one would make
        // every empty file on the device look like a broken archive.
        let error = match ArchiveReader::open_as(
            Cursor::new(Vec::new()),
            crate::format::Format::SingleStream(Compression::Gzip),
            &OpenOptions::default(),
        ) {
            Ok(_) => panic!("an empty file must not open as a compressed stream"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }

    #[test]
    fn a_truncated_stream_reads_what_it_can_and_then_stops() {
        // A half-downloaded `.gz` is ordinary. It must not hang, must not panic, and must not
        // produce more than the bytes that were actually there.
        let bytes = payload();
        let compressed = compress(Compression::Deflate, &bytes);
        let truncated = &compressed[..compressed.len() / 2];
        let format = crate::format::Format::SingleStream(Compression::Deflate);
        if let Ok(mut reader) = ArchiveReader::open_as(
            Cursor::new(truncated.to_vec()),
            format,
            &OpenOptions::default(),
        ) {
            let mut got = Vec::new();
            let _ = reader.open_entry(0).expect("open").read_to_end(&mut got);
            assert!(
                got.len() <= bytes.len(),
                "a truncated stream cannot produce more than the original"
            );
        }
    }

    #[test]
    fn a_recorded_size_of_zero_is_not_reported_as_the_size() {
        // A gzip stream of nothing, or a footer that was never written, must report no size rather
        // than a confident zero that the app would show as an empty file.
        assert_eq!(recorded_size(Compression::Gzip, &[0, 0, 0, 0]), None);
        assert_eq!(recorded_size(Compression::Gzip, &[0x10, 0, 0, 0]), Some(16));
        assert_eq!(recorded_size(Compression::Bzip2, &[0x10, 0, 0, 0]), None);
    }

    /// Wraps raw deflate bytes as a gzip member with the real CRC and ISIZE, so a test can damage
    /// the data while leaving the footer saying what it first promised.
    fn gzip(payload: &[u8]) -> Vec<u8> {
        let deflated = compress(Compression::Deflate, payload);
        let mut stream = vec![0x1F, 0x8B, 0x08, 0, 0, 0, 0, 0, 0, 0xFF];
        stream.extend_from_slice(&deflated);
        stream.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        stream.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        stream
    }

    /// Reads the single entry, mapping the stream's `io::Error` through this crate's error type so
    /// the kind is `ErrorKind::Malformed` and not a bare `io::ErrorKind`.
    fn read_single(bytes: Vec<u8>, method: Compression) -> Result<Vec<u8>, crate::error::Error> {
        let mut reader = ArchiveReader::open_as(
            Cursor::new(bytes),
            crate::format::Format::SingleStream(method),
            &OpenOptions::default(),
        )?;
        let mut stream = reader.open_entry(0)?;
        let mut got = Vec::new();
        stream
            .read_to_end(&mut got)
            .map_err(crate::error::Error::io)?;
        Ok(got)
    }

    /// A `.gz` whose data no longer matches its footer is refused, not read as wrong bytes.
    ///
    /// This is the single-stream half of the ZIP hole: the reader used to stop at the declared
    /// size and never drain the member, so the codec's own footer check could not run. The footer
    /// is now checked over the bytes as they stream past.
    #[test]
    fn a_gzip_whose_data_no_longer_matches_its_footer_is_refused() {
        let plain: Vec<u8> = b"the quick brown fox 0123456789\n".repeat(64);
        let good = gzip(&plain);
        assert_eq!(
            read_single(good.clone(), Compression::Gzip).expect("the intact stream reads"),
            plain
        );

        // The deflate data is everything between the 10-byte header and the 8-byte footer.
        let start = 10;
        let end = good.len() - 8;
        assert!(end > start + 8, "the fixture should hold real deflate data");
        for offset in start..end {
            let mut bad = good.clone();
            bad[offset] ^= 0xFF;
            match read_single(bad, Compression::Gzip) {
                Err(error) => assert_eq!(
                    error.kind(),
                    crate::error::ErrorKind::Malformed,
                    "a flipped byte at {offset} must read as Malformed, got {error}"
                ),
                Ok(got) => assert_eq!(
                    got, plain,
                    "flipping gzip byte {offset} changed the entry's contents with no error"
                ),
            }
        }
    }
}
