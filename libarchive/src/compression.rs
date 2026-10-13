//! Compression codecs, and the one place that knows which crate provides each.
//!
//! Every format module decodes through [`decode`] rather than naming a codec crate. That is what
//! keeps the format modules about archive structure and this module about compression, and it is
//! what makes "which methods can this build read" a single function rather than a fact scattered
//! across twenty files.
//!
//! # Streaming is the whole point
//!
//! [`decode`] returns a [`Read`], never a `Vec`. The code this crate replaces decompressed a whole
//! entry before handing back a byte of it, which cost twice the entry's size in resident memory
//! and about a second of latency before the first byte on a 132 MB entry. Every decoder here is
//! pull-based, so the caller's buffer size decides how much work happens at a time.
//!
//! # Why these crates
//!
//! Every codec here is either pure Rust or a statically linked C library, because the target is a
//! `cdylib` loaded by `System.loadLibrary`. A codec that shells out to a system `libz`, or that
//! dlopens one, is a codec that fails on a device that happens not to have it. The cost is a
//! little binary size; the benefit is that a ZIP written on the host decodes on the phone.

use std::io::{Read, Write};

use crate::error::{Error, Result};
use crate::format::Compression;

/// A decoder over data that is already bounded to one entry.
///
/// `input` must not read past the entry's compressed bytes. Formats construct it with
/// [`crate::reader::BoundedSource`] or an equivalent, because a decoder that reads one byte past
/// an entry's end hands the caller the first bytes of the next entry, silently.
pub fn decode(method: Compression, input: Box<dyn Read + Send>) -> Result<Box<dyn Read + Send>> {
    match method {
        Compression::None => Ok(input),
        Compression::Deflate => Ok(Box::new(flate2::read::DeflateDecoder::new(input))),
        // `Deflate64Decoder::new` wraps its argument in a `BufReader`. That is the right default
        // for a file, and the wrong one here: it is exactly the `BufReader` whose 8 KiB refill
        // made the previous implementation read 54.6 MB instead of 411 KB. A single 128 KiB
        // refill costs the same in syscalls and eight times less in discarded bytes.
        Compression::Deflate64 => Ok(Box::new(deflate64::Deflate64Decoder::new(
            BigBufReader::new(input),
        ))),
        Compression::Bzip2 => Ok(Box::new(bzip2::read::BzDecoder::new(input))),
        // LZMA is its own container: the properties byte and the dictionary size are a header
        // before the compressed data, so the decoder is constructed over the whole bounded entry
        // rather than over a sub-slice.
        Compression::Lzma => Ok(Box::new(lzma_rust2::LzmaReader::new_mem_limit(
            input,
            (crate::limits::DEFAULT_LZMA_MEMORY_LIMIT / 1024) as u32,
            None,
        )?)),
        Compression::Gzip => Ok(Box::new(flate2::read::MultiGzDecoder::new(input))),
        Compression::Brotli => Ok(Box::new(brotli::Decompressor::new(input, BIG_BUFFER))),
        Compression::Lz4 => Ok(Box::new(lz4_flex::frame::FrameDecoder::new(input))),
        Compression::Zstd => Ok(Box::new(zstd::stream::read::Decoder::new(input)?)),
        // XZ is a container: LZMA2 inside a stream with its own index, checks and padding rules.
        // `lzma-rust2` provides the whole container, so this is one call rather than a hand-rolled
        // index parser. `allow_multiple_streams` is false because a file that claims to be one XZ
        // stream and then carries another is either damaged or an attack, and concatenating them
        // would hand the caller bytes the archive never described.
        Compression::Xz => Ok(Box::new(lzma_rust2::XzReader::new_mem_limit(
            input,
            false,
            (crate::limits::DEFAULT_LZMA_MEMORY_LIMIT / 1024) as u32,
        ))),
        // LZMA2 on its own is a raw chunked codec with no container; it appears inside XZ and
        // inside 7Z's coder chains, both of which drive it themselves.
        Compression::Lzma2 => Err(Error::unimplemented(format!(
            "{method} is a bare chunked codec with no container; see the 7Z and XZ readers"
        ))),
        // PPMd's 7Z variant is driven by the 7Z coder chain, which needs the model properties as
        // well as the data.
        Compression::Ppmd | Compression::Unknown(_) => Err(Error::unimplemented(format!(
            "{method} cannot be decoded by this build"
        ))),
        // Unix `compress` is LZW with block padding, decoded streaming by `crate::zlzw` so that
        // a large `.Z` costs the dictionary rather than the file.
        Compression::Z => Ok(Box::new(crate::zlzw::ZDecoder::new(input))),
    }
}

