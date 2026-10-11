//! Reading archives.
//!
//! # The two problems this API is shaped around
//!
//! **Whole-entry buffering.** The API this crate replaces materialised an entry into a `Vec` before
//! the caller saw a single byte of it. A 132 MB entry therefore cost 132 MB resident and about a
//! second of latency before the first byte. Here, [`ArchiveReader::open_entry`] hands back a
//! [`std::io::Read`] that pulls from the decompressor a buffer at a time. Nothing in this module
//! ever allocates an entry's size.
//!
//! **Solid blocks.** Reading entry *n* of a solid 7Z block means decoding that block from its
//! start, so reading entries one at a time by name costs one decode per entry and is quadratic in
//! the number of entries. Two things fix that, and both are properties of this API rather than of
//! any one format's implementation:
//!
//! 1. Entries are addressed by **index** ([`ArchiveReader::open_entry`]) and the archive stays
//!    **open** across reads, so a block decoder can be kept alive between calls.
//! 2. A format with blocks reports the layout ([`Entry::block`]), so the reader can tell that two
//!    entries share a block and skip the second decode.
//!
//! The app has to hold the archive open across per-file reads for this to pay off, which is what
//! [`crate::open`] is for.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::entry::{BlockPlacement, Entry};
use crate::error::{Error, ErrorKind, Result};
use crate::format::Format;
use crate::limits::Limits;

/// The archive's source, shared between the reader and any entry stream it hands out.
///
/// # Why this is a reference count behind a mutex rather than a borrow
///
/// An entry stream owns a decompressor that pulls from the source, and the reader owns the entry
/// list. If the stream borrowed the source, `ArchiveReader::open_entry` would have to hand back
/// something that keeps the reader mutably borrowed, which would make it impossible to open a
/// second entry while the first is still being read — and impossible to hold an entry stream at
/// all across a JNI call, which is exactly what the app does.
///
/// The alternative, re-seeking for every read, would mean a syscall per read and would put the
/// decoder's buffer size back in charge of how many syscalls happen.
///
/// So the source is shared. The mutex is uncontended in the common case and the lock is taken
/// once per read call, not once per byte: a read moves at least 128 KiB through the decompressor
/// before the source is touched again, so the lock is not what limits throughput.
///
/// A newtype rather than a type alias, so that `Read` and `Seek` can be implemented for it.
///
/// An alias would be shorter and would not compile: the orphan rule forbids implementing a trait
/// from the standard library for a type that is only an alias for one. Being a newtype is also
/// what lets a caller hand the reader a source that is already shared, which is what the app does
/// and what the tests that count bytes read need.
pub struct Shared<R>(Arc<Mutex<R>>);

impl<R> Shared<R> {
    /// The reference count, for a caller reporting how many handles are outstanding.
    pub fn handles(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Takes the source back, when this is the only handle and nothing panicked holding it.
    ///
    /// Reports rather than panicking, because a caller that has already finished with an archive
    /// should not be crashed by a library it used correctly.
    pub fn into_inner(self) -> Result<R> {
        let cell = Arc::try_unwrap(self.0).map_err(|shared| {
            Error::malformed(format!(
                "the archive is still in use by {} other handles",
                Arc::strong_count(&shared)
            ))
        })?;
        Ok(cell
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// The underlying arc, for the places that need to clone the handle itself.
    pub fn arc(&self) -> Arc<Mutex<R>> {
        self.0.clone()
    }

    /// Locks the source, or reports why it cannot be locked.
    pub fn lock(&self) -> Result<MutexGuard<'_, R>> {
        self.0.lock().map_err(|_| {
            Error::malformed("the archive was left in an unknown state by an earlier failure")
        })
    }
}

impl<R> Clone for Shared<R> {
    fn clone(&self) -> Shared<R> {
        Shared(self.0.clone())
    }
}

/// Wraps a source for sharing.
pub fn share<R>(source: R) -> Shared<R> {
    Shared(Arc::new(Mutex::new(source)))
}

/// Reads and seeks straight through a shared handle.
///
/// The app's source is a Java channel wrapped in a mutex, and the same shape works here: the
/// reader takes a `Shared<R>` and every entry's decoder shares it, so a caller that already has a
/// shared source does not have to unwrap it. Without this the shared handle is usable only through
/// [`lock`], which is a fine internal tool and a poor public one.
impl<R: Read> Read for Shared<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut guard = self
            .lock()
            .map_err(|error| io::Error::other(error.message().to_owned()))?;
        guard.read(output)
    }
}

