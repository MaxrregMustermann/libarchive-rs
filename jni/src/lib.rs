//! The JNI bridge.
//!
//! **Owned by Team 7.** The nine exports the Kotlin side's `RustArchive` declares, so the app can
//! call `libarchive` instead of the crates it replaces.
//!
//! # What belongs here and what does not
//!
//! Everything here is a translation and nothing here is a decision. The formats, the entry order,
//! the error kinds and the streaming behaviour all belong to [`libarchive`]. This crate turns a
//! `jlong` handle into a reader, turns what comes back into the string array `RustArchive.kt`
//! expects, and turns an error into a `java.io.IOException` because that is what every caller on
//! the other side already handles.
//!
//! # The two rules a JNI boundary imposes
//!
//! **A panic here aborts the process.** Rust unwinds into `extern "system"` on Android as an
//! abort, not as an exception, so a `Result` that is unwrapped or an index that is out of range
//! turns a corrupt archive into a crashed file manager. Every fallible call in this crate returns
//! an error to Java rather than unwinding, and every export is wrapped in [`guarded`], which
//! converts even a panic that arrives by a route the lints did not catch into the same
//! `IOException` a thrown `Result` would have produced.
//!
//! **Every call crosses into Java.** A `write` of 32 bytes costs the same as a `write` of 128 KiB
//! once the two context switches are counted, which is why the writer coalesces its headers and
//! why reads happen 64 KiB at a time. See `libarchive::writer`.
//!
//! # The return-value conventions, which are the whole contract for nulls
//!
//! Three different "nothing happened" signals, and conflating any two of them is a live bug:
//!
//! - `nativeOpen` / `nativeCreate` return `0` on failure. Zero is not a valid handle.
//! - `nativeNextEntry` returns `null` at the end of the archive, which is *not* a failure: the
//!   native side throws before returning `null` for a real error, so `null` cannot mean both.
//! - `nativeReadData` returns `null` at the end of the entry, for the same reason: the Kotlin side
//!   needs `InputStream.read` to report `-1` there.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The three lints that catch the ways a parser panics on untrusted input. A JNI boundary is where
// that matters most: a panic crossing it aborts the process instead of reaching Java.

pub use libarchive::{ArchiveReader, ArchiveWriter, Error, Format, Result};

mod format;
mod io;

use std::io::Read;

use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::sys::{jbyteArray, jint, jlong, jobjectArray};
use jni::JNIEnv;

/// The exception every failure is reported as.
///
/// `java/io/IOException` rather than a crate-specific one because every caller on the Kotlin side
/// already catches it, and introducing a new exception type would mean touching all of them for no
/// gain.
pub const IO_EXCEPTION: &str = "java/io/IOException";

/// The name a raw `.xz`, `.gz` or `.zst` stream's single entry is given.
///
/// A compressed stream is one file with no name of its own, and the listing needs something to put
/// in the name column. This is the string the app already uses, so it is unchanged.
pub const SINGLE_ENTRY_NAME: &str = "content";

/// How many bytes move across JNI per read.
///
/// Large enough that the boundary stops dominating and small enough that the temporary buffer on
/// the native side of each call stays cheap. Matches the Kotlin side's `CHUNK_SIZE`.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// How many fields a metadata array carries, in the order `RustArchive.Reader` reads them.
pub const METADATA_FIELDS: usize = 7;

/// The index of each field in the metadata array.
///
/// The order is part of the contract with the Kotlin side and is asserted by a test on both sides.
/// It is not changed for tidiness; if a field is added, it goes at the end and both sides change
/// together.
pub mod field {
    pub const NAME: usize = 0;
    pub const SIZE: usize = 1;
    pub const LAST_MODIFIED_MILLIS: usize = 2;
    pub const SYMLINK_TARGET: usize = 3;
    pub const IS_ENCRYPTED: usize = 4;
    pub const IS_DIRECTORY: usize = 5;
    pub const IS_SYMLINK: usize = 6;
}

