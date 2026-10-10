//! Unix `compress` (`.Z`) decoding: LZW with a twist.
//!
//! A `.Z` file is a 3-byte header (`0x1F 0x9D` plus a flags byte) followed by an LZW stream in
//! the exact blocking the 1980s `compress` tool wrote. The twist versus textbook LZW is the
//! blocking: codes are packed LSB-first into blocks of `n_bits` bytes, and whenever the code
//! width grows or the table is cleared the rest of the current block is padding and the next
//! code starts a fresh block. A decoder that reads a flat bit stream instead of blocks goes out
//! of alignment at the first width change and produces garbage from there on.
//!
//! The algorithm here is derived from `unarc-rs` 0.7.2 (`src/z/lzw.rs`, Apache-2.0), which itself
//! follows ncompress's `getcode()`. What is different is the shape: `unarc-rs` reads the whole
//! compressed input into a `Vec` and decompresses it into another `Vec`, while this module is a
//! pull-based [`Read`] that holds only the dictionary (~192 KiB of working memory, freed on
//! drop) and whatever the caller asked for. A 4 GiB `.Z` costs the dictionary, not the file.
//!
//! [`ZDecoder`] is driven through [`crate::compression::decode`], never constructed directly by
//! format modules.

use std::io::Read;

use crate::error::{Error, Result};

/// The two bytes every `.Z` stream starts with.
const MAGIC: [u8; 2] = [0x1F, 0x9D];
/// Block mode: the stream carries CLEAR codes and the table resets mid-stream.
const BLOCK_MODE: u8 = 0x80;
/// The low five bits of the flags byte are the maximum code width.
const MAX_BITS_MASK: u8 = 0x1F;

/// The CLEAR code, which resets the table in block mode.
const CLEAR: usize = 256;
/// The width every stream starts at, whatever its maximum is.
const INIT_BITS: usize = 9;
/// Every code fits in 16 bits, so the dictionary needs exactly this many entries.
const DICT_SIZE: usize = 65536;
/// The block buffer holds one full block at the widest width: 16 bytes.
const BLOCK_BYTES: usize = 16;

/// A streaming `.Z` decompressor.
///
/// The 3-byte header is read lazily, on the first `read`, so constructing the decoder does no
/// I/O and a caller that never reads pays nothing. A bad header surfaces as an I/O error of kind
/// `InvalidData` (which is what [`Error::malformed`] becomes across a `Read` boundary); the
/// single-stream reader checks the magic eagerly at open so listing still reports a proper kind.
pub(crate) struct ZDecoder<R> {
    source: R,
    /// The widest code, once the header has been read.
    max_bits: Option<usize>,
    header_buf: [u8; 3],
    header_read: usize,
    /// LZW state, valid once the header is in.
    n_bits: usize,
    maxcode: usize,
    free_ent: usize,
    maxmaxcode: usize,
    block_mode: bool,
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// The current block and where the next code starts in it, in bits.
    block: [u8; BLOCK_BYTES],
    block_off: usize,
    block_size: usize,
    clear_flag: bool,
    /// The previous code and its first byte, which is what new table entries are built from.
    oldcode: usize,
    finchar: u8,
    started: bool,
    finished: bool,
    /// Decoded bytes not yet handed to the caller. One code always produces at least one byte,
    /// so this never has to hold more than one code's expansion.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Scratch for expanding one code's string, which is built back to front.
    stack: Vec<u8>,
}

impl<R: Read> ZDecoder<R> {
    /// Wraps `source`, which must be positioned at the start of the `.Z` stream, header included.
    pub(crate) fn new(source: R) -> ZDecoder<R> {
        ZDecoder {
            source,
            max_bits: None,
            header_buf: [0u8; 3],
            header_read: 0,
            n_bits: INIT_BITS,
            maxcode: maxcode(INIT_BITS),
            free_ent: 0,
            maxmaxcode: 0,
            block_mode: false,
            prefix: Vec::new(),
            suffix: Vec::new(),
            block: [0u8; BLOCK_BYTES],
            block_off: 1,
            block_size: 0,
            clear_flag: false,
            oldcode: 0,
            finchar: 0,
            started: false,
            finished: false,
            pending: Vec::new(),
            pending_pos: 0,
            stack: Vec::with_capacity(1024),
        }
    }