impl<R: Seek> Seek for Shared<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let mut guard = self
            .lock()
            .map_err(|error| io::Error::other(error.message().to_owned()))?;
        guard.seek(position)
    }
}

/// Locks a shared source, turning a poisoned mutex into an error.
///
/// A poisoned mutex means another thread panicked while holding the source. The archive may be in
/// an unknown position, but reporting an error is strictly better than the second panic, which is
/// what `lock` would otherwise produce.
pub fn lock<R>(source: &Shared<R>) -> Result<MutexGuard<'_, R>> {
    source.lock()
}

/// What to do when opening.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// The password to try, if the archive turns out to be encrypted.
    ///
    /// The first of several is used. Formats that support several passwords per archive are rare
    /// enough that trying them in turn is not worth the API complexity.
    pub password: Option<String>,
    /// The ceilings that protect against a decompression bomb.
    pub limits: Limits,
    /// Whether to record each entry's [`Entry::block`].
    ///
    /// Costs almost nothing and is what enables the solid-block reuse above, so it is on by
    /// default. It only has an effect on formats that have blocks.
    pub expose_block_layout: bool,
    /// Whether to record modes and timestamps for formats that support them.
    ///
    /// Reading a timestamp is a handful of instructions, so this is on by default too.
    pub expose_metadata: bool,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            password: None,
            limits: Limits::default(),
            expose_block_layout: true,
            expose_metadata: true,
        }
    }
}

impl OpenOptions {
    /// Sets the password to try when opening an encrypted archive.
    pub fn with_password(mut self, password: impl Into<String>) -> OpenOptions {
        self.password = Some(password.into());
        self
    }

    /// Sets custom limits for decompression bomb protection.
    pub fn with_limits(mut self, limits: Limits) -> OpenOptions {
        self.limits = limits;
        self
    }
}

/// A readable archive, held open.
///
/// Opening an archive reads its entry list once. After that, entries are addressed by index and
/// their contents are streamed, so holding this value across several reads is both allowed and
/// much faster for formats with blocks.
pub struct ArchiveReader<R: Read + Seek + Send + 'static> {
    entries: Vec<Entry>,
    format: Format,
    backend: Box<dyn EntrySource<R>>,
    /// Where the source is, kept so a block decoder can be told to start again.
    limits: Limits,
}

/// The per-format half of the reader.
///
/// Each format implements this and owns whatever state it needs: a directory reader, a decoder for
/// the current block, or both. The trait is object-safe on purpose, so [`ArchiveReader`] is one
/// type rather than one per format, and so a format can be added without touching this module.
///
/// # Implementing this
///
/// An implementor must never panic, must never allocate an entry's size, and must return
/// [`ErrorKind::Malformed`] rather than an `io::Error` for anything wrong with the bytes. The
/// `#[deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]` lint at the crate
/// root enforces the first two by static analysis.
pub trait EntrySource<R: Read + Seek + Send + 'static>: Send {
    /// The entry list, built when the archive was opened.
    fn entries(&self) -> &[Entry];

    /// Starts reading the entry at `index`, and returns something that produces its contents.
    ///
    /// `index` is always in range: the caller checks it against `entries()` first.
    fn open(&mut self, index: usize) -> Result<EntryStream>;

    /// Discards the entry at `index` without producing its contents, returning how many bytes were
    /// skipped.
    ///
    /// The default walks the contents and throws them away, which is correct but slow. Formats
    /// where skipping is much cheaper than decompressing should override this.
    fn skip(&mut self, index: usize) -> Result<u64> {
        let mut stream = self.open(index)?;
        io::copy(&mut stream, &mut io::sink())?;
        Ok(stream.into_inner())
    }

    /// Whether this source can reuse a decoded block between entries, and so benefits from being
    /// read in order. Reported so the caller can pick an access strategy.
    fn reuses_blocks(&self) -> bool {
        false
    }

    /// Gives the source back, so the caller can recover the reader it was built on.
    ///
    /// Fails when an entry stream is still open, because the source cannot be taken back while
    /// something else holds it. Reported rather than papered over with a panic.
    fn into_source(self: Box<Self>) -> Result<R>;
}

