//! The two ways a ZIP entry can be encrypted: the traditional PKWARE cipher and WinZip AES.
//!
//! Listing an encrypted entry needs no password; reading one does. Both readers below are
//! pull-based [`Read`]s that sit between the entry's raw bytes and its decompressor, so an
//! encrypted entry streams exactly like a plain one once the password has checked out.
//!
//! The password check happens eagerly, in `open`, while the data flows lazily through `read`.
//! That split is what turns "no password" and "wrong password" into [`ErrorKind::Password`] at
//! [`ArchiveReader::open_entry`] time rather than into an I/O error halfway through a read. The
//! one exception is WinZip AES's trailing authentication code, which by construction can only be
//! verified once every data byte has been seen; a mismatch there surfaces as an I/O error, and
//! only a wrong password that slipped past the 2-byte verification (1 in 65,536) ever reaches it.
//!
//! [`ErrorKind::Password`]: crate::error::ErrorKind::Password
//! [`ArchiveReader::open_entry`]: crate::reader::ArchiveReader::open_entry

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::Compression;

/// A WinZip AES strength: which key went into the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AesStrength {
    /// 128-bit keys, an 8-byte salt.
    Aes128,
    /// 192-bit keys, a 12-byte salt.
    Aes192,
    /// 256-bit keys, a 16-byte salt.
    Aes256,
}

impl AesStrength {
    /// The salt length for this strength, in bytes.
    pub fn salt_length(self) -> usize {
        self.key_length() / 2
    }

    /// The key length for this strength, in bytes.
    pub fn key_length(self) -> usize {
        match self {
            AesStrength::Aes128 => 16,
            AesStrength::Aes192 => 24,
            AesStrength::Aes256 => 32,
        }
    }

    /// The strength byte the AES extra field carries, or `None` when it names nothing real.
    fn from_extra_byte(value: u8) -> Option<AesStrength> {
        match value {
            0x01 => Some(AesStrength::Aes128),
            0x02 => Some(AesStrength::Aes192),
            0x03 => Some(AesStrength::Aes256),
            _ => None,
        }
    }
}

/// What the AES extra field says about one entry.
#[derive(Clone, Copy, Debug)]
pub struct AesParams {
    /// Which key length the archive used.
    pub strength: AesStrength,
    /// Whether this is AE-2 (no CRC is stored) rather than AE-1.
    pub ae2: bool,
    /// The real compression method: the entry's own method field reads 99 when AES is in use.
    pub method: Compression,
}

/// Finds the WinZip AES extra field (`0x9901`) in a central directory record's extra bytes.
///
/// Returns `None` when there is no such field or it is not one this build understands, so that a
/// corrupt extra field costs the entry its decryption rather than the archive its listing: the
/// entry still lists, and reading it reports the method unknown.
pub fn parse_aes_extra(extra: &[u8]) -> Option<AesParams> {
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[at], extra[at + 1]]);
        let size = u16::from_le_bytes([extra[at + 2], extra[at + 3]]) as usize;
        let payload = extra.get(at + 4..at + 4 + size)?;
        if id == 0x9901 {
            // Seven bytes: vendor version, vendor id (`AE`), strength byte, real method. Any
            // other length is damage, and damage here means "no AES parameters", not a dead
            // archive.
            if payload.len() != 7 {
                return None;
            }
            let version = u16::from_le_bytes([payload[0], payload[1]]);
            let vendor = u16::from_le_bytes([payload[2], payload[3]]);
            if vendor != 0x4541 {
                return None;
            }
            let ae2 = match version {
                0x0001 => false,
                0x0002 => true,
                _ => return None,
            };
            let strength = AesStrength::from_extra_byte(payload[4])?;
            let method = Compression::from_zip_method(u16::from_le_bytes([payload[5], payload[6]]));
            return Some(AesParams { strength, ae2, method });
        }
        at += 4 + size;
    }
    None
}

// --------------------------------------------------------------------------- ZipCrypto

/// The traditional PKWARE cipher's running keys.
struct ZipCryptoKeys {
    key0: u32,
    key1: u32,
    key2: u32,
}