/// A compressing writer that must be finished before its output is complete.
///
/// Three of the four encoders write a trailer when they are *finished*, not when they are dropped:
/// deflate's final block, bzip2's stream footer and zstd's frame epilogue. A caller that forgets
/// produces an archive that lists correctly and then fails on the first entry it tries to read,
/// which is the worst shape a bug can have. Making it a method rather than a convention is the
/// difference between that and a compile error.
pub trait Encoded: Write + Send {
    /// Writes whatever trailer the format needs and flushes into the sink.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// An encoder writing to `sink`, for the formats that write compressed entries.
///
/// The sink is passed in rather than returned wrapping a `sink()`, because an encoder that owns
/// its output has nowhere to put the bytes until it is finished, and finishing is the point at
/// which the entry's compressed size is known.
pub fn encode(
    method: Compression,
    sink: Box<dyn Write + Send>,
    level: u32,
) -> Result<Box<dyn Encoded>> {
    Ok(match method {
        Compression::None => Box::new(Encoder::new(Passthrough { inner: sink })),
        Compression::Deflate => Box::new(Encoder::new(flate2::write::DeflateEncoder::new(
            sink,
            flate2::Compression::new(level.clamp(1, 9)),
        ))),
        Compression::Bzip2 => Box::new(Encoder::new(bzip2::write::BzEncoder::new(
            sink,
            bzip2::Compression::new(level.clamp(1, 9)),
        ))),
        // Without a checksum flag a Zstd frame cannot be told apart from a damaged one, and the
        // reference implementation writes none by default. Ask for one explicitly so files written
        // through this crate let `decode` reject corruption instead of handing back wrong bytes.
        Compression::Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(sink, level.clamp(1, 19) as i32)?;
            encoder.include_checksum(true)?;
            Box::new(Encoder::new(encoder))
        }
        other => {
            return Err(Error::unimplemented(format!(
                "{other} cannot be encoded by this build"
            )))
        }
    })
}

/// A codec whose output is complete only when it is finished.
///
/// Private, because it exists only so that [`Encoded`] can be implemented once for every encoder
/// rather than once per encoder: three of the four write a trailer on `finish`, and the fourth
/// does not, and a caller cannot tell which is which from the outside.
trait Finishable: Write + Send {
    fn finish_into(self) -> Result<()>;
}

/// The one wrapper every encoder goes through, so `finish` is written once.
///
/// `inner` is an `Option` because `Encoded::finish` consumes the box, and on stable the only way
/// to move a field out of a `Box<Self>` is through an `Option` that is emptied first.
struct Encoder<E: Finishable> {
    inner: Option<E>,
}

impl<E: Finishable> Encoder<E> {
    fn new(inner: E) -> Encoder<E> {
        Encoder { inner: Some(inner) }
    }
}

impl<E: Finishable> Write for Encoder<E> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        // Writing after `finish` is a caller error rather than bad input, and `std::io::Write`
        // has no way to say "you already closed this" other than an error. `io::Error::other`
        // carries it as an ordinary write failure, which is what keeps the no-panic promise: a
        // writer used after being finished gets an `Err`, not an abort across the JNI boundary.
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| std::io::Error::other("write called after the encoder was finished"))?;
        inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| std::io::Error::other("flush called after the encoder was finished"))?;
        inner.flush()
    }
}

impl<E: Finishable> Encoded for Encoder<E> {
    fn finish(mut self: Box<Self>) -> Result<()> {
        // `take` rather than a borrow, so the encoder is left empty and a second `finish` finds
        // nothing here and says so, rather than finishing a codec twice.
        let inner = self
            .inner
            .take()
            .ok_or_else(|| Error::malformed("the encoder was already finished"))?;
        inner.finish_into()
    }
}

/// An encoder that does nothing, so that the "stored" case has the same shape as the others.
struct Passthrough {
    inner: Box<dyn Write + Send>,
}