    fn ensure_header(&mut self) -> Result<()> {
        if self.max_bits.is_some() {
            return Ok(());
        }
        while self.header_read < 3 {
            match self.source.read(&mut self.header_buf[self.header_read..]) {
                Ok(0) => {
                    return Err(Error::malformed("a .Z stream is at least 3 bytes long"));
                }
                Ok(read) => self.header_read += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::io(error).context("reading a .Z header")),
            }
        }
        if self.header_buf[0..2] != MAGIC {
            return Err(Error::malformed("not a .Z stream: the magic is not 0x1f 0x9d"));
        }
        let flags = self.header_buf[2];
        let max_bits = (flags & MAX_BITS_MASK) as usize;
        // `compress` only ever wrote 9 to 16. Anything else is either damaged or a different
        // format wearing the same two magic bytes, and carrying on would read codes of a width
        // the dictionary cannot hold.
        if !(9..=16).contains(&max_bits) {
            return Err(Error::malformed(format!(
                "a .Z stream with a code width of {max_bits} bits is not a compress stream"
            )));
        }
        let block_mode = flags & BLOCK_MODE != 0;
        self.block_mode = block_mode;
        self.maxmaxcode = 1 << max_bits;
        self.n_bits = INIT_BITS;
        self.maxcode = maxcode(INIT_BITS);
        self.free_ent = if block_mode { CLEAR + 1 } else { 256 };
        self.prefix = vec![0u16; DICT_SIZE];
        self.suffix = vec![0u8; DICT_SIZE];
        // An exhausted block, so the first code read fills one.
        self.block_off = 1;
        self.block_size = 0;
        self.max_bits = Some(max_bits);
        Ok(())
    }

    /// Reads the next code, or `None` at the end of the stream.
    ///
    /// This is ncompress's `getcode()`: codes come out of the current block until it is
    /// exhausted, the width grows, or a CLEAR was seen, at which point a fresh block of `n_bits`
    /// bytes is read and the padding at the end of the old one is discarded.
    fn getcode(&mut self) -> Result<Option<usize>> {
        let max_bits = match self.max_bits {
            Some(bits) => bits,
            None => return Err(Error::malformed("a .Z header must be read before its codes")),
        };
        if self.clear_flag || self.block_off >= self.block_size || self.free_ent > self.maxcode {
            // The dictionary outgrew the width: wider codes from here on. This cannot run past
            // the maximum, because `free_ent` never exceeds `maxmaxcode` (entries stop being
            // added there) and at the maximum width `maxcode` is `maxmaxcode`.
            if self.free_ent > self.maxcode {
                self.n_bits += 1;
                if self.n_bits > max_bits {
                    return Err(Error::malformed("a .Z stream grew its codes past its own limit"));
                }
                self.maxcode =
                    if self.n_bits == max_bits { self.maxmaxcode } else { maxcode(self.n_bits) };
            }
            if self.clear_flag {
                self.n_bits = INIT_BITS;
                self.maxcode = maxcode(self.n_bits);
                self.clear_flag = false;
            }
            // A fresh block is exactly `n_bits` bytes mid-stream, and whatever is left at the
            // end of the file. A short final block is how the stream ends: the encoder truncates
            // its last block to the bytes it actually used, so padding never decodes as data.
            let mut got = 0usize;
            while got < self.n_bits {
                match self.source.read(&mut self.block[got..self.n_bits]) {
                    Ok(0) => break,
                    Ok(read) => got += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(Error::io(error).context("reading a .Z block")),
                }
            }
            if got == 0 {
                return Ok(None);
            }
            let bits = (got << 3) as isize - (self.n_bits as isize - 1);
            if bits <= 0 {
                return Ok(None);
            }
            self.block_size = bits as usize;
            self.block_off = 0;
        }
        let byte = |at: usize| -> Result<usize> {
            self.block
                .get(at)
                .copied()
                .map(|value| value as usize)
                .ok_or_else(|| Error::malformed("a .Z block ended mid-code"))
        };
        let bp = self.block_off >> 3;
        let r_off = self.block_off & 7;
        let mut code = byte(bp)? >> r_off;
        let mut bits_got = 8 - r_off;
        if bits_got < self.n_bits {
            code |= byte(bp + 1)? << bits_got;
            bits_got += 8;
        }
        if bits_got < self.n_bits {
            code |= byte(bp + 2)? << bits_got;
        }
        code &= (1usize << self.n_bits) - 1;
        self.block_off += self.n_bits;
        Ok(Some(code))
    }

    /// Decodes one code into [`Self::pending`], returning whether any bytes were produced.
    fn decode_one(&mut self) -> Result<bool> {
        self.ensure_header()?;
        if !self.started {
            let first = match self.getcode()? {
                Some(code) => code,
                None => {
                    self.finished = true;
                    return Ok(false);
                }
            };
            if first > 255 {
                return Err(Error::malformed("a .Z stream must start with a literal byte"));
            }
            self.finchar = first as u8;
            self.oldcode = first;
            self.started = true;
            self.pending.push(self.finchar);
            self.pending_pos = 0;
            return Ok(true);
        }
        let incode = match self.getcode()? {
            Some(code) => code,
            None => {
                self.finished = true;
                return Ok(false);
            }
        };
        if incode == CLEAR && self.block_mode {
            self.prefix.fill(0);
            self.free_ent = CLEAR + 1;
            self.clear_flag = true;
            // After a CLEAR the stream restarts with a literal. This read sees the flag, drops
            // the rest of the block as padding, and resets the width first.
            let next = match self.getcode()? {
                Some(code) => code,
                None => {
                    self.finished = true;
                    return Ok(false);
                }
            };
            if next > 255 {
                return Err(Error::malformed("a .Z CLEAR must be followed by a literal byte"));
            }
            self.finchar = next as u8;
            self.oldcode = next;
            self.pending.push(self.finchar);
            self.pending_pos = 0;
            return Ok(true);
        }
        let mut code = incode;
        // The one case where the code is not in the table yet: the encoder just added it and the
        // decoder is one step behind, so the string is the previous one plus its own first byte.
        if code >= self.free_ent {
            if code > self.free_ent {
                return Err(Error::malformed(format!(
                    "a .Z code {code} is beyond the table of {} entries",
                    self.free_ent
                )));
            }
            self.stack.push(self.finchar);
            code = self.oldcode;
        }
        while code >= 256 {
            let prefix = self
                .prefix
                .get(code)
                .copied()
                .ok_or_else(|| Error::malformed("a .Z code walks past its table"))?;
            let suffix = self
                .suffix
                .get(code)
                .copied()
                .ok_or_else(|| Error::malformed("a .Z code walks past its table"))?;
            self.stack.push(suffix);
            code = prefix as usize;
        }
        self.finchar = code as u8;
        self.pending.push(self.finchar);
        while let Some(byte) = self.stack.pop() {
            self.pending.push(byte);
        }
        if self.free_ent < self.maxmaxcode {
            if let (Some(prefix), Some(suffix)) =
                (self.prefix.get_mut(self.free_ent), self.suffix.get_mut(self.free_ent))
            {
                *prefix = self.oldcode as u16;
                *suffix = self.finchar;
            }
            self.free_ent += 1;
        }
        self.oldcode = incode;
        self.pending_pos = 0;
        Ok(true)
    }
}

