//! Writing the single-stream compressors.
//!
//! **Owned by Team `codecs`.** The reader is in [`crate::formats::single`], and the compressor
//! in front of a TAR — [`crate::writer::tar::CompressedTarWriter`] — encodes through
//! [`StreamEncoder`], because the codec choice is this team's work whichever container it sits in.
//!
//! # A single stream is one entry, and the entry has no name
//!
//! A `.gz`, `.bz2`, `.xz`, `.zst`, `.lz4` or `.br` file is not an archive. It is one compressed
//! stream, and the whole file is one entry. Two things follow, and both are load-bearing for the
//! implementation below:
//!
//! * The entry's name, kind and timestamp are recorded nowhere: the formats have no header for
//!   them. [`SingleStreamWriter::begin`] writes the bytes of a file entry and ignores the rest,
//!   and anything that is not a file is refused rather than silently flattened into bytes. The
//!   reader lists the result as one file called `content` (see
//!   [`crate::formats::single::SINGLE_ENTRY_NAME`]); a caller that needs names writes a TAR.
//! * There is exactly one entry. A second `begin` is refused rather than concatenated, because
//!   concatenation means different things per format — a second gzip member decodes, while a
//!   second XZ stream does not under this crate's reader — and a writer that silently produces a
//!   file its own reader cannot fully read is worse than one that says no.
//!
//! # The encoder is owned, not borrowed
//!
//! Every compressor here owns its sink and is finished explicitly, because each of them writes a
//! trailer on finish: deflate's final block, gzip's CRC and length, bzip2's footer, the XZ index
//! and stream footer, the zstd frame epilogue, the LZ4 end mark, the brotli final block. The sink
//! comes back out of [`StreamEncoder::finish`], so [`EntryWriter::into_sink`] on a writer that was
//! never finished finalises best-effort first — the same backstop
//! [`crate::writer::sevenz::SevenZWriter`] keeps, for the same reason: the only public route
//! finishes first, and a caller holding the box directly would otherwise get a truncated stream.
//!
//! # Levels
//!
//! [`WriteOptions`] carries a method but no level, so every method compresses at an ordinary
//! default: deflate, gzip and bzip2 at 6, XZ at preset 6 with CRC64 (which is also what the `xz`
//! tool writes by default), zstd at 3, LZ4 at its default frame settings, brotli at quality 5.
//! Nothing here is tuned; a level knob is a later change, not a silent one.
//!
//! # What is refused rather than faked
//!
//! The methods with no encoder in this build — raw LZMA and LZMA2 (bare codecs with no container
//! here, driven by the XZ and 7Z paths instead), Deflate64 (read-only compat, nothing writes ZIP
//! method 9), PPMd (a 7Z-folder codec that needs its model properties), Unix `compress` (no LZW
//! encoder crate on this machine) — fail in `create` with `Unimplemented`, so a caller finds out
//! before writing a byte rather than after writing a file.

use std::io::{Seek, Write};

use crate::error::{Error, Result};
use crate::format::Compression;
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

/// The compressor a single-stream or compressed-TAR writer encodes through.
///
/// One enum rather than a trait object so the sink's type survives: every variant owns the same
/// `W`, and [`StreamEncoder::finish`] hands it back, which is what keeps the writers above from
/// buffering. A boxed `dyn Write` would erase `W` and leave `into_sink` nothing to return.
///
/// The `zstd` variant forces the `'static` bound: `zstd::stream::write::Encoder<'a, W>` carries a
/// lifetime for its optional dictionary, and with no dictionary any lifetime instantiates, so the
/// writers pick `'static`. Every sink the crate's writers accept is `'static` already
/// (`ArchiveWriter::create` requires it), so the bound rules nothing out.
pub(crate) enum StreamEncoder<W: Write + 'static> {
    Stored(W),
    Deflate(flate2::write::DeflateEncoder<W>),
    Gzip(flate2::write::GzEncoder<W>),
    Bzip2(bzip2::write::BzEncoder<W>),
    Xz(lzma_rust2::XzWriter<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
    Lz4(lz4_flex::frame::FrameEncoder<W>),
    Brotli(brotli::CompressorWriter<W>),
}

