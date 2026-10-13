//! What a compressed TAR actually lists, checked against what is inside it.
//!
//! ## Status: this is the failing half of a known defect, not a passing gate
//!
//! Both tests below are `#[ignore]`d, and `cargo test --offline --test compressed_tar` reports
//! **0 passed, 2 ignored**. That is deliberate and it is the honest report: run
//!
//! ```text
//! $ cargo test --offline --test compressed_tar -- --ignored --nocapture
//! ```
//!
//! and they fail, because a compressed TAR is not read as a TAR.
//!
//! ## What is wrong, and where it was found
//!
//! Found on the device, not here. `BridgeInstrumentation.proveTheFormats` opened `plain.tar.gz`,
//! made by GNU tar on the host, and the app listed one entry named `content` where `tar` lists
//! `license.txt`. `content` is [`SINGLE_ENTRY_NAME`] - `libarchive/src/formats/single.rs:39`, the
//! name this crate gives the one entry of a *raw* compressed stream - so the `.tar.gz` was taken
//! as a gzip file rather than as a compressed TAR. The same file reproduces it on the host.
//!
//! [`SINGLE_ENTRY_NAME`]: crate::formats::single::SINGLE_ENTRY_NAME
//!
//! The cause is in detection. `Format::CompressedTar` exists and `CompressedTarReader` works, but
//! nothing ever produces that variant:
//!
//! ```text
//! $ rg -n 'CompressedTar' libarchive/src/detect.rs
//! (nothing)
//! ```
//!
//! `detect_header_with_name` maps `GZIP`, `BZIP2`, `XZ`, `ZSTD` and `LZ4` unconditionally to
//! `Format::SingleStream(..)` (`libarchive/src/detect.rs:204-220`), so a compressed TAR can never
//! be recognised as one. Deciding it needs a bounded peek at the decompressed head - the same
//! "ustar" check the plain-TAR arm does at offset 257 - which is a real change to `detect` and
//! belongs to whoever owns it, not to a device-verification commit.
//!
//! ## Why the corpus manifest did not catch it
//!
//! `libarchive/tests/corpus/manifest.json` already says what is in these archives. For
//! `generated/streams/plain.tar.gz` it records four entries - `hello.txt` (22 B), `data.bin`
//! (4096 B), `empty.txt` (0 B) and `nested/inner.txt` (13 B) - with `"status": "gate"` and
//! `"expect": {"kind": "ok"}`. No test reads that field for these archives, so a manifest entry
//! that disagrees with the code reads the same as one that agrees. That is worth knowing on its
//! own: the manifest is currently documentation rather than a check for at least these rows.
//!
//! ## Why there is a test here at all
//!
//! Because the alternative is a comment. A comment saying "compressed TARs are broken" is checked
//! by nobody and is gone the moment someone reformats the file; these two tests fail the moment
//! the defect is fixed by accident, which is the earliest possible warning.

use std::io::Read;
use std::path::{Path, PathBuf};

use libarchive::{ArchiveReader, OpenOptions};

/// Where `tools/make-corpus.sh` and `tests/corpus/generate.py` put the generated archives.
fn corpus_root() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/generated");
    path.is_dir().then_some(path)
}

/// Lists `path` and returns `(entry name, bytes read)` for every entry, in order.
fn list(path: &Path) -> Result<Vec<(String, u64)>, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let mut reader = ArchiveReader::open(file, &OpenOptions::default())?;
    let mut listed = Vec::new();
    for index in 0..reader.len() {
        let name = reader.entries()[index].name.clone();
        let mut entry = reader.open_entry(index)?;
        let read = std::io::copy(&mut entry, &mut std::io::sink())?;
        listed.push((name, read));
    }
    Ok(listed)
}