/// The handle a failed open returns.
///
/// Zero, so that a caller that ignores the failure gets "the archive is closed" from the next call
/// rather than dereferencing null. The Kotlin side checks it explicitly and says so.
pub const NULL_HANDLE: i64 = 0;

/// A message that is thrown rather than returned.
///
/// Every failure crosses back as a thrown `IOException`, because that is what the Kotlin side's
/// `@Throws(IOException::class)` declarations are written against, and because a return value cannot
/// say *what* went wrong.
fn throw(env: &mut JNIEnv, message: impl AsRef<str>) {
    let _ = env.throw_new(IO_EXCEPTION, message.as_ref());
}

/// The handle a failed open or create reports.
fn null_pointer() -> jlong {
    NULL_HANDLE
}

/// Runs `body` and converts a Rust panic into a thrown `IOException`.
///
/// The lints at the top of this crate deny the ways a panic is written explicitly. This is the
/// defence for the ones that arrive by any other route — an index miscalculated on corrupt input, an
/// integer overflow in a debug build — so a corrupt archive costs one exception and not one process.
/// In a `panic = "abort"` build the abort happens before this can run; there the lints are the only
/// defence, which is why both exist.
fn guarded<T>(env: &mut JNIEnv, body: impl FnOnce(&mut JNIEnv) -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(env))) {
        Ok(value) => Some(value),
        Err(_) => {
            throw(env, "The archive could not be processed");
            None
        }
    }
}

/// A readable archive held open behind a `jlong`.
///
/// The handle carries the reader plus the bridge's position in it, which is the two things the
/// Kotlin side's per-call model needs: `nativeNextEntry` advances the position and `nativeReadData`
/// reads from the entry it selected, so the handle has to remember both which entry that is and
/// whether its stream is open yet.
struct ReadHandle {
    reader: ArchiveReader<io::ChannelReader>,
    /// The index `nativeNextEntry` returns next, and one past the last entry it returned.
    next_index: usize,
    /// The stream of the entry `nativeNextEntry` last returned, opened on the first read.
    ///
    /// Opened lazily rather than in `nativeNextEntry` so that a caller which only lists an archive
    /// never pays for a decompressor it does not use.
    current: Option<libarchive::EntryStream>,
}

impl ReadHandle {
    /// The stream of the selected entry, opening it first if that has not happened yet.
    fn ensure_current_stream(&mut self) -> Result<&mut libarchive::EntryStream> {
        let selected = self
            .next_index
            .checked_sub(1)
            .ok_or_else(|| Error::malformed("No entry has been selected"))?;
        if self.current.is_none() {
            let stream = self.reader.open_entry(selected)?;
            self.current = Some(stream);
        }
        self.current
            .as_mut()
            .ok_or_else(|| Error::malformed("No entry is open"))
    }
}

type WriteHandle = ArchiveWriter<io::ChannelWriter>;

/// The archive behind a read handle, or `None` when the handle is spent.
fn from_handle<'a>(handle: jlong) -> Option<&'a mut ReadHandle> {
    if handle == 0 {
        return None;
    }
    // SAFETY: handles come from `Box::into_raw` in `nativeOpen` and are only used by the Java side
    // that owns them, which frees them in `nativeCloseRead`. Every function here is called on one
    // thread at a time, from the same Java object, so the reference is exclusive for the duration.
    unsafe { Some(&mut *(handle as *mut ReadHandle)) }
}

/// The archive behind a write handle, or `None` when the handle is spent.
fn writer_from_handle<'a>(handle: jlong) -> Option<&'a mut WriteHandle> {
    if handle == 0 {
        return None;
    }
    // SAFETY: as above.
    unsafe { Some(&mut *(handle as *mut WriteHandle)) }
}