impl ZipCryptoKeys {
    fn derive(password: &[u8]) -> ZipCryptoKeys {
        // The password is bytes, not text: the specification never says how it is encoded, so a
        // UTF-8 assumption would lock out archives whose password is not valid UTF-8.
        let mut keys = ZipCryptoKeys { key0: 0x12345678, key1: 0x23456789, key2: 0x34567890 };
        for &byte in password {
            keys.update(byte);
        }
        keys
    }

    fn update(&mut self, byte: u8) {
        self.key0 = crc32(self.key0, byte);
        self.key1 = self
            .key1
            .wrapping_add(self.key0 & 0xFF)
            .wrapping_mul(0x0808_8405)
            .wrapping_add(1);
        self.key2 = crc32(self.key2, (self.key1 >> 24) as u8);
    }

    fn stream_byte(&self) -> u8 {
        let temp = (self.key2 as u16) | 3;
        ((temp.wrapping_mul(temp ^ 1)) >> 8) as u8
    }

    fn decrypt_byte(&mut self, cipher_byte: u8) -> u8 {
        let plain = self.stream_byte() ^ cipher_byte;
        self.update(plain);
        plain
    }
}

/// One step of the IEEE CRC-32, computed bitwise.
///
/// A table would be faster and 1 KiB of source; this cipher is byte-at-a-time anyway and is
/// legacy-only, so clarity wins over the few percent.
fn crc32(crc: u32, byte: u8) -> u32 {
    let mut value = crc ^ byte as u32;
    for _ in 0..8 {
        value = if value & 1 != 0 { (value >> 1) ^ 0xEDB8_8320 } else { value >> 1 };
    }
    value
}

/// A streaming ZipCrypto decryption layer.
///
/// `open` consumes and checks the 12-byte encryption header; `read` decrypts the rest in place.
/// The check byte is the CRC's high byte, or the modification time's high byte when the entry
/// uses a data descriptor and no CRC was recorded yet.
pub(crate) struct ZipCryptoReader<R> {
    source: R,
    keys: ZipCryptoKeys,
}

impl<R: Read> ZipCryptoReader<R> {
    pub(crate) fn open(mut source: R, password: &[u8], check: u8) -> Result<ZipCryptoReader<R>> {
        let mut keys = ZipCryptoKeys::derive(password);
        let mut header = [0u8; 12];
        read_exact(&mut source, &mut header, "reading a ZipCrypto header")?;
        for byte in header.iter_mut() {
            *byte = keys.decrypt_byte(*byte);
        }
        // Eleven random bytes and one check byte: a wrong password passes this with probability
        // 1/256, in which case the decompressor behind it fails on the garbage instead. Every
        // ZIP tool behaves the same way; there is no stronger check to do without the plaintext.
        if header[11] != check {
            return Err(Error::password("wrong password for this ZipCrypto entry"));
        }
        Ok(ZipCryptoReader { source, keys })
    }
}

impl<R: Read> Read for ZipCryptoReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let read = self.source.read(output)?;
        for byte in output[..read].iter_mut() {
            *byte = self.keys.decrypt_byte(*byte);
        }
        Ok(read)
    }
}

// --------------------------------------------------------------------------- WinZip AES

/// PBKDF2 iterations for WinZip AES key derivation, fixed by the specification.
const AES_ROUNDS: u32 = 1000;
/// The password verification value after the salt, in bytes.
const AES_VERIFY_LEN: usize = 2;
/// The trailing authentication code, in bytes (HMAC-SHA1-80, the first 10 bytes of the HMAC).
const AES_AUTH_LEN: usize = 10;

/// The slightly non-standard AES-CTR variant WinZip uses: no nonce, the counter alone as the
/// block in little-endian order, starting at 1. See the WinZip AES specification.
///
/// `aes-gcm` stays unused on purpose: nothing in scope uses GCM. WinZip AES is CTR plus a
/// separate HMAC-SHA1, and 7Z is CBC; wiring GCM would be a dependency without a reader.
struct AesCtr<C: aes::cipher::BlockSizeUser> {
    cipher: C,
    counter: u128,
    block: aes::cipher::Block<C>,
    /// How much of `block` has been used as keystream.
    used: usize,
}