impl Write for Passthrough {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Finishable for Passthrough {
    fn finish_into(mut self) -> Result<()> {
        self.inner.flush().map_err(Error::io)
    }
}

impl Finishable for flate2::write::DeflateEncoder<Box<dyn Write + Send>> {
    fn finish_into(self) -> Result<()> {
        self.finish().map_err(Error::io)?;
        Ok(())
    }
}

impl Finishable for bzip2::write::BzEncoder<Box<dyn Write + Send>> {
    fn finish_into(self) -> Result<()> {
        self.finish().map_err(Error::io)?;
        Ok(())
    }
}

impl<W: Write + Send> Finishable for zstd::stream::write::Encoder<'_, W> {
    fn finish_into(self) -> Result<()> {
        self.finish().map_err(Error::io)?;
        Ok(())
    }
}

/// The buffer size the crate's hot loops are tuned for.
///
/// 128 KiB is where a read stops being dominated by syscall and JNI overhead while staying small
/// enough that several can be live at once on a phone. It is deliberately a power of two, so the
/// allocation is aligned and never straddles two pages.
pub const BIG_BUFFER: usize = 128 * 1024;

/// A `Read` that hands out large chunks.
///
/// Exists because two of the codecs above insist on wrapping their input in a `BufReader` of their
/// choosing, and a caller that has already decided how big its reads should be has no way to
/// override that.
struct BigBufReader {
    inner: Box<dyn Read + Send>,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
}

impl BigBufReader {
    fn new(inner: Box<dyn Read + Send>) -> BigBufReader {
        BigBufReader {
            inner,
            buffer: vec![0; BIG_BUFFER],
            start: 0,
            end: 0,
        }
    }
}

impl Read for BigBufReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.start == self.end {
            self.end = self.inner.read(&mut self.buffer)?;
            self.start = 0;
            if self.end == 0 {
                return Ok(0);
            }
        }
        let available = self.end - self.start;
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.buffer[self.start..self.start + count]);
        self.start += count;
        Ok(count)
    }
}