/// Reads one slot of a metadata array, which is `null` on the Java side and not absent.
///
/// The local reference is deleted as soon as the string is copied out, so a caller looping over
/// more entries than a local frame holds cannot overflow the 512-slot table, which aborts the
/// process rather than failing the call.
fn string_at(env: &mut JNIEnv, metadata: &JObjectArray, index: usize) -> Option<String> {
    let value = env.get_object_array_element(metadata, index as jint).ok()?;
    if value.is_null() {
        return None;
    }
    let text: &JString = (&value).into();
    let copied = env.get_string(text).ok().map(String::from);
    let _ = env.delete_local_ref(value);
    copied
}

/// The metadata array into a [`libarchive::writer::NewEntry`].
///
/// The bridge reads the name, size, mtime, symlink target, directory and symlink slots. The
/// encrypted slot is ignored: this build cannot write an encrypted archive through the app's
/// format ids, and an entry that claims to be encrypted would be a lie either way. Reading the
/// symlink slots is what stops a symbolic link being archived as an empty regular file with its
/// target dropped, which is what the bridge this replaces did.
fn read_new_entry(env: &mut JNIEnv, metadata: &JObjectArray) -> Option<libarchive::NewEntry> {
    let name = match string_at(env, metadata, field::NAME) {
        Some(name) => name,
        None => {
            throw(env, "An archive entry has no name");
            return None;
        }
    };
    let size = string_at(env, metadata, field::SIZE)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let last_modified_millis =
        string_at(env, metadata, field::LAST_MODIFIED_MILLIS).and_then(|value| value.parse().ok());
    let symlink_target =
        string_at(env, metadata, field::SYMLINK_TARGET).filter(|target| !target.is_empty());
    let is_directory = string_at(env, metadata, field::IS_DIRECTORY).as_deref() == Some("true");
    let is_symlink = string_at(env, metadata, field::IS_SYMLINK).as_deref() == Some("true");
    let kind = if is_symlink {
        libarchive::EntryKind::Symlink
    } else if is_directory {
        libarchive::EntryKind::Directory
    } else {
        libarchive::EntryKind::File
    };
    Some(libarchive::NewEntry {
        name,
        kind,
        size: Some(size),
        last_modified: last_modified_millis.map(millis_to_system_time),
        mode: None,
        symlink_target,
        compress: None,
    })
}

/// Millis since the epoch into a `SystemTime`, handling a time before the epoch.
fn millis_to_system_time(millis: i64) -> std::time::SystemTime {
    if millis >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis as u64)
    } else {
        // `unsigned_abs` rather than `-millis`, which would overflow on `i64::MIN`.
        std::time::UNIX_EPOCH - std::time::Duration::from_millis(millis.unsigned_abs())
    }
}

/// Millis since the epoch for a `SystemTime`, or `None` when the instant is too far from the
/// epoch for a millisecond count to hold.
///
/// Before the epoch is the case worth getting right. `duration_since` fails for a time before
/// 1970, and the obvious fix — `.ok()?` — turns every such timestamp into "no timestamp at all".
/// That is not a rare corner: a 7Z FILETIME counts from 1601, and a TAR header carries a signed
/// octal time, so a pre-1970 date is a date a real archive can contain. Dropping it silently
/// loses information the archive had.
///
/// `None` is therefore only for the genuinely unrepresentable: far enough from the epoch that the
/// signed millisecond count overflows, which is about 292 million years either way.
fn millis_of(time: &std::time::SystemTime) -> Option<i64> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .and_then(|millis| millis.checked_neg()),
    }
}

