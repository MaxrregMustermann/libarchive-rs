//! Writing the single-stream compressors.
//!
//! **Owned by Team `codecs`.** A pending job.

use std::io::{Seek, Write};

use crate::error::{Error, Result};
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

/// Writes a single-stream compressed file: GZ, BZ2, XZ, Z, LZ4, Zstd or Brotli.
pub struct SingleStreamWriter<W: Write + Seek> {
    sink: Option<W>,
    compression: crate::format::Compression,
}

impl<W: Write + Seek> SingleStreamWriter<W> {
    pub fn create<W2: Write + Seek + 'static>(sink: W2, options: &WriteOptions) -> Result<Box<dyn EntryWriter<W2>>> {
        Err(Error::unimplemented(format!(
            "writing {} is Team codecs' work in progress",
            options.compression
        )))
    }
}

impl<W: Write + Seek> EntryWriter<W> for SingleStreamWriter<W> {
    fn begin<'a>(
        &'a mut self,
        _entry: &NewEntry,
        _options: &WriteOptions,
    ) -> Result<Box<dyn EntryWrite + 'a>> {
        Err(Error::unimplemented("pending"))
    }

    fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    fn into_sink(self: Box<Self>) -> W {
        self.sink.expect("a writer that never began still owns its sink")
    }
}
