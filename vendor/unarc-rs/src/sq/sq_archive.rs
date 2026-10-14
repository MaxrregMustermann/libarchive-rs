use std::io::{Read, Seek};

use crate::error::{ArchiveError, Result};

use super::header::Header;

pub struct SqArchive<T: Read + Seek> {
    reader: T,
    read_header: bool,
    /// The listed entry's decompressed data, waiting for [`SqArchive::read`] to hand it back.
    entry_data: Option<Vec<u8>>,
}

impl<T: Read + Seek> SqArchive<T> {
    pub fn new(reader: T) -> Result<Self> {
        Ok(Self {
            reader,
            read_header: false,
            entry_data: None,
        })
    }

    pub fn skip(&mut self, _header: &Header) -> Result<()> {
        // just 1 file in the archive
        Ok(())
    }

    /// Hands back the listed entry's decompressed data.
    ///
    /// An SQ archive holds exactly one file, and [`SqArchive::get_next_entry`] decompresses it:
    /// the SQ format records no size anywhere, so decompressing is the only way to know one, and
    /// listing is where a size is needed. `read` therefore returns what listing already produced
    /// rather than decoding the stream a second time.
    pub fn read(&mut self, _header: &Header) -> Result<Vec<u8>> {
        self.entry_data.take().ok_or_else(|| {
            ArchiveError::io_error(
                "SQ archive has no entry data: either no entry was listed, or it was already read",
            )
        })
    }

    /// The listed entry's decompressed size, or 0 when nothing has been listed yet.
    pub fn entry_size(&self) -> u64 {
        self.entry_data.as_ref().map_or(0, |data| data.len() as u64)
    }

    pub fn get_next_entry(&mut self) -> Result<Option<Header>> {
        if self.read_header {
            return Ok(None);
        }
        self.read_header = true;
        let header = Header::load_from(&mut self.reader)?;

        let mut compressed_buffer = Vec::new();
        self.reader.read_to_end(&mut compressed_buffer)?;

        let data = crate::arc::unsqueeze::unsqueeze(&compressed_buffer)?;

        let mut checksum: u16 = 0;
        for byte in data.iter() {
            checksum = checksum.wrapping_add(*byte as u16);
        }

        if checksum != header.checksum {
            return Err(ArchiveError::crc_mismatch(
                &header.name,
                header.checksum as u32,
                checksum as u32,
            ));
        }

        self.entry_data = Some(data);
        Ok(Some(header))
    }
}

/// Hyper uses a non standard checksum algorithm.
pub fn calculate_checksum(data: &[u8]) -> u32 {
    let mut checksum: u32 = 0;
    for byte in data {
        checksum = checksum.wrapping_add(*byte as u32).rotate_left(1);
    }
    checksum
}