impl<C> AesCtr<C>
where
    C: aes::cipher::BlockCipherEncrypt + aes::cipher::KeyInit + aes::cipher::BlockSizeUser,
{
    fn new(key: &[u8]) -> Result<AesCtr<C>> {
        // `try_from` fails rather than panics on the wrong length, so a key that does not match
        // the cipher is a malformed archive rather than a dead process. The lengths always match
        // by construction (the key is cut out of the derived block by strength).
        let key = aes::cipher::Key::<C>::try_from(key)
            .map_err(|_| Error::malformed("a WinZip AES key is not the length its strength says"))?;
        Ok(AesCtr {
            cipher: C::new(&key),
            counter: 1,
            block: aes::cipher::Block::<C>::default(),
            used: 16,
        })
    }

    fn crypt_in_place(&mut self, target: &mut [u8]) {
        // No trait import: `BlockCipherEncrypt` is already a bound on `C` in this impl block,
        // which is what resolves `encrypt_block` below.
        for byte in target.iter_mut() {
            if self.used == 16 {
                self.block.as_mut_slice().copy_from_slice(&self.counter.to_le_bytes());
                self.cipher.encrypt_block(&mut self.block);
                self.counter += 1;
                self.used = 0;
            }
            *byte ^= self.block.as_slice()[self.used];
            self.used += 1;
        }
    }
}

/// The cipher behind one entry, chosen by its strength.
enum AesCipher {
    Aes128(AesCtr<aes::Aes128>),
    Aes192(AesCtr<aes::Aes192>),
    Aes256(AesCtr<aes::Aes256>),
}

impl AesCipher {
    fn with_key(strength: AesStrength, key: &[u8]) -> Result<AesCipher> {
        Ok(match strength {
            AesStrength::Aes128 => AesCipher::Aes128(AesCtr::new(key)?),
            AesStrength::Aes192 => AesCipher::Aes192(AesCtr::new(key)?),
            AesStrength::Aes256 => AesCipher::Aes256(AesCtr::new(key)?),
        })
    }

    fn crypt_in_place(&mut self, target: &mut [u8]) {
        match self {
            AesCipher::Aes128(state) => state.crypt_in_place(target),
            AesCipher::Aes192(state) => state.crypt_in_place(target),
            AesCipher::Aes256(state) => state.crypt_in_place(target),
        }
    }
}

/// A streaming WinZip AES decryption layer.
///
/// `open` consumes the salt and verification value and derives the keys; `read` decrypts the
/// data and, once it ends, consumes and verifies the trailing authentication code. `total` is
/// the entry's whole compressed size: salt, verification, data and authentication code together.
pub(crate) struct AesReader<R> {
    source: R,
    cipher: AesCipher,
    hmac: hmac::SimpleHmacReset<sha1::Sha1>,
    remaining: u64,
    authed: bool,
}

impl<R: Read> AesReader<R> {
    pub(crate) fn open(
        mut source: R,
        total: u64,
        password: &[u8],
        params: AesParams,
    ) -> Result<AesReader<R>> {
        use hmac::KeyInit;
        let salt_len = params.strength.salt_length();
        let key_len = params.strength.key_length();
        let overhead = (salt_len + AES_VERIFY_LEN + AES_AUTH_LEN) as u64;
        let data_len = total.checked_sub(overhead).ok_or_else(|| {
            Error::malformed("a WinZip AES entry is shorter than its own salt and trailer")
        })?;
        let mut salt = vec![0u8; salt_len];
        read_exact(&mut source, &mut salt, "reading a WinZip AES salt")?;
        let mut stored_verify = [0u8; AES_VERIFY_LEN];
        read_exact(&mut source, &mut stored_verify, "reading a WinZip AES verification value")?;
        // PBKDF2-HMAC-SHA1, 1000 rounds, yielding two keys and the verification value. The only
        // failure mode is an output size the PRF cannot produce, which cannot happen with HMAC;
        // it is still reported rather than unwrapped.
        let mut derived = vec![0u8; 2 * key_len + AES_VERIFY_LEN];
        pbkdf2::pbkdf2::<hmac::SimpleHmacReset<sha1::Sha1>>(password, &salt, AES_ROUNDS, &mut derived)
            .map_err(|_| Error::malformed("a WinZip AES password could not be processed"))?;
        let cipher = AesCipher::with_key(params.strength, &derived[..key_len])?;
        let hmac =
            hmac::SimpleHmacReset::<sha1::Sha1>::new_from_slice(&derived[key_len..2 * key_len])
                .map_err(|_| Error::malformed("a WinZip AES authentication key is unusable"))?;
        let expected = &derived[2 * key_len..];
        // Compared without an early exit, so the timing does not say how many bytes matched.
        let mut diff = 0u8;
        for (got, want) in stored_verify.iter().zip(expected.iter()) {
            diff |= got ^ want;
        }
        if diff != 0 {
            return Err(Error::password("wrong password for this WinZip AES entry"));
        }
        Ok(AesReader { source, cipher, hmac, remaining: data_len, authed: false })
    }

