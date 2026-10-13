use crc32fast::Hasher;
use std::io::{Cursor, Read, Seek, SeekFrom};

use crate::error::{ArchiveError, Result};
use crate::ha::asc::decompress_asc;

use super::header::{ArchiveHeader, CompressionMethod, FileHeader};
use super::hsc::decompress_hsc;

/// HA Archive reader
pub struct HaArchive<T: Read + Seek> {
    reader: T,
    pub file_count: u16,
    files_read: u16,
}

impl<T: Read + Seek> HaArchive<T> {
    pub fn new(mut reader: T) -> Result<Self> {
        let header = ArchiveHeader::load_from(&mut reader)?;

        Ok(Self {
            reader,
            file_count: header.file_count,
            files_read: 0,
        })
    }

    pub fn get_next_entry(&mut self) -> Result<Option<FileHeader>> {
        if self.files_read >= self.file_count {
            return Ok(None);
        }

        let header = FileHeader::load_from(&mut self.reader)?;
        self.files_read += 1;
        Ok(Some(header))
    }

    pub fn skip(&mut self, header: &FileHeader) -> Result<()> {
        self.reader
            .seek(SeekFrom::Current(header.compressed_size as i64))?;
        Ok(())
    }

    pub fn read(&mut self, header: &FileHeader) -> Result<Vec<u8>> {
        let compressed =
            crate::limits::read_exact_vec(&mut self.reader, header.compressed_size as u64)?;

        let decompressed = match header.method {
            CompressionMethod::Cpy => compressed,
            CompressionMethod::Dir => {
                // Directory entries must have zero size. If the header claims a non-zero
                // size, the archive is corrupted (e.g., a flipped method byte turned a
                // regular file into a directory).
                if header.original_size != 0 {
                    return Err(ArchiveError::corrupted_entry_named(
                        "HA",
                        header.full_path(),
                        format!(
                            "Directory entry claims non-zero original_size {}",
                            header.original_size
                        ),
                    ));
                }
                Vec::new()
            }
            CompressionMethod::Asc => decompress_asc(Cursor::new(compressed))?,
            CompressionMethod::Hsc => decompress_hsc(Cursor::new(compressed))?,
            CompressionMethod::Special => {
                // Special entries (symlinks, devices, etc.) must have zero size.
                if header.original_size != 0 {
                    return Err(ArchiveError::corrupted_entry_named(
                        "HA",
                        header.full_path(),
                        format!(
                            "Special entry claims non-zero original_size {}",
                            header.original_size
                        ),
                    ));
                }
                log::debug!(
                    "Skipping special file '{}' (symlink, device, fifo, or socket)",
                    header.full_path()
                );
                Vec::new()
            }
            CompressionMethod::Unknown(m) => {
                return Err(ArchiveError::unsupported_method(
                    "HA",
                    format!("Unknown({})", m),
                ));
            }
        };

        // Verify CRC-32 (skip for directories and specials)
        if header.method != CompressionMethod::Dir && header.method != CompressionMethod::Special {
            let mut hasher = Hasher::new();
            hasher.update(&decompressed);
            let calculated_crc = hasher.finalize();

            if calculated_crc != header.crc32 {
                return Err(ArchiveError::crc_mismatch(
                    header.full_path(),
                    header.crc32,
                    calculated_crc,
                ));
            }
        }

        Ok(decompressed)
    }
}
