//! Writing 7Z archives.
//!
//! **Owned by Team `container`.**
//!
//! # Why this file wraps a vendored writer rather than writing the container
//!
//! A 7Z file is a signature header, a pack of compressed bytes, and a *coded* header describing
//! folders, bind pairs and sub-stream sizes. That coded header is the hard part. It is fuzzed,
//! tested against every 7Z this project can lay hands on, and already present in this repository
//! as [`sevenz_rust2::ArchiveWriter`]. Writing a second one here to avoid the dependency would put
//! two implementations of the same format in one binary, and the second would be the one nobody
//! had tested against real archives.
//!
//! So this module is an adapter, exactly as [`crate::formats::sevenz`] adapts
//! [`sevenz_rust2::BlockCursor`] for reading. The trade is recorded in
//! `vendor/sevenz-rust2/PATCHES.md`, which is where the reader's is.
//!
//! # Solid and non-solid
//!
//! A 7Z folder is a run of entries compressed together. `push_archive_entry` starts a folder per
//! entry, and `push_archive_entries` puts every entry in one folder. The second compresses better
//! and is what `7z` does by default; the first lets each entry be extracted without decoding the
//! ones before it.
//!
//! The default here is **non-solid**, for the reason the reader's design is built around: a solid
//! folder costs one decode per extraction whichever entry is wanted, and a file manager extracts
//! entries one at a time. [`WriteOptions::solid`] turns it on.
//!
//! # The one place this writer buffers
//!
//! [`sevenz_rust2::ArchiveWriter`] takes an entry and a reader and returns once the reader is
//! drained, so an entry's bytes have to exist before the archive can be told about it. The plumbing
//! in [`crate::writer`] hands them over a block at a time through [`std::io::Write`], so they are
//! accumulated here and handed on when the entry closes. That is one entry's size, which
//! [`crate::limits::Limits`] has already bounded, and it is the only place in this crate's writers
//! that holds an entry whole.
//!
//! A solid archive buffers every entry, because the folder is one stream and nothing can go into
//! it until the last entry closes. That is inherent to the shape, so it is stated rather than
//! hidden: peak memory is the archive's uncompressed size, and a multi-gigabyte archive is
//! therefore a non-solid one.
//!
//! # What is refused rather than faked
//!
//! The vendored writer has no password support, so a [`WriteOptions`] carrying one is an error
//! rather than an archive that silently drops the encryption. Writing an encrypted 7Z is not
//! something this crate can do yet, and SPEC.md says so.
//!
//! [`EntryWriter::into_sink`] has one arm it cannot satisfy: the vendored writer owns the sink for
//! as long as the archive is open, so a caller that drops the writer without finishing it cannot
//! get the sink back at all. It is unreachable through [`crate::writer::ArchiveWriter`], whose
//! `finish_into` always finishes first, and it is flagged to Team `core` because the fix is a
//! signature change on a trait they own, not a local one.

use std::io::{Seek, Write};

use sevenz_rust2::{
    ArchiveEntry, ArchiveWriter as VendoredWriter, EncoderConfiguration, EncoderMethod, NtTime,
    SourceReader,
};

use crate::entry::{EntryKind, normalise_name};
use crate::error::{Error, Result};
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

/// Writes a 7Z archive.
pub struct SevenZWriter<W: Write + Seek> {
    /// The vendored archive, built on the first entry and consumed by [`EntryWriter::finish`].
    ///
    /// `None` means the sink is back in [`SevenZWriter::sink`], which is the state either before
    /// anything is written or after the archive is closed.
    archive: Option<VendoredWriter<W>>,
    /// The sink. Held here while no archive is open, and again once it is finished.
    sink: Option<W>,
    /// Whether the entries go into one folder or one folder each.
    solid: bool,
    /// The entries of a solid folder, waiting for the archive to close.
    pending: Vec<PendingEntry>,
    /// Whether [`EntryWriter::finish`] has run, so a later `begin` is refused rather than writing
    /// into an archive whose header has already been written out.
    finished: bool,
    /// The method to compress the folder with, taken from [`WriteOptions`] and applied by
    /// [`content_methods`] when the vendored writer is built.
    compression: crate::format::Compression,
}

/// One entry held back until a solid folder closes.
struct PendingEntry {
    entry: ArchiveEntry,
    contents: Vec<u8>,
}

