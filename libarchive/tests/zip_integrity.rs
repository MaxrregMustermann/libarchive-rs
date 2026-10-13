//! A ZIP entry's CRC is checked as its bytes stream past, so a corrupt entry is refused.
//!
//! The central directory carries a CRC-32 for every entry, and before this test existed the
//! reader put it in a field and never looked at it again. A byte flipped inside an entry's *data*
//! (not its header) therefore streamed back as the right number of wrong bytes, with no error: a
//! file manager extracting it would write garbage to disk and tell the user it worked.
//!
//! These tests corrupt data, never headers, so the CRC is the only thing that can catch them. The
//! stored entry is the important one: a flipped byte in stored data decodes cleanly, so if the CRC
//! comparison is deleted the read succeeds and this file goes red — which is the point. A
//! deflated entry is included too, because its corruption may surface either as a codec error or
//! as a CRC mismatch and both must read as [`ErrorKind::Malformed`].
//!
//! Where `unzip` and `7z` are installed, the same archives are handed to them: one that both
//! tools accept must be accepted here, and one they reject must be rejected here. That is what
//! makes this check *correct* rather than merely present.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use libarchive::{
    ArchiveReader, ArchiveWriter, Error, ErrorKind, Format, NewEntry, OpenOptions, WriteOptions,
};

/// The stored entry's 256 bytes of `A`, and the offset the acceptance criteria corrupts.
const A_LEN: usize = 256;

fn stored_bytes() -> Vec<u8> {
    vec![b'A'; A_LEN]
}

/// Long enough that the writer actually deflates rather than storing it.
fn deflatable_bytes() -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog 0123456789\n".repeat(64)
}

/// Writes a ZIP whose entries are named `name`; `stored` picks the no-compression method.
fn write_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let sink = Cursor::new(Vec::new());
    let mut writer =
        ArchiveWriter::create(sink, Format::Zip, &WriteOptions::default()).expect("a ZIP writer");
    for (name, contents, stored) in entries {
        let mut entry = NewEntry::file(*name);
        if *stored {
            entry = entry.stored();
        }
        writer
            .write_entry(&entry, &mut &contents[..])
            .expect("writing an entry");
    }
    writer
        .finish_into()
        .expect("finishing the archive")
        .into_inner()
}

