//! Bridges between the archive library and the `java.nio` channels the app already has.
//!
//! An archive can live on FTP, SMB or a document provider, so it cannot be assumed to be a file on
//! local storage. Everything here reads and writes through a Java object instead.
//!
//! Derived from the native bridge in the app this library replaces, which read and wrote through
//! the same four `SeekableByteChannel` methods. The signatures of those four methods are part of
//! the contract with `ArchiveChannel` on the Kotlin side: getting one wrong is not a compile error
//! on either side, it is a `NoSuchMethodError` on the first read, which aborts the process.

use std::io::{self, Read, Seek, SeekFrom, Write};

use jni::objects::{GlobalRef, JObject};
use jni::sys::jint;
use jni::{AttachGuard, JNIEnv, JavaVM};

/// How many local references each bridge call is allowed to keep alive at once.
///
/// A JNI local reference lives until the native method that created it returns, so a call that
/// reads a whole archive would otherwise accumulate one per read and overflow the 512 slot table,
/// which aborts the process rather than failing the call. Each operation gets a frame of its own
/// instead, so its references are released when that operation ends rather than when the whole
/// method does. A loop that makes more references than this in one operation has to delete them
/// as it goes.
const LOCAL_FRAME: i32 = 16;

/// Attaches the calling thread and hands back the guard, which detaches on drop.
fn attach(vm: &JavaVM) -> io::Result<AttachGuard<'_>> {
    vm.attach_current_thread()
        .map_err(|error| io::Error::other(error.to_string()))
}

/// JNI failures are reported to Rust as I/O failures, which is what the callers do with them.
///
/// The failure is usually a pending Java exception — a channel that throws `IOException` mid-read
/// leaves it pending and the error surfaces at the next JNI call. Mapping it here means the
/// archive library sees an ordinary `io::Error` and the caller turns it into the `IOException`
/// every Kotlin-side handler already catches.
pub fn java_error(error: jni::errors::Error) -> io::Error {
    io::Error::other(error.to_string())
}

/// A `Read + Seek` over the Java object passed to `nativeOpen`.
pub struct ChannelReader {
    vm: JavaVM,
    channel: GlobalRef,
    position: u64,
}

impl ChannelReader {
    pub fn new(env: &mut JNIEnv, channel: &JObject) -> jni::errors::Result<ChannelReader> {
        Ok(ChannelReader {
            vm: env.get_java_vm()?,
            channel: env.new_global_ref(channel)?,
            position: 0,
        })
    }
}

/// The signature of `SeekableByteChannel.read`, which takes a buffer and not a byte array.
///
/// The app's channel is a `java8.nio` one, so going through it means going through
/// `java.nio.ByteBuffer`. Getting this wrong is not a compile error on either side: the JNI lookup
/// fails at runtime, on the first read, as a `NoSuchMethodError`.
const CHANNEL_READ: &str = "(Ljava/nio/ByteBuffer;)I";

/// The matching signature of `SeekableByteChannel.write`, which reports how much it took.
const CHANNEL_WRITE: &str = "(Ljava/nio/ByteBuffer;)I";

impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut guard = attach(&self.vm)?;
        let wanted = buffer.len();
        // SAFETY: the pointer is the caller's own slice, `wanted` is its length, and the frame is
        // closed before this returns, before the slice can be moved or resized.
        let read = unsafe {
            guard.with_local_frame(LOCAL_FRAME, |env| {
                let channel = env.new_local_ref(self.channel.as_obj())?;
                // A direct buffer over the caller's own memory, so the bytes land where Rust expects
                // them without a second copy back. The length is what the channel is told it may
                // fill, which is not the whole slice: the caller may have handed over a larger one.
                let direct = env.new_direct_byte_buffer(buffer.as_mut_ptr(), wanted)?;
                env.call_method(&channel, "read", CHANNEL_READ, &[(&direct).into()])?
                    .i()
            })
        }
        .map_err(java_error)?;
        if read > 0 {
            self.position += read as u64;
        }
        // A channel returns 0 at the end of the stream, which is what std::io expects too.
        Ok(read.max(0) as usize)
    }
}

impl Seek for ChannelReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(offset) => self.position as i64 + offset,
            SeekFrom::End(offset) => self.size()? as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the archive",
            ));
        }
        let mut guard = attach(&self.vm)?;
        let new_position = guard
            .with_local_frame(LOCAL_FRAME, |env| {
                let channel = env.new_local_ref(self.channel.as_obj())?;
                env.call_method(
                    &channel,
                    "position",
                    "(J)J",
                    &[jni::objects::JValue::Long(target)],
                )?
                .j()
            })
            .map_err(java_error)?;
        if new_position < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the archive provider refused to seek",
            ));
        }
        self.position = new_position as u64;
        Ok(self.position)
    }
}

impl ChannelReader {
    fn size(&mut self) -> io::Result<u64> {
        let mut guard = attach(&self.vm)?;
        let size = guard
            .with_local_frame(LOCAL_FRAME, |env| {
                let channel = env.new_local_ref(self.channel.as_obj())?;
                env.call_method(&channel, "size", "()J", &[])?.j()
            })
            .map_err(java_error)?;
        Ok(size.max(0) as u64)
    }
}

/// A `Write + Seek` over the Java channel passed to `nativeCreate`.
///
/// Only the 7Z writer needs the seek, to fill in the signature header it leaves a placeholder for.
/// The position is tracked here rather than asked of the channel, because `SeekFrom::Current` and
/// `SeekFrom::End` have to be resolved against where the writer is, and the channel only offers an
/// absolute `position(long)`.
pub struct ChannelWriter {
    vm: JavaVM,
    channel: GlobalRef,
    position: u64,
    buffer: Vec<u8>,
}

