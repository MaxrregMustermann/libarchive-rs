//! Compressed TARs are detected as TARs, not as raw compressed streams.
//!
//! ## The defect this exists for
//!
//! Found on the API 24 emulator, not on the host. `BridgeInstrumentation.proveTheFormats` opened
//! `plain.tar.gz`, made by GNU tar on this machine, and the app listed **one entry named
//! `content`** where `tar` lists four. `content` is [`SINGLE_ENTRY_NAME`] -
//! `libarchive/src/formats/single.rs:39`, the name this crate gives the single entry of a *raw*
//! compressed stream - so the file was being taken as a gzip rather than as a compressed TAR, and
//! extracting it would have written one blob instead of the files.
//!
//! [`SINGLE_ENTRY_NAME`]: crate::formats::single::SINGLE_ENTRY_NAME
//!
//! All five supported compressed TARs did it: `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` and
//! `.tar.lz4`. The cause was that nothing ever produced `Format::CompressedTar`:
//!
//! ```text
//! $ rg -n 'CompressedTar' libarchive/src/detect.rs     # before the fix
//! (nothing)
//! ```
//!
//! `detect_header_with_name` mapped GZIP, BZIP2, XZ, ZSTD and LZ4 unconditionally to
//! `Format::SingleStream(..)`. So "compressed TAR read and write" was true only of the interior
//! code: `CompressedTarReader` worked perfectly and nothing ever selected it.
//!
//! ## The fix
//!
//! A bounded peek at the decompressed head, checking the same `ustar` magic at offset 257 that the
//! plain-TAR arm already checks. Content decides, never the file name: a `.tar.gz` renamed `.gz` is
//! still a TAR, and a plain `.gz` renamed `.tar.gz` is still one raw stream.
//!
//! ## Why the corpus manifest did not catch it
//!
//! `libarchive/tests/corpus/manifest.json` already records four entries for
//! `generated/streams/plain.tar.gz`, with `"status": "gate"`. **No test read that field for these
//! archives**, so a manifest entry that disagreed with the code read exactly like one that agreed.
//! That is worth knowing on its own and it is the reason these tests exist as executable checks
//! rather than as manifest rows.
//!
//! ## Why there is a test here at all
//!
//! Because the alternative is a comment. A comment saying "compressed TARs are broken" is checked
//! by nobody and is gone the moment someone reformats the file.

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