impl<W: Write + Seek> SevenZWriter<W> {
    pub fn create<W2: Write + Seek + 'static>(sink: W2, options: &WriteOptions) -> Result<Box<dyn EntryWriter<W2>>> {
        // The vendored writer has no password support at all. Producing an archive that silently
        // omits the encryption the caller asked for is worse than refusing: the file would look
        // like it worked.
        if options.password.is_some() {
            return Err(Error::unimplemented(
                "7Z writing cannot encrypt: the vendored writer takes no password",
            ));
        }
        Ok(Box::new(SevenZWriter {
            archive: None,
            sink: Some(sink),
            solid: options.solid,
            pending: Vec::new(),
            finished: false,
            compression: options.compression,
        }))
    }

    /// The vendored archive, building it on the first entry.
    ///
    /// Building lazily is what keeps a `create` that is never written to harmless: the sink is
    /// neither seeked nor written, so a caller that aborts before the first entry leaves no file.
    fn ensure_open(&mut self) -> Result<&mut VendoredWriter<W>> {
        if self.archive.is_none() {
            let sink = self
                .sink
                .take()
                .ok_or_else(|| Error::malformed("the 7Z writer has already been finished"))?;
            let mut archive = VendoredWriter::new(sink)
                .map_err(|error| Error::malformed(format!("7z: {error}")))?;
            archive.set_content_methods(content_methods(self.compression));
            self.archive = Some(archive);
        }
        self.archive
            .as_mut()
            .ok_or_else(|| Error::malformed("the 7Z writer is closed"))
    }

    /// Writes one entry, or holds it back until a solid folder closes.
    fn push_entry(&mut self, entry: &NewEntry, contents: Vec<u8>) -> Result<()> {
        let archive_entry = vendored_entry(entry);
        if self.solid {
            // The sink goes into the archive so the vendored writer has somewhere to seek; nothing
            // is written until the folder closes.
            self.ensure_open()?;
            self.pending.push(PendingEntry { entry: archive_entry, contents });
            return Ok(());
        }
        // A directory has no stream, and the vendored writer decides that from the entry rather
        // than from the reader, so `None` is the honest answer for one.
        let reader = match entry.kind {
            EntryKind::Directory => None,
            _ => Some(std::io::Cursor::new(contents)),
        };
        let archive = self.ensure_open()?;
        archive
            .push_archive_entry(archive_entry, reader)
            .map_err(|error| Error::malformed(format!("7z: {error}")))?;
        Ok(())
    }

    /// Writes every held-back entry into one folder.
    fn close_solid_folder(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::with_capacity(pending.len());
        let mut readers = Vec::with_capacity(pending.len());
        for held in pending {
            entries.push(held.entry);
            readers.push(SourceReader::new(std::io::Cursor::new(held.contents)));
        }
        let archive = self.ensure_open()?;
        archive
            .push_archive_entries(entries, readers)
            .map_err(|error| Error::malformed(format!("7z: {error}")))?;
        Ok(())
    }
}

impl<W: Write + Seek> EntryWriter<W> for SevenZWriter<W> {
    fn begin<'a>(
        &'a mut self,
        entry: &NewEntry,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWrite + 'a>> {
        if self.finished {
            return Err(Error::malformed(
                "the 7Z archive has already been finished and takes no more entries",
            ));
        }
        Ok(Box::new(EntrySink {
            writer: self,
            name: entry.name.clone(),
            kind: entry.kind,
            last_modified: entry.last_modified,
            mode: entry.mode,
            buffer: Vec::new(),
            written: 0,
            closed: false,
        }))
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.close_solid_folder()?;
        // Built even when nothing was written, so that an empty 7Z is a valid empty 7Z rather than
        // a zero-byte file that nothing can open.
        self.ensure_open()?;
        let archive = self
            .archive
            .take()
            .ok_or_else(|| Error::malformed("the 7Z writer is closed"))?;
        let sink = archive
            .finish()
            .map_err(|error| Error::malformed(format!("7z: {error}")))?;
        self.sink = Some(sink);
        self.finished = true;
        Ok(())
    }

    fn into_sink(self: Box<Self>) -> W {
        // `finish_into` — the only public route here — finishes first, so the sink is already back.
        // A caller holding the box directly gets the archive finished on its behalf, because the
        // vendored writer owns the sink for as long as the archive is open. If the sink itself
        // refuses to take the header there is no sink left to return, and the crate's rule against
        // panicking has no answer for that; see the module docs.
        let mut writer = *self;
        let _ = writer.finish();
        // The exception lib.rs allows: this cannot fail, and there is no error to return even if
        // it could, because `EntryWriter::into_sink` is declared as returning `W`. The sink is
        // only ever taken by `finish`, and the public route through `finish_into` reports that
        // failure as an `Err` before it reaches here - a caller holding this handle has already
        // been told the write failed. See the module docs.
        #[allow(clippy::expect_used, reason = "unreachable: the sink is returned only after finish, whose failure is already reported")]
        writer
            .sink
            .expect("the 7Z archive was finished before its sink was returned")
    }
}