impl ChannelWriter {
    pub fn new(env: &mut JNIEnv, channel: &JObject) -> jni::errors::Result<ChannelWriter> {
        Ok(ChannelWriter {
            vm: env.get_java_vm()?,
            channel: env.new_global_ref(channel)?,
            position: 0,
            buffer: vec![0; crate::CHUNK_SIZE],
        })
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < data.len() {
            let end = (written + self.buffer.len()).min(data.len());
            let chunk = &data[written..end];
            let mut guard = attach(&self.vm)?;
            // A frame per chunk, because writing an archive is one JNI call that pushes every byte
            // through here, and one local reference set per chunk would overflow the table.
            let accepted = guard
                .with_local_frame(LOCAL_FRAME, |env| {
                    let channel = env.new_local_ref(self.channel.as_obj())?;
                    let buffer = wrap(env, chunk)?;
                    // A channel reports what it took rather than throwing when it takes less, so a
                    // short write has to be resumed rather than treated as done.
                    env.call_method(&channel, "write", CHANNEL_WRITE, &[(&buffer).into()])?
                        .i()
                })
                .map_err(java_error)?;
            if accepted <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "The channel accepted none of the archive",
                ));
            }
            written += accepted as usize;
            self.position += accepted as u64;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `ByteBuffer.wrap`, which is how a read-only buffer over existing bytes is made.
fn wrap<'a>(env: &mut JNIEnv<'a>, data: &[u8]) -> jni::errors::Result<JObject<'a>> {
    let array = env.byte_array_from_slice(data)?;
    let buffer = env.call_static_method(
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
        &[(&array).into()],
    )?;
    buffer.l()
}

impl Seek for ChannelWriter {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(offset) => self.position as i64 + offset,
            SeekFrom::End(offset) => self.size()? as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the archive",
            ));
        }
        let mut guard = attach(&self.vm)?;
        let new_position = guard
            .with_local_frame(LOCAL_FRAME, |env| {
                let channel = env.new_local_ref(self.channel.as_obj())?;
                env.call_method(
                    &channel,
                    "position",
                    "(J)J",
                    &[jni::objects::JValue::Long(target)],
                )?
                .j()
            })
            .map_err(java_error)?;
        if new_position < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the archive provider refused to seek",
            ));
        }
        self.position = new_position as u64;
        Ok(self.position)
    }
}

impl ChannelWriter {
    fn size(&mut self) -> io::Result<u64> {
        let mut guard = attach(&self.vm)?;
        let size = guard
            .with_local_frame(LOCAL_FRAME, |env| {
                let channel = env.new_local_ref(self.channel.as_obj())?;
                env.call_method(&channel, "size", "()J", &[])?.j()
            })
            .map_err(java_error)?;
        Ok(size.max(0) as u64)
    }
}

/// A `Read` that pulls the entry currently being written out of the app.
pub struct EntrySource {
    vm: JavaVM,
    source: GlobalRef,
    /// The largest read the Rust side will ask for, which caps the Java array it allocates.
    buffer: Vec<u8>,
    /// Scratch for the bytes Java handed back, kept between calls so each one does not allocate.
    chunk: Vec<u8>,
}

impl EntrySource {
    pub fn new(env: &mut JNIEnv, source: &JObject) -> jni::errors::Result<EntrySource> {
        Ok(EntrySource {
            vm: env.get_java_vm()?,
            source: env.new_global_ref(source)?,
            buffer: vec![0; crate::CHUNK_SIZE],
            chunk: Vec::new(),
        })
    }
}

impl Read for EntrySource {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut guard = attach(&self.vm)?;
        let wanted = output.len().min(self.buffer.len());
        // The bytes are read into a Java array and copied back out, because that is what
        // `InputStream.read` takes. Copying them back is not optional: `new_byte_array` hands over
        // an array Java fills in place, and nothing on this side ever sees those bytes unless they
        // are read out again. Passing a buffer that was already filled and then copying from it
        // hands over whatever was there before, which is how this once wrote zeros.
        let read = guard
            .with_local_frame(LOCAL_FRAME, |env| {
                let source = env.new_local_ref(self.source.as_obj())?;
                // Sized to what the caller asked for, rather than to the scratch buffer. Asking for
                // more than is wanted means allocating a larger array for every call.
                let array = env.new_byte_array(wanted as jint)?;
                // `InputStream.read(byte[], int, int)`, which is the one overload whose signature is
                // fixed by the platform rather than by this crate, so it does not have to change in
                // step with the Kotlin side.
                let read = env
                    .call_method(
                        &source,
                        "read",
                        "([BII)I",
                        &[
                            (&array).into(),
                            jni::objects::JValue::Int(0),
                            jni::objects::JValue::Int(wanted as jint),
                        ],
                    )?
                    .i()?
                    .max(0) as usize;
                let read = read.min(wanted);
                if read > 0 {
                    let mut scratch = std::mem::take(&mut self.chunk);
                    scratch.resize(read, 0);
                    let result = {
                        let signed = unsafe {
                            std::slice::from_raw_parts_mut(scratch.as_mut_ptr() as *mut i8, read)
                        };
                        let result = env.get_byte_array_region(&array, 0, signed);
                        if result.is_ok() {
                            output[..read].copy_from_slice(&scratch);
                        }
                        result
                    };
                    // Kept for the next call rather than reallocated per call.
                    self.chunk = scratch;
                    result?;
                }
                Ok(read)
            })
            .map_err(java_error)?;
        Ok(read)
    }
}
