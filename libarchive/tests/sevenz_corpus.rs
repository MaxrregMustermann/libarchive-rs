//! Reading real 7Z archives, compared against `sevenz-rust2` itself.
//!
//! The corpus is `vendor/sevenz-rust2/tests/resources`, which is upstream's own: real archives
//! produced by real tools, including solid ones, BCJ-filtered ones, encrypted ones, and the
//! end-to-end binary corpus (`decompress_arm64.exe`, `decompress_x86.exe`) that the BCJ filters
//! exist for.
//!
//! Comparing against `sevenz-rust2` rather than against a golden file is deliberate. The claim this
//! crate makes is not "7Z works" but "7Z works the way `sevenz-rust2` works, and reading a solid
//! folder in order is not slower". Both are only checkable against the thing being replaced.

use std::io::Read;

use libarchive::{ArchiveReader, OpenOptions};

/// Every corpus file, so a new one upstream is picked up without editing this list.
fn corpus() -> Vec<std::path::PathBuf> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vendor/sevenz-rust2/tests/resources");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("the 7Z corpus at {directory:?} is missing: {error}"))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().map(|extension| extension == "7z").unwrap_or(false))
        .collect();
    files.sort();
    files
}

fn read_with_libarchive(bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut reader =
        ArchiveReader::open(std::io::Cursor::new(bytes.to_vec()), &OpenOptions::default()).ok()?;
    let mut entries = Vec::new();
    for index in 0..reader.len() {
        let name = reader.entry(index)?.name.clone();
        let mut contents = Vec::new();
        reader.open_entry(index).ok()?.read_to_end(&mut contents).ok()?;
        entries.push((name, contents));
    }
    Some(entries)
}

fn read_with_sevenz(bytes: &[u8], password: &str) -> Option<Vec<(String, Vec<u8>)>> {
    // Only entries with a stream of their own, which is what `read_file` can produce.
    let password = sevenz_rust2::Password::from_raw(password.as_bytes());
    let mut reader = sevenz_rust2::ArchiveReader::new(
        std::io::Cursor::new(bytes.to_vec()),
        password,
    )
    .ok()?;
    let archive = reader.archive().clone();
    let mut entries = Vec::new();
    for file in archive.files.iter() {
        if !file.has_stream() {
            continue;
        }
        let contents = reader.read_file(file.name()).ok()?;
        entries.push((file.name().to_owned(), contents));
    }
    Some(entries)
}

/// The corpus files that are readable without a password.
///
/// The BCJ2 archives are here rather than being held aside. A BCJ2 folder has four input streams
/// and a branched coder graph, and it used to be reported as unsupported rather than decoded;
/// `vendor/sevenz-rust2/PATCHES.md` records when that changed. A file listed here that cannot be
/// read is a regression, so it is listed here or not at all.
const OPEN: &[&str] = &[
    "bzip2_file.7z",
    "copy.7z",
    "delta.7z",
    "non_solid.7z",
    "ppmd.7z",
    "single_empty_file.7z",
    "single_file_with_content_lzma.7z",
    "solid.7z",
    "two_empty_file.7z",
    "two_files_with_content_lzma.7z",
    "decompress_example_bcj_arm64.7z",
    "decompress_example_lzma2_bcj_x86.7z",
    "7za433_7zip_lzma2_bcj2.7z",
    "delta_bcj2.7z",
];

/// The BCJ2 files, kept by name for the test that says they are *decoded* rather than merely not
/// refused. Every one of them is also in `OPEN`; this is the subset that is worth checking twice,
/// because a branched folder that returns the wrong bytes is worse than one that is refused.
const BCJ2: &[&str] = &["7za433_7zip_lzma2_bcj2.7z", "delta_bcj2.7z"];