impl<W: Write + 'static> StreamEncoder<W> {
    /// Wraps `sink` in the compressor for `method`.
    ///
    /// Nothing is written yet — several of these encoders defer even their header until the first
    /// bytes arrive — so creating a writer that is never written to leaves no partial file.
    pub(crate) fn new(sink: W, method: Compression) -> Result<StreamEncoder<W>> {
        match method {
            Compression::None => Ok(StreamEncoder::Stored(sink)),
            Compression::Deflate => Ok(StreamEncoder::Deflate(flate2::write::DeflateEncoder::new(
                sink,
                flate2::Compression::new(6),
            ))),
            Compression::Gzip => Ok(StreamEncoder::Gzip(flate2::write::GzEncoder::new(
                sink,
                flate2::Compression::new(6),
            ))),
            Compression::Bzip2 => Ok(StreamEncoder::Bzip2(bzip2::write::BzEncoder::new(
                sink,
                bzip2::Compression::new(6),
            ))),
            Compression::Xz => {
                lzma_rust2::XzWriter::new(sink, lzma_rust2::XzOptions::with_preset(6))
                    .map(StreamEncoder::Xz)
                    .map_err(Error::io)
            }
            Compression::Zstd => zstd::stream::write::Encoder::new(sink, 3)
                .map(StreamEncoder::Zstd)
                .map_err(Error::io),
            Compression::Lz4 => Ok(StreamEncoder::Lz4(lz4_flex::frame::FrameEncoder::new(sink))),
            Compression::Brotli => Ok(StreamEncoder::Brotli(brotli::CompressorWriter::new(
                sink, 4096, 5, 22,
            ))),
            Compression::Deflate64
            | Compression::Lzma
            | Compression::Lzma2
            | Compression::Ppmd
            | Compression::Z
            | Compression::Unknown(_) => Err(Error::unimplemented(format!(
                "{method} cannot be encoded by this build"
            ))),
        }
    }

    /// Writes whatever trailer the format needs and gives the sink back.
    ///
    /// Without this the output is a truncated stream that lists correctly and then fails on read,
    /// which is why the writers call it from `EntryWriter::finish` rather than leaving it to drop:
    /// only brotli finalises on the way out (`into_inner` finishes the stream), and relying on
    /// that for one method while every other needs an explicit call is how a caller forgets.
    pub(crate) fn finish(self) -> Result<W> {
        match self {
            StreamEncoder::Stored(sink) => Ok(sink),
            StreamEncoder::Deflate(encoder) => encoder.finish().map_err(Error::io),
            StreamEncoder::Gzip(encoder) => encoder.finish().map_err(Error::io),
            StreamEncoder::Bzip2(encoder) => encoder.finish().map_err(Error::io),
            // With `std`, `lzma_rust2::Error` is `std::io::Error`, so there is nothing to map.
            StreamEncoder::Xz(encoder) => encoder.finish().map_err(Error::io),
            StreamEncoder::Zstd(encoder) => encoder.finish().map_err(Error::io),
            StreamEncoder::Lz4(encoder) => encoder.finish().map_err(std::io::Error::from).map_err(Error::io),
            // `into_inner` finishes the brotli stream on the way out, swallowing a final-flush
            // error: by then every content byte has already gone through `write`, which is where
            // a broken sink reports itself.
            StreamEncoder::Brotli(encoder) => Ok(encoder.into_inner()),
        }
    }
}

impl<W: Write + 'static> Write for StreamEncoder<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            StreamEncoder::Stored(sink) => sink.write(buffer),
            StreamEncoder::Deflate(encoder) => encoder.write(buffer),
            StreamEncoder::Gzip(encoder) => encoder.write(buffer),
            StreamEncoder::Bzip2(encoder) => encoder.write(buffer),
            StreamEncoder::Xz(encoder) => encoder.write(buffer),
            StreamEncoder::Zstd(encoder) => encoder.write(buffer),
            StreamEncoder::Lz4(encoder) => encoder.write(buffer),
            StreamEncoder::Brotli(encoder) => encoder.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            StreamEncoder::Stored(sink) => sink.flush(),
            StreamEncoder::Deflate(encoder) => encoder.flush(),
            StreamEncoder::Gzip(encoder) => encoder.flush(),
            StreamEncoder::Bzip2(encoder) => encoder.flush(),
            StreamEncoder::Xz(encoder) => encoder.flush(),
            StreamEncoder::Zstd(encoder) => encoder.flush(),
            StreamEncoder::Lz4(encoder) => encoder.flush(),
            StreamEncoder::Brotli(encoder) => encoder.flush(),
        }
    }
}