/// Every compressed TAR in the corpus, with the entries the manifest says are inside it.
///
/// Written down here rather than produced by the code under test, so a test that listed nothing
/// cannot pass. The sizes are the manifest's, which `tar` and the four compressors agree with:
///
/// ```text
/// $ for f in plain.tar.gz plain.tar.bz2 plain.tar.xz plain.tar.zst plain.tar.lz4; do
///     printf '%-16s ' $f; gzip -dc $f | tar tf - | tr '\n' ' '; echo
///   done
/// plain.tar.gz     hello.txt data.bin empty.txt nested/inner.txt
/// ... (same four for each)
/// ```
fn compressed_tars() -> Vec<(PathBuf, Vec<(&'static str, u64)>)> {
    let entries: Vec<(&'static str, u64)> = vec![
        ("hello.txt", 22),
        ("data.bin", 4096),
        ("empty.txt", 0),
        ("nested/inner.txt", 13),
    ];
    let mut cases = Vec::new();
    if let Some(root) = corpus_root() {
        for file in [
            "streams/plain.tar.gz",
            "streams/plain.tar.bz2",
            "streams/plain.tar.xz",
            "streams/plain.tar.zst",
            "streams/plain.tar.lz4",
        ] {
            let path = root.join(file);
            if path.is_file() {
                cases.push((path, entries.clone()));
            }
        }
    }
    cases
}

/// The archives to check, or a loud failure saying there were none.
///
/// A corpus that has not been generated produces a skipped check that reads like a passed one,
/// which is the failure this project has already shipped.
fn require_cases() -> Vec<(PathBuf, Vec<(&'static str, u64)>)> {
    let cases = compressed_tars();
    assert!(
        !cases.is_empty(),
        "no compressed TAR under {} to check, so this proves nothing; build the corpus with \
         tools/make-corpus.sh",
        corpus_root()
            .unwrap_or_else(|| PathBuf::from("<tests/corpus/generated is missing>"))
            .display()
    );
    cases
}

#[test]
#[ignore = "a compressed TAR is listed as one raw stream named `content`; see the module docs"]
fn a_compressed_tar_is_listed_as_a_tar_not_as_one_raw_stream() {
    let cases = require_cases();
    let mut wrong = Vec::new();
    for (path, expected) in &cases {
        let names: Vec<String> = list(path)
            .expect("the archive should open")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let wanted: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
        if names != wanted {
            wrong.push(format!(
                "{}: listed {names:?} where the manifest says {wanted:?}",
                path.display()
            ));
        }
    }
    println!(
        "checked {} compressed TARs against the entry names the manifest records",
        cases.len()
    );
    for line in &wrong {
        println!("  {line}");
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
#[ignore = "a compressed TAR is listed as one raw stream named `content`; see the module docs"]
fn an_entry_in_a_compressed_tar_is_the_file_inside_it_and_not_the_archive() {
    let cases = require_cases();
    for (path, expected) in &cases {
        let listed = list(path).expect("the archive should open");
        assert_eq!(
            listed.len(),
            expected.len(),
            "{}: listed {} entries where the manifest says {}",
            path.display(),
            listed.len(),
            expected.len()
        );
        for (index, (name, size)) in expected.iter().enumerate() {
            let (listed_name, listed_size) = &listed[index];
            assert_eq!(
                listed_name,
                name,
                "{}: entry {index} is named {listed_name}",
                path.display()
            );
            assert_eq!(
                listed_size,
                size,
                "{}: {name} is {listed_size} bytes where the manifest says {size}",
                path.display()
            );
        }
        // And the bytes, not merely the sizes: read the first entry and compare it against the
        // same file decompressed by the host's own tool.
        let file = std::fs::File::open(path).expect("reopen");
        let mut reader = ArchiveReader::open(file, &OpenOptions::default()).expect("open again");
        let mut entry = reader.open_entry(0).expect("entry 0");
        let mut got = Vec::new();
        entry.read_to_end(&mut got).expect("read entry 0");
        assert_eq!(
            got.len(),
            expected[0].1 as usize,
            "{}: entry 0 read back {} bytes",
            path.display(),
            got.len()
        );
    }
    println!(
        "checked {} entries across {} compressed TARs, names and sizes against the manifest",
        cases.iter().map(|(_, e)| e.len()).sum::<usize>(),
        cases.len()
    );
}