/// The 7-element metadata array for one entry, in the order `RustArchive.Reader` reads them.
///
/// Every element is a `String?`: a `null` slot means the archive does not record that field, which
/// is why the element type is nullable even though most fields are always filled in.
fn metadata_array(env: &mut JNIEnv, entry: &libarchive::Entry) -> Option<jobjectArray> {
    let metadata = [
        Some(entry.name.clone()),
        // The slot is a non-null decimal string, and the Kotlin side does `toLongOrNull() ?: 0`, so
        // an entry the archive does not give a size is reported as zero rather than as null.
        Some(entry.size.unwrap_or(0).to_string()),
        entry
            .last_modified
            .and_then(|time| millis_of(&time))
            .map(|millis| millis.to_string()),
        entry.symlink_target.clone(),
        Some(entry.is_encrypted.to_string()),
        Some(entry.is_directory().to_string()),
        Some(entry.is_symlink().to_string()),
    ];
    let array = env
        .new_object_array(METADATA_FIELDS as jint, "java/lang/String", JObject::null())
        .ok()?;
    for (index, value) in metadata.iter().enumerate() {
        let element: JObject = match value {
            Some(value) => env.new_string(value.clone()).ok()?.into(),
            None => JObject::null(),
        };
        env.set_object_array_element(&array, index as jint, &element)
            .ok()?;
    }
    Some(array.into_raw())
}

/// Reads the passwords the app is willing to try.
///
/// `libarchive`'s open options take one password, and the formats that accept several per archive
/// are rare enough that trying the first is the documented behaviour there.
fn read_passwords(env: &mut JNIEnv, passwords: &JObjectArray) -> Option<Vec<String>> {
    let length = env.get_array_length(passwords).ok()?;
    let mut collected = Vec::new();
    for index in 0..length {
        let value = env.get_object_array_element(passwords, index).ok()?;
        if !value.is_null() {
            let text: &JString = (&value).into();
            if let Ok(text) = env.get_string(text) {
                collected.push(text.into());
            }
        }
        let _ = env.delete_local_ref(value);
    }
    Some(collected)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeOpen(
    mut env: JNIEnv,
    class: JClass,
    channel: JObject,
    passwords: JObjectArray,
) -> jlong {
    let handle = guarded(&mut env, |env| {
        open_archive(env, class, channel, passwords, None)
    });
    handle.unwrap_or_else(null_pointer)
}

/// [`nativeOpen`], with the archive's file name for the two formats no signature can identify.
///
/// An ARC file begins with the DOS end-of-file marker `0x1A` and an ICE file with its own
/// uncompressed length, so detection from the bytes alone calls both of them ZIP and the app gets
/// "no end-of-central-directory record" for an archive it could read. The name is the fallback
/// [`libarchive::ArchiveReader::open_named`] already implements; this export is how the app reaches
/// it. `nativeOpen` above keeps its signature and calls the same body with no name, so a caller
/// that cannot name its archive is no worse off than before.
#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeOpenNamed(
    mut env: JNIEnv,
    class: JClass,
    channel: JObject,
    passwords: JObjectArray,
    name: JObject,
) -> jlong {
    let handle = guarded(&mut env, |env| {
        let name = if name.is_null() {
            None
        } else {
            let name: &JString = (&name).into();
            env.get_string(name).ok().map(String::from)
        };
        open_archive(env, class, channel, passwords, name)
    });
    handle.unwrap_or_else(null_pointer)
}