/// Writes a single-stream compressed file: GZ, BZ2, XZ, Zstd, LZ4, Brotli, raw deflate, or stored.
pub struct SingleStreamWriter<W: Write + Seek + 'static> {
    encoder: Option<StreamEncoder<W>>,
    sink: Option<W>,
    entries: u32,
    finished: bool,
}

impl<W: Write + Seek + 'static> SingleStreamWriter<W> {
    /// Creates a single-stream writer over `sink`.
    ///
    /// The method comes from `options`, which is how `formats::create` passes the `Format`'s own
    /// method through. A method with no encoder fails here, before a byte is written.
    pub fn create<W2: Write + Seek + 'static>(
        sink: W2,
        options: &WriteOptions,
    ) -> Result<Box<dyn EntryWriter<W2>>> {
        // None of these formats has anywhere to put a password: refusing is the difference
        // between "cannot encrypt" and an archive that silently drops the encryption.
        if options.password.is_some() {
            return Err(Error::unimplemented(
                "single-stream files cannot be encrypted by this build",
            ));
        }
        Ok(Box::new(SingleStreamWriter {
            encoder: Some(StreamEncoder::new(sink, options.compression)?),
            sink: None,
            entries: 0,
            finished: false,
        }))
    }
}

impl<W: Write + Seek + 'static> EntryWriter<W> for SingleStreamWriter<W> {
    fn begin<'a>(
        &'a mut self,
        entry: &NewEntry,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWrite + 'a>> {
        if self.finished {
            return Err(Error::malformed(
                "the stream has already been finished and takes no more entries",
            ));
        }
        // The counter only ever reaches one: the second entry is refused below, so no wraparound.
        if self.entries > 0 {
            return Err(Error::malformed(
                "a single-stream file holds one entry; a caller with more entries needs a TAR",
            ));
        }
        if entry.kind != crate::entry::EntryKind::File {
            return Err(Error::malformed(format!(
                "a single stream holds file bytes, not a {:?}; a caller with names needs a TAR",
                entry.kind
            )));
        }
        if self.encoder.is_none() {
            return Err(Error::malformed("the stream writer is closed"));
        }
        self.entries += 1;
        Ok(Box::new(SingleEntrySink { writer: self, written: 0, closed: false }))
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        match self.encoder.take() {
            Some(encoder) => {
                let sink = encoder.finish()?;
                self.sink = Some(sink);
                Ok(())
            }
            // Taken without being finished only by `into_sink`'s backstop, which finishes on the
            // spot; through the public API the encoder is always here.
            None => Ok(()),
        }
    }

    fn into_sink(self: Box<Self>) -> W {
        let mut owned = *self;
        // `ArchiveWriter::finish_into` — the only public route — finishes first, so the sink is
        // already back. A caller holding the box directly gets best-effort finalization rather
        // than a truncated stream, mirroring `SevenZWriter::into_sink`, whose module documents
        // why this cannot be total: a failed finalize consumes the sink with the encoder, and the
        // trait signature has nowhere to put the error.
        if owned.sink.is_none() {
            if let Some(encoder) = owned.encoder.take() {
                if let Ok(sink) = encoder.finish() {
                    owned.sink = Some(sink);
                }
            }
        }
        // Reachable only when finalization failed and took the sink with it, which the public
        // route reports as `Err` from `finish` before ever getting here.
        // The exception lib.rs allows: unreachable, and `into_sink` returns `W` so there is no
        // error to return. The sink is taken only by `finish`, and a caller holding this handle
        // has already been given the `Err` that says so.
        #[allow(clippy::expect_used, reason = "unreachable: the sink is returned only after finish, whose failure is already reported")]
        owned.sink.expect("the stream was finished before its sink was returned")
    }
}

/// One entry's contents, streaming straight into the compressor.
struct SingleEntrySink<'a, W: Write + Seek + 'static> {
    writer: &'a mut SingleStreamWriter<W>,
    written: u64,
    closed: bool,
}