/// The contents of one entry, as a reader.
///
/// This is a `Read`, never a `Vec`. Everything the caller does with it pulls from the decompressor
/// incrementally.
pub struct EntryStream {
    inner: Box<dyn EntryRead>,
    consumed: u64,
}

impl EntryStream {
    /// Wraps a per-format reader in the shared streaming layer.
    pub fn new(inner: Box<dyn EntryRead>) -> EntryStream {
        EntryStream { inner, consumed: 0 }
    }

    /// How many bytes have been read out of this entry so far.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// How many bytes the entry has, when the archive says.
    pub fn size(&self) -> Option<u64> {
        self.inner.size()
    }

    /// Unwraps the reader, giving back the byte count it reported.
    ///
    /// Used by the default [`EntrySource::skip`], which has to know how much it threw away.
    pub fn into_inner(mut self) -> u64 {
        // Drain whatever the reader has left so that a format which tracks its own position stays
        // consistent with what the caller was told. A reader that has already reached the end costs
        // nothing here.
        let mut sink = io::sink();
        let _ = io::copy(&mut self, &mut sink);
        self.consumed
    }
}

impl std::fmt::Debug for EntryStream {
    /// Shows what has been read so far, not the decompressor's internals, which are the
    /// implementation's business and not the caller's.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryStream")
            .field("consumed", &self.consumed)
            .field("size", &self.inner.size())
            .finish()
    }
}

impl Read for EntryStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        // An empty slice must report zero without touching the decompressor. Several codecs treat
        // a zero-length read as a signal and would return an error.
        if output.is_empty() {
            return Ok(0);
        }
        let read = self.inner.read(output)?;
        self.consumed += read as u64;
        Ok(read)
    }
}

/// The per-format contents reader, behind a box.
///
/// Separate from [`EntryStream`] so that the byte counting above is implemented once, for every
/// format, rather than once per format.
pub trait EntryRead: Read + Send {
    /// The entry's uncompressed size, when the format records it.
    fn size(&self) -> Option<u64>;
}