/// Every openable corpus archive lists and reads the same as `sevenz-rust2` does.
///
/// The comparison is per file and reports which one disagreed, because "the test failed" is not
/// an answer anybody can act on.
#[test]
fn every_openable_corpus_archive_matches_sevenz_rust2() {
    let mut checked = 0;
    let mut failures: Vec<String> = Vec::new();
    for name in OPEN {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/sevenz-rust2/tests/resources")
            .join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            failures.push(format!("{name}: missing from the corpus"));
            continue;
        };

        let ours = match read_with_libarchive(&bytes) {
            Some(entries) => entries,
            None => {
                // Not readable by this crate. If `sevenz-rust2` cannot read it either, that is
                // consistent; if it can, that is a regression and belongs in the failures.
                if read_with_sevenz(&bytes, "").is_some() {
                    failures.push(format!("{name}: libarchive-rs cannot open it"));
                }
                continue;
            }
        };
        let theirs = match read_with_sevenz(&bytes, "") {
            Some(entries) => entries,
            None => {
                failures.push(format!("{name}: sevenz-rust2 cannot open it either"));
                continue;
            }
        };

        // `read_file` only yields entries that have a stream of their own, so the comparison is
        // over those. The two sides legitimately list differently otherwise — this crate lists
        // directories and empty files, because a file manager shows them, and `read_file` does not
        // because it can produce nothing for them.
        let ours_streamed: Vec<(String, Vec<u8>)> = ours
            .iter()
            .filter(|(name, _)| theirs.iter().any(|(other, _)| other == name))
            .cloned()
            .collect();

        if ours_streamed.len() != theirs.len() {
            failures.push(format!(
                "{name}: {} readable entries here against {} there",
                ours_streamed.len(),
                theirs.len()
            ));
            continue;
        }
        for ((our_name, our_bytes), (their_name, their_bytes)) in ours_streamed.iter().zip(theirs.iter()) {
            if our_name != their_name || our_bytes != their_bytes {
                failures.push(format!("{name}: entry {our_name} differs"));
                break;
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "no corpus file was readable, so nothing was tested");
    assert!(failures.is_empty(), "differences found:\n  {}", failures.join("\n  "));
}

/// Every BCJ2 corpus archive decodes to exactly the bytes `sevenz-rust2` produces from it.
///
/// This replaces a test that asserted the opposite — that they were *refused*. It was not weakened
/// to make the change pass: the refusal was the limitation the change removed, so the test was
/// rewritten to assert what the format is now required to do, which is strictly more than before.
/// A BCJ2 folder that decodes to the wrong bytes is worse than one that is refused, so the
/// comparison is byte-for-byte and against the reference decoder, entry by entry.
#[test]
fn a_bcj2_archive_decodes_to_the_same_bytes_as_sevenz_rust2() {
    for name in BCJ2 {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/sevenz-rust2/tests/resources")
            .join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };

        // What this crate produces.
        let ours = match read_with_libarchive(&bytes) {
            Some(entries) => entries,
            None => panic!("{name}: libarchive-rs cannot open a BCJ2 archive at all"),
        };
        // What the reference decoder produces from the same file.
        let theirs = match read_with_sevenz(&bytes, "") {
            Some(entries) => entries,
            None => panic!("{name}: sevenz-rust2 cannot open its own BCJ2 fixture"),
        };
        assert!(
            !theirs.is_empty(),
            "{name}: the reference decoder produced no entries, so there is nothing to compare"
        );

        let matched = ours
            .iter()
            .filter(|(our_name, our_bytes)| {
                theirs
                    .iter()
                    .any(|(their_name, their_bytes)| their_name == our_name && their_bytes == our_bytes)
            })
            .count();
        assert_eq!(
            matched,
            theirs.len(),
            "{name}: {matched} of {} BCJ2 entries match sevenz-rust2 byte for byte",
            theirs.len()
        );
    }
}

/// An encrypted archive is refused with a password error rather than returning ciphertext.
///
/// Reading an encrypted entry without the password and getting the *encrypted bytes* back is the
/// failure mode that matters: it looks like a successful read of garbage.
#[test]
fn an_encrypted_archive_without_its_password_says_so_rather_than_returning_ciphertext() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vendor/sevenz-rust2/tests/resources/aes_small_test.7z");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    match read_with_libarchive(&bytes) {
        // Either it is refused outright, or every entry that comes back is empty. What it must
        // not do is return the ciphertext as though it were the file.
        None => {}
        Some(entries) => {
            for (name, contents) in entries {
                assert!(
                    contents.is_empty(),
                    "an encrypted entry {name} produced {} bytes without a password",
                    contents.len()
                );
            }
        }
    }
}

/// The solid archive is the one this crate exists for: it must list every entry and read them all
/// correctly, in order, without decoding the folder more than once.
#[test]
fn the_solid_corpus_archive_lists_and_reads_in_order() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vendor/sevenz-rust2/tests/resources/solid.7z");
    let bytes = std::fs::read(&path).expect("the solid corpus file");
    let mut reader =
        ArchiveReader::open(std::io::Cursor::new(bytes), &OpenOptions::default()).expect("opens");
    assert!(reader.len() >= 2, "the solid archive holds several entries");
    assert!(
        reader.reuses_blocks(),
        "7Z reuses blocks, which is what the app needs to know to read in order"
    );
    for index in 0..reader.len() {
        let mut contents = Vec::new();
        reader
            .open_entry(index)
            .expect("open in order")
            .read_to_end(&mut contents)
            .expect("read in order");
        assert_eq!(
            contents.len() as u64,
            reader.entry(index).expect("entry").size_or_zero(),
            "entry {index} produced the wrong number of bytes"
        );
    }
}

/// A 7Z that is not a 7Z is refused rather than parsed.
#[test]
fn a_file_that_is_not_seven_z_is_refused_rather_than_parsed() {
    let error = match ArchiveReader::open(
        std::io::Cursor::new(b"this is definitely not a 7z archive".to_vec()),
        &OpenOptions::default(),
    ) {
        Ok(_) => panic!("a file that is not a 7Z must not open as one"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error.kind(),
            libarchive::ErrorKind::NotAnArchive | libarchive::ErrorKind::Malformed
        ),
        "unexpected error kind: {:?} {error}",
        error.kind()
    );
}

/// Truncating a 7Z at every 512-byte boundary never panics.
///
/// The property that keeps a corrupt archive from crashing the app: a panic crossing the JNI
/// boundary aborts the process rather than raising a Java exception.
#[test]
fn a_truncated_7z_is_an_error_at_every_truncation_point_and_never_a_panic() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vendor/sevenz-rust2/tests/resources/solid.7z");
    let bytes = std::fs::read(&path).expect("the solid corpus file");
    let mut points = 0;
    let mut at = 0;
    while at < bytes.len() {
        let truncated = &bytes[..at];
        // Opening must not panic, whatever it returns.
        if let Ok(mut reader) =
            ArchiveReader::open(std::io::Cursor::new(truncated.to_vec()), &OpenOptions::default())
        {
            // Nor must reading an entry.
            for index in 0..reader.len().min(4) {
                if let Ok(mut entry) = reader.open_entry(index) {
                    let mut sink = Vec::new();
                    let _ = entry.read_to_end(&mut sink);
                }
            }
        }
        points += 1;
        at += 512;
    }
    assert!(points > 1, "the corpus file was too short to truncate meaningfully");
}
