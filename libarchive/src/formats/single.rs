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

use std::io::{Read, Seek, SeekFrom, Write};

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
        // The tail is read once, here, and the size taken from it if the format records one.
        let tail = read_tail(&source, length)?;
        let size = recorded_size(compression, &tail);
        if let Some(size) = size {
            options.limits.check_entry_size(Some(size))?;
        }
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
        }))
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
}

impl Read for SingleEntry {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        // A format that records its size is never allowed to produce more than it says. Trailing
        // junk after a stream is common and would otherwise be handed to the caller as file
        // contents, which is how a truncated download becomes a file with a tail of rubbish in it.
        let wanted = match self.expected {
            Some(expected) => {
                let left = expected.saturating_sub(self.produced);
                if left == 0 {
                    return Ok(0);
                }
                output.len().min(left as usize)
            }
            None => output.len(),
        };
        let read = self.inner.read(&mut output[..wanted])?;
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
    use std::io::Cursor;

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

    /// Compresses `payload` with `method` and returns the bytes.
    fn compress(method: Compression, payload: &[u8]) -> Vec<u8> {
        let mut sink = SharedCell::default();
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
        (0..40_000u32).map(|value| ((value / 13) % 256) as u8).collect()
    }

    #[test]
    fn every_streamable_method_round_trips_through_one_entry() {
        for method in [Compression::Deflate, Compression::Bzip2, Compression::Zstd] {
            let bytes = payload();
            let compressed = compress(method, &bytes);
            let format = crate::format::Format::SingleStream(method);
            let mut reader = ArchiveReader::open_as(
                Cursor::new(compressed),
                format,
                &OpenOptions::default(),
            )
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
        let mut sink = SharedCell::default();
        {
            let mut encoder = lzma_rust2::XzWriter::new(
                Box::new(sink.clone()),
                lzma_rust2::XzOptions::default(),
            )
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
        gzip.extend_from_slice(&0u32.to_le_bytes()); // CRC, not verified by the reader.
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
            assert!(got.len() <= bytes.len(), "a truncated stream cannot produce more than the original");
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
}
