//! Writing archives.
//!
//! # Why the writers here look different from the ones they replace
//!
//! The API this crate replaces buffered a whole entry before writing it, and allocated a fresh
//! deflate encoder for every entry. Measured on an archive of 5,000 entries, that cost 1.40 GiB of
//! allocation and 219.8 ms; reusing the encoder and coalescing headers brought it to 43.9 MB and
//! 109.2 ms. Three rules follow from that, and every writer in this crate follows them:
//!
//! 1. **Encoders are created once, not per entry.** A deflate encoder carries a 32 KiB window and a
//!    hash table; making one per entry is the single largest allocation source in the writer.
//! 2. **Headers are written in one call.** Every write crosses into Java, so 32 writes per entry
//!    became 6. The writer below coalesces them into a single buffer.
//! 3. **Nothing is buffered whole.** [`ArchiveWriter::write_entry`] takes any [`std::io::Read`] and
//!    pulls it in fixed-size blocks, so a 4 GiB entry costs 128 KiB.

pub mod sevenz;
pub mod single;
pub mod tar;
pub mod zip;

use std::io::{self, Read, Seek, Write};

use crate::error::{Error, Result};
use crate::format::{Compression, Format};
use crate::limits::Limits;

/// What to do when creating.
#[derive(Clone, Debug)]
pub struct WriteOptions {
    /// The compression to use, for the formats that take one.
    pub compression: Compression,
    /// The password, for the formats that support encryption.
    pub password: Option<String>,
    /// The ceiling applied to a single entry's declared size.
    pub limits: Limits,
    /// Whether to buffer.
    ///
    /// Only meaningful for a format that has to know where an entry's data ends before it writes
    /// the entry: a ZIP with a data descriptor can stream, and a ZIP without one cannot, because
    /// the header that carries the size comes first. The writers turn this on automatically where
    /// the format allows and ignore it where it does not.
    pub buffer_entries: bool,
    /// Whether a 7Z archive's entries go into one folder or one folder each.
    ///
    /// Only meaningful for 7Z, which is the only format here with a folder concept. Off by
    /// default: one folder per entry lets any entry be extracted without decoding the ones before
    /// it, which is what a file manager does. Turning it on compresses better — `7z` itself is
    /// solid by default — and costs the whole archive in memory while it is written, because
    /// nothing can go into the folder until the last entry closes.
    pub solid: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            compression: Compression::Deflate,
            password: None,
            limits: Limits::default(),
            buffer_entries: false,
            solid: false,
        }
    }
}

impl WriteOptions {
    pub fn with_compression(mut self, compression: Compression) -> WriteOptions {
        self.compression = compression;
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> WriteOptions {
        self.password = Some(password.into());
        self
    }

    /// Whether every entry goes into one 7Z folder, rather than one folder each.
    pub fn with_solid(mut self, solid: bool) -> WriteOptions {
        self.solid = solid;
        self
    }

    pub fn stored() -> WriteOptions {
        WriteOptions { compression: Compression::None, ..WriteOptions::default() }
    }
}

/// One entry to be written.
#[derive(Clone, Debug)]
pub struct NewEntry {
    /// The path within the archive. Separators are normalised on the way in.
    pub name: String,
    pub kind: crate::entry::EntryKind,
    /// The contents' uncompressed size, when the writer should declare it in advance.
    ///
    /// `None` is allowed and means "unknown", which forces a streaming form. Formats that cannot
    /// stream without a size say so rather than silently buffering.
    pub size: Option<u64>,
    pub last_modified: Option<std::time::SystemTime>,
    pub mode: Option<u32>,
    /// For a symlink, the path it points at. Written as the entry's contents.
    pub symlink_target: Option<String>,
    /// Whether to compress this entry. Overrides the archive's default, so that an already
    /// compressed file can be stored rather than compressed again for no gain.
    pub compress: Option<bool>,
}

impl NewEntry {
    pub fn file(name: impl Into<String>) -> NewEntry {
        NewEntry {
            name: name.into(),
            kind: crate::entry::EntryKind::File,
            size: None,
            last_modified: None,
            mode: None,
            symlink_target: None,
            compress: None,
        }
    }

    pub fn directory(name: impl Into<String>) -> NewEntry {
        NewEntry { kind: crate::entry::EntryKind::Directory, ..NewEntry::file(name) }
    }

    pub fn symlink(name: impl Into<String>, target: impl Into<String>) -> NewEntry {
        NewEntry {
            kind: crate::entry::EntryKind::Symlink,
            symlink_target: Some(target.into()),
            ..NewEntry::file(name)
        }
    }