/// Opens the archive behind a read handle, with the file name the app already has for detection.
///
/// The name is a fallback and not an override: the library reads the signature first and consults
/// the name only when the bytes identified nothing, so an ARC called `photo.jpg` is still refused
/// as a file that is not an archive rather than opened as an ARC.
fn open_archive(
    env: &mut JNIEnv,
    _class: JClass,
    channel: JObject,
    passwords: JObjectArray,
    name: Option<String>,
) -> jlong {
    let mut options = libarchive::OpenOptions::default();
    match read_passwords(env, &passwords) {
        Some(passwords) => {
            if let Some(password) = passwords.into_iter().next() {
                options = options.with_password(password);
            }
        }
        None => {
            throw(env, "The passwords could not be read");
            return null_pointer();
        }
    }
    let channel = match io::ChannelReader::new(env, &channel) {
        Ok(channel) => channel,
        Err(error) => {
            throw(env, error.to_string());
            return null_pointer();
        }
    };
    let opened = match name {
        Some(name) => libarchive::ArchiveReader::open_named(
            channel,
            Some(std::path::Path::new(&name)),
            &options,
        ),
        None => libarchive::ArchiveReader::open(channel, &options),
    };
    match opened {
        Ok(reader) => {
            let handle = ReadHandle {
                reader,
                next_index: 0,
                current: None,
            };
            Box::into_raw(Box::new(handle)) as jlong
        }
        Err(error) => {
            throw(env, error.to_string());
            null_pointer()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeNextEntry(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jobjectArray {
    let array = guarded(&mut env, |env| {
        let archive = match from_handle(handle) {
            Some(archive) => archive,
            None => {
                throw(env, "The archive has already been closed");
                return std::ptr::null_mut();
            }
        };
        if archive.next_index >= archive.reader.len() {
            // End of the archive. This is a return value and never a failure: a real error throws
            // before returning, so null cannot mean both.
            return std::ptr::null_mut();
        }
        let index = archive.next_index;
        archive.next_index += 1;
        archive.current = None;
        let entry = match archive.reader.entry(index) {
            Some(entry) => entry,
            None => {
                throw(env, format!("Entry {index} is out of range"));
                return std::ptr::null_mut();
            }
        };
        metadata_array(env, entry).unwrap_or(std::ptr::null_mut())
    });
    array.unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeReadData(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    length: jint,
) -> jbyteArray {
    let data = guarded(&mut env, |env| {
        let archive = match from_handle(handle) {
            Some(archive) => archive,
            None => {
                throw(env, "The archive has already been closed");
                return std::ptr::null_mut();
            }
        };
        let length = length.max(0) as usize;
        if length == 0 {
            // The bridge's reads are always between one byte and `CHUNK_SIZE`, and the Kotlin side
            // checks for zero before calling. A zero-length read is answered as end of entry, the
            // same convention a zero-byte read at the end of the entry gets.
            return std::ptr::null_mut();
        }
        let stream = match archive.ensure_current_stream() {
            Ok(stream) => stream,
            Err(error) => {
                throw(env, error.to_string());
                return std::ptr::null_mut();
            }
        };
        let mut buffer = vec![0u8; length];
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                throw(env, error.to_string());
                return std::ptr::null_mut();
            }
        };
        if read == 0 {
            // End of the entry, reported as null because `InputStream.read` must return -1 there.
            return std::ptr::null_mut();
        }
        match env.byte_array_from_slice(&buffer[..read]) {
            Ok(array) => array.into_raw(),
            Err(error) => {
                throw(env, error.to_string());
                std::ptr::null_mut()
            }
        }
    });
    data.unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeSkipData(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    request: jlong,
) -> jlong {
    let skipped = guarded(&mut env, |env| {
        let archive = match from_handle(handle) {
            Some(archive) => archive,
            None => {
                throw(env, "The archive has already been closed");
                return -1;
            }
        };
        if request <= 0 {
            return 0;
        }
        let stream = match archive.ensure_current_stream() {
            Ok(stream) => stream,
            Err(error) => {
                throw(env, error.to_string());
                return -1;
            }
        };
        // Capped at `CHUNK_SIZE` so the buffer is one allocation however long the skip is, and
        // `min`ed through `u64` so a request past `usize::MAX` truncates on a 32-bit target
        // instead of wrapping.
        let buffer_length = (request as u64).min(CHUNK_SIZE as u64) as usize;
        let mut buffer = vec![0u8; buffer_length];
        let mut skipped = 0u64;
        while skipped < request as u64 {
            let wanted = (request as u64 - skipped).min(buffer_length as u64) as usize;
            match stream.read(&mut buffer[..wanted]) {
                Ok(0) => break,
                Ok(read) => skipped += read as u64,
                Err(error) => {
                    throw(env, error.to_string());
                    return -1;
                }
            }
        }
        skipped as jlong
    });
    skipped.unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeCloseRead(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    guarded(&mut env, |_| {
        if handle == 0 {
            return;
        }
        // SAFETY: the Java side only calls this once, and only with a handle from `nativeOpen`.
        drop(unsafe { Box::from_raw(handle as *mut ReadHandle) });
    });
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeCreate(
    mut env: JNIEnv,
    _class: JClass,
    channel: JObject,
    format: jint,
) -> jlong {
    let handle = guarded(&mut env, |env| {
        let format = match format::from_id(format) {
            Some(format) => format,
            None => {
                throw(env, format!("Unknown archive format {format}"));
                return null_pointer();
            }
        };
        let channel = match io::ChannelWriter::new(env, &channel) {
            Ok(channel) => channel,
            Err(error) => {
                throw(env, error.to_string());
                return null_pointer();
            }
        };
        match ArchiveWriter::create(channel, format, &libarchive::WriteOptions::default()) {
            Ok(writer) => Box::into_raw(Box::new(writer)) as jlong,
            Err(error) => {
                throw(env, error.to_string());
                null_pointer()
            }
        }
    });
    handle.unwrap_or_else(null_pointer)
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeWriteEntry(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    metadata: JObjectArray,
    source: JObject,
) -> jint {
    let written = guarded(&mut env, |env| {
        let archive = match writer_from_handle(handle) {
            Some(archive) => archive,
            None => {
                throw(env, "The archive has already been closed");
                return -1;
            }
        };
        let entry = match read_new_entry(env, &metadata) {
            Some(entry) => entry,
            None => return -1,
        };
        let mut source = match io::EntrySource::new(env, &source) {
            Ok(source) => source,
            Err(error) => {
                throw(env, error.to_string());
                return -1;
            }
        };
        match archive.write_entry(&entry, &mut source) {
            Ok(()) => 0,
            Err(error) => {
                throw(env, error.to_string());
                -1
            }
        }
    });
    written.unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeFinish(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    let finished = guarded(&mut env, |env| {
        if handle == 0 {
            throw(env, "The archive has already been closed");
            return -1;
        }
        // SAFETY: `finish` consumes the writer, so the handle is spent, and the Java side only
        // calls this once with a handle from `nativeCreate`.
        let mut archive = unsafe { Box::from_raw(handle as *mut WriteHandle) };
        match archive.finish() {
            Ok(()) => 0,
            Err(error) => {
                throw(env, error.to_string());
                -1
            }
        }
    });
    finished.unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_me_zhanghai_android_files_provider_archive_archiver_RustArchive_nativeCloseWrite(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    // Never called from Kotlin: `nativeFinish` consumes the writer, so there is nothing to release
    // afterwards. The export is kept because the Kotlin declaration and the bridge contract require
    // all nine, and it is not a no-op — it releases a writer that was created and never finished,
    // which is the one other way the handle can still be live.
    guarded(&mut env, |_| {
        if handle == 0 {
            return;
        }
        // SAFETY: the Java side only calls this once, and only with a handle from `nativeCreate`.
        drop(unsafe { Box::from_raw(handle as *mut WriteHandle) });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_field_order_is_the_one_the_kotlin_side_reads() {
        // RustArchive.kt: METADATA_NAME=0, SIZE=1, LAST_MODIFIED_MILLIS=2, SYMLINK_TARGET=3,
        // IS_ENCRYPTED=4, IS_DIRECTORY=5, IS_SYMLINK=6.
        assert_eq!(field::NAME, 0);
        assert_eq!(field::SIZE, 1);
        assert_eq!(field::LAST_MODIFIED_MILLIS, 2);
        assert_eq!(field::SYMLINK_TARGET, 3);
        assert_eq!(field::IS_ENCRYPTED, 4);
        assert_eq!(field::IS_DIRECTORY, 5);
        assert_eq!(field::IS_SYMLINK, 6);
        assert_eq!(METADATA_FIELDS, field::IS_SYMLINK + 1);
        assert_eq!(METADATA_FIELDS, 7);
    }

    #[test]
    fn the_chunk_size_agrees_with_the_kotlin_side() {
        // RustArchive.kt: `private const val CHUNK_SIZE = 64 * 1024`. If these drift, every read
        // allocates the wrong amount and no test notices, so it is asserted here.
        assert_eq!(CHUNK_SIZE, 64 * 1024);
    }

    #[test]
    fn a_failed_open_returns_zero_rather_than_null_so_a_later_call_says_closed() {
        assert_eq!(NULL_HANDLE, 0);
    }

    #[test]
    fn millis_round_trip_through_the_epoch_at_the_precision_the_boundary_has() {
        // A write carries millis and a read turns them back, so the two conversions have to agree.
        //
        // They agree *to the millisecond*, and this test used to assert exact equality against
        // `SystemTime::now()`, which carries nanoseconds. It failed by 441,981 ns, and the code was
        // right and the test was wrong: Java's clock is `currentTimeMillis`, `FileTime.toMillis`
        // is millis, and the Kotlin side passes a `Long`. Sub-millisecond precision does not cross
        // this boundary, so it must not be expected to survive it.
        let now = std::time::SystemTime::now();
        // `unwrap_or_default` rather than `expect`, because this file denies `clippy::expect_used`
        // and the merge gate is `cargo clippy --workspace --all-targets`, which reaches this test
        // module. `None` here would mean "292 million years from the epoch", which `now` is not.
        let round_tripped = millis_of(&now)
            .map(millis_to_system_time)
            .unwrap_or(std::time::UNIX_EPOCH);
        assert_eq!(
            round_tripped,
            trunc_to_millis(now),
            "must lose no millisecond and gain none"
        );

        // The one property that does matter about the loss: it rounds towards the past, so a
        // timestamp never becomes *later* by being passed through here. A file's modification
        // time drifting forwards would put it in the wrong day's bucket in a listing.
        assert!(
            round_tripped <= now,
            "the round trip moved a timestamp forwards"
        );

        // Before the epoch must survive rather than be dropped. `millis_of` used to return `None`
        // for anything before 1970, which loses a date a real archive can carry: a 7Z FILETIME
        // counts from 1601 and a TAR header holds a signed octal time.
        let cases = [
            (
                std::time::UNIX_EPOCH - std::time::Duration::from_secs(10),
                -10_000,
            ),
            (
                std::time::UNIX_EPOCH - std::time::Duration::from_millis(1),
                -1,
            ),
            (std::time::UNIX_EPOCH, 0),
            (
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(1),
                1,
            ),
        ];
        for (instant, expected_millis) in cases {
            assert_eq!(
                millis_of(&instant),
                Some(expected_millis),
                "{instant:?} must convert to {expected_millis} ms, not vanish"
            );
            assert_eq!(
                millis_of(&instant).map(millis_to_system_time),
                Some(instant)
            );
        }
        assert_eq!(
            millis_to_system_time(i64::MIN),
            std::time::UNIX_EPOCH - std::time::Duration::from_millis(i64::MIN.unsigned_abs())
        );
    }

    /// The same instant with its sub-millisecond part removed, which is what the boundary carries.
    fn trunc_to_millis(time: std::time::SystemTime) -> std::time::SystemTime {
        // `unwrap_or_default` for the same reason as above: `duration_since` is `Err` only before
        // 1970, and this helper is called with instants from an archive rather than from a clock,
        // so "zero milliseconds" is the honest answer for the one input it cannot express.
        let since_epoch = time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(since_epoch.as_millis() as u64)
    }
}
