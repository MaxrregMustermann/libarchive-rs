//! Encrypted ZIP through the public API: ZipCrypto and WinZip AES, read with a password.
//!
//! Fixtures are generated at test time by the real tools on this machine — `7z` (ZipCrypto and
//! AES-128/192/256, stored and deflated) and Info-ZIP `zip` (ZipCrypto deflate) — because an
//! archive that only the code under test can produce proves nothing about the code under test.
//! Each fixture is first extracted by an independent tool (`7z x` or `unzip -p`) to prove the
//! fixture itself is valid; only then does this crate read it. If no tool is installed the tests
//! pass having asserted nothing, and say so.
//!
//! What is pinned, per fixture:
//!
//! * listing works without any password and marks the entries encrypted;
//! * opening without a password, or with the wrong one, reports `ErrorKind::Password`;
//! * opening with the password streams the exact plaintext bytes.

use std::io::Read;
use std::path::PathBuf;

use libarchive::{ArchiveReader, ArchiveWriter, ErrorKind, Format, NewEntry, OpenOptions, WriteOptions};

const PASSWORD: &str = "s3cret-password-42";

/// Plaintexts: one tiny file (writers store it) and one large enough to deflate.
fn plaintexts(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let tiny = b"a tiny secret\n".to_vec();
    let big: Vec<u8> = b"a deflatable line of secret text 0123456789\n".repeat(200).to_vec();
    std::fs::write(dir.join("tiny.txt"), &tiny).expect("write");
    std::fs::write(dir.join("big.txt"), &big).expect("write");
    vec![("tiny.txt".to_owned(), tiny), ("big.txt".to_owned(), big)]
}

fn tool(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|path| path.is_file())
    })
}

fn run(tool: &PathBuf, args: &[&str]) {
    let out = std::process::Command::new(tool)
        .args(args)
        .output()
        .expect("tool runs");
    assert!(out.status.success(), "{} {args:?} failed: {out:?}", tool.display());
}

/// One generated fixture: the archive, and the independent tool that proves it valid.
struct Fixture {
    archive: PathBuf,
    /// How to extract one entry's bytes with an independent tool: (tool, args...).
    oracle: (PathBuf, Vec<String>),
}

/// Generates every ZipCrypto and WinZip AES combination the tools on this machine can write.
fn generate(dir: &std::path::Path) -> Vec<(&'static str, Fixture)> {
    let sevenz = tool("7z");
    let zip = tool("zip");
    let mut fixtures = Vec::new();
    let tiny = dir.join("tiny.txt").to_string_lossy().into_owned();
    let big = dir.join("big.txt").to_string_lossy().into_owned();
    if let Some(sevenz) = sevenz.clone() {
        // (name, 7z -mem value, extra method args, which file)
        let cases: &[(&str, &str, &[&str], &str)] = &[
            ("7z-crypto-stored", "ZipCrypto", &[], &tiny),
            ("7z-crypto-deflate", "ZipCrypto", &["-mm=Deflate"], &big),
            ("7z-aes128-stored", "AES128", &[], &tiny),
            ("7z-aes192-stored", "AES192", &[], &tiny),
            ("7z-aes256-stored", "AES256", &[], &tiny),
            ("7z-aes128-deflate", "AES128", &["-mm=Deflate"], &big),
            ("7z-aes192-deflate", "AES192", &["-mm=Deflate"], &big),
            ("7z-aes256-deflate", "AES256", &["-mm=Deflate"], &big),
        ];
        for (name, mem, method, file) in cases {
            let archive = dir.join(format!("{name}.zip"));
            let archive_str = archive.to_string_lossy().into_owned();
            // Owned strings throughout: the arg list borrows them and outlives the push.
            let mem_arg = format!("-mem={mem}");
            let pw_arg = format!("-p{PASSWORD}");
            let mut args: Vec<&str> = vec!["a", "-tzip", "-y", &mem_arg, &pw_arg];
            args.extend_from_slice(method);
            args.push(&archive_str);
            args.push(file);
            run(&sevenz, &args);
            let oracle = (
                sevenz.clone(),
                vec!["x".to_owned(), "-so".to_owned(), pw_arg, archive_str],
            );
            fixtures.push((*name, Fixture { archive, oracle }));
        }
    }
    if let Some(zip) = zip {
        // Info-ZIP `zip` writes ZipCrypto (its `-e` prompts, so the password goes via `-P`).
        let archive = dir.join("infozip-crypto-deflate.zip");
        let archive_str = archive.to_string_lossy().into_owned();
        run(&zip, &["-P", PASSWORD, "-j", &archive_str, &big]);
        if let Some(unzip) = tool("unzip") {
            let oracle = (
                unzip,
                vec!["-P".to_owned(), PASSWORD.to_owned(), "-p".to_owned(), archive_str],
            );
            fixtures.push(("infozip-crypto-deflate", Fixture { archive, oracle }));
        }
    }
    fixtures
}

/// Extracts one entry with the fixture's oracle tool, proving the fixture valid independently.
fn oracle_extract(fixture: &Fixture, entry: &str) -> Vec<u8> {
    let (tool, args) = &fixture.oracle;
    let mut full: Vec<String> = args.clone();
    // `7z x -so archive.zip entry` prints just the entry; `unzip -p archive.zip entry` the same.
    full.push(entry.to_owned());
    let out = std::process::Command::new(tool)
        .args(&full)
        .output()
        .expect("oracle runs");
    assert!(out.status.success(), "oracle for {} failed: {out:?}", fixture.archive.display());
    out.stdout
}