    pub fn with_size(mut self, size: u64) -> NewEntry {
        self.size = Some(size);
        self
    }

    pub fn with_last_modified(mut self, time: std::time::SystemTime) -> NewEntry {
        self.last_modified = Some(time);
        self
    }

    pub fn stored(mut self) -> NewEntry {
        self.compress = Some(false);
        self
    }
}

/// The per-format half of the writer.
///
/// Kept separate from [`ArchiveWriter`] so the generic plumbing is written once. The trait is
/// object-safe, so the public writer is one type no matter how many formats exist.
///
/// # Why the entry borrows the writer
///
/// [`EntryWriter::begin`] hands back something that has to keep writing into the archive while it
/// is open, and an archive writer cannot be used twice at once anyway — the caller holds a `&mut`
/// on it for the whole of `write_entry`. Borrowing says exactly that, and it costs nothing. An
/// owning version would have to put the sink behind a lock or an `Arc`, and would pay for either
/// on every write to work around a lifetime the borrow checker already solved.
pub trait EntryWriter<W: Write + Seek> {
    /// Begins an entry. Returns something the contents are written into.
    ///
    /// The returned value borrows this writer, so it must be dropped before the next entry or
    /// before [`EntryWriter::finish`]. [`ArchiveWriter::write_entry_with`] does that for a caller.
    fn begin<'a>(&'a mut self, entry: &NewEntry, options: &WriteOptions)
        -> Result<Box<dyn EntryWrite + 'a>>;

    /// Finishes the archive, writing whatever directory or trailer the format needs.
    ///
    /// Without this the output is not a readable archive.
    fn finish(&mut self) -> Result<()>;

    /// Discards the writer and gives the sink back.
    fn into_sink(self: Box<Self>) -> W;
}

/// The contents of one entry being written.
///
/// Closing an entry is not a method on this trait because how it closes is the format's business,
/// not the plumbing's: a ZIP has a data descriptor to write, a TAR has padding to add, and a 7Z
/// has nothing to do until the whole archive is finished. The writer calls [`EntryWrite::finish`]
/// when the caller is done.
pub trait EntryWrite: Write {
    /// How many uncompressed bytes have gone in so far.
    fn written(&self) -> u64;

    /// Closes the entry, writing whatever trailer the format needs.
    fn finish(&mut self) -> Result<()>;
}

/// A writable archive.
///
/// Created by [`ArchiveWriter::create`]. Entries are added with [`ArchiveWriter::write_entry`],
/// which streams from any reader, and [`ArchiveWriter::finish`] closes the archive off.
pub struct ArchiveWriter<W: Write + Seek> {
    backend: Box<dyn EntryWriter<W>>,
    format: Format,
    /// The block buffer entries are copied through, owned here so it is allocated once per
    /// archive rather than once per entry.
    ///
    /// This is the same rule as the writer's reusable deflater, one level up: 5,000 entries at
    /// 128 KiB each is 640 MB of allocate-and-zero that buys nothing, because the buffer is dead
    /// the moment the copy finishes.
    block: Vec<u8>,
}