impl<W: Write + Seek + 'static> Write for SingleEntrySink<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.closed {
            return Err(std::io::Error::other("this entry has already been finished"));
        }
        match self.writer.encoder.as_mut() {
            Some(encoder) => {
                let wrote = encoder.write(buffer)?;
                self.written += wrote as u64;
                Ok(wrote)
            }
            None => Err(std::io::Error::other("the stream writer is closed")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.writer.encoder.as_mut() {
            Some(encoder) => encoder.flush(),
            None => Err(std::io::Error::other("the stream writer is closed")),
        }
    }
}

impl<W: Write + Seek + 'static> EntryWrite for SingleEntrySink<'_, W> {
    fn written(&self) -> u64 {
        self.written
    }

    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        // Pushed through but not closed off: the compressor's trailer is the archive's, and it is
        // written by `EntryWriter::finish`, not here.
        match self.writer.encoder.as_mut() {
            Some(encoder) => encoder.flush().map_err(Error::io),
            None => Err(Error::malformed("the stream writer is closed")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Format;
    use crate::reader::{ArchiveReader, OpenOptions};
    use crate::writer::ArchiveWriter;
    use std::io::{Cursor, Read};

    fn payload() -> Vec<u8> {
        (0..100_000u32).map(|value| ((value / 13) % 256) as u8).collect()
    }

    /// Writes `payload` as `method` and reads it back, both through this crate.
    fn check_round_trip(method: Compression) {
        let bytes = payload();
        let options = WriteOptions::default().with_compression(method);
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::SingleStream(method), &options)
            .unwrap_or_else(|error| panic!("{method} should create a writer: {error}"));
        writer
            .write_entry_from_slice(&NewEntry::file("whatever.txt"), &bytes)
            .unwrap_or_else(|error| panic!("{method} should write: {error}"));
        let compressed = writer.finish_into().expect("finish").into_inner();
        assert!(!compressed.is_empty(), "{method} wrote nothing");

        let mut reader = ArchiveReader::open_as(
            Cursor::new(compressed),
            Format::SingleStream(method),
            &OpenOptions::default(),
        )
        .unwrap_or_else(|error| panic!("{method} should open what it wrote: {error}"));
        assert_eq!(reader.len(), 1, "{method} is one entry");
        let mut got = Vec::new();
        reader
            .open_entry(0)
            .unwrap_or_else(|error| panic!("{method} open entry: {error}"))
            .read_to_end(&mut got)
            .unwrap_or_else(|error| panic!("{method} read: {error}"));
        assert_eq!(got, bytes, "{method} did not round trip");
    }

    #[test]
    fn every_encodable_method_round_trips_through_its_own_writer() {
        for method in [
            Compression::None,
            Compression::Deflate,
            Compression::Gzip,
            Compression::Bzip2,
            Compression::Xz,
            Compression::Zstd,
            Compression::Lz4,
            Compression::Brotli,
        ] {
            check_round_trip(method);
        }
    }

    #[test]
    fn a_gzip_stream_written_here_carries_the_magic_the_reader_detects() {
        let options = WriteOptions::default().with_compression(Compression::Gzip);
        let sink = Cursor::new(Vec::new());
        let mut writer =
            ArchiveWriter::create(sink, Format::SingleStream(Compression::Gzip), &options)
                .expect("a gzip writer");
        writer
            .write_entry_from_slice(&NewEntry::file("a"), b"hello")
            .expect("write");
        let bytes = writer.finish_into().expect("finish").into_inner();
        assert!(
            bytes.starts_with(&[0x1F, 0x8B, 0x08]),
            "a gzip stream starts with its magic"
        );
    }

    #[test]
    fn an_xz_stream_written_here_carries_the_magic_the_reader_detects() {
        let options = WriteOptions::default().with_compression(Compression::Xz);
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::SingleStream(Compression::Xz), &options)
            .expect("an xz writer");
        writer
            .write_entry_from_slice(&NewEntry::file("a"), b"hello")
            .expect("write");
        let bytes = writer.finish_into().expect("finish").into_inner();
        assert!(
            bytes.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]),
            "an xz stream starts with its magic"
        );
    }

    #[test]
    fn methods_without_an_encoder_are_refused_at_create_not_at_first_write() {
        for method in [
            Compression::Deflate64,
            Compression::Lzma,
            Compression::Lzma2,
            Compression::Ppmd,
            Compression::Z,
            Compression::Unknown(99),
        ] {
            let options = WriteOptions::default().with_compression(method);
            let error = match ArchiveWriter::create(
                Cursor::new(Vec::new()),
                Format::SingleStream(method),
                &options,
            ) {
                Ok(_) => panic!("{method} has no encoder and must not create a writer"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), crate::error::ErrorKind::Unimplemented, "{method}: {error}");
        }
    }

    #[test]
    fn a_second_entry_is_refused_rather_than_concatenated() {
        // Concatenation decodes for gzip and silently truncates for XZ under this crate's reader,
        // so accepting it would produce files that read back differently per format.
        let options = WriteOptions::default().with_compression(Compression::Gzip);
        let sink = Cursor::new(Vec::new());
        let mut writer =
            ArchiveWriter::create(sink, Format::SingleStream(Compression::Gzip), &options)
                .expect("a writer");
        writer
            .write_entry_from_slice(&NewEntry::file("first"), b"one")
            .expect("the first entry writes");
        let error = match writer.write_entry_from_slice(&NewEntry::file("second"), b"two") {
            Ok(()) => panic!("a second entry must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
        assert!(error.message().contains("one entry"), "{error}");
    }

    #[test]
    fn a_directory_is_refused_rather_than_flattened_into_bytes() {
        let options = WriteOptions::default().with_compression(Compression::Xz);
        let sink = Cursor::new(Vec::new());
        let mut writer = ArchiveWriter::create(sink, Format::SingleStream(Compression::Xz), &options)
            .expect("a writer");
        let error = match writer
            .write_entry(&NewEntry::directory("dir").with_size(0), &mut std::io::empty())
        {
            Ok(()) => panic!("a directory has no bytes and must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }

    #[test]
    fn an_entry_after_finish_is_refused_rather_than_written_into_a_closed_stream() {
        let options = WriteOptions::default().with_compression(Compression::Zstd);
        let sink = Cursor::new(Vec::new());
        let mut writer =
            ArchiveWriter::create(sink, Format::SingleStream(Compression::Zstd), &options)
                .expect("a writer");
        writer
            .write_entry_from_slice(&NewEntry::file("a"), b"data")
            .expect("write");
        writer.finish().expect("finish");
        let error = match writer.write_entry_from_slice(&NewEntry::file("b"), b"more") {
            Ok(()) => panic!("writing after finish must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }

    #[test]
    fn an_empty_archive_finishes_to_a_valid_empty_stream() {
        // A caller that creates an archive and writes nothing gets a readable empty file, not a
        // zero-byte one: gzip and XZ both have headers that decoders expect.
        for method in [Compression::Gzip, Compression::Xz, Compression::Zstd] {
            let options = WriteOptions::default().with_compression(method);
            let sink = Cursor::new(Vec::new());
            let mut writer = ArchiveWriter::create(sink, Format::SingleStream(method), &options)
                .unwrap_or_else(|error| panic!("{method} create: {error}"));
            writer.finish().expect("finishing nothing still finishes");
            let bytes = writer.finish_into().expect("finish").into_inner();
            let mut reader = ArchiveReader::open_as(
                Cursor::new(bytes),
                Format::SingleStream(method),
                &OpenOptions::default(),
            )
            .unwrap_or_else(|error| panic!("{method} empty stream should open: {error}"));
            let mut got = Vec::new();
            reader
                .open_entry(0)
                .expect("open")
                .read_to_end(&mut got)
                .expect("read");
            assert!(got.is_empty(), "{method} wrote an empty stream with trailing bytes");
        }
    }

    #[test]
    fn a_password_is_refused_rather_than_silently_dropped() {
        let options = WriteOptions::default()
            .with_compression(Compression::Xz)
            .with_password("secret");
        let error = match ArchiveWriter::create(
            Cursor::new(Vec::new()),
            Format::SingleStream(Compression::Xz),
            &options,
        ) {
            Ok(_) => panic!("encryption must be refused, not dropped"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Unimplemented);
    }
}