/// Where one entry's data sits in a ZIP, read from the actual bytes rather than assumed.
struct Located {
    data_start: usize,
    compressed_size: usize,
    /// Where the central directory's compressed-size field is, for the truncation test.
    compressed_size_field: usize,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Parses the central directory and returns each entry's data location, in order.
fn locate(zip: &[u8]) -> Vec<Located> {
    let eocd = (0..zip.len() - 3)
        .rev()
        .find(|&at| &zip[at..at + 4] == b"PK\x05\x06")
        .expect("an end-of-central-directory record");
    let count = u16_at(zip, eocd + 10) as usize;
    let mut at = u32_at(zip, eocd + 16) as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        assert_eq!(
            &zip[at..at + 4],
            b"PK\x01\x02",
            "a central directory header"
        );
        let compressed_size = u32_at(zip, at + 20) as usize;
        let name_len = u16_at(zip, at + 28) as usize;
        let extra_len = u16_at(zip, at + 30) as usize;
        let comment_len = u16_at(zip, at + 32) as usize;
        let local = u32_at(zip, at + 42) as usize;
        assert_eq!(&zip[local..local + 4], b"PK\x03\x04", "a local file header");
        let local_name = u16_at(zip, local + 26) as usize;
        let local_extra = u16_at(zip, local + 28) as usize;
        out.push(Located {
            data_start: local + 30 + local_name + local_extra,
            compressed_size,
            compressed_size_field: at + 20,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    out
}

/// Flips one byte, and only that byte, inside the entry's data.
fn corrupt_byte(zip: &mut [u8], at: usize) {
    zip[at] ^= 0xFF;
}

/// Opens `zip` and reads entry `index` to the end, giving back either the bytes or the error.
///
/// The entry stream's `read` speaks `io::Error`; converting through [`Error::io`] is how every
/// caller reaches this crate's kind, and it is the conversion the app applies, so it is the one
/// under test here.
fn read_entry(zip: &[u8], index: usize) -> Result<Vec<u8>, Error> {
    let mut reader = ArchiveReader::open_as(
        Cursor::new(zip.to_vec()),
        Format::Zip,
        &OpenOptions::default(),
    )
    .expect("the archive opens");
    let mut stream = reader.open_entry(index).expect("the entry opens");
    let mut got = Vec::new();
    stream.read_to_end(&mut got).map_err(Error::io).map(|_| got)
}

#[test]
fn an_intact_stored_and_deflated_entry_still_read() {
    let stored = stored_bytes();
    let deflated = deflatable_bytes();
    let zip = write_zip(&[("aaaa.bin", &stored, true), ("text.txt", &deflated, false)]);
    assert_eq!(read_entry(&zip, 0).expect("stored reads"), stored);
    assert_eq!(read_entry(&zip, 1).expect("deflated reads"), deflated);
}

/// The heart of the job, and the test that goes red if the CRC check is deleted.
///
/// A stored entry's bytes decode one-for-one, so deleting the CRC comparison changes nothing
/// except that the read succeeds and hands back the wrong bytes. Every corruption below is a
/// single flipped byte of *data*, at the first, middle and last positions, with the declared
/// length untouched: the only thing that can tell the copy apart from the original is the CRC.
#[test]
fn a_stored_entry_corrupted_in_its_data_is_refused_at_every_position() {
    let good = stored_bytes();
    let original = write_zip(&[("aaaa.bin", &good, true)]);
    let located = locate(&original);
    assert_eq!(located.len(), 1);
    let data = located[0].data_start;
    assert_eq!(
        located[0].compressed_size, A_LEN,
        "the entry is stored whole"
    );
    // 30 (local header) + 8 (name) == 38, which is where the acceptance criteria corrupts.
    assert_eq!(data, 38, "the stored data starts where the criteria say");

    for offset in [0usize, 100, A_LEN - 1] {
        let mut zip = original.clone();
        corrupt_byte(&mut zip, data + offset);
        let error = read_entry(&zip, 0).expect_err(&format!(
            "a byte flipped at data + {offset} must not read back as the entry's contents"
        ));
        assert_eq!(error.kind(), ErrorKind::Malformed, "{error}");
        assert!(
            error.message().contains("CRC"),
            "a data corruption should read as a CRC mismatch, got: {error}"
        );
        assert!(
            error.message().contains(&format!("{:#010x}", crc32(&good))),
            "the message must name the CRC the archive promised: {error}"
        );
    }
}

/// A deflated entry must never return corrupted bytes without an error.
///
/// The first and middle bytes of the compressed data are refused outright. The *last* byte cannot
/// always be: a deflate stream is padded to a byte boundary, and flipping a padding bit changes no
/// decoded byte at all. That is not a missed corruption — there is nothing to detect — so the test
/// states the property that actually holds over every position: either the read fails as
/// [`ErrorKind::Malformed`], or it returns exactly the original bytes. A position that returned
/// *different* bytes with no error is the bug this job exists to remove.
#[test]
fn a_deflated_entry_never_returns_corrupted_bytes_without_an_error() {
    let pooled = deflatable_bytes();
    let original = write_zip(&[("text.txt", &pooled, false)]);
    let located = locate(&original);
    assert_eq!(located.len(), 1);
    let data = located[0].data_start;
    let length = located[0].compressed_size;
    assert!(length > 3, "the payload should deflate to more than a stub");

    for offset in [0usize, length / 2] {
        let mut zip = original.clone();
        corrupt_byte(&mut zip, data + offset);
        let error = read_entry(&zip, 0).expect_err(&format!(
            "a deflated entry corrupted at compressed byte {offset} must be refused"
        ));
        assert_eq!(error.kind(), ErrorKind::Malformed, "{error}");
    }

    for offset in 0..length {
        let mut zip = original.clone();
        corrupt_byte(&mut zip, data + offset);
        match read_entry(&zip, 0) {
            Err(error) => assert_eq!(
                error.kind(),
                ErrorKind::Malformed,
                "at byte {offset}: {error}"
            ),
            Ok(got) => assert_eq!(
                got, pooled,
                "flipping compressed byte {offset} changed the entry's contents with no error"
            ),
        }
    }
}

/// Both kinds of damage are `Malformed`, but a caller can tell them apart by message.
///
/// A CRC mismatch is "the bytes are there and they are wrong"; a truncation is "the bytes are
/// not all there". They share a kind because both mean "refuse", but a file manager that wants to
/// offer "download it again" (truncated) rather than "the file is damaged" needs the difference,
/// so it is pinned here rather than left to chance.
#[test]
fn a_crc_mismatch_is_told_apart_from_a_truncated_entry() {
    let good = stored_bytes();
    let original = write_zip(&[("aaaa.bin", &good, true)]);
    let located = locate(&original);
    let data = located[0].data_start;

    // Damage one data byte: full length, wrong contents.
    let mut wrong = original.clone();
    corrupt_byte(&mut wrong, data + 100);
    let crc_error = read_entry(&wrong, 0).expect_err("a CRC mismatch is refused");
    assert_eq!(crc_error.kind(), ErrorKind::Malformed);
    assert!(
        crc_error.message().contains("CRC"),
        "a CRC mismatch must say so: {crc_error}"
    );

    // Shrink the recorded compressed size below the uncompressed size: the reader runs out of
    // data before it has produced what the archive says the entry holds.
    let mut short = original.clone();
    let field = located[0].compressed_size_field;
    short[field..field + 4].copy_from_slice(&(A_LEN as u32 - 56).to_le_bytes());
    let short_error = read_entry(&short, 0).expect_err("a truncated entry is refused");
    assert_eq!(short_error.kind(), ErrorKind::Malformed);
    assert!(
        short_error.message().contains("truncated"),
        "a truncation must say so: {short_error}"
    );
    assert!(
        !crc_error.message().contains("truncated"),
        "the two damages must not share a message"
    );
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Stage an archive on disk so an independent tool can be pointed at it.
fn stage(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("zip-integrity-{name}.zip"));
    std::fs::write(&path, bytes).expect("writing the archive for the external tools");
    path
}

fn has(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
        .unwrap_or(false)
}

/// Runs `unzip -t` and `7z t` over `path`, returning whether the ones installed accepted it.
fn external_tests(path: &Path) -> Vec<(&'static str, bool)> {
    let mut results = Vec::new();
    if has("unzip") {
        let out = Command::new("unzip")
            .arg("-t")
            .arg(path)
            .output()
            .expect("unzip runs");
        results.push(("unzip", out.status.success()));
    }
    if has("7z") {
        let out = Command::new("7z")
            .arg("t")
            .arg(path)
            .output()
            .expect("7z runs");
        results.push(("7z", out.status.success()));
    }
    results
}

/// An archive both tools accept must be accepted here, and one they reject must be rejected here.
///
/// This is the check that makes the CRC comparison a correctness fix rather than a decoration:
/// the reader and the external tools have to agree on which archives are sound.
#[test]
fn the_external_tools_and_this_reader_agree_on_sound_and_damaged_archives() {
    if !has("unzip") && !has("7z") {
        eprintln!("neither unzip nor 7z is installed; nothing to compare against");
        return;
    }

    let good = stored_bytes();
    let deflated = deflatable_bytes();
    let intact = write_zip(&[("aaaa.bin", &good, true), ("text.txt", &deflated, false)]);
    let intact_path = stage("intact", &intact);
    let results = external_tests(&intact_path);
    assert!(!results.is_empty());
    for (tool, ok) in &results {
        assert!(ok, "{tool} refused an archive this crate wrote");
    }
    read_entry(&intact, 0).expect("an archive the tools accept must read here");
    read_entry(&intact, 1).expect("an archive the tools accept must read here");

    let located = locate(&intact);
    let mut damaged = intact.clone();
    corrupt_byte(&mut damaged, located[0].data_start + 100);
    let damaged_path = stage("damaged", &damaged);
    let results = external_tests(&damaged_path);
    for (tool, ok) in &results {
        assert!(!ok, "{tool} accepted an archive with a corrupted entry");
    }
    let error = read_entry(&damaged, 0).expect_err("a damaged archive must be refused here too");
    assert_eq!(error.kind(), ErrorKind::Malformed, "{error}");
}