    /// Consumes the trailing authentication code and checks it against the HMAC over the
    /// ciphertext. Runs once, when the data ends.
    fn verify_auth(&mut self) -> std::io::Result<()> {
        use hmac::Mac;
        let mut stored = [0u8; AES_AUTH_LEN];
        self.source.read_exact(&mut stored).map_err(|error| {
            std::io::Error::new(error.kind(), format!("reading a WinZip AES trailer: {error}"))
        })?;
        let computed = self.hmac.finalize_reset().into_bytes();
        let mut diff = 0u8;
        for (got, want) in stored.iter().zip(computed[..AES_AUTH_LEN].iter()) {
            diff |= got ^ want;
        }
        if diff != 0 {
            // Almost always a wrong password that slipped past the 2-byte verification (1 in
            // 65,536); occasionally damage. Reported as a password failure either way, because a
            // caller that just typed a password needs "wrong password", not "corrupt".
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a WinZip AES entry failed authentication: wrong password or damaged data",
            ));
        }
        self.authed = true;
        Ok(())
    }
}

impl<R: Read> Read for AesReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        use hmac::Mac;
        if self.remaining == 0 {
            if !self.authed {
                self.verify_auth()?;
            }
            return Ok(0);
        }
        let wanted = (self.remaining.min(output.len() as u64)) as usize;
        if wanted == 0 {
            return Ok(0);
        }
        let read = self.source.read(&mut output[..wanted])?;
        // The HMAC covers the ciphertext, so it is updated before decryption.
        self.hmac.update(&output[..read]);
        self.cipher.crypt_in_place(&mut output[..read]);
        self.remaining -= read as u64;
        if self.remaining == 0 && !self.authed {
            self.verify_auth()?;
        }
        Ok(read)
    }
}

