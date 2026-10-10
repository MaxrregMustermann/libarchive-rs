//! Writing 7Z archives.
//!
//! **Owned by Team `container`.** A pending job, and the one that matters most: 7Z is the only
//! format here with solid blocks, so it is the format the whole index-addressed API exists for.

use std::io::{Seek, Write};

use crate::error::{Error, Result};
use crate::writer::{EntryWrite, EntryWriter, NewEntry, WriteOptions};

/// Writes a 7Z archive.
pub struct SevenZWriter<W: Write + Seek> {
    sink: Option<W>,
}

impl<W: Write + Seek> SevenZWriter<W> {
    pub fn create<W2: Write + Seek + 'static>(sink: W2, _options: &WriteOptions) -> Result<Box<dyn EntryWriter<W2>>> {
        Err(Error::unimplemented("7Z writing is Team container's work in progress"))
    }
}

impl<W: Write + Seek> EntryWriter<W> for SevenZWriter<W> {
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
