use crate::arc::rle::unpack_rle;
use crate::error::{ArchiveError, Result};
use bitstream_io::{BitRead, BitReader, LittleEndian};

const NUMVALS: usize = 257;
const SPEOF: i16 = 256;

/// Maximum iterations for safety against corrupt data (should be enough for any reasonable file)
const MAX_ITERATIONS: usize = 100_000_000;

pub fn unsqueeze(mut buf: &[u8]) -> Result<Vec<u8>> {
    // Bounds check for numnodes
    if buf.len() < 2 {
        return Err(ArchiveError::decompression_failed(
            "Squeeze",
            "truncated input (need 2 bytes for numnodes)",
        ));
    }
    convert_u16!(numnodes, buf);
    if numnodes as usize >= NUMVALS {
        return Err(ArchiveError::decompression_failed(
            "Squeeze",
            format!("invalid number of nodes {} (max {})", numnodes, NUMVALS - 1),
        ));
    }
    if numnodes == 0 {
        return Ok(Vec::new());
    }
    // Bounds check for dnode entries (2 i16 per node = 4 bytes per node)
    if buf.len() < (numnodes as usize) * 4 {
        return Err(ArchiveError::decompression_failed(
            "Squeeze",
            "truncated input (insufficient bytes for dnode table)",
        ));
    }
    let mut dnode = Vec::with_capacity(numnodes as usize);
    for _ in 0..numnodes as usize {
        dnode.push([get_i16!(buf), get_i16!(buf)]);
    }
    let mut reader = BitReader::endian(buf, LittleEndian);
    let mut i: i16 = 0;
    let mut decoded = Vec::new();
    let mut iterations = 0usize;
    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            return Err(ArchiveError::decompression_failed(
                "Squeeze",
                "iteration limit exceeded (corrupt data or wrong password)",
            ));
        }

        // Bounds check for node index
        if i < 0 || i as usize >= dnode.len() {
            return Err(ArchiveError::decompression_failed(
                "Squeeze",
                format!("invalid node index {} (max {})", i, dnode.len()),
            ));
        }

        let bit = match reader.read::<1, u8>() {
            Ok(b) => b,
            Err(_) => break, // EOF in bitstream
        };
        i = dnode[i as usize][bit as usize];
        if i < 0 {
            i = -(i + 1);
            if i == SPEOF {
                break;
            } else {
                decoded.push(i as u8);
                i = 0;
            }
        }
    }
    Ok(unpack_rle(&decoded))
}