#[test]
fn encrypted_entries_list_without_a_password_and_read_with_one() {
    let dir = tempfile::tempdir().expect("scratch");
    let expected = plaintexts(dir.path());
    let fixtures = generate(dir.path());
    if fixtures.is_empty() {
        eprintln!("neither 7z nor zip found; skipping the encrypted-ZIP tests");
        return;
    }
    assert!(fixtures.len() >= 2, "expected several fixtures, got {}", fixtures.len());
    for (name, fixture) in &fixtures {
        let bytes = std::fs::read(&fixture.archive).expect("fixture");
        // Listing works with no password and marks every entry encrypted.
        let reader =
            ArchiveReader::open(std::io::Cursor::new(bytes.clone()), &OpenOptions::default())
                .expect("listing needs no password");
        assert!(!reader.is_empty(), "{name} has entries");
        for entry in reader.entries() {
            assert!(entry.is_encrypted, "{name}: {} should be marked encrypted", entry.name);
        }
        // Every entry reads back exactly, through the streaming API in small chunks.
        for (index, entry) in reader.entries().iter().enumerate() {
            let want = oracle_extract(fixture, &entry.name);
            let plain = expected
                .iter()
                .find(|(file, _)| entry.name.ends_with(file.as_str()))
                .map(|(_, bytes)| bytes.clone())
                .unwrap_or(want.clone());
            assert_eq!(want, plain, "{name}: oracle disagrees with the plaintext");
            let mut reader = ArchiveReader::open(
                std::io::Cursor::new(bytes.clone()),
                &OpenOptions::default().with_password(PASSWORD),
            )
            .expect("open with password");
            let mut stream = reader.open_entry(index).expect("entry opens with password");
            // Small reads, so a buffering decoder cannot hide behind one big read.
            let mut got = Vec::new();
            let mut chunk = [0u8; 17];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => got.extend_from_slice(&chunk[..read]),
                    Err(error) => panic!("{name}: read failed: {error}"),
                }
            }
            assert_eq!(got, plain, "{name}: {} did not decrypt", entry.name);
        }
    }
}

/// Writing an encrypted ZIP must not return `Ok` on an unencrypted archive.
///
/// This build reads ZipCrypto and WinZip AES but writes no encryption, so a password passed to
/// `create` is refused before a byte is written. A silent `Ok` here is how a file manager with an
/// "encrypt" toggle ships plaintext while believing it shipped a secret.
#[test]
fn writing_an_encrypted_zip_is_refused_rather_than_written_in_plaintext() {
    let options = WriteOptions::default().with_password(PASSWORD);
    let error = match ArchiveWriter::create(std::io::Cursor::new(Vec::new()), Format::Zip, &options)
    {
        Ok(_) => panic!("a ZIP writer that cannot encrypt accepted a password"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unimplemented, "{error}");
}

/// A password supplied per entry, through `write_entry_with`, is refused too.
///
/// `create` with no password is legal, and the entry options are the other place a caller could
/// slip a secret past the check and get an unencrypted archive back.
#[test]
fn a_password_passed_to_write_entry_with_is_refused_too() {
    let sink = std::io::Cursor::new(Vec::new());
    let mut writer = ArchiveWriter::create(sink, Format::Zip, &WriteOptions::default())
        .expect("a plain ZIP writer");
    let options = WriteOptions::default().with_password(PASSWORD);
    let error = match writer.write_entry_with(
        &NewEntry::file("secret.txt"),
        &mut &b"plaintext"[..],
        &options,
    ) {
        Ok(()) => panic!("writing an entry with a password must not succeed unencrypted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unimplemented, "{error}");
}

/// Encrypted 7Z writing is refused as well, and this test is the guard on that.
///
/// The wrapper refuses a password at `create` because the vendored writer it adapts is not wired
/// for encryption here (see `writer/sevenz.rs`). Whatever the eventual decision, the one outcome
/// that is not allowed is `Ok` on a plaintext archive, so that is what is pinned.
#[test]
fn writing_an_encrypted_seven_z_is_refused_rather_than_written_in_plaintext() {
    let options = WriteOptions::default().with_password(PASSWORD);
    let error =
        match ArchiveWriter::create(std::io::Cursor::new(Vec::new()), Format::SevenZ, &options) {
            Ok(_) => panic!("a 7Z writer that cannot encrypt accepted a password"),
            Err(error) => error,
        };
    assert_eq!(error.kind(), ErrorKind::Unimplemented, "{error}");
}

#[test]
fn encrypted_entries_report_password_without_one_or_with_the_wrong_one() {
    let dir = tempfile::tempdir().expect("scratch");
    let _ = plaintexts(dir.path());
    let fixtures = generate(dir.path());
    if fixtures.is_empty() {
        eprintln!("neither 7z nor zip found; skipping the encrypted-ZIP tests");
        return;
    }
    for (name, fixture) in &fixtures {
        let bytes = std::fs::read(&fixture.archive).expect("fixture");
        for options in [OpenOptions::default(), OpenOptions::default().with_password("wrong")] {
            let mut reader =
                ArchiveReader::open(std::io::Cursor::new(bytes.clone()), &options)
                    .expect("listing never needs the password");
            for index in 0..reader.len() {
                let error = match reader.open_entry(index) {
                    Ok(_) => panic!("{name} entry {index} opened without the right password"),
                    Err(error) => error,
                };
                assert_eq!(
                    error.kind(),
                    ErrorKind::Password,
                    "{name} entry {index} should need a password"
                );
            }
        }
    }
}