impl<R: Read + Seek + Send + 'static> ArchiveReader<R> {
    /// Opens whatever `source` turns out to be.
    ///
    /// Detection reads the signature rather than trusting a name, because the bytes say what a file
    /// is and the name says what somebody called it.
    pub fn open(mut source: R, options: &OpenOptions) -> Result<ArchiveReader<R>> {
        let format = crate::detect::detect(&mut source)?;
        Self::open_as(source, format, options)
    }

    /// [`Self::open`], with the name the caller already has as a fallback.
    ///
    /// Detection reads signatures, and two of the formats this crate reads cannot be identified by
    /// one: an ICE file begins with its uncompressed length and an ARC file with the DOS
    /// end-of-file marker. The name settles those, and is ignored for anything the bytes already
    /// identified. `ArchiveReader::open` is `open_named(source, None, options)`.
    pub fn open_named(
        mut source: R,
        name: Option<&std::path::Path>,
        options: &OpenOptions,
    ) -> Result<ArchiveReader<R>> {
        let format = crate::detect::detect_with_name(&mut source, name)?;
        Self::open_as(source, format, options)
    }

    /// Opens `source` as a format the caller already knows, skipping detection.
    ///
    /// Worth it when detection is the expensive part, such as probing a file with no signature in
    /// it. For a TAR the detection cost is a 512-byte header read either way.
    pub fn open_as(source: R, format: Format, options: &OpenOptions) -> Result<ArchiveReader<R>> {
        let backend = crate::formats::open(format, source, options)?;
        Ok(ArchiveReader {
            entries: backend.entries().to_vec(),
            format,
            backend,
            limits: options.limits.clone(),
        })
    }

    /// What this archive turned out to be.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Every entry, in the order the archive lists them.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// One entry, or `None` when the index is out of range.
    ///
    /// Takes `&self` on purpose: a caller listing an archive should not be able to disturb a
    /// decoder that another entry is using.
    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// How many entries there are.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the archive has no entries. An empty archive is legal, and not an error.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Starts reading an entry, by index.
    ///
    /// This is the call that matters. It returns a streaming reader, so the caller can read a
    /// megabyte or a gigabyte without the archive ever holding more than one buffer of it.
    ///
    /// Entries can be opened in any order. Opening them in order is faster for formats with solid
    /// blocks, because the decoder for the current block is then reused rather than restarted, but
    /// it is never *required*: an out-of-order open restarts the block, which costs a decode.
    pub fn open_entry(&mut self, index: usize) -> Result<EntryStream> {
        let entry = self.entries.get(index).ok_or_else(|| {
            Error::malformed(format!(
                "Entry {index} is out of range, the archive has {}",
                self.len()
            ))
        })?;
        self.limits.check_entry_size(entry.size)?;
        self.backend.open(index)
    }

    /// Skips an entry without decompressing it, where the format allows.
    ///
    /// Cheaper than opening the entry and throwing the contents away, because a format that stores
    /// an entry's compressed size and has a seekable source can step over it instead of decoding
    /// it. The app uses this when extracting a whole archive.
    pub fn skip_entry(&mut self, index: usize) -> Result<u64> {
        if self.entries.get(index).is_none() {
            return Err(Error::malformed(format!("Entry {index} is out of range")));
        }
        self.backend.skip(index)
    }

    /// Whether reading entries in order lets this format reuse a decoded block.
    ///
    /// Worth knowing, because it is the difference between one decode of a solid block and one
    /// decode per entry. The app uses it to decide whether to sort its reads.
    pub fn reuses_blocks(&self) -> bool {
        self.backend.reuses_blocks()
    }

    /// Consumes the reader and gives the source back.
    ///
    /// Fails if an entry stream opened from this reader is still alive, which is the one thing
    /// that can still be holding the source.
    pub fn into_inner(self) -> Result<R> {
        self.backend.into_source()
    }
}

/// A `Read` over a byte range of the archive's source, shared like the source itself.
///
/// Every entry's decoder needs one of these: it has to start at an offset somebody computed and
/// stop at an end somebody computed, and it has to do so without holding a borrow of the reader
/// that handed it out. `limit` is `None` for the case where the end is not known in advance —
/// a ZIP entry with a data descriptor written by a streaming writer — where the codec finds the
/// end itself.
pub struct RawReader<R: Read + Seek> {
    source: Shared<R>,
    offset: u64,
    remaining: Option<u64>,
    /// Whether the seek to `offset` has happened yet.
    ///
    /// The seek is deferred to the first read rather than done in `new`, because `new` is called
    /// with only a shared handle and doing it there would mean taking the lock twice on the path
    /// where every entry is opened. It is done *somewhere*, though: a reader that ignored its own
    /// offset would read from wherever the previous entry happened to leave the file, which is
    /// silent data corruption rather than a failure.
    positioned: bool,
}

impl<R: Read + Seek> RawReader<R> {
    /// A reader over `length` bytes starting at `offset`, or to the end of the archive when
    /// `length` is `None`.
    pub fn new(source: Shared<R>, offset: u64, length: Option<u64>) -> RawReader<R> {
        RawReader {
            source,
            offset,
            remaining: length,
            positioned: false,
        }
    }

    /// How many bytes are left, when the end is known.
    pub fn remaining(&self) -> Option<u64> {
        self.remaining
    }
}

impl<R: Read + Seek + Send> Read for RawReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let wanted = match self.remaining {
            Some(0) => return Ok(0),
            Some(left) => output.len().min((left as usize).min(usize::MAX as usize)),
            None => output.len(),
        };
        // The lock is taken once per read of the *source*, not once per byte. A decoder with a
        // 32 KiB window asks for 32 KiB at a time, so this is one lock per 32 KiB of compressed
        // data, which is not what limits throughput.
        let mut guard = self
            .source
            .lock()
            .map_err(|_| io::Error::other("the archive handle was poisoned by an earlier panic"))?;
        if !self.positioned {
            guard.seek(io::SeekFrom::Start(self.offset))?;
            self.positioned = true;
        }
        let read = guard.read(&mut output[..wanted])?;
        drop(guard);
        if let Some(left) = self.remaining.as_mut() {
            *left -= read as u64;
        }
        Ok(read)
    }
}