impl<R: Read> Read for ZDecoder<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.pending_pos >= self.pending.len() {
            if self.finished {
                return Ok(0);
            }
            let produced = self.decode_one().map_err(|error| {
                std::io::Error::new(
                    if error.kind() == crate::error::ErrorKind::Io {
                        std::io::ErrorKind::Other
                    } else {
                        std::io::ErrorKind::InvalidData
                    },
                    error.message().to_owned(),
                )
            })?;
            if !produced {
                return Ok(0);
            }
        }
        let available = self.pending.len() - self.pending_pos;
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + count]);
        self.pending_pos += count;
        if self.pending_pos >= self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
        Ok(count)
    }
}

/// The largest code a width of `n` bits can hold.
fn maxcode(n: usize) -> usize {
    (1 << n) - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Decodes `input` fully, for tests that hand over complete streams.
    fn decode_all(input: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        ZDecoder::new(Cursor::new(input.to_vec())).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn a_header_on_its_own_decodes_to_nothing() {
        // `compress` of an empty file is just the header. It must read as empty, not as an error.
        assert_eq!(decode_all(&[0x1F, 0x9D, 0x90]).expect("empty"), Vec::<u8>::new());
    }

    #[test]
    fn a_truncated_header_is_refused() {
        assert!(decode_all(&[0x1F]).is_err());
        assert!(decode_all(&[0x1F, 0x9D]).is_err());
        assert!(decode_all(&[]).is_err());
    }

    #[test]
    fn a_wrong_magic_is_refused() {
        assert!(decode_all(&[0x1F, 0x8B, 0x08, 0, 0]).is_err(), "gzip is not compress");
    }

    #[test]
    fn a_code_width_outside_9_to_16_is_refused() {
        // Bit 7 is block mode, so 0x08 means 8-bit codes without it and 0x91 means 17-bit codes.
        assert!(decode_all(&[0x1F, 0x9D, 0x08]).is_err());
        assert!(decode_all(&[0x1F, 0x9D, 0x91]).is_err());
    }

    #[test]
    fn one_literal_round_trips() {
        // 'A' = 65 at 9 bits, then the block is truncated to the one byte used... which holds
        // only 8 bits, so nothing is readable and the stream is just the header plus one byte
        // that decodes to nothing? No: two bytes hold 16 bits, one 9-bit code, 7 pad bits.
        let compressed = encode(&[b'A']);
        assert_eq!(decode_all(&compressed).expect("one byte"), vec![b'A']);
    }

    #[test]
    fn every_single_byte_value_round_trips() {
        for value in 0u8..=255 {
            let compressed = encode(&[value]);
            assert_eq!(decode_all(&compressed).expect("one byte"), vec![value], "byte {value}");
        }
    }

    #[test]
    fn compressible_text_round_trips() {
        let payload: Vec<u8> = b"hello compress world, ".repeat(40).to_vec();
        let compressed = encode(&payload);
        assert!(compressed.len() < payload.len(), "it should actually compress");
        assert_eq!(decode_all(&compressed).expect("text"), payload);
    }

    #[test]
    fn incompressible_bytes_round_trip() {
        // Every byte value in rotation: the table fills with two-byte strings and the width grows.
        let payload: Vec<u8> = (0..6000u32).map(|value| (value % 256) as u8).collect();
        let compressed = encode(&payload);
        assert_eq!(decode_all(&compressed).expect("rotation"), payload);
    }

    #[test]
    fn a_large_varied_input_exercises_clear_and_the_full_width_range() {
        // 300 KiB of pseudo-random bytes: almost nothing matches, so nearly every byte is an
        // insert and the 64 KiB table fills several times over. The stream then carries CLEAR
        // codes and codes at every width from 9 to 16 bits.
        let mut state = 0x12345678u32;
        let payload: Vec<u8> = (0..300_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xFF) as u8
            })
            .collect();
        let (compressed, clears, max_width) = encode_stats(&payload);
        assert!(clears > 0, "this input should force at least one table reset");
        assert_eq!(max_width, 16, "this input should reach the widest codes");
        assert_eq!(decode_all(&compressed).expect("large"), payload);
    }

    #[test]
    fn decoding_a_byte_at_a_time_gives_the_same_bytes() {
        let payload: Vec<u8> = b"the quick brown fox jumps over the lazy dog. ".repeat(30).to_vec();
        let compressed = encode(&payload);
        let mut decoder = ZDecoder::new(Cursor::new(compressed));
        let mut got = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match decoder.read(&mut one) {
                Ok(0) => break,
                Ok(1) => got.push(one[0]),
                Ok(_) => unreachable!(),
                Err(error) => panic!("read: {error}"),
            }
        }
        assert_eq!(got, payload);
    }

    #[test]
    fn non_block_mode_decodes() {
        // Flags byte 0x10: 16-bit codes, no CLEAR. Rare in the wild, legal all the same.
        let payload: Vec<u8> = b"abcabcabcabcabcabcabcabc".to_vec();
        let compressed = encode_with(&payload, 16, false);
        assert_eq!(decode_all(&compressed).expect("non-block"), payload);
    }

    // ------------------------------------------------------- the test-only encoder
    //
    // `compress` is not installed on this machine, so fixtures are produced here, by an encoder
    // that mirrors the decoder's block semantics exactly (8 codes per full block, width growth
    // and CLEAR both end the block, the final block truncated to the bytes used). The encoder is
    // *not* trusted: the integration test decodes its output with the real `gzip`/`7z` tooling
    // and requires all three to agree.

    use std::collections::HashMap;

    fn encode(payload: &[u8]) -> Vec<u8> {
        encode_with(payload, 16, true)
    }

    fn encode_stats(payload: &[u8]) -> (Vec<u8>, usize, usize) {
        let mut enc = TestEncoder::new(16, true);
        enc.encode(payload);
        let (clears, max_width) = (enc.clears, enc.max_width);
        (enc.finish(), clears, max_width)
    }

    fn encode_with(payload: &[u8], max_bits: u8, block_mode: bool) -> Vec<u8> {
        let mut enc = TestEncoder::new(max_bits, block_mode);
        enc.encode(payload);
        enc.finish()
    }

    struct TestEncoder {
        max_bits: usize,
        block_mode: bool,
        maxmaxcode: usize,
        dict: HashMap<u32, usize>,
        n_bits: usize,
        maxcode: usize,
        free_ent: usize,
        /// How many codes have been emitted since the stream started or the last CLEAR.
        /// Width growth is driven by this absolute count, not by the table size: the decoder
        /// bumps its width before code `j` when `256 + j` (block mode) exceeds the current
        /// maximum, so the encoder must emit code 255 — not 254 — as the last 9-bit code.
        /// Bumping on the insert count is one code early, because the encoder has always seen
        /// one byte more than the decoder has. This is deliberately *not* reset by a width
        /// change, only by a CLEAR, mirroring the decoder's own absolute code index.
        codes_emitted: usize,
        out: Vec<u8>,
        block: Vec<u8>,
        bits_used: usize,
        clears: usize,
        max_width: usize,
    }

    impl TestEncoder {
        fn new(max_bits: u8, block_mode: bool) -> TestEncoder {
            let max_bits = max_bits as usize;
            let out = vec![0x1F, 0x9D, (if block_mode { BLOCK_MODE } else { 0 }) | max_bits as u8];
            TestEncoder {
                max_bits,
                block_mode,
                maxmaxcode: 1 << max_bits,
                dict: HashMap::new(),
                n_bits: INIT_BITS,
                maxcode: maxcode(INIT_BITS),
                free_ent: if block_mode { CLEAR + 1 } else { 256 },
                codes_emitted: 0,
                out,
                block: vec![0u8; INIT_BITS],
                bits_used: 0,
                clears: 0,
                max_width: INIT_BITS,
            }
        }

        fn usable(&self) -> usize {
            // A full block is exactly `n_bits` bytes holding exactly 8 codes, with no padding:
            // 8 codes need 8 * n bits and the block has 8 * n bits, and the decoder's
            // `got * 8 - (n - 1)` size always admits the 8th code (7n < 8n - n + 1). The
            // `-(n - 1)` term only matters for the truncated final block, which is how the
            // stream ends rather than more padding.
            self.n_bits * 8
        }

        fn flush_full(&mut self) {
            self.out.extend_from_slice(&self.block);
            self.block = vec![0u8; self.n_bits];
            self.bits_used = 0;
        }

        fn emit(&mut self, code: usize) {
            self.emit_raw(code);
            self.codes_emitted += 1;
            self.max_width = self.max_width.max(self.n_bits);
            // The width for the *next* code grows when the decoder would grow its own: before
            // its code `j`, with `256 + j` over the maximum in block mode (`255 + j` without,
            // where the table starts one entry lower). That is after this many emits, not when
            // the table crosses — the table is always one insert ahead of the decoder.
            let threshold = if self.block_mode { 256 } else { 255 };
            if self.n_bits < self.max_bits && threshold + self.codes_emitted > self.maxcode {
                self.flush_full();
                self.n_bits += 1;
                self.maxcode =
                    if self.n_bits == self.max_bits { self.maxmaxcode } else { maxcode(self.n_bits) };
                self.block = vec![0u8; self.n_bits];
            }
        }

        /// Writes one code at the current width, with no bookkeeping.
        fn emit_raw(&mut self, code: usize) {
            if self.bits_used + self.n_bits > self.usable() {
                self.flush_full();
            }
            let mut value = code;
            let mut bit = self.bits_used;
            for _ in 0..self.n_bits {
                if value & 1 != 0 {
                    self.block[bit >> 3] |= 1 << (bit & 7);
                }
                value >>= 1;
                bit += 1;
            }
            self.bits_used += self.n_bits;
        }

        /// Emits a CLEAR and resets the table, when it is full.
        fn clear(&mut self) {
            // A CLEAR is emitted at the current width like any other code, then the block ends:
            // the decoder drops the rest of the block as padding when it sees the flag. It goes
            // through `emit_raw` rather than `emit` so a width change can never slip in between
            // the CLEAR and the reset, which would strand the decoder at the wrong width.
            self.emit_raw(CLEAR);
            self.clears += 1;
            self.flush_full();
            self.dict.clear();
            self.n_bits = INIT_BITS;
            self.maxcode = maxcode(self.n_bits);
            self.free_ent = if self.block_mode { CLEAR + 1 } else { 256 };
            self.block = vec![0u8; self.n_bits];
            self.codes_emitted = 0;
        }

        fn encode(&mut self, payload: &[u8]) {
            if payload.is_empty() {
                return;
            }
            let mut prefix = payload[0] as usize;
            for &byte in &payload[1..] {
                let key = ((prefix as u32) << 8) | byte as u32;
                if let Some(&code) = self.dict.get(&key) {
                    prefix = code;
                } else {
                    self.emit(prefix);
                    if self.free_ent < self.maxmaxcode {
                        self.dict.insert(key, self.free_ent);
                        self.free_ent += 1;
                    } else if self.block_mode {
                        // The table is full and the decoder is exactly as full: CLEAR, which the
                        // decoder reads as a reset at whatever width both sides agree on.
                        self.clear();
                    }
                    // Without block mode a full table just freezes: later strings stay literals.
                    prefix = byte as usize;
                }
            }
            self.emit(prefix);
        }

        fn finish(mut self) -> Vec<u8> {
            // The last block is truncated to the bytes actually used. That truncation is the
            // stream's terminator: the decoder extracts codes only while they start inside the
            // bytes present, so no padding zero ever decodes as a trailing NUL.
            if self.bits_used > 0 {
                let bytes = (self.bits_used + 7) / 8;
                self.out.extend_from_slice(&self.block[..bytes]);
            }
            self.out
        }
    }
}