/// Reads exactly `into.len()` bytes, looping over short reads and retries.
fn read_exact<R: Read>(source: &mut R, into: &mut [u8], what: &str) -> Result<()> {
    let mut filled = 0usize;
    while filled < into.len() {
        match source.read(&mut into[filled..]) {
            Ok(0) => {
                return Err(Error::malformed(format!("{what}: the entry ends first")));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::io(error).context(String::from(what))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn the_aes_extra_field_parses_the_values_real_writers_emit() {
        // Taken from a 7-Zip-written AES-256 entry: version 2 (AE-2), vendor `AE`, strength 3,
        // method 0 (stored).
        let extra = [0x01, 0x99, 0x07, 0x00, 0x02, 0x00, 0x41, 0x45, 0x03, 0x00, 0x00];
        let params = parse_aes_extra(&extra).expect("7-Zip's AES field");
        assert_eq!(params.strength, AesStrength::Aes256);
        assert!(params.ae2);
        assert_eq!(params.method, Compression::None);
    }

    #[test]
    fn the_aes_extra_field_is_found_among_other_fields() {
        // A ZIP64 field followed by the AES field: the scan must skip what it does not know.
        let mut extra = vec![0x01, 0x00, 0x08, 0x00, 1, 2, 3, 4, 5, 6, 7, 8];
        extra.extend_from_slice(&[0x01, 0x99, 0x07, 0x00, 0x01, 0x00, 0x41, 0x45, 0x01, 0x08, 0x00]);
        let params = parse_aes_extra(&extra).expect("AES after ZIP64");
        assert_eq!(params.strength, AesStrength::Aes128);
        assert!(!params.ae2, "version 1 is AE-1");
        assert_eq!(params.method, Compression::Deflate);
    }

    #[test]
    fn a_damaged_aes_extra_field_is_none_rather_than_an_error() {
        assert!(parse_aes_extra(&[]).is_none());
        // Right id, wrong length.
        assert!(parse_aes_extra(&[0x01, 0x99, 0x03, 0x00, 0x01, 0x02, 0x03]).is_none());
        // Wrong vendor.
        assert!(parse_aes_extra(&[0x01, 0x99, 0x07, 0x00, 0x02, 0x00, 0x58, 0x58, 0x03, 0x00, 0x00])
            .is_none());
        // Unknown strength.
        assert!(parse_aes_extra(&[0x01, 0x99, 0x07, 0x00, 0x02, 0x00, 0x41, 0x45, 0x09, 0x00, 0x00])
            .is_none());
        // Truncated mid-field.
        assert!(parse_aes_extra(&[0x01, 0x99, 0x07, 0x00, 0x02]).is_none());
    }

    #[test]
    fn the_strength_lengths_are_the_ones_the_specification_fixes() {
        assert_eq!(AesStrength::Aes128.key_length(), 16);
        assert_eq!(AesStrength::Aes192.key_length(), 24);
        assert_eq!(AesStrength::Aes256.key_length(), 32);
        assert_eq!(AesStrength::Aes128.salt_length(), 8);
        assert_eq!(AesStrength::Aes192.salt_length(), 12);
        assert_eq!(AesStrength::Aes256.salt_length(), 16);
    }

    /// Encrypts with ZipCrypto, mirroring the decryption layer, so the round trip below tests
    /// the key schedule rather than agreeing with itself through one shared function.
    fn zipcrypto_encrypt(plain: &[u8], password: &[u8], check: u8) -> Vec<u8> {
        let mut keys = ZipCryptoKeys::derive(password);
        // Eleven fixed bytes stand in for the random header: the cipher is deterministic given
        // the header, and the header round-trips through `open` either way.
        let mut header = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, check];
        let mut out = Vec::with_capacity(12 + plain.len());
        for byte in header.iter_mut() {
            let cipher = keys.stream_byte() ^ *byte;
            keys.update(*byte);
            out.push(cipher);
        }
        for &byte in plain {
            let cipher = keys.stream_byte() ^ byte;
            keys.update(byte);
            out.push(cipher);
        }
        out
    }

    #[test]
    fn zipcrypto_round_trips_through_the_reader() {
        let plain = b"a ZipCrypto secret, stored".repeat(50).to_vec();
        let encrypted = zipcrypto_encrypt(&plain, b"password", 0x5A);
        let mut reader = ZipCryptoReader::open(Cursor::new(encrypted), b"password", 0x5A)
            .expect("right password opens");
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read");
        assert_eq!(got, plain);
    }

    #[test]
    fn zipcrypto_with_the_wrong_password_is_a_password_error() {
        let encrypted = zipcrypto_encrypt(b"secret", b"right", 0x5A);
        let error = match ZipCryptoReader::open(Cursor::new(encrypted), b"wrong", 0x5A) {
            Ok(_) => panic!("wrong password must not open"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Password);
    }

    #[test]
    fn zipcrypto_decrypts_a_byte_at_a_time() {
        let plain: Vec<u8> = (0..1000u32).map(|value| (value % 251) as u8).collect();
        let encrypted = zipcrypto_encrypt(&plain, b"pw", 0x00);
        let mut reader =
            ZipCryptoReader::open(Cursor::new(encrypted), b"pw", 0x00).expect("opens");
        let mut got = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match reader.read(&mut one) {
                Ok(0) => break,
                Ok(1) => got.push(one[0]),
                Ok(_) => unreachable!(),
                Err(error) => panic!("read: {error}"),
            }
        }
        assert_eq!(got, plain);
    }

    #[test]
    fn aes_with_a_short_total_is_refused_rather_than_reading_past_the_end() {
        let params =
            AesParams { strength: AesStrength::Aes256, ae2: true, method: Compression::None };
        let error = match AesReader::open(Cursor::new(vec![0u8; 64]), 10, b"pw", params) {
            Ok(_) => panic!("shorter than salt plus trailer"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), crate::error::ErrorKind::Malformed);
    }
}