/// One entry's contents, accumulated until the entry closes.
struct EntrySink<'a, W: Write + Seek> {
    writer: &'a mut SevenZWriter<W>,
    name: String,
    kind: EntryKind,
    last_modified: Option<std::time::SystemTime>,
    mode: Option<u32>,
    buffer: Vec<u8>,
    written: u64,
    closed: bool,
}

impl<W: Write + Seek> Write for EntrySink<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.closed {
            return Err(std::io::Error::other("this 7Z entry has already been finished"));
        }
        self.buffer.extend_from_slice(bytes);
        self.written += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Seek> EntryWrite for EntrySink<'_, W> {
    fn written(&self) -> u64 {
        self.written
    }

    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let entry = NewEntry {
            name: normalise_name(&self.name),
            kind: self.kind,
            size: Some(self.written),
            last_modified: self.last_modified,
            mode: self.mode,
            symlink_target: None,
            compress: None,
        };
        let contents = std::mem::take(&mut self.buffer);
        self.writer.push_entry(&entry, contents)
    }
}

/// Turns a [`NewEntry`] into the vendored writer's entry.
fn vendored_entry(entry: &NewEntry) -> ArchiveEntry {
    let mut archive_entry = match entry.kind {
        EntryKind::Directory => ArchiveEntry::new_directory(&entry.name),
        _ => ArchiveEntry::new_file(&entry.name),
    };
    if let Some(time) = entry.last_modified {
        archive_entry.last_modified_date = NtTime::from(system_time_to_nt(time));
        archive_entry.has_last_modified_date = true;
    }
    if let Some(mode) = entry.mode {
        // 7Z records the unix mode in the high 16 bits of the Windows attribute word, which is
        // where `crate::formats::sevenz::mode_from_windows` looks for it on the way back.
        archive_entry.windows_attributes = (mode << 16) | 0x20;
        archive_entry.has_windows_attributes = true;
    }
    archive_entry
}

/// 7Z stores Windows FILETIME, 100-nanosecond ticks since 1601, so this is the same arithmetic the
/// reader does in the other direction, run backwards.
fn system_time_to_nt(time: std::time::SystemTime) -> u64 {
    /// 1601-01-01 to 1970-01-01, in seconds.
    const EPOCH_DIFFERENCE: i64 = 11_644_473_600;
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = (duration.as_secs() as i64).saturating_add(EPOCH_DIFFERENCE);
    let ticks = seconds.max(0).unsigned_abs().saturating_mul(10_000_000);
    ticks + u64::from(duration.subsec_nanos()) * 100
}

/// The method chain the vendored writer is configured with.
///
/// LZMA2 is 7Z's own default and the one every tool reads, so it is also the fallback for a method
/// this build cannot encode: refusing would mean the app cannot create a 7Z at all with an unusual
/// default, which is the same call the ZIP writer makes.
fn content_methods(compression: crate::format::Compression) -> Vec<EncoderConfiguration> {
    let method = match compression {
        crate::format::Compression::None => EncoderMethod::COPY,
        crate::format::Compression::Lzma => EncoderMethod::LZMA,
        crate::format::Compression::Bzip2 => EncoderMethod::BZIP2,
        crate::format::Compression::Ppmd => EncoderMethod::PPMD,
        crate::format::Compression::Lzma2 | _ => EncoderMethod::LZMA2,
    };
    vec![EncoderConfiguration::from(method)]
}