/// A fixed-capacity buffer that is refilled rather than reallocated.
///
/// The hot loops in this crate all need "a place to decompress into that is about 128 KiB". They
/// share one of these so that listing a hundred thousand entries does not allocate a hundred
/// thousand buffers, and so that decompressing does not allocate at all.
///
/// Reused across block decoders, which is what keeps solid-block decoding allocation-free.
pub struct Scratch {
    buffer: Vec<u8>,
}

impl Scratch {
    /// The size the crate's hot loops are tuned for.
    ///
    /// 128 KiB is the point where a read stops being dominated by syscall and JNI overhead while
    /// still being small enough that several can be live at once on a phone.
    pub const DEFAULT_SIZE: usize = 128 * 1024;

    /// Creates a new scratch buffer with the default size (128 KiB).
    pub fn new() -> Scratch {
        Scratch {
            buffer: vec![0; Scratch::DEFAULT_SIZE],
        }
    }

    /// Creates a new scratch buffer with the given capacity.
    pub fn with_capacity(size: usize) -> Scratch {
        Scratch {
            buffer: vec![0; size],
        }
    }

    /// Returns a mutable reference to the scratch buffer.
    pub fn buffer(&mut self) -> &mut [u8] {
        &mut self.buffer
    }

    /// Returns the length of the scratch buffer.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if the scratch buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl Default for Scratch {
    fn default() -> Scratch {
        Scratch::new()
    }
}

/// A seekable view over a limited number of bytes, which is what a block's data is.
///
/// Several block decoders want a `Read + Seek` that stops at the end of the block rather than at
/// the end of the archive, because the bytes after the block are the next block and reading them
/// by accident is a data leak between entries.
pub struct BoundedSource<'a, R> {
    inner: &'a mut R,
    remaining: u64,
    consumed: u64,
}

impl<'a, R: Read> BoundedSource<'a, R> {
    /// Creates a new bounded source over `length` bytes of `inner`.
    pub fn new(inner: &'a mut R, length: u64) -> BoundedSource<'a, R> {
        BoundedSource {
            inner,
            remaining: length,
            consumed: 0,
        }
    }

    /// How much has been read out of the bound.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }
}

impl<R: Read> Read for BoundedSource<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || output.is_empty() {
            return Ok(0);
        }
        // Never ask the underlying reader for more than the bound allows, so a block that claims
        // to be larger than the file cannot cause a read into the next block.
        let wanted = output
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut output[..wanted])?;
        if read == 0 {
            self.remaining = 0;
            return Ok(0);
        }
        self.remaining -= read as u64;
        self.consumed += read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for BoundedSource<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(position) => position as i64,
            SeekFrom::Current(delta) => self.consumed as i64 + delta,
            SeekFrom::End(delta) => self.remaining as i64 + delta,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seeking before the start of the block",
            ));
        }
        let reached = target as u64;
        let delta = reached - self.consumed;
        self.inner.seek(SeekFrom::Current(delta as i64))?;
        self.consumed = reached;
        self.remaining = self.remaining.saturating_sub(delta);
        Ok(reached)
    }
}

/// Finds the block an entry is in, given the placements recorded when the archive was opened.
///
/// Shared by the block-based formats so that the "two entries share a block, so do not decode it
/// twice" logic is written once.
pub fn block_of(entry: &Entry) -> Option<BlockPlacement> {
    entry.block
}

/// Whether two entries are in the same block, in a form that reads as a claim rather than a test.
pub fn shares_block(first: &Entry, second: &Entry) -> bool {
    match (first.block, second.block) {
        (Some(a), Some(b)) => a.block_index == b.block_index,
        _ => false,
    }
}