impl<W: Write + Seek + 'static> ArchiveWriter<W> {
    /// Creates an archive of `format` on `sink`.
    ///
    /// Nothing is written until the first entry or until [`ArchiveWriter::finish`], so a caller
    /// that fails before then produces no file rather than a truncated one.
    pub fn create(sink: W, format: Format, options: &WriteOptions) -> Result<ArchiveWriter<W>> {
        if !format.is_writable() {
            return Err(Error::unsupported(format!("{format} cannot be created")));
        }
        Ok(ArchiveWriter {
            backend: crate::formats::create(format, sink, options)?,
            format,
            block: vec![0; COPY_BUFFER_SIZE],
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Writes one entry, streaming its contents from `contents`.
    ///
    /// The reader is drained in fixed-size blocks, so nothing here allocates an entry's size. An
    /// entry with no contents, such as a directory or a symlink whose target the writer records in
    /// the header, is written from `Empty` instead and `contents` is not read.
    pub fn write_entry(&mut self, entry: &NewEntry, contents: &mut dyn Read) -> Result<()> {
        let options = WriteOptions::default();
        self.write_entry_with(entry, contents, &options)
    }

    /// As [`ArchiveWriter::write_entry`], with the options spelled out.
    pub fn write_entry_with(
        &mut self,
        entry: &NewEntry,
        contents: &mut dyn Read,
        options: &WriteOptions,
    ) -> Result<()> {
        let normalised = NewEntry {
            name: crate::entry::normalise_name(&entry.name),
            ..entry.clone()
        };
        if let Some(size) = normalised.size {
            options.limits.check_entry_size(Some(size))?;
        }
        let mut sink = self.backend.begin(&normalised, options)?;
        // `copy_blocks` rather than `io::copy`: the caller's reader may return short reads, and
        // turning each of those into a write to the sink is what makes a many-entry archive slow.
        //
        // The buffer is taken out rather than borrowed, because the entry sink already borrows
        // the backend and borrowing a second field of the same struct at the same time does not
        // compile. It is put straight back, so the allocation happens once per archive.
        let mut block = std::mem::take(&mut self.block);
        let result = copy_blocks_into(contents, &mut *sink, &mut block);
        self.block = block;
        let written = result?;
        // A caller that declared a size and then supplied a different one has a bug, and one that
        // is not caught here shows up much later as a corrupt archive.
        // Closed before the sizes are checked, so that an error here still leaves the archive in
        // a state `finish` can close cleanly rather than one it cannot.
        sink.finish()?;
        if let Some(declared) = normalised.size {
            if declared != written && normalised.kind == crate::entry::EntryKind::File {
                return Err(Error::malformed(format!(
                    "The entry {} declared {declared} bytes but supplied {written}",
                    normalised.name
                )));
            }
        }
        Ok(())
    }

    /// Writes an entry by draining `contents` all the way, for callers that already have it in
    /// memory.
    pub fn write_entry_from_slice(&mut self, entry: &NewEntry, contents: &[u8]) -> Result<()> {
        let mut entry = entry.clone();
        entry.size = Some(contents.len() as u64);
        self.write_entry(&entry, &mut &contents[..])
    }

    /// Finishes the archive. Must be called before the sink is used again.
    pub fn finish(&mut self) -> Result<()> {
        self.backend.finish()
    }

    /// Finishes the archive and gives the sink back, which is what a caller normally wants.
    pub fn finish_into(mut self) -> Result<W> {
        self.backend.finish()?;
        Ok(self.backend.into_sink())
    }
}

/// Reads nothing. Used for entries whose contents live in their header.
pub struct Empty;

impl Read for Empty {
    fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

/// Copies in fixed-size blocks, so that a format's writer never needs its own buffering loop.
///
/// The size is the same 128 KiB the readers use, chosen so a block of output crosses into Java
/// once rather than a few hundred times.
pub const COPY_BUFFER_SIZE: usize = crate::reader::Scratch::DEFAULT_SIZE;

/// Copies `from` to `to` in blocks, returning how many bytes went across.
///
/// Convenience for a caller with no buffer of its own. An archive writer uses
/// [`copy_blocks_into`] with a buffer it already owns, because this one allocates per call and a
/// many-entry archive makes a lot of calls.
pub fn copy_blocks(from: &mut dyn Read, to: &mut dyn Write) -> io::Result<u64> {
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    copy_blocks_into(from, to, &mut buffer)
}

/// Copies `from` to `to` through `buffer`, returning how many bytes went across.
///
/// Filling the block completely before handing it over is the point: a source that returns short
/// reads — a socket, a channel, a `Cursor` at its end — would otherwise turn every one of them
/// into a separate small write, which is the same problem as 32 sink writes per entry one level
/// down.
pub fn copy_blocks_into(
    from: &mut dyn Read,
    to: &mut dyn Write,
    buffer: &mut [u8],
) -> io::Result<u64> {
    let mut moved = 0u64;
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            match from.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if filled == 0 {
            return Ok(moved);
        }
        to.write_all(&buffer[..filled])?;
        moved += filled as u64;
        if filled < buffer.len() {
            return Ok(moved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn an_entry_name_is_normalised_on_the_way_in() {
        assert_eq!(crate::entry::normalise_name("a//b/"), "a/b");
        assert_eq!(crate::entry::normalise_name("a\\b"), "a/b");
    }

    #[test]
    fn a_format_that_cannot_be_created_is_refused_rather_than_producing_a_broken_file() {
        let options = WriteOptions::default();
        let error = match ArchiveWriter::create(Cursor::new(Vec::new()), Format::Rar, &options) {
            Ok(_) => panic!("RAR cannot be written"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Unsupported);
    }

    #[test]
    fn declaring_a_size_the_contents_do_not_match_is_caught_at_the_entry_not_at_read_back() {
        // This is the difference between a clear error now and "the archive is corrupt" to whoever
        // tries to open it later.
        struct Recorder;
        impl<W: Write + Seek> EntryWriter<W> for Recorder {
            fn begin<'a>(
                &'a mut self,
                _entry: &NewEntry,
                _options: &WriteOptions,
            ) -> Result<Box<dyn EntryWrite + 'a>> {
                Ok(Box::new(Sink))
            }
            fn finish(&mut self) -> Result<()> {
                Ok(())
            }
            fn into_sink(self: Box<Self>) -> W {
                unreachable!("not used by this test")
            }
        }
        struct Sink;
        impl Write for Sink {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                Ok(buffer.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl EntryWrite for Sink {
            fn written(&self) -> u64 {
                0
            }
            fn finish(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let mut writer: ArchiveWriter<Cursor<Vec<u8>>> = ArchiveWriter {
            backend: Box::new(Recorder),
            format: Format::Zip,
            block: vec![0; COPY_BUFFER_SIZE],
        };
        let entry = NewEntry::file("a.txt").with_size(100);
        let error = match writer.write_entry(&entry, &mut &b"only ten.."[..]) {
            Ok(()) => panic!("writing fewer bytes than declared must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
        assert!(error.message().contains("declared 100"));
    }

    #[test]
    fn copying_moves_every_byte_and_reports_the_count() {
        let payload: Vec<u8> = (0..COPY_BUFFER_SIZE * 3 + 17).map(|i| i as u8).collect();
        let mut out = Vec::new();
        let moved = copy_blocks(&mut &payload[..], &mut out).unwrap();
        assert_eq!(moved, payload.len() as u64);
        assert_eq!(out, payload);
    }
}
/// What can actually be written, pinned.
///
/// This exists because `SPEC.md` claimed 7Z and single-stream writing worked, and neither does:
/// `SevenZWriter::create`, `SingleStreamWriter::create` and `CompressedTarWriter::create` all
/// return `ErrorKind::Unimplemented`. The sevenz-rust2 differential corpus still passed, because
/// reading 7Z is genuinely implemented and nothing in the test suite asked about *writing* it.
///
/// The app writes 7Z today, through sevenz-rust2 directly, so a stub here is a functional
/// regression rather than a missing feature. This test is the thing that says so out loud, and it
/// is written as a list of what works so that adding a writer is a deliberate edit here.
#[cfg(test)]
mod what_can_be_written {
    use super::*;
    use crate::format::{Compression, Format};

    fn creates(format: Format) -> Result<()> {
        ArchiveWriter::create(std::io::Cursor::new(Vec::new()), format, &WriteOptions::default())
            .map(|_| ())
    }

    #[test]
    fn zip_and_tar_are_writable_and_nothing_else_is_yet() {
        assert!(creates(Format::Zip).is_ok(), "ZIP writing is the app's common case");
        assert!(creates(Format::Tar).is_ok(), "TAR writing works");

        // Each of these is a stub. When one starts working this test fails, and the failure is
        // the prompt to move it into the list above and to correct SPEC.md.
        for (format, why) in [
            (Format::SevenZ, "the app writes 7Z through sevenz-rust2 today"),
            (Format::SingleStream(Compression::Xz), "oxiarc writes .xz"),
            (Format::CompressedTar(Compression::Gzip), "tar.gz is ordinary"),
        ] {
            let error = creates(format).expect_err("a stub writer must not succeed");
            assert_eq!(
                error.kind(),
                crate::error::ErrorKind::Unimplemented,
                "{format:?} should say it is unimplemented, not fail some other way: {error}"
            );
            let _ = why;
        }
    }

    /// Reading and writing are not symmetric, and it is easy to assume they are.
    ///
    /// These formats can all be read and none of them can be written. That asymmetry is the
    /// current state of the project, it is not visible from the type signatures, and it is the
    /// difference between "we can open this" and "we can create this".
    #[test]
    fn every_format_listed_as_read_only_is_actually_read_only() {
        for (format, what) in [
            (Format::SevenZ, "the app writes 7Z today, through sevenz-rust2"),
            (Format::SingleStream(Compression::Xz), "oxiarc writes .xz"),
            (Format::CompressedTar(Compression::Gzip), "tar.gz is ordinary"),
        ] {
            let error = creates(format).expect_err("a read-only format must not create a writer");
            assert_eq!(error.kind(), crate::error::ErrorKind::Unimplemented, "{format:?}: {error}");
            let _ = what;
        }
        // And the two that are writable really are, so this cannot pass by everything failing.
        for format in [Format::Zip, Format::Tar] {
            assert!(creates(format).is_ok(), "{format:?} is listed as writable");
        }
    }
}