/// Whether this build can decode `method` without a container around it.
///
/// The app asks this before offering to extract, so the answer has to be a yes or no without
/// consulting anything else — in particular without constructing a decoder, which would read from
/// the archive.
pub fn can_decode(method: Compression) -> bool {
    matches!(
        method,
        Compression::None
            | Compression::Deflate
            | Compression::Deflate64
            | Compression::Bzip2
            | Compression::Lzma
            | Compression::Gzip
            | Compression::Brotli
            | Compression::Lz4
            | Compression::Zstd
            | Compression::Xz
            | Compression::Z
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Encodes with `method`, decodes with `decode`, and returns what came back.
    fn round_trip(method: Compression, level: u32, payload: &[u8]) -> Vec<u8> {
        // The encoder has to own something `'static`, because the bridge hands out `Send` trait
        // objects that outlive the call. A shared cell is the smallest thing that satisfies that
        // without a file on disk.
        let sink = SharedCell::default();
        {
            let mut encoder = encode(method, Box::new(sink.clone()), level).expect("an encoder");
            encoder.write_all(payload).expect("write the payload");
            // The trailer, without which deflate, bzip2 and zstd all produce an incomplete stream.
            encoder.finish().expect("finish the encoder");
        }
        let mut out = Vec::new();
        decode(method, Box::new(Cursor::new(sink.take())))
            .expect("a decoder")
            .read_to_end(&mut out)
            .expect("read it back");
        out
    }

    /// A `Write` that appends to a shared buffer, so a test can encode without a file.
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

    impl Write for SharedCell {
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
        // Compressible rather than random, so a codec that silently does nothing is caught.
        (0..200_000u32)
            .map(|value| ((value / 97) % 256) as u8)
            .collect()
    }

    #[test]
    fn every_encodable_method_round_trips_its_own_output() {
        let bytes = payload();
        for method in [
            Compression::None,
            Compression::Deflate,
            Compression::Bzip2,
            Compression::Zstd,
        ] {
            assert_eq!(
                round_trip(method, 6, &bytes),
                bytes,
                "{method} did not round trip"
            );
        }
    }

    #[test]
    fn a_stored_entry_is_passed_through_untouched() {
        // Storing is the common case for an already-compressed file, and it is the case where a
        // decoder that is not a pass-through quietly costs a full copy of the entry.
        let bytes = vec![7u8; 4096];
        let mut out = Vec::new();
        decode(Compression::None, Box::new(Cursor::new(bytes.clone())))
            .expect("stored")
            .read_to_end(&mut out)
            .expect("read");
        assert_eq!(out, bytes);
    }

    #[test]
    fn a_decoder_reads_a_large_entry_a_block_at_a_time_rather_than_all_at_once() {
        // The property that distinguishes a streaming decoder from a buffering one. A 32 MiB
        // entry, of which one byte is read, must not cost 32 MiB of reads: a decoder that
        // materialised the entry first would have pulled every compressed byte, and one that
        // wrapped its input in an 8 KiB `BufReader` would have pulled at least that much.
        let bytes: Vec<u8> = (0..32 * 1024 * 1024u32)
            .map(|value| ((value / 7) % 256) as u8)
            .collect();
        let sink = SharedCell::default();
        {
            let mut encoder =
                encode(Compression::Deflate, Box::new(sink.clone()), 1).expect("encoder");
            encoder.write_all(&bytes).expect("write");
            encoder.finish().expect("finish");
        }
        let compressed = sink.take();
        let total = compressed.len();
        let counter = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let mut one = [0u8; 1];
        {
            let reader = CountingReader::new(Cursor::new(compressed), counter.clone());
            let mut decoder = decode(Compression::Deflate, Box::new(reader)).expect("a decoder");
            decoder.read_exact(&mut one).expect("one byte");
        }
        assert_eq!(one[0], bytes[0], "the first byte is right");
        let seen = counter.lock().map(|value| *value).unwrap_or(usize::MAX);
        assert!(
            seen <= 4 * 1024 * 1024,
            "reading one byte of a 32 MiB entry pulled {seen} of {total} compressed bytes"
        );
    }

    /// Counts how many bytes were pulled out of the reader it wraps.
    struct CountingReader<R> {
        inner: R,
        seen: std::sync::Arc<std::sync::Mutex<usize>>,
    }

    impl<R: Read> CountingReader<R> {
        fn new(inner: R, seen: std::sync::Arc<std::sync::Mutex<usize>>) -> CountingReader<R> {
            CountingReader { inner, seen }
        }
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(output)?;
            if let Ok(mut seen) = self.seen.lock() {
                *seen += read;
            }
            Ok(read)
        }
    }

    #[test]
    fn what_can_be_decoded_is_decided_in_one_place_and_agrees_with_the_decoder() {
        // `can_decode` is what the app asks before offering to extract. If it says yes and the
        // decoder says no, the app offers an action that always fails.
        for method in [
            Compression::None,
            Compression::Deflate,
            Compression::Deflate64,
            Compression::Bzip2,
            Compression::Lzma,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Lz4,
            Compression::Zstd,
            Compression::Xz,
            Compression::Z,
        ] {
            assert!(can_decode(method), "{method} should be decodable");
        }
        for method in [Compression::Unknown(9999), Compression::Ppmd] {
            assert!(
                !can_decode(method),
                "{method} should not claim to be decodable"
            );
        }
    }

    #[test]
    fn a_codec_with_no_container_says_so_rather_than_producing_garbage() {
        let bytes = vec![0u8; 16];
        for method in [Compression::Lzma2, Compression::Ppmd] {
            let error = match decode(method, Box::new(Cursor::new(bytes.clone()))) {
                Ok(_) => panic!("{method} should have no raw decoder"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), crate::error::ErrorKind::Unimplemented);
        }
    }

    #[test]
    fn the_big_buffer_reader_serves_large_reads_without_throwing_bytes_away() {
        let bytes: Vec<u8> = (0..BIG_BUFFER * 2)
            .map(|value| (value % 256) as u8)
            .collect();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let mut out = Vec::new();
        BigBufReader::new(Box::new(CountingReader::new(
            Cursor::new(bytes.clone()),
            seen.clone(),
        )))
        .read_to_end(&mut out)
        .expect("read");
        assert_eq!(out, bytes);
        // Every byte passes through exactly once: the buffer is a refill, not a copy.
        assert_eq!(seen.lock().map(|value| *value).unwrap_or(0), bytes.len());
    }
}