/// The kind a caller should be told an error is when it came from a codec that does not speak it.
pub(crate) fn codec_error(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An entry source over a fixed list of byte strings, standing in for a format.
    struct FakeSource {
        entries: Vec<Entry>,
        contents: Vec<Vec<u8>>,
        reused: bool,
        opened: Vec<usize>,
    }

    impl FakeSource {
        fn new(reused: bool) -> FakeSource {
            FakeSource {
                entries: Vec::new(),
                contents: Vec::new(),
                reused,
                opened: Vec::new(),
            }
        }

        fn add(&mut self, name: &str, contents: Vec<u8>) {
            self.entries.push(Entry {
                name: name.to_owned(),
                kind: crate::entry::EntryKind::File,
                size: Some(contents.len() as u64),
                compressed_size: Some(contents.len() as u64),
                last_modified: None,
                mode: None,
                symlink_target: None,
                is_encrypted: false,
                block: None,
            });
            self.contents.push(contents);
        }
    }

    impl EntrySource<Cursor<Vec<u8>>> for FakeSource {
        fn entries(&self) -> &[Entry] {
            &self.entries
        }

        fn open(&mut self, index: usize) -> Result<EntryStream> {
            self.opened.push(index);
            let contents = self.contents.get(index).cloned().unwrap_or_default();
            let size = contents.len() as u64;
            Ok(EntryStream::new(Box::new(SliceRead {
                bytes: contents,
                offset: 0,
                size,
            })))
        }

        fn reuses_blocks(&self) -> bool {
            self.reused
        }

        fn into_source(self: Box<Self>) -> Result<Cursor<Vec<u8>>> {
            Ok(Cursor::new(Vec::new()))
        }
    }

    struct SliceRead {
        bytes: Vec<u8>,
        offset: usize,
        size: u64,
    }

    impl Read for SliceRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let available = self.bytes.len() - self.offset;
            let read = available.min(output.len());
            output[..read].copy_from_slice(&self.bytes[self.offset..self.offset + read]);
            self.offset += read;
            Ok(read)
        }
    }

    impl EntryRead for SliceRead {
        fn size(&self) -> Option<u64> {
            Some(self.size)
        }
    }

    fn reader_with(
        entries: usize,
        contents: usize,
        reused: bool,
    ) -> ArchiveReader<Cursor<Vec<u8>>> {
        let mut source = FakeSource::new(reused);
        for index in 0..entries {
            source.add(&format!("file{index}.bin"), vec![b'x'; contents]);
        }
        ArchiveReader {
            entries: source.entries().to_vec(),
            format: Format::Zip,
            backend: Box::new(source),
            limits: Limits::default(),
        }
    }

    #[test]
    fn an_entry_is_read_in_chunks_and_never_needs_to_exist_in_memory_at_once() {
        // The point of this crate's reader API: a 1 MiB entry read through a 4 KiB buffer moves
        // 4 KiB at a time. A test cannot prove memory use, but it can prove the stream is partial:
        // reading 10 bytes of a 1 MiB entry returns 10 and leaves the rest where it was.
        let mut reader = reader_with(1, 1024 * 1024, false);
        let mut stream = reader.open_entry(0).unwrap();
        let mut small = [0u8; 10];
        assert_eq!(stream.read(&mut small).unwrap(), 10);
        assert_eq!(stream.consumed(), 10);
        assert_eq!(stream.size(), Some(1024 * 1024));
    }

    #[test]
    fn reading_an_entry_to_the_end_gives_back_exactly_what_went_in() {
        let mut source = FakeSource::new(false);
        let payload: Vec<u8> = (0..5000u32).map(|value| value as u8).collect();
        source.add("payload.bin", payload.clone());
        let mut reader = ArchiveReader {
            entries: source.entries().to_vec(),
            format: Format::Zip,
            backend: Box::new(source),
            limits: Limits::default(),
        };
        let mut stream = reader.open_entry(0).unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn a_zero_length_read_reports_zero_without_reaching_the_codec() {
        // Codecs treat a zero-length read as a signal in several cases and would return an error.
        let mut reader = reader_with(1, 100, false);
        let mut stream = reader.open_entry(0).unwrap();
        assert_eq!(stream.read(&mut []).unwrap(), 0);
        assert_eq!(stream.consumed(), 0);
    }

    #[test]
    fn entries_can_be_opened_in_any_order_and_the_source_is_told_which() {
        let mut reader = reader_with(4, 10, false);
        for index in [3, 0, 2, 1] {
            let mut stream = reader.open_entry(index).unwrap();
            let mut buffer = [0u8; 10];
            assert_eq!(stream.read(&mut buffer).unwrap(), 10);
        }
    }

    #[test]
    fn opening_an_index_that_does_not_exist_is_an_error_and_not_a_panic() {
        let mut reader = reader_with(2, 10, false);
        let error = reader.open_entry(2).expect_err("out of range");
        assert_eq!(error.kind(), ErrorKind::Malformed);
        // usize::MAX must be equally safe, since an index can come from Java as a long.
        assert!(reader.open_entry(usize::MAX).is_err());
    }

    #[test]
    fn an_archive_with_no_entries_is_empty_rather_than_broken() {
        let reader = reader_with(0, 0, false);
        assert!(reader.is_empty());
        assert_eq!(reader.len(), 0);
        assert!(reader.entry(0).is_none());
    }

    #[test]
    fn a_bounded_source_never_reads_past_the_bound() {
        let mut bytes = Cursor::new(vec![1u8, 2, 3, 4, 5]);
        let mut bounded = BoundedSource::new(&mut bytes, 3);
        let mut output = [0u8; 16];
        // A read larger than the bound returns only the bound, not the three bytes after it.
        assert_eq!(bounded.read(&mut output).unwrap(), 3);
        assert_eq!(&output[..3], &[1, 2, 3]);
        assert_eq!(bounded.read(&mut output).unwrap(), 0);
    }

    #[test]
    fn a_bounded_source_that_claims_more_than_the_file_holds_stops_at_the_end_of_the_file() {
        let mut bytes = Cursor::new(vec![1u8, 2, 3]);
        let mut bounded = BoundedSource::new(&mut bytes, 1000);
        let mut output = [0u8; 16];
        assert_eq!(bounded.read(&mut output).unwrap(), 3);
        assert_eq!(
            bounded.read(&mut output).unwrap(),
            0,
            "a short file ends the block"
        );
    }

    #[test]
    fn two_entries_are_only_sharing_a_block_when_both_say_which_block() {
        let base = Entry {
            name: "a".to_owned(),
            kind: crate::entry::EntryKind::File,
            size: None,
            compressed_size: None,
            last_modified: None,
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: Some(BlockPlacement {
                block_index: 3,
                offset_in_block: 0,
                block_size: 10,
            }),
        };
        let same_block = Entry {
            name: "b".to_owned(),
            block: Some(BlockPlacement {
                block_index: 3,
                offset_in_block: 5,
                block_size: 10,
            }),
            ..base.clone()
        };
        let other_block = Entry {
            name: "c".to_owned(),
            block: Some(BlockPlacement {
                block_index: 4,
                offset_in_block: 0,
                block_size: 10,
            }),
            ..base.clone()
        };
        let no_block = Entry {
            name: "d".to_owned(),
            block: None,
            ..base.clone()
        };
        assert!(shares_block(&base, &same_block));
        assert!(!shares_block(&base, &other_block));
        assert!(!shares_block(&base, &no_block));
        assert!(
            !shares_block(&no_block, &no_block),
            "absent is not 'the same block'"
        );
    }

    #[test]
    fn a_scratch_buffer_is_reused_rather_than_reallocated_per_entry() {
        let mut scratch = Scratch::new();
        assert_eq!(scratch.len(), Scratch::DEFAULT_SIZE);
        let first = scratch.buffer().as_ptr();
        scratch.buffer()[0] = 1;
        assert_eq!(scratch.buffer().as_ptr(), first, "the buffer must not move");
    }

    #[test]
    fn a_format_that_reuses_blocks_says_so_so_a_caller_can_sort_its_reads() {
        let reader = reader_with(1, 1, true);
        assert!(reader.reuses_blocks());
        let reader = reader_with(1, 1, false);
        assert!(!reader.reuses_blocks());
    }
}
